//! La API local del MCP de nido: `POST /api/v1/mcp/<herramienta>`, lo que piden las
//! herramientas de `nido mcp` (ver `mcp.rs`) desde una sesión de este equipo.
//!
//! Quien llama va con el token principal y dice qué sesión es en `X-Nido-Session` (su id). El
//! cuerpo son los argumentos de la herramienta y la respuesta, lo que lee el modelo. Los errores
//! se escriben en español, como en el resto de nido, y salen en inglés: `nido mcp` no manda
//! `Accept-Language`.
//!
//! Los mensajes de `send` esperan en la bandeja de la sesión de destino (`inbox.rs`); su
//! `nido mcp` la recoge con `GET /api/v1/mcp/inbox` y confirma lo recogido con `POST
//! /api/v1/mcp/inbox/ack`.
//!
//! Las sesiones de otro equipo del grupo piden lo mismo por el canal «peer» (`e2e::MODE_PEER`):
//! `peer_call` atiende esas peticiones con las mismas herramientas, salvo `alert`. Quien llama es
//! un [`Caller`]: una sesión de aquí o una de otro equipo, siempre con su dirección canónica.
//! Lo que pide una de aquí para una sesión de otro equipo se le pasa a ese equipo por el mismo
//! canal (`AppState::peer_member`), y `sessions` junta las de todos. Lo que pide una de otro
//! equipo se queda en las de aquí: no se pasa a un tercero.

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path as UrlPath, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::group::Member;
use crate::inbox::{self, Claim, Msg};
use crate::prompt::{self, Menu};
use crate::proto::{Hello, Role};
use crate::registry::{ClaudeLink, Registry, Session, new_id, now, now_ms};
use crate::server::{self, AppState, SessionView, error};
use crate::transcript::{self, ChatItem};
use crate::{auth, claude, e2e, i18n, launch};

/// Lo más que ocupa la respuesta de `read` (el JSON entero): fuera antes los turnos más antiguos.
pub const MAX_READ: usize = 48 * 1024;
/// Con `wait`, cada cuánto se mira la sesión.
pub const WAIT_STEP: Duration = Duration::from_secs(2);
/// Con `wait`, cuánto se espera si no se dice, y como mucho (s).
pub const WAIT_DEFAULT_S: u64 = 600;
pub const WAIT_MAX_S: u64 = 1800;
/// Lo más que puede ocupar el mensaje inicial de `open` (va en la orden de Claude), y un
/// mensaje de `send`.
pub const MAX_PROMPT: usize = 24 * 1024;
/// Lo más largo que puede ser el modelo de `open` (en caracteres).
pub const MAX_MODEL: usize = 128;
/// Cuántas sesiones abiertas con `open` desde un mismo equipo (este también) puede haber aquí en
/// marcha a la vez, y apuntadas (en marcha o no): un equipo del grupo no llena este de Claudes.
/// Las paradas cuentan solo para el segundo tope, más holgado: las huérfanas no dejan fuera al
/// equipo.
pub const MAX_REMOTE_OPEN: usize = 10;
pub const MAX_REMOTE_REGISTERED: usize = 50;

/// ¿Se puede abrir otra desde ese equipo? `alive`: si está en marcha cada una de las que ya abrió.
pub fn open_allowed(alive: impl Iterator<Item = bool>) -> bool {
    let (mut running, mut registered) = (0, 0);
    for a in alive {
        registered += 1;
        running += usize::from(a);
    }
    running < MAX_REMOTE_OPEN && registered < MAX_REMOTE_REGISTERED
}
/// `send` con `wait`: cuánto se espera, como mucho, a que la sesión recoja el mensaje y se ponga
/// a trabajar.
pub const PICKUP: Duration = Duration::from_secs(30);
/// Una sesión viva sin channel recibe mensajes (en cola) si arrancó hace menos de esto: aún se
/// está poniendo en marcha su `nido mcp` (p. ej., recién abierta con `open`).
pub const STARTUP: Duration = Duration::from_secs(120);
/// Con `last`, cuánto del final de la transcripción se lee primero (si no salen bastantes turnos,
/// cada vez cuatro veces más).
const TAIL: u64 = 256 * 1024;
/// Lo que se espera a cada equipo cuando se pregunta a todos a la vez (`sessions`, las alertas).
pub const FAN_OUT: Duration = Duration::from_secs(5);
/// Cuánto se recuerda dónde estaba la conversación de una sesión al encolarle un mensaje.
pub const MARK_TTL: Duration = Duration::from_secs(30 * 60);
/// Cuánto se recuerda si vive la sesión madre (de otro equipo) de una abierta con `open`.
pub const PARENT_TTL: Duration = Duration::from_secs(60);

// ---------- Equipos y direcciones ----------

/// Este equipo: su nombre, su id en el hub si está conectado y los demás equipos del grupo.
#[derive(Debug, Clone, PartialEq)]
pub struct Machine {
    pub name: String,
    pub key: Option<String>,
    pub others: Vec<Member>,
}

impl Machine {
    pub fn of(st: &AppState) -> Self {
        Self {
            name: <AppState as e2e::Host>::host_name(st),
            key: st.hub.pair_info().map(|(_, id)| id),
            others: st.group.others(),
        }
    }

    /// Cuántos equipos del grupo (este y los demás) se llaman `name`, sin mirar mayúsculas.
    /// Un id en el hub (de este o de otro) que se escribe igual también cuenta: el nombre no
    /// valdría de etiqueta, porque `find` mira antes los ids.
    fn named(&self, name: &str) -> usize {
        let same = |n: &str| n.eq_ignore_ascii_case(name);
        let others = |f: &dyn Fn(&Member) -> bool| self.others.iter().filter(|m| f(m)).count();
        usize::from(same(&self.name))
            + usize::from(self.key.as_deref().is_some_and(same))
            + others(&|m| same(&clean_name(&m.name)))
            + others(&|m| same(&m.id))
    }

    /// Cómo se escribe este equipo en las direcciones que ve el modelo: su nombre, si ningún
    /// otro del grupo se llama igual (WSL toma el del Windows en que corre) y no es el id de
    /// ninguno; si no, su id en el hub.
    pub fn label(&self) -> String {
        match &self.key {
            Some(k) if self.named(&self.name) > 1 => k.clone(),
            _ => self.name.clone(),
        }
    }

    /// Lo mismo para otro equipo del grupo.
    pub fn label_of(&self, m: &Member) -> String {
        let name = clean_name(&m.name);
        if !name.is_empty() && self.named(&name) == 1 {
            name
        } else {
            m.id.clone()
        }
    }

    /// El nombre de otro equipo, para enseñarlo (sin nombre, su id).
    pub fn name_of(m: &Member) -> String {
        let name = clean_name(&m.name);
        if name.is_empty() { m.id.clone() } else { name }
    }

    /// Qué equipo es `machine` (su id en el hub, su nombre o lo que dé `label`): este (`None`) u
    /// otro del grupo. Primero por el id; un nombre que llevan varios no vale (hay que dar el id).
    pub fn find(&self, machine: &str) -> Result<Option<&Member>, Fail> {
        let m = machine.trim();
        if self.key.as_deref() == Some(m) {
            return Ok(None);
        }
        if let Some(o) = self.others.iter().find(|o| o.id == m) {
            return Ok(Some(o));
        }
        let mine = self.name.eq_ignore_ascii_case(m);
        let named: Vec<&Member> = self
            .others
            .iter()
            .filter(|o| clean_name(&o.name).eq_ignore_ascii_case(m))
            .collect();
        match (mine, &named[..]) {
            (true, []) => Ok(None),
            (false, [o]) => Ok(Some(o)),
            (false, []) => Err(fail(
                StatusCode::NOT_FOUND,
                format!("«{m}» no es ningún equipo del grupo"),
            )),
            _ => Err(bad_request(server::ambiguous_member(m))),
        }
    }

    /// Cómo se apunta este equipo en `opened_by`: su id en el hub o, sin él, su nombre.
    pub fn key(&self) -> &str {
        self.key.as_deref().unwrap_or(&self.name)
    }

    /// ¿Es este equipo? Por el nombre (sin mirar mayúsculas) o por el id en el hub.
    pub fn is_me(&self, machine: &str) -> bool {
        let m = machine.trim();
        m.eq_ignore_ascii_case(&self.name) || self.key.as_deref() == Some(m)
    }

    /// La dirección canónica de una sesión de este equipo (`<id en el hub o nombre>/<id>`).
    pub fn address(&self, id: &str) -> String {
        format!("{}/{id}", self.key())
    }

    /// La dirección que ve el modelo de una sesión de este equipo (`<label>/<id>`).
    pub fn shown(&self, id: &str) -> String {
        format!("{}/{id}", self.label())
    }
}

/// Dónde está una sesión, según cómo la escribió quien llama.
#[derive(Debug, PartialEq)]
pub enum Where<'a> {
    /// En este equipo, con este id.
    Here(&'a str),
    /// En otro equipo del grupo.
    There { machine: Member, id: &'a str },
}

/// `equipo/id` (el equipo como lo acepta `Machine::find`) o solo `id`: este equipo.
pub fn locate<'a>(me: &Machine, address: &'a str) -> Result<Where<'a>, Fail> {
    let address = address.trim();
    Ok(match address.rsplit_once('/') {
        Some((m, id)) if !m.trim().is_empty() => match me.find(m)? {
            None => Where::Here(id.trim()),
            Some(o) => Where::There {
                machine: o.clone(),
                id: id.trim(),
            },
        },
        Some((_, id)) => Where::Here(id.trim()),
        None => Where::Here(address),
    })
}

/// El id de la sesión si `address` es de este equipo por su nombre o su id en el hub, sin mirar
/// los demás equipos: así se leen las direcciones canónicas (`opened_by`) y lo que pide otro
/// equipo.
fn here<'a>(me: &Machine, address: &'a str) -> Option<&'a str> {
    let address = address.trim();
    match address.rsplit_once('/') {
        Some((m, id)) if m.trim().is_empty() || me.is_me(m) => Some(id.trim()),
        Some(_) => None,
        None => Some(address),
    }
}

// ---------- Quién llama ----------

/// La sesión que llama a una herramienta: una de este equipo (por la API local) o una de otro
/// equipo del grupo (por el canal «peer»). Lo que se apunta o se compara de ella va por su
/// dirección canónica (`address`), así que una de otro equipo con el mismo id que una de aquí es
/// otra. La de otro equipo no está en el registro: no se busca en él.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Caller {
    /// Su equipo: el id en el hub (la de aquí, sin hub, el nombre del equipo).
    pub machine_key: String,
    pub machine_name: String,
    pub id: String,
    pub name: String,
    /// Es de este equipo.
    pub local: bool,
}

/// `params.caller` de una petición de otro equipo. Lo que no conoce se deja: una versión más
/// nueva puede mandar más.
#[derive(Deserialize)]
struct CallerArg {
    id: String,
    #[serde(default)]
    name: String,
}

/// Un nombre que llega de otro equipo: sin caracteres de control, sin espacios a los lados y de
/// 64 caracteres como mucho.
fn clean_name(n: &str) -> String {
    let n: String = n.chars().filter(|c| !c.is_control()).collect();
    n.trim().chars().take(64).collect()
}

impl Caller {
    /// Una sesión de este equipo (la de `X-Nido-Session`).
    pub fn local(me: &Machine, id: &str, name: &str) -> Self {
        Self {
            machine_key: me.key().to_owned(),
            machine_name: me.name.clone(),
            id: id.to_owned(),
            name: name.to_owned(),
            local: true,
        }
    }

    /// Una sesión de otro equipo del grupo. El equipo sale de su clave Noise, ya comprobada
    /// (`member`), nunca de lo que diga; la sesión, de `caller` (`{id, name}`), que vale porque
    /// el equipo es del grupo. El id, como los de nido (lo mismo que `NIDO_SESSION`). Un miembro
    /// cuyo id es el de este equipo (o su nombre, si no hay hub) no: su dirección sería de aquí.
    /// Con el mismo nombre sí: WSL toma el del Windows en que corre.
    pub fn remote(
        me: &Machine,
        member: &crate::group::Member,
        caller: Option<Value>,
    ) -> Result<Self, Fail> {
        let machine_name = clean_name(&member.name);
        if me.is_me(&member.id) {
            return Err(bad_request(
                "la sesión que llama no puede ser de este equipo".into(),
            ));
        }
        let invalid = |e: String| bad_request(format!("sesión que llama no válida: {e}"));
        let c: CallerArg = serde_json::from_value(
            caller.ok_or_else(|| bad_request("falta la sesión que llama".into()))?,
        )
        .map_err(|e| invalid(e.to_string()))?;
        let id = crate::mcp::session_from(Some(c.id.clone()))
            .filter(|id| id.len() <= 64)
            .ok_or_else(|| invalid(format!("{:?}", c.id)))?;
        let name = clean_name(&c.name);
        Ok(Self {
            machine_key: member.id.clone(),
            machine_name: if machine_name.is_empty() {
                member.id.clone()
            } else {
                machine_name
            },
            name: if name.is_empty() { id.clone() } else { name },
            id,
            local: false,
        })
    }

    /// Su dirección canónica: `<equipo>/<id>`.
    pub fn address(&self) -> String {
        format!("{}/{}", self.machine_key, self.id)
    }

    /// Con qué lleva sus posiciones de lectura (`Cursors`): la de aquí, con su id, que es como
    /// se olvidan al borrarla; la de otro equipo, con su dirección (un id no lleva «/»: no se
    /// confunden). La posición se mueve al leer, antes de contestar: si la respuesta no le llega
    /// a la de otro equipo (el hub tarda más de la cuenta), esos turnos ya cuentan como leídos y
    /// los pierde (con `last` los puede volver a pedir).
    pub fn reader(&self) -> String {
        if self.local {
            self.id.clone()
        } else {
            self.address()
        }
    }

    /// ¿Abrió ella la sesión de aquí con este `opened_by`? La de aquí, como dirección de este
    /// equipo (ver `opened_by`); la de otro, por su dirección tal cual.
    pub fn owns(&self, me: &Machine, opened_by: Option<&str>) -> bool {
        if self.local {
            self::opened_by(me, opened_by, &self.id)
        } else {
            opened_by.is_some_and(|o| o.trim() == self.address())
        }
    }
}

/// ¿La abrió la sesión `caller` de este equipo? `opened_by` se lee como dirección: se apuntó con
/// el id en el hub o, si entonces no había conexión, con el nombre del equipo. Guarda el id en el
/// hub de cuando se abrió: si el equipo se vuelve a emparejar y el hub le da otro, la madre deja
/// de poder contestar y cerrar a sus hijas.
pub fn opened_by(me: &Machine, opened_by: Option<&str>, caller: &str) -> bool {
    opened_by.is_some_and(|o| here(me, o) == Some(caller))
}

// ---------- Estado, turnos y pregunta ----------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Working,
    Idle,
    /// Tiene un menú en pantalla: una pregunta, un permiso, el plan…
    Question,
    /// No está en marcha.
    Stopped,
}

impl Status {
    /// Ya no hace falta esperar: terminó, pregunta o no está en marcha.
    pub fn settled(self) -> bool {
        self != Status::Working
    }
}

/// El estado de una sesión de Claude. Viva y sin Claude a la vista está arrancando (recién
/// abierta, antes de que Claude se apunte): cuenta como trabajando.
pub fn status(alive: bool, running: bool, busy: bool, menu: bool) -> Status {
    if !alive {
        Status::Stopped
    } else if menu {
        Status::Question
    } else if !running || busy {
        Status::Working
    } else {
        Status::Idle
    }
}

/// Un turno de la conversación, para que lo lea un modelo.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Turn {
    /// `user`, `assistant`, `tool` (una línea: la herramienta y lo esencial) o `notice` (avisos
    /// de Claude Code, como el final de una orden en segundo plano).
    pub role: &'static str,
    pub text: String,
}

/// Los turnos de lo que da `/chat` (los mismos mensajes), con solo quién y qué.
pub fn turns(items: Vec<ChatItem>) -> Vec<Turn> {
    items
        .into_iter()
        .filter_map(|item| {
            let (role, text) = match item {
                ChatItem::User { text, images, .. } => {
                    let text = match images {
                        0 => text,
                        1 => format!("{text} [1 image]"),
                        n => format!("{text} [{n} images]"),
                    };
                    ("user", text)
                }
                ChatItem::Assistant { text, .. } => ("assistant", text),
                ChatItem::Notice { text, .. } => ("notice", text),
                ChatItem::Tool { name, summary, .. } if summary.trim().is_empty() => ("tool", name),
                ChatItem::Tool { name, summary, .. } => ("tool", format!("{name}: {summary}")),
            };
            let text = text.trim();
            (!text.is_empty()).then(|| Turn {
                role,
                text: text.to_owned(),
            })
        })
        .collect()
}

/// La pregunta pendiente (el menú en pantalla), para `answer`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Question {
    /// `question`, `permission`, `plan`, `review`, `limit` o `menu`.
    pub kind: prompt::Kind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Las opciones, por su etiqueta.
    pub options: Vec<String>,
    /// Una opción es un campo de texto: se contesta con `text`.
    pub free_text: bool,
}

impl From<&Menu> for Question {
    fn from(m: &Menu) -> Self {
        Self {
            kind: m.kind,
            title: m.title.clone(),
            options: m.options.iter().map(|o| o.label.clone()).collect(),
            free_text: m.options.iter().any(|o| o.input),
        }
    }
}

/// Lo que devuelve `read`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ReadOut {
    pub state: Status,
    pub turns: Vec<Turn>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub question: Option<Question>,
    /// Se quitaron turnos antiguos (o se cortó el único) para no pasar de `MAX_READ`.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
    /// Mensajes para ella que no llegaron (ver `delivery_error`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delivery_error: Option<String>,
    /// La pregunta es la de confiar en la carpeta: solo la contesta el usuario (ver
    /// `trust_note`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

fn json_len<T: Serialize>(v: &T) -> usize {
    serde_json::to_vec(v).map_or(0, |b| b.len())
}

const CUT: &str = " […]";

/// Recorta la salida de `read` a `max` bytes de JSON: fuera los turnos más antiguos y, si el
/// que queda aún no cabe, su texto se corta por el final.
pub fn fit(out: &mut ReadOut, max: usize) {
    let mut total = json_len(out);
    if total <= max {
        return;
    }
    // Lo que ocupa cada turno con su coma: así no hay que medirlo todo otra vez por cada uno.
    let mut drop = 0;
    while total > max && drop + 1 < out.turns.len() {
        total -= json_len(&out.turns[drop]) + 1;
        drop += 1;
    }
    if drop > 0 {
        out.turns.drain(..drop);
        out.truncated = true;
    }
    let total = json_len(out);
    if total > max
        && let Some(t) = out.turns.first_mut()
    {
        // Lo que cabe del texto: el sitio que deja lo demás, carácter a carácter como va en JSON.
        let room = max.saturating_sub(total - json_len(&t.text) + 2 + CUT.len());
        let mut used = 0;
        let keep = t
            .text
            .char_indices()
            .find(|&(_, c)| {
                used += json_width(c);
                used > room
            })
            .map_or(t.text.len(), |(i, _)| i);
        t.text.truncate(keep);
        t.text.push_str(CUT);
        out.truncated = true;
    }
}

/// Lo que ocupa un carácter dentro de una cadena JSON (como la escribe serde_json).
fn json_width(c: char) -> usize {
    match c {
        '"' | '\\' | '\n' | '\r' | '\t' | '\u{8}' | '\u{c}' => 2,
        c if (c as u32) < 0x20 => 6,
        c => c.len_utf8(),
    }
}

/// Lo más que se guardan posiciones de lectoras de otros equipos (las que llevan «/» en su
/// clave, ver `Caller::reader`): pasado eso, fuera las usadas hace más tiempo. Las de aquí se
/// olvidan al borrar la sesión; de las de otro equipo no se entera nadie.
pub const MAX_REMOTE_READERS: usize = 256;

/// Hasta dónde leyó cada sesión la conversación de otra: (la que lee, la leída) → (su
/// transcripción, posición). Solo en memoria: al reiniciar nido, se vuelve a leer todo.
#[derive(Default)]
pub struct Cursors(std::sync::Mutex<CursorMap>);

#[derive(Default)]
struct CursorMap {
    /// Con la vuelta en que se movió por última vez (para saber cuáles son las más antiguas).
    map: HashMap<(String, String), (PathBuf, u64, u64)>,
    tick: u64,
}

impl Cursors {
    /// Dónde seguir leyendo: 0 si no la leyó nunca o la transcripción es otra (tras `/clear`).
    pub fn get(&self, reader: &str, target: &str, path: &Path) -> u64 {
        self.0
            .lock()
            .expect("cursores")
            .map
            .get(&(reader.to_owned(), target.to_owned()))
            .filter(|(p, _, _)| p == path)
            .map_or(0, |(_, o, _)| *o)
    }

    /// Lo lleva de `from` a `to`, si sigue en `from` (lo que daría `get`). Si no, otra lectura
    /// de la misma sesión se adelantó y ya devolvió esos turnos: `false`, y no se toca.
    pub fn advance(&self, reader: &str, target: &str, path: &Path, from: u64, to: u64) -> bool {
        let mut c = self.0.lock().expect("cursores");
        let key = (reader.to_owned(), target.to_owned());
        let now = c
            .map
            .get(&key)
            .filter(|(p, _, _)| p == path)
            .map_or(0, |(_, o, _)| *o);
        if now != from {
            return false;
        }
        c.tick += 1;
        let tick = c.tick;
        c.map.insert(key, (path.to_owned(), to, tick));
        if reader.contains('/') {
            let remote = |r: &str| r.contains('/');
            while c.map.keys().filter(|(r, _)| remote(r)).count() > MAX_REMOTE_READERS {
                let oldest = c
                    .map
                    .iter()
                    .filter(|((r, _), _)| remote(r))
                    .min_by_key(|(_, (_, _, t))| *t)
                    .map(|(k, _)| k.clone());
                if let Some(k) = oldest {
                    c.map.remove(&k);
                }
            }
        }
        true
    }

    /// Una sesión que ya no está: fuera lo suyo, como lectora y como leída.
    pub fn forget(&self, id: &str) {
        self.0
            .lock()
            .expect("cursores")
            .map
            .retain(|(r, t), _| r != id && t != id);
    }
}

/// Lo que mira una espera de lo que dice `status` de la sesión de destino.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub struct Seen {
    pub state: Status,
    /// El mensaje ya no está en su bandeja.
    #[serde(default)]
    pub taken: bool,
    /// Claude ha escrito algo desde que se encoló el mensaje.
    #[serde(default)]
    pub replied: bool,
}

/// Otro equipo no contestó a tiempo.
fn no_answer() -> Fail {
    fail(StatusCode::GATEWAY_TIMEOUT, "el otro equipo no contesta")
}

/// ¿Es un fallo del canal con otro equipo (no contesta, el hub tarda, sin conexión)? La
/// siguiente vez puede ir bien.
fn passing(f: &Fail) -> bool {
    f.0.is_server_error()
}

/// Por qué no se supo cómo está la sesión de destino en una vuelta de la espera.
#[derive(Debug)]
pub enum Miss {
    /// Falló el canal con su equipo: la siguiente vuelta puede ir bien.
    Channel(Fail),
    /// Otra cosa (ya no existe, una respuesta que no se entiende): no se vuelve a intentar.
    Fatal(Fail),
}

/// Tras tantos fallos seguidos del canal se deja de esperar.
pub const CHANNEL_TRIES: usize = 3;
/// Lo más que se espera cada `status` de otro equipo (o lo que quede de espera, si es menos).
pub const STATUS_POLL: Duration = Duration::from_secs(10);

/// La espera de `send` y `read` con `wait`, igual para una sesión de aquí que para una de otro
/// equipo: `status` dice cómo está (un futuro nuevo cada vez: con un cierre `async`, el futuro no
/// se puede probar `Send` y axum lo exige). Con `pickup` (la de `send`), primero, hasta ese
/// tiempo, a que la sesión recoja el mensaje y se ponga con él (`picked_up`); si no llega a
/// ponerse (o está parada), no se sigue esperando. Luego (o desde el principio, con `read`),
/// hasta `total`, a que deje de trabajar. Si el canal falla en la primera vuelta, o
/// `CHANNEL_TRIES` veces seguidas, sale ese fallo; un fallo suelto no corta la espera. Mirar el
/// estado no mueve la posición de lectura.
pub async fn wait_on<F: Future<Output = Result<Seen, Miss>>>(
    mut status: impl FnMut() -> F,
    pickup: Option<Duration>,
    step: Duration,
    total: Duration,
) -> Result<(), Fail> {
    let start = tokio::time::Instant::now();
    let mut picked = pickup.is_none();
    let (mut answered, mut failures) = (false, 0);
    loop {
        match status().await {
            Ok(s) => {
                (answered, failures) = (true, 0);
                picked = picked || picked_up(s.taken, s.state, s.replied);
                if s.state.settled() && (picked || s.state == Status::Stopped) {
                    break;
                }
            }
            Err(Miss::Channel(f)) => {
                failures += 1;
                if !answered || failures >= CHANNEL_TRIES {
                    return Err(f);
                }
            }
            Err(Miss::Fatal(f)) => return Err(f),
        }
        let limit = match pickup {
            Some(p) if !picked => p.min(total),
            _ => total,
        };
        let gone = start.elapsed();
        if gone >= limit {
            break;
        }
        tokio::time::sleep(step.min(limit - gone)).await;
    }
    Ok(())
}

/// `wait_on` y luego una sola lectura (`read`): lo que haya, también si se acabó el tiempo (con
/// `state` aún `working`, el modelo sabe que puede volver a esperar con `read`).
pub async fn wait_then_read<F, R>(
    status: impl FnMut() -> F,
    read: impl FnOnce() -> R,
    pickup: Option<Duration>,
    step: Duration,
    total: Duration,
) -> Reply
where
    F: Future<Output = Result<Seen, Miss>>,
    R: Future<Output = Reply>,
{
    wait_on(status, pickup, step, total).await?;
    read().await
}

/// Cuánto esperar con `wait`: lo pedido (o 10 min), como mucho media hora.
pub fn wait_timeout(secs: Option<u64>) -> Duration {
    Duration::from_secs(secs.unwrap_or(WAIT_DEFAULT_S).min(WAIT_MAX_S))
}

// ---------- send ----------

/// Qué pasó con un mensaje de `send`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Delivery {
    /// La sesión tiene channel: lo recoge ya.
    Sent,
    /// En la bandeja, hasta que la sesión arranque (o acabe de arrancar).
    Queued,
}

/// Lo que se sabe de la sesión de destino al mandarle un mensaje.
#[derive(Debug, Clone, Copy)]
pub struct Dest {
    pub alive: bool,
    /// Tiene channel ahora (ver `inbox::live`).
    pub channel: bool,
    /// Hace cuánto arrancó su guardián, si se sabe.
    pub started: Option<Duration>,
    /// La abrió quien manda el mensaje.
    pub mine: bool,
    /// Su pantalla dijo que no tiene channel, y por qué (ver `Inbox::refusal`).
    pub refused: Option<inbox::Refusal>,
}

/// Lo que contesta `send`: el estado y, si hace falta, una nota para el modelo (en español; sale
/// traducida).
#[derive(Debug, PartialEq, Eq)]
pub struct Decision {
    pub status: Delivery,
    pub note: Option<&'static str>,
}

/// Hace cuánto arrancó la sesión, para la ventana de arranque: lo que diga su guardián, pero
/// nunca más que lo que lleva en marcha `nido serve`. Tras reiniciarlo, ningún `nido mcp` ha
/// vuelto aún a recoger la bandeja: todas tienen el rato de arranque para volver.
pub fn started(holder: Option<Duration>, uptime: Duration) -> Duration {
    holder.map_or(uptime, |h| h.min(uptime))
}

/// ¿Ya recogió la sesión el mensaje (`taken`: ya no está en su bandeja) y se puso con él? Si no
/// está parada, o si Claude ya ha escrito algo después (`replied`: contestó enseguida). Que su
/// conversación crezca no basta: lo primero que entra es el propio mensaje.
pub fn picked_up(taken: bool, state: Status, replied: bool) -> bool {
    taken && (state != Status::Idle || replied)
}

/// ¿Se manda el mensaje a sí misma la sesión `caller`?
pub fn to_self(me: &Machine, address: &str, caller: &str) -> bool {
    matches!(locate(me, address), Ok(Where::Here(id)) if id == caller)
}

/// Qué hacer con un mensaje para `d`. Parada, a la cola (se entrega al arrancar); viva con
/// channel, se entrega; viva sin channel pero recién arrancada, a la cola; viva sin channel desde
/// hace rato, no admite mensajes (y nunca se escribe en su consola).
pub fn decide(d: &Dest) -> Result<Decision, Fail> {
    // Su pantalla ya dijo que no tiene channel: no, aunque acabe de arrancar, y por qué.
    if let (true, Some(why)) = (d.alive, d.refused) {
        let msg = match why {
            inbox::Refusal::Blocked => {
                "esa sesión no admite mensajes (no tiene channel): la organización de su cuenta \
                 de Claude no permite channels (channelsEnabled)"
            }
            inbox::Refusal::NoServer => {
                "esa sesión no admite mensajes (no tiene channel): su Claude no tiene el MCP de \
                 nido"
            }
            inbox::Refusal::NoClaude => {
                "esa sesión no admite mensajes (no tiene channel): su Claude no está en marcha"
            }
            inbox::Refusal::OldClaude => {
                "esa sesión no admite mensajes (no tiene channel): su Claude Code es anterior a \
                 los channels"
            }
        };
        return Err(fail(StatusCode::CONFLICT, msg));
    }
    let status = if !d.alive {
        Delivery::Queued
    } else if d.channel {
        Delivery::Sent
    } else if d.started.is_some_and(|t| t < STARTUP) {
        Delivery::Queued
    } else {
        return Err(fail(
            StatusCode::CONFLICT,
            "esa sesión no admite mensajes (no tiene channel)",
        ));
    };
    let note = match (status, d.alive, d.mine) {
        (Delivery::Sent, _, true) => None,
        (Delivery::Sent, _, false) => Some("entregado; la sesión decide si lo atiende"),
        (Delivery::Queued, false, true) => Some("en cola; se entregará cuando arranque la sesión"),
        (Delivery::Queued, false, false) => {
            Some("en cola; se entregará cuando arranque la sesión, y ella decide si lo atiende")
        }
        (Delivery::Queued, true, true) => {
            Some("en cola; se entregará cuando la sesión acabe de arrancar")
        }
        (Delivery::Queued, true, false) => Some(
            "en cola; se entregará cuando la sesión acabe de arrancar, y ella decide si lo atiende",
        ),
    };
    Ok(Decision { status, note })
}

/// Dónde estaba la conversación de la sesión de destino de cada mensaje de `send` al encolarlo
/// (por el id del mensaje), para saber si ya contestó. Solo en memoria y un rato (`MARK_TTL`).
#[derive(Default)]
pub struct Marks(std::sync::Mutex<HashMap<String, Mark>>);

#[derive(Debug, Clone)]
struct Mark {
    /// La sesión de destino.
    session: String,
    /// Su transcripción (`None`: aún no tenía) y hasta dónde se ha mirado sin ver respuesta.
    path: Option<PathBuf>,
    from: u64,
    replied: bool,
    at: Instant,
}

impl Marks {
    /// Apunta dónde estaba la conversación de `session` (`path`, de `len` bytes) al encolarle el
    /// mensaje `msg`. Fuera las de hace más de `MARK_TTL`.
    pub fn put(&self, msg: &str, session: &str, path: Option<PathBuf>, len: u64, now: Instant) {
        let mut m = self.0.lock().expect("marcas");
        m.retain(|_, k| now.saturating_duration_since(k.at) < MARK_TTL);
        m.insert(
            msg.to_owned(),
            Mark {
                session: session.to_owned(),
                path,
                from: len,
                replied: false,
                at: now,
            },
        );
    }

    /// La de `msg` para `session`, si no ha pasado `MARK_TTL`.
    fn get(&self, msg: &str, session: &str) -> Option<Mark> {
        let now = Instant::now();
        self.0
            .lock()
            .expect("marcas")
            .get(msg)
            .filter(|k| k.session == session && now.saturating_duration_since(k.at) < MARK_TTL)
            .cloned()
    }

    /// Ya se miró `path` hasta `from` (y si contestó).
    fn seen(&self, msg: &str, path: PathBuf, from: u64, replied: bool) {
        if let Some(k) = self.0.lock().expect("marcas").get_mut(msg) {
            k.path = Some(path);
            k.from = from;
            k.replied |= replied;
        }
    }
}

/// ¿Hay un turno de Claude con texto en `now` (la transcripción de ahora) desde la marca (`mark`,
/// hasta `from`)? Si la transcripción es otra (tras `/clear`) o no la había, desde el principio.
/// Devuelve también hasta dónde se miró, para seguir desde ahí.
pub fn replied_since(mark: Option<&Path>, from: u64, now: &Path) -> std::io::Result<(bool, u64)> {
    let len = std::fs::metadata(now)?.len();
    // La marca puede caer a media línea: lo que se estaba escribiendo es de antes.
    let start = if mark == Some(now) && from <= len {
        line_start(now, from)?
    } else {
        0
    };
    let chunk = transcript::read_chunk(now, start, None)?;
    let replied = chunk
        .items
        .iter()
        .any(|i| matches!(i, ChatItem::Assistant { text, .. } if !text.trim().is_empty()));
    Ok((replied, chunk.offset))
}

// ---------- open ----------

/// Un nombre de sesión válido y libre a partir de la carpeta: su último trozo, con lo que no vale
/// en un nombre cambiado por «-», y «-2», «-3»… si ya está cogido.
pub fn name_for(folder: &Path, taken: impl Fn(&str) -> bool) -> String {
    let last = folder
        .file_name()
        .map(|f| f.to_string_lossy().into_owned())
        .unwrap_or_default();
    let clean: String = last
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '-'
            }
        })
        .collect();
    // Sitio para el «-N» dentro de los 64 caracteres.
    let base: String = clean
        .trim_start_matches(['-', '.'])
        .chars()
        .take(56)
        .collect();
    let base = if base.is_empty() {
        "claude".to_owned()
    } else {
        base
    };
    if !taken(&base) {
        return base;
    }
    (2..)
        .map(|n| format!("{base}-{n}"))
        .find(|n| !taken(n))
        .expect("algún número libre")
}

/// El mensaje inicial de una sesión abierta con `open`: la línea que le dice de quién vienen los
/// mensajes que le lleguen por nido, una línea en blanco y lo que pide quien la abre. En inglés:
/// lo lee el modelo.
pub fn first_message(
    caller_name: &str,
    machine: &str,
    caller_address: &str,
    prompt: &str,
) -> String {
    format!(
        "[nido] You were opened by the session \"{caller_name}\" on {machine} ({caller_address}). \
         Messages that reach you through nido from that session come from whoever opened you: \
         act on them and reply with the nido send tool to {caller_address}.\n\n{prompt}"
    )
}

/// La orden de una sesión abierta con `open`: un Claude nuevo con su conversación y su nombre, el
/// modelo si se pidió y el mensaje inicial al final (un texto suelto: al retomarla no se repite,
/// ver `claude::extra_args`).
pub fn open_argv(name: &str, session_id: &str, model: Option<&str>, message: &str) -> Vec<String> {
    let mut argv = claude::new_argv(name, session_id);
    if let Some(m) = model {
        argv.extend(["--model".to_owned(), m.to_owned()]);
    }
    argv.push(message.to_owned());
    argv
}

// ---------- La API ----------

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NoArgs {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SessionArg {
    session: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StopArgs {
    session: String,
    #[serde(default)]
    force: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadArgs {
    session: String,
    #[serde(default)]
    wait: bool,
    timeout_s: Option<u64>,
    last: Option<usize>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SendArgs {
    session: String,
    text: String,
    /// Cuántos segundos esperar la respuesta (sin él, o 0, no se espera).
    wait: Option<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AckArgs {
    ids: Vec<String>,
    /// Los metió con su id en el evento (`msg_id`). Un `nido mcp` de antes no lo manda.
    #[serde(default)]
    tracked: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OpenArgs {
    machine: String,
    folder: String,
    prompt: String,
    name: Option<String>,
    model: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AnswerArgs {
    session: String,
    option: Option<String>,
    text: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AlertArgs {
    text: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StatusArgs {
    session: String,
    /// Un mensaje de `send` (su `msg_id`): si ya lo recogió y si ha contestado.
    msg: Option<String>,
}

/// Lo que devuelve `status` (por el canal «peer», para las esperas del nido de quien llama, y
/// para saber si vive una sesión madre).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StatusOut {
    pub state: Status,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub question: Option<Question>,
    /// Con `msg`: ya no está en su bandeja.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub taken: Option<bool>,
    /// Con `msg`: Claude ha escrito algo desde que se encoló.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub replied: Option<bool>,
}

/// Una sesión de Claude en la lista de `sessions` (y lo que devuelve `open`).
#[derive(Debug, Serialize)]
pub struct Entry {
    /// `<equipo>/<id>` (el equipo como lo da `Machine::label`): lo que se pasa a las demás
    /// herramientas.
    pub session: String,
    /// El nombre del equipo.
    pub machine: String,
    /// Su id en el hub (sin hub, su nombre).
    pub machine_id: String,
    pub id: String,
    pub name: String,
    pub cwd: PathBuf,
    pub state: Status,
    /// Desde cuándo (ms desde 1970) está en marcha o parada; sin él, no se sabe.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub since: Option<u64>,
    /// Quién la abrió con `open` (`<equipo>/<id>`); `null`: el usuario.
    pub opened_by: Option<String>,
    /// La abrió quien llama: puede contestarla y cerrarla.
    pub mine: bool,
    /// Recibe mensajes de `send`: su `nido mcp` está recogiendo la bandeja.
    pub channel: bool,
    /// Por qué no tiene channel, si su pantalla lo dijo (ver `inbox::Refusal::code`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel_error: Option<&'static str>,
    /// Mensajes que no se ven en su conversación tras darlos `inbox::MAX_TRIES` veces (ver
    /// `delivery_error`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delivery_error: Option<String>,
}

/// Lo que se dice de los mensajes de la bandeja de `id` que no llegaron (siguen en ella).
pub fn delivery_error(st: &AppState, id: &str) -> Option<String> {
    match st.inbox.undelivered(id) {
        0 => None,
        n => Some(format!(
            "{n} message(s) not delivered after {} tries; still queued",
            inbox::MAX_TRIES
        )),
    }
}

/// Desde cuándo (ms) está como está: en marcha, desde que arrancó su guardián
/// (`launch::holder_started`); parada, desde que nido la vio pararse (`stopped`; 0: no se sabe).
pub fn since(alive: bool, holder: Option<SystemTime>, stopped: u64) -> Option<u64> {
    if alive {
        holder
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as u64)
    } else {
        (stopped > 0).then_some(stopped)
    }
}

fn entry(st: &AppState, me: &Machine, caller: &Caller, v: &SessionView, state: Status) -> Entry {
    Entry {
        session: me.shown(&v.id),
        machine: me.name.clone(),
        machine_id: me.key().to_owned(),
        id: v.id.clone(),
        name: v.name.clone(),
        cwd: v.cwd.clone(),
        state,
        since: since(v.alive, launch::holder_started(&st.paths, &v.id), v.stopped),
        opened_by: v.opened_by.clone(),
        mine: caller.owns(me, v.opened_by.as_deref()),
        channel: st.inbox.channel(&v.id),
        channel_error: st.inbox.refusal(&v.id).map(inbox::Refusal::code),
        delivery_error: delivery_error(st, &v.id),
    }
}

type Reply = Result<Value, Fail>;

/// Un error de una herramienta: el código y el mensaje, en español (sale como `server::error`,
/// en el idioma de la petición).
#[derive(Debug)]
pub struct Fail(pub StatusCode, pub String);

fn fail(status: StatusCode, msg: impl Into<String>) -> Fail {
    Fail(status, msg.into())
}

impl From<crate::registry::RegistryError> for Fail {
    fn from(e: crate::registry::RegistryError) -> Self {
        Fail(server::registry_status(&e), e.to_string())
    }
}

impl IntoResponse for Fail {
    fn into_response(self) -> Response {
        error(self.0, self.1)
    }
}

/// El error que contestó otro equipo, con su código si es de error (400 a 599); si no, 502.
fn peer_fail((code, msg): (u16, String)) -> Fail {
    let code = StatusCode::from_u16(code)
        .ok()
        .filter(|c| c.is_client_error() || c.is_server_error())
        .unwrap_or(StatusCode::BAD_GATEWAY);
    Fail(code, msg)
}

/// `POST /api/v1/mcp/{tool}`.
pub async fn api_mcp(
    State(st): State<Arc<AppState>>,
    UrlPath(tool): UrlPath<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let session = match authorize(&st, &headers).await {
        Ok(s) => s,
        Err(f) => return f.into_response(),
    };
    let args = match body_args(&body) {
        Ok(a) => a,
        Err(f) => return f.into_response(),
    };
    let me = Machine::of(&st);
    let reply = match tool.as_str() {
        "alert" => tool_alert(&st, &session, args).await,
        _ => {
            let caller = Caller::local(&me, &session.id, &session.name);
            run_tool(&st, &me, &caller, &tool, args).await
        }
    };
    match reply {
        Ok(v) => Json(v).into_response(),
        Err(f) => f.into_response(),
    }
}

/// Lo que se puede pedir desde otro equipo: las herramientas, menos `alert` (una alerta va
/// siempre al nido de quien la manda), y, para su nido, `status` (lo que miran sus esperas) y
/// `alerts` (las de aquí sin leer, para `nido alerts`).
pub const PEER_TOOLS: [&str; 10] = [
    "sessions", "send", "read", "open", "answer", "close", "start", "stop", "status", "alerts",
];

/// Una herramienta (menos `alert`), para quien llame: de aquí o de otro equipo.
async fn run_tool(st: &AppState, me: &Machine, caller: &Caller, tool: &str, args: Value) -> Reply {
    match tool {
        "sessions" => tool_sessions(st, me, caller, args).await,
        "send" => tool_send(st, me, caller, args).await,
        "read" => tool_read(st, me, caller, args).await,
        "open" => tool_open(st, me, caller, args).await,
        "answer" => tool_answer(st, me, caller, args).await,
        "close" => tool_close(st, me, caller, args).await,
        "start" => tool_start(st, me, caller, args).await,
        "stop" => tool_stop(st, me, caller, args).await,
        "status" => tool_status(st, me, caller, args).await,
        _ => Err(fail(
            StatusCode::NOT_FOUND,
            format!("no hay ninguna herramienta {tool:?}"),
        )),
    }
}

/// Lo que pide otro equipo del grupo por el canal «peer» (`e2e::MODE_PEER`): `member` es el
/// equipo (por su clave, ya comprobada) y `params`, los argumentos de la herramienta con la
/// sesión que llama en `caller` (`{id, name}`). Los errores van en español, como en la API
/// local: los traduce el nido de quien llama.
pub async fn peer_call(
    st: &AppState,
    member: &crate::group::Member,
    method: &str,
    mut params: Value,
) -> e2e::PeerResult {
    let code = |f: Fail| (f.0.as_u16(), f.1);
    if !PEER_TOOLS.contains(&method) {
        return Err(code(bad_request(format!(
            "«{method}» no se puede pedir desde otro equipo"
        ))));
    }
    let me = Machine::of(st);
    let caller = params.as_object_mut().and_then(|p| p.remove("caller"));
    // Las alertas son del equipo, no de una sesión: las pide `nido alerts`.
    if method == "alerts" {
        let NoArgs {} = args_of(params).map_err(code)?;
        return Ok(fit_alerts(local_alerts(st, &me).await, MAX_READ));
    }
    let caller = Caller::remote(&me, member, caller).map_err(code)?;
    run_tool(st, &me, &caller, method, params)
        .await
        .map_err(code)
}

/// Una alerta sin leer, para `nido alerts`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AlertOut {
    /// El nombre del equipo.
    pub machine: String,
    pub machine_id: String,
    /// La sesión que la mandó (`<equipo>/<id>`, como en `sessions`) y su nombre.
    pub session: String,
    pub name: String,
    pub text: String,
    pub at: u64,
}

/// Las alertas sin leer de las sesiones de aquí.
async fn local_alerts(st: &AppState, me: &Machine) -> Vec<AlertOut> {
    st.scan().await;
    st.views
        .borrow()
        .sessions
        .iter()
        .flat_map(|v| {
            v.alerts.iter().map(|a| AlertOut {
                machine: me.name.clone(),
                machine_id: me.key().to_owned(),
                session: me.shown(&v.id),
                name: v.name.clone(),
                text: a.text.clone(),
                at: a.at,
            })
        })
        .collect()
}

/// Las alertas para otro equipo: `{alerts: [...]}` con las más nuevas que quepan en `max` bytes
/// de JSON y, si no cupieron todas, `truncated: true`.
pub fn fit_alerts(mut list: Vec<AlertOut>, max: usize) -> Value {
    list.sort_by_key(|a| std::cmp::Reverse(a.at));
    let truncated = fit_list(&mut list, max, r#"{"alerts":[],"truncated":true}"#.len());
    let mut out = json!({ "alerts": list });
    if truncated {
        out["truncated"] = true.into();
    }
    out
}

/// Deja en `list` los primeros que caben en `max` bytes de JSON, con `used` ya ocupados por lo
/// de alrededor. `true` si sobraba alguno.
fn fit_list<T: Serialize>(list: &mut Vec<T>, max: usize, mut used: usize) -> bool {
    let mut keep = 0;
    for e in list.iter() {
        let n = json_len(e) + 1;
        if used + n > max {
            break;
        }
        used += n;
        keep += 1;
    }
    let truncated = keep < list.len();
    list.truncate(keep);
    truncated
}

/// `GET /api/v1/mcp/alerts`: las alertas sin leer de todos los equipos del grupo, para `nido
/// alerts` (con el token principal, sin sesión): `{alerts: [...], unreachable?: [...]}`, de la
/// más antigua a la más nueva. A cada equipo se le pregunta a la vez, `FAN_OUT` como mucho.
pub async fn api_alerts(State(st): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(f) = main_token(&st, &headers) {
        return f.into_response();
    }
    let me = Machine::of(&st);
    let (local, remote) = tokio::join!(
        local_alerts(&st, &me),
        ask_others(&st, &me, "alerts", json!({}))
    );
    let mut alerts = local;
    let mut unreachable = vec![];
    for (m, r) in remote {
        let list = r
            .and_then(|v| list_of::<AlertOut>(&v, "alerts"))
            .and_then(|l| {
                l.iter()
                    .map(|a| {
                        let v = relabel(&me, m, to_json(a)?);
                        serde_json::from_value(v)
                            .map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
                    })
                    .collect::<Result<Vec<AlertOut>, Fail>>()
            });
        match list {
            Ok(list) => alerts.extend(list),
            Err(f) => unreachable.push(unreachable_entry(m, &f)),
        }
    }
    alerts.sort_by_key(|a| a.at);
    let mut out = json!({ "alerts": alerts });
    if !unreachable.is_empty() {
        out["unreachable"] = unreachable.into();
    }
    Json(out).into_response()
}

/// Lo que contesta otro equipo en `key` (una lista), o el error de que no se entiende.
fn list_of<T: DeserializeOwned>(v: &Value, key: &str) -> Result<Vec<T>, Fail> {
    serde_json::from_value(v.get(key).cloned().unwrap_or(Value::Null)).map_err(|_| {
        fail(
            StatusCode::BAD_GATEWAY,
            "respuesta no válida del otro equipo",
        )
    })
}

/// Un equipo que no contestó: `{machine, machine_id, error}`.
fn unreachable_entry(m: &Member, f: &Fail) -> Value {
    json!({ "machine": Machine::name_of(m), "machine_id": m.id, "error": i18n::tr(&f.1) })
}

/// Pide `method` a todos los demás equipos del grupo a la vez, `FAN_OUT` como mucho a cada uno.
async fn ask_others<'m>(
    st: &AppState,
    me: &'m Machine,
    method: &str,
    params: Value,
) -> Vec<(&'m Member, Result<Value, Fail>)> {
    futures_util::future::join_all(me.others.iter().map(|m| {
        let params = params.clone();
        async move {
            let r = tokio::time::timeout(FAN_OUT, st.peer_member(m, method, params)).await;
            let r = match r {
                Ok(r) => r.map_err(peer_fail),
                Err(_) => Err(fail(
                    StatusCode::GATEWAY_TIMEOUT,
                    "el otro equipo no contesta",
                )),
            };
            (m, r)
        }
    }))
    .await
}

/// Pide `method` a otro equipo del grupo de parte de `caller` (una sesión de aquí) y devuelve lo
/// que conteste, con la sesión que traiga escrita como se escribe aquí (`relabel`).
async fn forward(
    st: &AppState,
    me: &Machine,
    caller: &Caller,
    machine: &Member,
    method: &str,
    mut params: Value,
) -> Reply {
    params["caller"] = json!({ "id": caller.id, "name": caller.name });
    let v = st
        .peer_member(machine, method, params)
        .await
        .map_err(peer_fail)?;
    Ok(relabel(me, machine, v))
}

/// Una sesión de otro equipo (`v`, con `session`) como se ve desde aquí: su dirección con la
/// etiqueta de ese equipo aquí (`Machine::label_of`), y el equipo por su nombre y su id en el
/// grupo, no por lo que diga él.
fn relabel(me: &Machine, m: &Member, mut v: Value) -> Value {
    let Some(o) = v.as_object_mut() else {
        return v;
    };
    let Some(session) = o.get("session").and_then(Value::as_str) else {
        return v;
    };
    let id = session.rsplit_once('/').map_or(session, |x| x.1).to_owned();
    o.insert("session".into(), format!("{}/{id}", me.label_of(m)).into());
    o.insert("machine".into(), Machine::name_of(m).into());
    o.insert("machine_id".into(), m.id.clone().into());
    v
}

/// Los argumentos para otro equipo, con la sesión por su id (allí es de aquí).
fn with_session(mut args: Value, id: &str) -> Value {
    args["session"] = id.into();
    args
}

/// Cuánto retiene una recogida mientras no se sabe cuál es el Claude de la sesión.
const WAIT_UNKNOWN: Duration = Duration::from_secs(2);

/// `GET /api/v1/mcp/inbox`: lo que espera en la bandeja de la sesión que llama, del más antiguo
/// al más nuevo (`{messages: [...]}`, cada uno con su intento, `try`). Si no hay nada, espera a
/// que llegue algo (25 s como mucho; luego, vacío). Lo que su `nido mcp` no dijo que metió
/// vuelve a salir en la siguiente; lo metido, solo si no se ve en la transcripción (ver
/// `check_delivery`). Antes de volver a dar algo se mira la transcripción: lo que ya está no se
/// da otra vez.
pub async fn api_inbox(State(st): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let caller = match authorize(&st, &headers).await {
        Ok(s) => s,
        Err(f) => return f.into_response(),
    };
    match own_claude(&st, &caller.id, &headers).await {
        Ok(Claim::Yes) => {}
        // Aún no se sabe de quién es (arranca, o se está reiniciando y este es el `nido mcp` de
        // antes): nada, y sin contar como channel, para que vuelva a preguntar enseguida.
        Ok(_) => {
            tokio::time::sleep(st.inbox.poll_wait().min(WAIT_UNKNOWN)).await;
            return Json(json!({ "messages": [] })).into_response();
        }
        Err(f) => return f.into_response(),
    }
    // Sin channel (Claude tiraría los mensajes), o mientras se mira su arranque: nada, y sin
    // contar como channel. Lo que espera sigue en la cola.
    if st.inbox.held(&caller.id) {
        let wait = match st.inbox.refusal(&caller.id) {
            Some(_) => st.inbox.poll_wait(),
            None => st.inbox.poll_wait().min(WAIT_UNKNOWN),
        };
        tokio::time::sleep(wait).await;
        return Json(json!({ "messages": [] })).into_response();
    }
    check_delivery(&st, &caller.id, None).await;
    // Dónde se buscará lo que se dé (desde donde acabe al darlo: ver `poll_from`).
    let transcript = transcript_where(&st, &caller.id).await;
    let messages: Vec<Value> = st
        .inbox
        .poll_from(&caller.id, transcript)
        .await
        .into_iter()
        .map(|(m, n)| {
            let mut v = json!(m);
            v["try"] = n.into();
            v
        })
        .collect();
    Json(json!({ "messages": messages })).into_response()
}

/// `POST /api/v1/mcp/inbox/ack {ids: [...]}`: ya están en la sesión; fuera de su bandeja (los
/// de otras no se tocan).
pub async fn api_inbox_ack(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let caller = match authorize(&st, &headers).await {
        Ok(s) => s,
        Err(f) => return f.into_response(),
    };
    let a: AckArgs = match body_args(&body).and_then(args_of) {
        Ok(a) => a,
        Err(f) => return f.into_response(),
    };
    match own_claude(&st, &caller.id, &headers).await {
        Ok(Claim::Yes) => {}
        Ok(_) => return Json(json!({ "ok": true, "acked": 0 })).into_response(),
        Err(f) => return f.into_response(),
    }
    let acked = st.inbox.ack(&caller.id, &a.ids, a.tracked);
    Json(json!({ "ok": true, "acked": acked })).into_response()
}

/// Mira en la transcripción de `id` si ya entró lo dado a su `nido mcp` (desde donde se miró la
/// otra vez: ver `Inbox::scan_from`), y lo quita de su bandeja. Con `tick` (el rato desde la
/// vuelta anterior de `spawn_delivery_watch`), lo que sigue sin verse cuenta ese rato si Claude
/// no trabajaba (ver `clock_runs`), y se vuelve a dar o se deja (ver `inbox::step`). Sin
/// transcripción aún (un Claude nuevo que tiró el primer evento no escribe nada) también corre:
/// si no hay fichero, no está escribiendo.
pub async fn check_delivery(st: &AppState, id: &str, tick: Option<Duration>) {
    let mut silent = None;
    if let Some(path) = transcript_now(st, id).await {
        let Some(from) = st.inbox.scan_from(id, &path) else {
            return;
        };
        let scan = {
            let path = path.clone();
            tokio::task::spawn_blocking(move || {
                let found = inbox::channel_deliveries(&path, from)?;
                let written = std::fs::metadata(&path)?.modified()?;
                Ok::<_, std::io::Error>((found, written))
            })
            .await
        };
        let Ok(Ok(((found, upto), written))) = scan else {
            // Ya no está (o no se puede leer): se buscará otra vez.
            st.inbox.forget_transcript(id);
            return;
        };
        st.inbox.confirm(id, &path, upto, &found);
        silent = Some(
            SystemTime::now()
                .duration_since(written)
                .unwrap_or_default(),
        );
    }
    let Some(elapsed) = tick else {
        return;
    };
    let runs = clock_runs(
        st.views.borrow().sessions.iter().find(|v| v.id == id),
        silent,
        st.inbox.transcript_quiet(),
    );
    st.inbox.tick(id, runs, elapsed);
}

/// ¿Corre el reloj de un mensaje emitido y aún no visto? Con la sesión viva, su Claude ni
/// trabajando (`busy`) ni esperándote (`waiting`: un permiso o una pregunta en pantalla; en esos
/// casos Claude Code guarda el evento y lo pasa a la conversación después, y darlo otra vez lo
/// duplicaría), y su transcripción sin escribirse (`silent`: cuánto hace; `None`, no la hay) en
/// `quiet.0`. Sin saber su estado, el rato es `quiet.1`, más largo; sin transcripción no se espera
/// ningún rato, porque entonces nada ha llegado a la conversación. Se supone que al compactar
/// Claude dice `busy` (no se ha podido comprobar).
fn clock_runs(
    v: Option<&SessionView>,
    silent: Option<Duration>,
    quiet: (Duration, Duration),
) -> bool {
    let Some(v) = v.filter(|v| v.alive) else {
        return false;
    };
    let status = v.claude.as_ref().and_then(|c| c.status.as_deref());
    let need = match status {
        Some("busy" | "waiting") => return false,
        Some(_) => quiet.0,
        None => quiet.1,
    };
    silent.is_none_or(|s| s >= need)
}

/// La bandeja de la sesión `id` solo la recoge (y confirma) el `nido mcp` de su Claude. Su pid
/// viene en `X-Nido-Mcp-Pid`; de ahí se sube por los padres hasta el primer Claude y se compara
/// con el de la sesión (`claude::session_claude`; ver `inbox::claim`). Si pasado el arranque no
/// se encuentra el de la sesión, se da a quien la pida, y se dice una vez por arranque.
async fn own_claude(st: &AppState, id: &str, headers: &HeaderMap) -> Result<Claim, Fail> {
    let Some(mcp) = headers
        .get("X-Nido-Mcp-Pid")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u32>().ok())
    else {
        return Ok(Claim::Yes);
    };
    let paths = st.paths.clone();
    let session = id.to_owned();
    let (ancestor, known, age) = tokio::task::spawn_blocking(move || {
        let procfs = claude::Procfs::default();
        let known = launch::holder_pid(&paths, &session)
            .and_then(|p| u32::try_from(p).ok())
            .and_then(|holder| claude::session_claude(&procfs, holder, &paths.home));
        // Sin el Claude de la sesión no hace falta subir.
        let ancestor = known.and_then(|_| claude::claude_ancestor(&procfs, mcp));
        let age = launch::holder_started(&paths, &session)
            .map(|t| SystemTime::now().duration_since(t).unwrap_or_default());
        (ancestor, known, age)
    })
    .await
    .unwrap_or((None, None, None));
    let starting = inbox::starting(age);
    let c = inbox::claim(true, ancestor, known, starting);
    if c == Claim::Yes && known.is_none() && st.inbox.unguarded(id) {
        eprintln!("nido: no encuentro el Claude de la sesión {id}; la bandeja no se protege");
    }
    match c {
        Claim::No => Err(fail(StatusCode::CONFLICT, "no es el Claude de esta sesión")),
        c => Ok(c),
    }
}

/// Con el token principal (las sesiones van con el de la consola, no con el de un teléfono) y
/// desde una sesión de aquí: cuál.
async fn authorize(st: &AppState, headers: &HeaderMap) -> Result<Session, Fail> {
    main_token(st, headers)?;
    caller(st, headers).await
}

/// Con el token principal, el de la consola (no el de un teléfono).
fn main_token(st: &AppState, headers: &HeaderMap) -> Result<(), Fail> {
    if let Some(token) = &st.token {
        let given = headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "));
        if !given.is_some_and(|g| auth::ct_eq(g, token)) {
            return Err(fail(
                StatusCode::FORBIDDEN,
                "la API del MCP solo va con el token de este equipo",
            ));
        }
    }
    Ok(())
}

/// Los argumentos del cuerpo; vacío es `{}`.
fn body_args(body: &Bytes) -> Result<Value, Fail> {
    if body.iter().all(u8::is_ascii_whitespace) {
        return Ok(json!({}));
    }
    serde_json::from_slice(body).map_err(|e| {
        fail(
            StatusCode::BAD_REQUEST,
            format!("argumentos no válidos: {e}"),
        )
    })
}

/// La sesión que llama: la de `X-Nido-Session`, que tiene que existir aquí.
async fn caller(st: &AppState, headers: &HeaderMap) -> Result<Session, Fail> {
    let id = headers
        .get("x-nido-session")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .unwrap_or_default();
    if id.is_empty() {
        return Err(not_from_session());
    }
    let found = st.registry.lock().await.get(id).cloned();
    found.ok_or_else(not_from_session)
}

fn not_from_session() -> Fail {
    fail(
        StatusCode::FORBIDDEN,
        "no se llamó desde una sesión de nido",
    )
}

fn args_of<T: DeserializeOwned>(args: Value) -> Result<T, Fail> {
    serde_json::from_value(args).map_err(|e| {
        fail(
            StatusCode::BAD_REQUEST,
            format!("argumentos no válidos: {e}"),
        )
    })
}

/// Dónde está la sesión `address` para quien llama. Para una de aquí, en este equipo o en otro
/// del grupo; para una de otro equipo, solo en este: lo que pide otro equipo no se pasa a un
/// tercero.
fn place<'a>(me: &Machine, caller: &Caller, address: &'a str) -> Result<Where<'a>, Fail> {
    if caller.local {
        return locate(me, address);
    }
    here(me, address)
        .map(Where::Here)
        .ok_or_else(|| fail(StatusCode::BAD_REQUEST, "esa sesión no es de este equipo"))
}

/// La sesión de Claude de aquí con ese id.
async fn session_here(st: &AppState, id: &str) -> Result<Session, Fail> {
    st.registry
        .lock()
        .await
        .get(id)
        .filter(|s| s.claude.is_some())
        .cloned()
        .ok_or_else(|| fail(StatusCode::NOT_FOUND, "no existe esa sesión"))
}

fn to_json<T: Serialize>(v: &T) -> Reply {
    serde_json::to_value(v).map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

fn bad_request(msg: String) -> Fail {
    fail(StatusCode::BAD_REQUEST, msg)
}

fn viewer() -> Hello {
    Hello {
        role: Role::Viewer,
        cols: 80,
        rows: 24,
    }
}

/// La pantalla de la sesión ahora (`None` si no está en marcha o no contesta).
async fn screen_of(st: &AppState, id: &str) -> Option<String> {
    let stream = server::holder_connect(st, id, viewer()).await.ok()?;
    let (rd, wr) = crate::ipc::split(stream);
    let mut scr = prompt::Screen::new(rd, wr);
    tokio::time::timeout(Duration::from_secs(3), scr.screen())
        .await
        .ok()?
        .ok()
}

/// Cómo está la sesión, con su menú si tiene uno en pantalla.
async fn status_of(st: &AppState, v: &SessionView) -> (Status, Option<Menu>) {
    if !v.alive {
        return (Status::Stopped, None);
    }
    let menu = screen_of(st, &v.id)
        .await
        .as_deref()
        .and_then(prompt::parse);
    let c = v.claude.as_ref();
    let running = c.is_some_and(|c| c.running);
    let busy = c.is_some_and(|c| c.status.as_deref() == Some("busy"));
    (status(true, running, busy, menu.is_some()), menu)
}

/// La sesión como la ve la web ahora mismo.
async fn view(st: &AppState, id: &str) -> Option<SessionView> {
    st.scan().await;
    st.views
        .borrow()
        .sessions
        .iter()
        .find(|v| v.id == id)
        .cloned()
}

/// `sessions`: para una de aquí, las de este equipo y, a la vez, las de los demás del grupo
/// (`FAN_OUT` como mucho cada uno; los que no contestan, en `unreachable`), en
/// `{sessions, unreachable?, truncated?}`. Para una de otro equipo, solo las de aquí.
async fn tool_sessions(st: &AppState, me: &Machine, caller: &Caller, args: Value) -> Reply {
    let NoArgs {} = args_of(args)?;
    if !caller.local {
        // A otro equipo va en un solo mensaje Noise: con tope, y diciendo si se recortó.
        return Ok(fit_sessions(local_sessions(st, me, caller).await, MAX_READ));
    }
    let who = json!({ "caller": { "id": caller.id, "name": caller.name } });
    let (local, remote) = tokio::join!(
        local_sessions(st, me, caller),
        ask_others(st, me, "sessions", who)
    );
    let mut sessions: Vec<Value> = local.iter().map(to_json).collect::<Result<_, _>>()?;
    let (mut unreachable, mut truncated) = (vec![], false);
    for (m, r) in remote {
        match r.and_then(|v| list_of::<Value>(&v, "sessions").map(|l| (l, v))) {
            Ok((list, v)) => {
                truncated |= v["truncated"] == true;
                sessions.extend(list.into_iter().map(|e| relabel(me, m, e)));
            }
            Err(f) => unreachable.push(unreachable_entry(m, &f)),
        }
    }
    let mut out = json!({ "sessions": sessions });
    if !unreachable.is_empty() {
        out["unreachable"] = unreachable.into();
    }
    if truncated {
        out["truncated"] = true.into();
    }
    Ok(out)
}

/// Las sesiones de Claude de este equipo, como las ve `caller`.
async fn local_sessions(st: &AppState, me: &Machine, caller: &Caller) -> Vec<Entry> {
    st.scan().await;
    let views: Vec<SessionView> = st
        .views
        .borrow()
        .sessions
        .iter()
        .filter(|v| v.claude.is_some())
        .cloned()
        .collect();
    let states = futures_util::future::join_all(views.iter().map(|v| status_of(st, v))).await;
    views
        .iter()
        .zip(states)
        .map(|(v, (s, _))| entry(st, me, caller, v, s))
        .collect()
}

/// La lista de `sessions` para otro equipo: `{sessions: [...]}` con las primeras que quepan en
/// `max` bytes de JSON y, si no cupieron todas, `truncated: true`.
pub fn fit_sessions(mut list: Vec<Entry>, max: usize) -> Value {
    let truncated = fit_list(&mut list, max, r#"{"sessions":[],"truncated":true}"#.len());
    let mut out = json!({ "sessions": list });
    if truncated {
        out["truncated"] = true.into();
    }
    out
}

/// Los turnos que `reader` aún no ha leído de la conversación de `target`, y su posición hasta el
/// final; con `last`, los `last` últimos, sin mover la posición.
async fn unread_turns(
    st: &AppState,
    reader: &str,
    target: &str,
    link: &ClaudeLink,
    last: Option<usize>,
) -> Result<Vec<Turn>, Fail> {
    let Some(path) = server::transcript_of(st, link) else {
        // Un Claude recién abierto aún no ha escrito nada.
        return Ok(vec![]);
    };
    let internal = |e: String| fail(StatusCode::INTERNAL_SERVER_ERROR, e);
    if let Some(n) = last {
        return tokio::task::spawn_blocking(move || last_turns(&path, n))
            .await
            .map_err(|e| internal(e.to_string()))?
            .map_err(|e| internal(e.to_string()));
    }
    // Dos lecturas a la vez de la misma sesión: la que llega tarde al cursor vuelve a leer desde
    // donde lo dejó la otra, para no devolver dos veces lo mismo.
    loop {
        let from = st.mcp_cursors.get(reader, target, &path);
        let p = path.clone();
        let chunk = tokio::task::spawn_blocking(move || transcript::read_chunk(&p, from, None))
            .await
            .map_err(|e| internal(e.to_string()))?
            .map_err(|e| internal(e.to_string()))?;
        if st
            .mcp_cursors
            .advance(reader, target, &path, from, chunk.offset)
        {
            return Ok(turns(chunk.items));
        }
    }
}

/// Los `n` últimos turnos, leyendo solo el final de la transcripción (y más hacia atrás si en ese
/// trozo no salen bastantes).
pub fn last_turns(path: &Path, n: usize) -> std::io::Result<Vec<Turn>> {
    let len = std::fs::metadata(path)?.len();
    let mut window = TAIL;
    loop {
        let from = line_start(path, len.saturating_sub(window))?;
        let mut t = turns(transcript::read_chunk(path, from, None)?.items);
        if t.len() >= n || from == 0 {
            t.drain(..t.len().saturating_sub(n));
            return Ok(t);
        }
        window = window.saturating_mul(4);
    }
}

/// El principio de la primera línea entera a partir de `at` (lo de antes es media línea).
fn line_start(path: &Path, at: u64) -> std::io::Result<u64> {
    use std::io::{BufRead, Seek, SeekFrom};
    if at == 0 {
        return Ok(0);
    }
    let mut f = std::io::BufReader::new(std::fs::File::open(path)?);
    f.seek(SeekFrom::Start(at - 1))?;
    let mut skipped = Vec::new();
    let read = f.read_until(b'\n', &mut skipped)?;
    Ok(at - 1 + read as u64)
}

/// Cómo está la sesión `id` ahora. Hace un `scan()` entero (todas las sesiones) cada vez, también
/// en cada `status` que pide la espera de otro equipo (cada 2 s): aún sin mirar solo la pedida.
async fn poll_status(st: &AppState, id: &str) -> (Status, Option<Menu>) {
    match view(st, id).await {
        Some(v) => status_of(st, &v).await,
        None => (Status::Stopped, None),
    }
}

/// La transcripción de ahora de la sesión `id`, si ya la tiene.
async fn transcript_now(st: &AppState, id: &str) -> Option<PathBuf> {
    transcript_where(st, id)
        .await
        .filter(|p| p.parent() != Some(Path::new("")))
}

/// La transcripción de la conversación de `id` (guardada en la bandeja tras encontrarla la
/// primera vez, y buscada fuera del runtime) o, si su Claude aún no la ha escrito, su nombre
/// (`<session_id>.jsonl`, ver `inbox::Mark`). `None`: la sesión no tiene Claude.
async fn transcript_where(st: &AppState, id: &str) -> Option<PathBuf> {
    let link = st
        .registry
        .lock()
        .await
        .get(id)
        .and_then(|s| s.claude.clone())?;
    let config = link
        .config_dir
        .clone()
        .unwrap_or_else(|| claude::default_config_dir(&st.paths.home));
    let conversation = format!("{}\n{}", link.session_id, config.display());
    if let Some(p) = st.inbox.transcript(id, &conversation) {
        return Some(p);
    }
    let sid = link.session_id.clone();
    let found = tokio::task::spawn_blocking(move || transcript::find(&config, &sid))
        .await
        .ok()
        .flatten();
    match found {
        Some(p) => {
            st.inbox.set_transcript(id, &conversation, p.clone());
            Some(p)
        }
        None => Some(PathBuf::from(format!("{}.jsonl", link.session_id))),
    }
}

/// Lo que devuelve `read` de `s` para `reader`, con el estado y el menú que se acaban de ver: lo
/// nuevo de su conversación (o los `last` últimos turnos) y la pregunta, recortado a `MAX_READ`.
async fn read_out(
    st: &AppState,
    reader: &str,
    s: &Session,
    state: Status,
    menu: Option<Menu>,
    last: Option<usize>,
) -> Result<ReadOut, Fail> {
    // Mientras se esperaba pudo empezar otra conversación (`/clear`).
    let link = st
        .registry
        .lock()
        .await
        .get(&s.id)
        .and_then(|x| x.claude.clone())
        .or_else(|| s.claude.clone())
        .expect("sesión de Claude");
    let turns = unread_turns(st, reader, &s.id, &link, last).await?;
    // Si la pregunta es la de confiar en la carpeta, se dice: `answer` no la contesta, y así lo
    // sabe la madre en cuanto la lee (sale unos segundos después de `open`).
    let note = match menu {
        Some(_) => screen_of(st, &s.id)
            .await
            .filter(|scr| prompt::trust_screen(scr))
            .map(|_| i18n::tr(&trust_note(&s.cwd, s.opened_by.is_some()))),
        None => None,
    };
    let mut out = ReadOut {
        state,
        turns,
        question: menu.as_ref().map(Question::from),
        truncated: false,
        delivery_error: delivery_error(st, &s.id),
        note,
    };
    fit(&mut out, MAX_READ);
    Ok(out)
}

/// `read` de la sesión `at` (aquí o, para una de aquí, en otro equipo), sin esperar. Con
/// `limit` (la lectura del final de una espera), a otro equipo se le espera eso como mucho.
async fn read_at(
    st: &AppState,
    me: &Machine,
    caller: &Caller,
    at: &Where<'_>,
    last: Option<usize>,
    limit: Option<Duration>,
) -> Reply {
    match at {
        Where::Here(id) => {
            let s = session_here(st, id).await?;
            let (state, menu) = poll_status(st, id).await;
            to_json(&read_out(st, &caller.reader(), &s, state, menu, last).await?)
        }
        Where::There { machine, id } => {
            let mut args = json!({ "session": id });
            if let Some(n) = last {
                args["last"] = n.into();
            }
            let ask = forward(st, me, caller, machine, "read", args);
            match limit {
                None => ask.await,
                Some(l) => tokio::time::timeout(l, ask)
                    .await
                    .unwrap_or_else(|_| Err(no_answer())),
            }
        }
    }
}

/// Cómo está la sesión `id` de aquí y, con `msg` (de `send`), si ya lo recogió y si ha contestado.
/// Vale para cualquier sesión, sea o no de Claude: también dice si vive una sesión madre. De una
/// que no es de Claude, solo si vive (`{state}`): nada de su pantalla ni de su bandeja.
async fn status_here(st: &AppState, id: &str, msg: Option<&str>) -> Result<StatusOut, Fail> {
    let found = st.registry.lock().await.get(id).map(|s| s.claude.is_some());
    let Some(claude) = found else {
        return Err(fail(StatusCode::NOT_FOUND, "no existe esa sesión"));
    };
    if !claude {
        let alive = view(st, id).await.is_some_and(|v| v.alive);
        return Ok(StatusOut {
            state: status(alive, false, false, false),
            question: None,
            taken: None,
            replied: None,
        });
    }
    let (state, menu) = poll_status(st, id).await;
    let (taken, replied) = match msg {
        Some(m) => (Some(st.inbox.taken(id, m)), Some(replied(st, id, m).await)),
        None => (None, None),
    };
    Ok(StatusOut {
        state,
        question: menu.as_ref().map(Question::from),
        taken,
        replied,
    })
}

/// ¿Ha escrito Claude algo en la conversación de `id` desde que se encoló `msg`? Sin marca (más
/// de `MARK_TTL`, o `nido serve` se reinició), no se sabe: no.
async fn replied(st: &AppState, id: &str, msg: &str) -> bool {
    let Some(mark) = st.mcp_marks.get(msg, id) else {
        return false;
    };
    if mark.replied {
        return true;
    }
    let Some(now) = transcript_now(st, id).await else {
        return false;
    };
    let seen = {
        let now = now.clone();
        tokio::task::spawn_blocking(move || replied_since(mark.path.as_deref(), mark.from, &now))
            .await
    };
    match seen {
        Ok(Ok((yes, upto))) => {
            st.mcp_marks.seen(msg, now, upto, yes);
            yes
        }
        _ => false,
    }
}

/// Lo que miran las esperas de la sesión `at` (aquí o en otro equipo), con la espera hasta
/// `until`: a otro equipo se le espera `STATUS_POLL` como mucho, o lo que quede.
async fn status_at(
    st: &AppState,
    me: &Machine,
    caller: &Caller,
    at: &Where<'_>,
    msg: Option<&str>,
    until: tokio::time::Instant,
) -> Result<Seen, Miss> {
    let v = match at {
        // Como en `read`: solo las de Claude.
        Where::Here(id) => async {
            session_here(st, id).await?;
            to_json(&status_here(st, id, msg).await?)
        }
        .await
        .map_err(Miss::Fatal)?,
        Where::There { machine, id } => {
            let mut args = json!({ "session": id });
            if let Some(m) = msg {
                args["msg"] = m.into();
            }
            let left = until.saturating_duration_since(tokio::time::Instant::now());
            let ask = forward(st, me, caller, machine, "status", args);
            match tokio::time::timeout(STATUS_POLL.min(left).max(WAIT_STEP / 2), ask).await {
                Ok(Ok(v)) => v,
                Ok(Err(f)) if passing(&f) => return Err(Miss::Channel(f)),
                Ok(Err(f)) => return Err(Miss::Fatal(f)),
                Err(_) => return Err(Miss::Channel(no_answer())),
            }
        }
    };
    serde_json::from_value(v).map_err(|_| {
        Miss::Fatal(fail(
            StatusCode::BAD_GATEWAY,
            "respuesta no válida del otro equipo",
        ))
    })
}

async fn tool_status(st: &AppState, me: &Machine, caller: &Caller, args: Value) -> Reply {
    let raw = args.clone();
    let a: StatusArgs = args_of(args)?;
    match place(me, caller, &a.session)? {
        Where::Here(id) => to_json(&status_here(st, id, a.msg.as_deref()).await?),
        Where::There { machine, id } => {
            forward(st, me, caller, &machine, "status", with_session(raw, id)).await
        }
    }
}

async fn tool_read(st: &AppState, me: &Machine, caller: &Caller, args: Value) -> Reply {
    let a: ReadArgs = args_of(args)?;
    let at = place(me, caller, &a.session)?;
    // Para otro equipo no se espera aquí: el ida y vuelta por el hub dura 15 s como mucho. Espera
    // el nido de quien llama, preguntando cada poco.
    if !(a.wait && caller.local) {
        return read_at(st, me, caller, &at, a.last, None).await;
    }
    let read = || read_at(st, me, caller, &at, a.last, Some(STATUS_POLL));
    let total = wait_timeout(a.timeout_s);
    let until = tokio::time::Instant::now() + total;
    wait_then_read(
        || status_at(st, me, caller, &at, None, until),
        read,
        None,
        WAIT_STEP,
        total,
    )
    .await
}

async fn tool_send(st: &AppState, me: &Machine, caller: &Caller, args: Value) -> Reply {
    let a: SendArgs = args_of(args)?;
    let text = a.text.trim();
    if text.is_empty() {
        return Err(bad_request("falta el texto".into()));
    }
    if text.len() > MAX_PROMPT {
        return Err(bad_request(format!(
            "el texto pasa de {} KB",
            MAX_PROMPT / 1024
        )));
    }
    // Antes de buscarla: la que llama puede no ser de Claude, y saldría «no existe». La de otro
    // equipo no está aquí: su dirección ya da error en `place`.
    if caller.local && to_self(me, &a.session, &caller.id) {
        return Err(bad_request("no puedes mandarte mensajes a ti misma".into()));
    }
    let at = place(me, caller, &a.session)?;
    let mut out = match &at {
        Where::Here(id) => send_here(st, me, caller, id, text).await?,
        // Sin `wait`: se espera aquí.
        Where::There { machine, id } => {
            let args = json!({ "session": id, "text": text });
            forward(st, me, caller, machine, "send", args).await?
        }
    };
    // Como en `read`: para otro equipo, sin esperar aquí.
    let Some(secs) = a.wait.filter(|&w| w > 0 && caller.local) else {
        return Ok(out);
    };
    // Un nido antiguo no da el id: sin él no se sabe si lo recogió, y se espera solo el rato de
    // recogida.
    let msg = out["msg_id"].as_str().map(str::to_owned);
    let total = wait_timeout(Some(secs));
    let until = tokio::time::Instant::now() + total;
    let read = wait_then_read(
        || status_at(st, me, caller, &at, msg.as_deref(), until),
        || read_at(st, me, caller, &at, None, Some(STATUS_POLL)),
        Some(PICKUP),
        WAIT_STEP,
        total,
    )
    .await;
    // El mensaje ya está en la cola: lo que falle al esperar no lo deshace.
    out["read"] = read.unwrap_or_else(|f| json!({ "error": i18n::tr(&f.1) }));
    Ok(out)
}

/// Encola el mensaje para la sesión `id` de aquí: `{status, note?, msg_id}`.
async fn send_here(st: &AppState, me: &Machine, caller: &Caller, id: &str, text: &str) -> Reply {
    let s = session_here(st, id).await?;
    let alive = view(st, &s.id).await.is_some_and(|v| v.alive);
    let holder = launch::holder_started(&st.paths, &s.id)
        .map(|t| SystemTime::now().duration_since(t).unwrap_or_default());
    let d = decide(&Dest {
        alive,
        channel: st.inbox.channel(&s.id),
        started: Some(started(holder, st.inbox.uptime())),
        mine: caller.owns(me, s.opened_by.as_deref()),
        refused: st.inbox.refusal(&s.id),
    })?;
    let msg = Msg {
        id: server::uuid_v4(),
        from: caller.name.clone(),
        from_id: caller.address(),
        machine: caller.machine_name.clone(),
        text: text.to_owned(),
        at: now_ms(),
    };
    let msg_id = msg.id.clone();
    // Dónde está su conversación antes de mandarlo: lo que escriba Claude después es la respuesta.
    let path = transcript_now(st, &s.id).await;
    let len = path
        .as_ref()
        .and_then(|p| std::fs::metadata(p).ok())
        .map_or(0, |m| m.len());
    st.mcp_marks.put(&msg_id, &s.id, path, len, Instant::now());
    st.inbox
        .push(&s.id, msg)
        .map_err(|_| fail(StatusCode::CONFLICT, "la bandeja de esa sesión está llena"))?;
    let mut out = json!({ "status": d.status, "msg_id": msg_id });
    if let Some(note) = d.note {
        out["note"] = i18n::tr(note).into();
    }
    Ok(out)
}

async fn tool_open(st: &AppState, me: &Machine, caller: &Caller, args: Value) -> Reply {
    let raw = args.clone();
    let a: OpenArgs = args_of(args)?;
    if !caller.local && !me.is_me(&a.machine) {
        return Err(bad_request(format!(
            "«{}» no es este equipo",
            a.machine.trim()
        )));
    }
    if caller.local
        && let Some(m) = me.find(&a.machine)?
    {
        // Allí, el equipo por su id: es el suyo.
        let mut args = raw;
        args["machine"] = m.id.clone().into();
        return forward(st, me, caller, m, "open", args).await;
    }
    let cwd = server::expand_home(&st.paths.home, Some(PathBuf::from(a.folder.trim())));
    if !cwd.is_absolute() {
        return Err(bad_request(format!(
            "«{}» no es una ruta completa",
            cwd.display()
        )));
    }
    if !cwd.is_dir() {
        return Err(bad_request(format!(
            "no existe la carpeta {}",
            cwd.display()
        )));
    }
    let model = a.model.as_deref().map(str::trim).map(str::to_owned);
    if let Some(m) = model.as_deref()
        && (m.is_empty() || m.starts_with('-'))
    {
        return Err(bad_request(format!("modelo no válido: {m:?}")));
    }
    if model
        .as_deref()
        .is_some_and(|m| m.chars().count() > MAX_MODEL)
    {
        return Err(bad_request(format!(
            "el modelo pasa de {MAX_MODEL} caracteres"
        )));
    }
    if a.prompt.trim().is_empty() {
        return Err(bad_request("falta el mensaje".into()));
    }
    if a.prompt.trim().len() > MAX_PROMPT {
        return Err(bad_request(format!(
            "el mensaje pasa de {} KB",
            MAX_PROMPT / 1024
        )));
    }
    let opener = caller.address();
    let message = first_message(&caller.name, &caller.machine_name, &opener, a.prompt.trim());
    // Sin caracteres de control y con tope (`clean_name`); vacío, el de la carpeta.
    let name = a.name.as_deref().map(clean_name).filter(|n| !n.is_empty());
    let created = {
        let mut reg = st.registry.lock().await;
        // Con tope también las de aquí (las abiertas con `open` desde este equipo, de cualquier
        // sesión): una sesión desbocada no llena el equipo de Claudes.
        let from = format!("{}/", caller.machine_key);
        let alive = reg
            .sessions()
            .iter()
            .filter(|s| s.opened_by.as_deref().is_some_and(|o| o.starts_with(&from)))
            .map(|s| launch::is_alive(&st.paths, &s.id));
        if !open_allowed(alive) {
            return Err(fail(
                StatusCode::CONFLICT,
                if caller.local {
                    "demasiadas sesiones abiertas con open en este equipo; cierra alguna"
                } else {
                    "demasiadas sesiones abiertas desde ese equipo; cierra alguna"
                },
            ));
        }
        let session = opened_session(&reg, cwd, name, model.as_deref(), &message, opener);
        reg.add(session).map_err(Fail::from)?
    };
    let hold = st.launching(&created.id);
    if let Err(e) = launch::start(&st.paths, st.launcher, &created.id) {
        // Que no quede apuntada una sesión que no arrancó.
        let _ = st.registry.lock().await.remove(&created.id);
        return Err(fail(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("no se pudo arrancar: {e}"),
        ));
    }
    server::wait_alive(st, &created.id).await;
    st.launched(&created.id, hold);
    st.refresh.notify_one();
    // La pregunta de confiar en la carpeta sale unos segundos después: no se espera aquí (a otro
    // equipo se le contesta en 15 s como mucho). La dice `read` (ver `trust_note`).
    let Some(v) = view(st, &created.id).await else {
        return Err(fail(StatusCode::NOT_FOUND, "no existe esa sesión"));
    };
    let (state, _) = status_of(st, &v).await;
    to_json(&entry(st, me, caller, &v, state))
}

/// Lo que se dice de una sesión en la pregunta de confiar en su carpeta (en `read`, y el error
/// de `answer`). Si la abrió otra con `open` (`opened`), al usuario ya se le ha avisado (ver
/// `AppState::trust_waiting`); si no, la tiene delante.
fn trust_note(cwd: &Path, opened: bool) -> String {
    if opened {
        format!(
            "la sesión espera a que el usuario confíe en la carpeta {}; solo el usuario puede \
             hacerlo (ya se le ha avisado)",
            cwd.display()
        )
    } else {
        format!(
            "la sesión espera a que el usuario confíe en la carpeta {}; solo el usuario puede \
             hacerlo",
            cwd.display()
        )
    }
}

/// La sesión que apunta `open` en el registro (sin arrancarla): con el nombre pedido o uno libre
/// sacado de la carpeta.
fn opened_session(
    reg: &Registry,
    cwd: PathBuf,
    name: Option<String>,
    model: Option<&str>,
    message: &str,
    opened_by: String,
) -> Session {
    let name = name.unwrap_or_else(|| name_for(&cwd, |n| reg.by_name(n).is_some()));
    let sid = server::uuid_v4();
    Session {
        id: new_id(),
        argv: open_argv(&name, &sid, model, message),
        name,
        cwd,
        env: vec![],
        autostart: true,
        created: now(),
        claude: Some(ClaudeLink {
            session_id: sid,
            config_dir: None,
            account: None,
            inside_shell: false,
        }),
        folder: None,
        finished: 0,
        stopped: 0,
        seen: 0,
        opened_by: Some(opened_by),
        alerts: vec![],
    }
}

async fn tool_answer(st: &AppState, me: &Machine, caller: &Caller, args: Value) -> Reply {
    let raw = args.clone();
    let a: AnswerArgs = args_of(args)?;
    let id = match place(me, caller, &a.session)? {
        Where::Here(id) => id,
        Where::There { machine, id } => {
            return forward(st, me, caller, &machine, "answer", with_session(raw, id)).await;
        }
    };
    let s = session_here(st, id).await?;
    if !caller.owns(me, s.opened_by.as_deref()) {
        return Err(fail(
            StatusCode::FORBIDDEN,
            "solo puedes contestar las sesiones que abriste",
        ));
    }
    if a.option.is_none() && a.text.is_none() {
        return Err(fail(StatusCode::BAD_REQUEST, "falta la opción o el texto"));
    }
    let _one = st.answering.lock().await;
    let Ok(stream) = server::holder_connect(st, &s.id, viewer()).await else {
        return Err(fail(StatusCode::CONFLICT, "la sesión no está en marcha"));
    };
    let (rd, wr) = crate::ipc::split(stream);
    let mut scr = prompt::Screen::new(rd, wr);
    let screen = scr
        .screen()
        .await
        .map_err(|e| fail(StatusCode::CONFLICT, e))?;
    // Confiar en la carpeta es siempre del usuario (a él se le avisa: ver
    // `AppState::trust_waiting`).
    if prompt::trust_screen(&screen) {
        return Err(fail(
            StatusCode::FORBIDDEN,
            trust_note(&s.cwd, s.opened_by.is_some()),
        ));
    }
    let Some(menu) = prompt::parse(&screen) else {
        return Err(fail(
            StatusCode::CONFLICT,
            "no hay ninguna pregunta pendiente",
        ));
    };
    // Solo texto: el campo de texto del menú.
    let option = match a.option {
        Some(o) => o,
        None => menu
            .options
            .iter()
            .find(|o| o.input)
            .map(|o| o.n.map_or_else(|| o.label.clone(), |n| n.to_string()))
            .ok_or_else(|| fail(StatusCode::BAD_REQUEST, "la pregunta no admite texto libre"))?,
    };
    let req = prompt::Request {
        questions: vec![],
        option: Some(option),
        suggestion: None,
        message: None,
        form: None,
        text: a.text,
    };
    prompt::answer(&mut scr, &req)
        .await
        .map_err(|e| fail(StatusCode::CONFLICT, e))?;
    Ok(json!({ "ok": true }))
}

async fn tool_close(st: &AppState, me: &Machine, caller: &Caller, args: Value) -> Reply {
    let raw = args.clone();
    let a: SessionArg = args_of(args)?;
    let id = match place(me, caller, &a.session)? {
        Where::Here(id) => id,
        Where::There { machine, id } => {
            return forward(st, me, caller, &machine, "close", with_session(raw, id)).await;
        }
    };
    let s = session_here(st, id).await?;
    if !caller.owns(me, s.opened_by.as_deref()) {
        return Err(fail(
            StatusCode::FORBIDDEN,
            "solo puedes cerrar las sesiones que abriste",
        ));
    }
    server::delete_session(st, &s.id)
        .await
        .map_err(Fail::from)?;
    Ok(json!({ "ok": true }))
}

/// `start`: arranca una sesión de Claude parada de cualquier equipo del grupo, retomando su
/// conversación (como «Arrancar» en la web; recibe los mensajes que tuviera en cola). Espera a
/// que esté en marcha y devuelve su estado. Pasa por el permiso de Claude.
async fn tool_start(st: &AppState, me: &Machine, caller: &Caller, args: Value) -> Reply {
    let raw = args.clone();
    let a: SessionArg = args_of(args)?;
    let id = match place(me, caller, &a.session)? {
        Where::Here(id) => id,
        Where::There { machine, id } => {
            return forward(st, me, caller, &machine, "start", with_session(raw, id)).await;
        }
    };
    let s = session_here(st, id).await?;
    let was_alive = launch::is_alive(&st.paths, &s.id);
    if !was_alive {
        match server::start_session(st, &s.id).await {
            Ok(true) => {}
            Ok(false) => {
                return Err(fail(
                    StatusCode::GATEWAY_TIMEOUT,
                    "arrancó pero no llegó a estar en marcha",
                ));
            }
            Err(e) => {
                return Err(fail(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("no se pudo arrancar: {e}"),
                ));
            }
        }
    }
    let state = match view(st, &s.id).await {
        Some(v) => status_of(st, &v).await.0,
        None => Status::Stopped,
    };
    let mut out = json!({ "state": state });
    if was_alive {
        out["note"] = i18n::tr("ya estaba en marcha").into();
    }
    Ok(out)
}

/// Si se puede parar una sesión en este estado: trabajando o con una pregunta pendiente, solo con
/// `force`.
fn may_stop(state: Status, force: bool) -> Result<(), Fail> {
    if !force && matches!(state, Status::Working | Status::Question) {
        return Err(fail(
            StatusCode::CONFLICT,
            "está trabajando (o tiene una pregunta pendiente): para pararla igualmente, force: true",
        ));
    }
    Ok(())
}

/// `stop`: para una sesión de Claude de cualquier equipo del grupo (la conversación se conserva;
/// se retoma con `start`). Pasa por el permiso de Claude: no va en `--allowedTools`.
async fn tool_stop(st: &AppState, me: &Machine, caller: &Caller, args: Value) -> Reply {
    let raw = args.clone();
    let a: StopArgs = args_of(args)?;
    let id = match place(me, caller, &a.session)? {
        Where::Here(id) => id,
        Where::There { machine, id } => {
            return forward(st, me, caller, &machine, "stop", with_session(raw, id)).await;
        }
    };
    if caller.local && id == caller.id {
        return Err(fail(
            StatusCode::BAD_REQUEST,
            "una sesión no puede pararse a sí misma",
        ));
    }
    let s = session_here(st, id).await?;
    let state = match view(st, &s.id).await {
        Some(v) => status_of(st, &v).await.0,
        None => Status::Stopped,
    };
    if state == Status::Stopped {
        // Se apunta que se queda parada: la que salió sola volvería a arrancar con el equipo.
        // Y se para igual, por si su guardián quedó a medio arrancar (`launch::stop` no manda
        // nada a un pidfile viejo cuyo número es ya de otro proceso).
        server::stop_session(st, &s.id).await;
        return Ok(json!({ "state": Status::Stopped, "note": i18n::tr("ya estaba parada") }));
    }
    may_stop(state, a.force)?;
    server::stop_session(st, &s.id).await;
    Ok(json!({ "state": Status::Stopped }))
}

/// Solo desde una sesión de aquí: no está entre las de `PEER_TOOLS`.
async fn tool_alert(st: &Arc<AppState>, caller: &Session, args: Value) -> Reply {
    let a: AlertArgs = args_of(args)?;
    let Some(text) = server::alert_text(&a.text) else {
        return Err(fail(StatusCode::BAD_REQUEST, "la alerta no tiene texto"));
    };
    st.session_alert(&caller.id, &text)
        .await
        .map_err(Fail::from)?;
    Ok(json!({ "ok": true }))
}

// ---------- La sesión madre ----------

/// Si vive la sesión madre (de otro equipo) de una abierta con `open`, por su dirección, y
/// cuándo se preguntó; se recuerda `ttl()` (`PARENT_TTL`, o lo que pongan las pruebas).
pub struct Parents {
    map: std::sync::Mutex<HashMap<String, (Instant, bool)>>,
    ttl_ms: std::sync::atomic::AtomicU64,
}

impl Default for Parents {
    fn default() -> Self {
        Self {
            map: Default::default(),
            ttl_ms: (PARENT_TTL.as_millis() as u64).into(),
        }
    }
}

impl Parents {
    /// Cuánto se recuerda, y cada cuánto se vuelve a mirar un aviso que se calló por ella.
    pub fn ttl(&self) -> Duration {
        Duration::from_millis(self.ttl_ms.load(std::sync::atomic::Ordering::Relaxed))
    }

    /// Para las pruebas.
    pub fn set_ttl(&self, d: Duration) {
        let ms = u64::try_from(d.as_millis()).unwrap_or(u64::MAX);
        self.ttl_ms.store(ms, std::sync::atomic::Ordering::Relaxed);
    }

    fn get(&self, by: &str, now: Instant) -> Option<bool> {
        let ttl = self.ttl();
        self.map
            .lock()
            .expect("madres")
            .get(by)
            .filter(|(t, _)| now.saturating_duration_since(*t) < ttl)
            .map(|(_, alive)| *alive)
    }

    fn put(&self, by: &str, alive: bool, now: Instant) {
        let ttl = self.ttl();
        let mut m = self.map.lock().expect("madres");
        m.retain(|_, (t, _)| now.saturating_duration_since(*t) < ttl);
        m.insert(by.to_owned(), (now, alive));
    }
}

/// ¿Vive la sesión que abrió con `open` a la sesión `child` (`by`, su `opened_by`)? Lo que
/// avisaría de la hija es para ella mientras viva. Vive si no está parada (`status`), sea o no de
/// Claude, aquí o en otro equipo: la de aquí se mira aquí; a la de otro equipo se le pide su
/// `status` (`FAN_OUT` como mucho, y se recuerda `Parents::ttl`, salvo con `fresh`). Sin
/// respuesta, con un error o de un equipo que ya no está en el grupo, no vive.
pub async fn parent_alive(
    st: &AppState,
    by: &str,
    child: &str,
    child_name: &str,
    fresh: bool,
) -> bool {
    let by = by.trim();
    let me = Machine::of(st);
    if let Some(id) = here(&me, by) {
        return alive_state(status_here(st, id, None).await.map(|s| s.state));
    }
    let Some((machine, id)) = by.rsplit_once('/') else {
        return false;
    };
    let Some(m) = me.others.iter().find(|m| m.id == machine.trim()) else {
        return false;
    };
    if !fresh && let Some(alive) = st.mcp_parents.get(by, Instant::now()) {
        return alive;
    }
    let ask = st.peer_member(
        m,
        "status",
        json!({ "session": id.trim(), "caller": { "id": child, "name": child_name } }),
    );
    let state = match tokio::time::timeout(FAN_OUT, ask).await {
        Ok(Ok(v)) => serde_json::from_value::<Seen>(v)
            .map(|s| s.state)
            .map_err(|e| fail(StatusCode::BAD_GATEWAY, e.to_string())),
        Ok(Err(e)) => Err(peer_fail(e)),
        Err(_) => Err(fail(StatusCode::GATEWAY_TIMEOUT, "")),
    };
    let alive = alive_state(state);
    st.mcp_parents.put(by, alive, Instant::now());
    alive
}

/// La regla de `parent_alive`, la misma aquí y en otro equipo.
fn alive_state(state: Result<Status, Fail>) -> bool {
    state.is_ok_and(|s| s != Status::Stopped)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn since_is_when_it_started_or_when_it_stopped() {
        let t = UNIX_EPOCH + Duration::from_millis(5_000);
        // En marcha: cuando arrancó su guardián (lo que haya en `stopped` es de antes).
        assert_eq!(since(true, Some(t), 9_000), Some(5_000));
        assert_eq!(since(true, None, 9_000), None);
        // Parada: cuando nido la vio pararse; si no se sabe, nada.
        assert_eq!(since(false, Some(t), 9_000), Some(9_000));
        assert_eq!(since(false, Some(t), 0), None);
    }

    #[test]
    fn stop_refuses_a_busy_session_unless_forced() {
        for busy in [Status::Working, Status::Question] {
            let e = may_stop(busy, false).unwrap_err();
            assert_eq!(e.0, StatusCode::CONFLICT);
            assert!(e.1.starts_with("está trabajando"), "{}", e.1);
            assert!(may_stop(busy, true).is_ok());
        }
        assert!(may_stop(Status::Idle, false).is_ok());
        assert!(may_stop(Status::Stopped, false).is_ok());
    }

    #[test]
    fn the_delivery_clock_runs_only_with_claude_idle() {
        let view = |alive: bool, status: Option<&str>| SessionView {
            id: "s".into(),
            name: "s".into(),
            cwd: "/tmp".into(),
            argv: vec!["claude".into()],
            autostart: false,
            created: 0,
            stopped: 0,
            alive,
            claude: Some(crate::server::ClaudeView {
                background: None,
                session_id: "c".into(),
                running: true,
                status: status.map(str::to_owned),
                title: None,
                account: None,
            }),
            folder: None,
            unread: false,
            opened_by: None,
            alerts: vec![],
            secret: None,
        };
        let secs = Duration::from_secs;
        let quiet = (secs(5), secs(90));
        let runs =
            |v: SessionView, silent: Option<u64>| clock_runs(Some(&v), silent.map(secs), quiet);
        // Quieta y sin escribir en un rato; o sin transcripción (no está escribiendo).
        assert!(runs(view(true, Some("idle")), Some(5)));
        assert!(runs(view(true, Some("idle")), None));
        assert!(!runs(view(true, Some("idle")), Some(4)), "aún escribe");
        // Trabajando, o con un permiso o una pregunta en pantalla: Claude lo guarda para luego.
        assert!(!runs(view(true, Some("busy")), Some(500)));
        assert!(!runs(view(true, Some("waiting")), None));
        // Sin saber su estado: solo tras mucho más rato sin escribir (o sin transcripción).
        assert!(!runs(view(true, None), Some(60)));
        assert!(runs(view(true, None), Some(90)));
        assert!(runs(view(true, None), None));
        let mut unseen = view(true, None);
        unseen.claude = None;
        assert!(!runs(unseen.clone(), Some(60)));
        assert!(runs(unseen, None));
        // Parada, o aún sin ver por el escáner: no.
        assert!(!runs(view(false, Some("idle")), None));
        assert!(!clock_runs(None, None, quiet));
    }

    fn me(key: Option<&str>) -> Machine {
        Machine {
            name: "Portatil".into(),
            key: key.map(str::to_owned),
            others: vec![],
        }
    }

    fn member(id: &str, name: &str) -> Member {
        Member {
            id: id.into(),
            key: format!("clave-{id}"),
            name: name.into(),
            at: 0,
            removed: false,
        }
    }

    /// Este equipo con otros en el grupo (id, nombre).
    fn grouped(key: Option<&str>, others: &[(&str, &str)]) -> Machine {
        Machine {
            others: others.iter().map(|(i, n)| member(i, n)).collect(),
            ..me(key)
        }
    }

    fn here_of<'a>(m: &Machine, a: &'a str) -> &'a str {
        match locate(m, a).unwrap() {
            Where::Here(id) => id,
            w => panic!("{a}: {w:?}"),
        }
    }

    fn there_of(m: &Machine, a: &str) -> (String, String) {
        match locate(m, a).unwrap() {
            Where::There { machine, id } => (machine.id, id.to_owned()),
            w => panic!("{a}: {w:?}"),
        }
    }

    #[test]
    fn addresses() {
        let m = grouped(Some("k123"), &[("mac-id", "Mac")]);
        assert_eq!(here_of(&m, "ab12cd34"), "ab12cd34");
        assert_eq!(here_of(&m, "Portatil/ab12cd34"), "ab12cd34");
        assert_eq!(here_of(&m, "PORTATIL/ab12cd34"), "ab12cd34");
        assert_eq!(here_of(&m, "k123/ab12cd34"), "ab12cd34");
        assert_eq!(here_of(&m, " Portatil/ab12cd34 "), "ab12cd34");
        // Otro del grupo, por su nombre (sin mirar mayúsculas) o por su id.
        for a in [
            "Mac/ab12cd34",
            "mac/ab12cd34",
            "mac-id/ab12cd34",
            " Mac/ab12cd34 ",
        ] {
            assert_eq!(there_of(&m, a), ("mac-id".into(), "ab12cd34".into()), "{a}");
        }
        // Uno que no es del grupo: 404. El id en el hub tal cual: otro id no es este.
        for a in ["Sobremesa/ab12cd34", "K123/ab12cd34", "MAC-ID/ab"] {
            let e = locate(&m, a).unwrap_err();
            assert_eq!(e.0, StatusCode::NOT_FOUND, "{a}");
        }
        assert_eq!(
            locate(&m, "Sobremesa/x").unwrap_err().1,
            "«Sobremesa» no es ningún equipo del grupo"
        );
        // Sin conexión con el hub, este solo por el nombre.
        assert!(locate(&me(None), "k123/ab12cd34").is_err());
        assert_eq!(here_of(&me(None), "portatil/ab"), "ab");
    }

    #[test]
    fn machine_labels_are_the_name_unless_it_is_repeated() {
        // Nombres distintos: cada uno por su nombre.
        let m = grouped(Some("k123"), &[("mac-id", "Mac"), ("win-id", "Sobremesa")]);
        assert_eq!(m.label(), "Portatil");
        assert_eq!(m.shown("ab"), "Portatil/ab");
        assert_eq!(m.label_of(&m.others[0]), "Mac");
        // WSL toma el nombre del Windows en que corre: los dos, por su id (sin mirar mayúsculas).
        let m = grouped(Some("k123"), &[("mac-id", "Mac"), ("win-id", "portatil")]);
        assert_eq!(m.label(), "k123");
        assert_eq!(m.shown("ab"), "k123/ab");
        assert_eq!(m.label_of(&m.others[1]), "win-id");
        assert_eq!(m.label_of(&m.others[0]), "Mac");
        // Dos de los otros con el mismo nombre; este sin hub sigue con el suyo.
        let m = grouped(None, &[("a-id", "Gemelo"), ("b-id", "gemelo")]);
        assert_eq!(m.label(), "Portatil");
        assert_eq!(m.label_of(&m.others[0]), "a-id");
        assert_eq!(m.label_of(&m.others[1]), "b-id");
        // Sin nombre, su id; el nombre, limpio.
        let m = grouped(Some("k"), &[("x-id", " \u{7} "), ("y-id", " Mac\u{7} ")]);
        assert_eq!(m.label_of(&m.others[0]), "x-id");
        assert_eq!(m.label_of(&m.others[1]), "Mac");
        assert_eq!(Machine::name_of(&m.others[0]), "x-id");
    }

    #[test]
    fn a_name_that_is_a_hub_id_is_taken() {
        // Otro equipo se llama como el id de este: su nombre no vale de etiqueta.
        let m = grouped(Some("k123"), &[("mac-id", "k123"), ("win-id", "Sobremesa")]);
        assert_eq!(m.label_of(&m.others[0]), "mac-id");
        assert_eq!(m.label_of(&m.others[1]), "Sobremesa");
        assert_eq!(m.label(), "Portatil");
        // Ni como el id de otro del grupo.
        let m = grouped(
            Some("k123"),
            &[("mac-id", "win-id"), ("win-id", "Sobremesa")],
        );
        assert_eq!(m.label_of(&m.others[0]), "mac-id");
        // Ni este, si su nombre es el id de otro.
        let m = grouped(Some("k123"), &[("Portatil", "Mac")]);
        assert_eq!(m.label(), "k123");
        // Las etiquetas llevan siempre al equipo que nombran.
        for m in [
            grouped(Some("k123"), &[("mac-id", "k123"), ("win-id", "Sobremesa")]),
            grouped(
                Some("k123"),
                &[("mac-id", "win-id"), ("win-id", "Sobremesa")],
            ),
            grouped(Some("k123"), &[("Portatil", "Mac")]),
        ] {
            assert_eq!(here_of(&m, &m.shown("ab")), "ab", "{m:?}");
            for o in &m.others {
                let a = format!("{}/ab", m.label_of(o));
                assert_eq!(there_of(&m, &a).0, o.id, "{a}");
            }
        }
    }

    #[test]
    fn locate_by_label_name_or_id_and_ambiguous_names() {
        let m = grouped(Some("k123"), &[("win-id", "Portatil"), ("mac-id", "Mac")]);
        // Este y el Windows se llaman igual: por el nombre no, por el id (o la etiqueta) sí.
        for a in ["Portatil/ab", "portatil/ab"] {
            let e = locate(&m, a).unwrap_err();
            assert_eq!(e.0, StatusCode::BAD_REQUEST, "{a}");
        }
        assert_eq!(
            locate(&m, "Portatil/ab").unwrap_err().1,
            "hay varios equipos «Portatil» en el grupo: usa su id"
        );
        assert_eq!(here_of(&m, &format!("{}/ab", m.label())), "ab");
        assert_eq!(here_of(&m, "k123/ab"), "ab");
        assert_eq!(there_of(&m, "win-id/ab"), ("win-id".into(), "ab".into()));
        let win = m.label_of(&m.others[0]);
        assert_eq!(there_of(&m, &format!("{win}/ab")).0, "win-id");
        assert_eq!(there_of(&m, "MAC/ab").0, "mac-id");
        // Solo el id, siempre de aquí.
        assert_eq!(here_of(&m, "ab"), "ab");
        // Un id que es justo lo pedido gana a los nombres.
        let m = grouped(Some("k"), &[("Gemelo", "Otro"), ("g2", "gemelo")]);
        assert_eq!(there_of(&m, "Gemelo/ab").0, "Gemelo");
        assert_eq!(there_of(&m, "gemelo/ab").0, "g2");
        // `find`, igual: el equipo de `open`.
        let m = grouped(Some("k123"), &[("win-id", "Portatil"), ("mac-id", "Mac")]);
        assert_eq!(m.find("k123").unwrap(), None);
        assert_eq!(
            m.find("mac").unwrap().map(|x| x.id.as_str()),
            Some("mac-id")
        );
        assert_eq!(m.find("portatil").unwrap_err().0, StatusCode::BAD_REQUEST);
        assert_eq!(m.find("Nadie").unwrap_err().0, StatusCode::NOT_FOUND);
        assert_eq!(me(None).find("PORTATIL").unwrap(), None);
    }

    #[test]
    fn canonical_address_is_the_hub_id_or_the_name() {
        assert_eq!(me(Some("k123")).address("ab"), "k123/ab");
        assert_eq!(me(None).address("ab"), "Portatil/ab");
    }

    #[test]
    fn opened_by_compares_addresses() {
        // Aunque otro equipo se llame igual: la dirección canónica va por el id.
        let m = grouped(Some("k123"), &[("win-id", "Portatil")]);
        assert!(opened_by(&m, Some("k123/padre"), "padre"));
        // Apuntada sin conexión con el hub (por el nombre): sigue siendo suya.
        assert!(opened_by(&m, Some("Portatil/padre"), "padre"));
        assert!(!opened_by(&m, Some("k123/otra"), "padre"));
        assert!(!opened_by(&m, Some("Sobremesa/padre"), "padre"));
        assert!(!opened_by(&m, None, "padre"));
    }

    fn mac() -> crate::group::Member {
        crate::group::Member {
            id: "mac-id".into(),
            key: "clave".into(),
            name: "Mac".into(),
            at: 0,
            removed: false,
        }
    }

    fn remote(id: &str) -> Caller {
        Caller::remote(
            &me(Some("k123")),
            &mac(),
            Some(json!({"id": id, "name": "padre"})),
        )
        .unwrap()
    }

    #[test]
    fn local_and_remote_callers() {
        let m = me(Some("k123"));
        let here = Caller::local(&m, "padre1", "padre");
        assert_eq!(here.address(), "k123/padre1");
        assert_eq!((here.machine_name.as_str(), here.local), ("Portatil", true));
        let there = remote("padre1");
        assert_eq!(there.address(), "mac-id/padre1");
        assert_eq!(
            (
                there.machine_name.as_str(),
                there.name.as_str(),
                there.local
            ),
            ("Mac", "padre", false)
        );
        // Sin hub, la de aquí va por el nombre del equipo.
        assert_eq!(
            Caller::local(&me(None), "padre1", "padre").address(),
            "Portatil/padre1"
        );
        // Cada una lleva sus cursores: la de aquí por su id (así se olvidan al borrarla), la de
        // otro equipo por su dirección.
        assert_eq!(here.reader(), "padre1");
        assert_eq!(there.reader(), "mac-id/padre1");
    }

    #[test]
    fn ownership_by_canonical_address() {
        let m = me(Some("k123"));
        let here = Caller::local(&m, "padre1", "padre");
        let there = remote("padre1");
        // Abierta por la de aquí (con el id en el hub o, sin él, con el nombre).
        for by in ["k123/padre1", "Portatil/padre1"] {
            assert!(here.owns(&m, Some(by)), "{by}");
            assert!(!there.owns(&m, Some(by)), "{by}: mismo id, otro equipo");
        }
        // Abierta por la del Mac.
        assert!(there.owns(&m, Some("mac-id/padre1")));
        assert!(there.owns(&m, Some(" mac-id/padre1 ")));
        assert!(!here.owns(&m, Some("mac-id/padre1")));
        assert!(!remote("otra").owns(&m, Some("mac-id/padre1")));
        assert!(!there.owns(&m, Some("padre1")));
        assert!(!there.owns(&m, None) && !here.owns(&m, None));
    }

    #[test]
    fn remote_caller_must_be_valid() {
        let m = me(Some("k123"));
        let remote_with = |v: Option<Value>| Caller::remote(&m, &mac(), v);
        let bad = |v: Option<Value>| remote_with(v).unwrap_err();
        assert_eq!(bad(None).1, "falta la sesión que llama");
        assert_eq!(bad(None).0, StatusCode::BAD_REQUEST);
        for id in ["", "a/b", "../x", "x y", "ñ", &"a".repeat(65)] {
            assert!(
                bad(Some(json!({"id": id, "name": "n"})))
                    .1
                    .starts_with("sesión que llama no válida"),
                "{id:?}"
            );
        }
        assert!(
            bad(Some(json!("ab")))
                .1
                .starts_with("sesión que llama no válida")
        );
        assert!(
            bad(Some(json!({"name": "n"})))
                .1
                .starts_with("sesión que llama no válida")
        );
        // Lo que no conoce, se deja (versiones más nuevas pueden mandar más).
        let c = remote_with(Some(json!({"id": "ab", "name": "n", "luego": 1}))).unwrap();
        assert_eq!(c.id, "ab");
        // El nombre, sin caracteres de control y no muy largo; sin nombre, el id.
        let c = remote_with(Some(
            json!({"id": "a_B-9", "name": format!("  pa\u{7}dre{}", "x".repeat(100))}),
        ))
        .unwrap();
        assert!(c.name.starts_with("padre"), "{}", c.name);
        assert_eq!(c.name.chars().count(), 64);
        for v in [json!({"id": "ab", "name": "  "}), json!({"id": "ab"})] {
            assert_eq!(
                remote_with(Some(v)).unwrap().name,
                "ab",
                "sin nombre, el id"
            );
        }
    }

    #[test]
    fn remote_machine_name_is_cleaned_and_never_this_one() {
        let m = me(Some("k123"));
        let who = Some(json!({"id": "ab", "name": "padre"}));
        let member = |id: &str, name: &str| crate::group::Member {
            id: id.into(),
            name: name.into(),
            ..mac()
        };
        let c = Caller::remote(
            &m,
            &member("mac-id", &format!(" \u{1b}[31mMac{}\n", "x".repeat(100))),
            who.clone(),
        )
        .unwrap();
        assert!(c.machine_name.starts_with("[31mMac"), "{}", c.machine_name);
        assert_eq!(c.machine_name.chars().count(), 64);
        let c = Caller::remote(&m, &member("mac-id", " \t "), who.clone()).unwrap();
        assert_eq!(c.machine_name, "mac-id", "sin nombre, su id");
        // Un miembro cuyo id es el de este equipo (o su nombre, sin hub): no, su dirección
        // sería de aquí.
        for (id, name) in [("k123", "Mac"), ("Portatil", "Mac")] {
            let e = Caller::remote(&m, &member(id, name), who.clone()).unwrap_err();
            assert_eq!(
                e.1, "la sesión que llama no puede ser de este equipo",
                "{id} {name}"
            );
        }
        // Con el mismo nombre, sí: WSL toma el nombre del Windows en que corre, y la dirección
        // va por el id.
        for name in ["portatil", "k123", " Portatil\u{7} "] {
            let c = Caller::remote(&m, &member("mac-id", name), who.clone()).unwrap();
            assert_eq!(c.address(), "mac-id/ab", "{name}");
        }
    }

    #[test]
    fn trust_note_says_who_was_told() {
        let cwd = Path::new("/p/x");
        assert_eq!(
            i18n::english(&trust_note(cwd, true)),
            "the session is waiting for the user to trust the folder /p/x; only the user can do \
             that (they have been told)"
        );
        assert_eq!(
            i18n::english(&trust_note(cwd, false)),
            "the session is waiting for the user to trust the folder /p/x; only the user can do \
             that"
        );
    }

    #[test]
    fn open_limits_per_machine() {
        // (en marcha) de las abiertas desde ese equipo.
        let ok = |alive: &[bool]| open_allowed(alive.iter().copied());
        assert!(ok(&[]));
        assert!(ok(&[true; 9]));
        assert!(!ok(&[true; 10]));
        // Las paradas no cuentan para las 10…
        let mut many = vec![true; 9];
        many.extend([false; 40]);
        assert!(ok(&many));
        // …pero sí para las 50.
        many.push(false);
        assert!(!ok(&many));
        assert_eq!((MAX_REMOTE_OPEN, MAX_REMOTE_REGISTERED), (10, 50));
    }

    #[test]
    fn states() {
        assert_eq!(status(false, true, true, true), Status::Stopped);
        assert_eq!(status(true, true, true, true), Status::Question);
        assert_eq!(status(true, true, true, false), Status::Working);
        // Viva y Claude aún sin apuntarse: arrancando.
        assert_eq!(status(true, false, false, false), Status::Working);
        // La pantalla de confianza en la carpeta sale antes de que Claude se apunte.
        assert_eq!(status(true, false, false, true), Status::Question);
        assert_eq!(status(true, true, false, false), Status::Idle);
        assert!(!Status::Working.settled());
        assert!(Status::Idle.settled() && Status::Question.settled() && Status::Stopped.settled());
        assert_eq!(serde_json::to_value(Status::Question).unwrap(), "question");
    }

    fn turn(role: &'static str, text: &str) -> Turn {
        Turn {
            role,
            text: text.into(),
        }
    }

    #[test]
    fn turns_keep_who_and_what() {
        let items = vec![
            ChatItem::User {
                id: "u".into(),
                ts: None,
                text: "mira esto".into(),
                images: 2,
            },
            ChatItem::Assistant {
                id: "a".into(),
                ts: None,
                text: "  Vale.\n".into(),
            },
            ChatItem::Assistant {
                id: "b".into(),
                ts: None,
                text: "  ".into(),
            },
            ChatItem::Tool {
                id: "t".into(),
                ts: None,
                tool_id: "t1".into(),
                name: "Bash".into(),
                summary: "ls".into(),
                input: "{}".into(),
                result: Some("mucho".into()),
                is_error: false,
                ask: None,
                plan: None,
                answers: None,
            },
            ChatItem::Notice {
                id: "n".into(),
                ts: None,
                text: "terminó".into(),
                status: None,
                result: None,
            },
        ];
        assert_eq!(
            turns(items),
            vec![
                turn("user", "mira esto [2 images]"),
                turn("assistant", "Vale."),
                turn("tool", "Bash: ls"),
                turn("notice", "terminó"),
            ]
        );
    }

    #[test]
    fn question_from_the_screen() {
        let screen = include_str!("../tests/pantallas/permiso.txt");
        let m = prompt::parse(screen).expect("menú");
        let q = Question::from(&m);
        assert_eq!(q.kind, prompt::Kind::Permission);
        assert_eq!(q.title.as_deref(), Some("Do you want to proceed?"));
        assert_eq!(q.options.first().map(String::as_str), Some("Yes"));
        assert_eq!(q.options.last().map(String::as_str), Some("No"));
        assert!(!q.free_text);
        let v = serde_json::to_value(&q).unwrap();
        assert_eq!(v["kind"], "permission");
    }

    fn out(turns: Vec<Turn>) -> ReadOut {
        ReadOut {
            state: Status::Idle,
            turns,
            question: None,
            truncated: false,
            delivery_error: None,
            note: None,
        }
    }

    #[test]
    fn fit_drops_the_oldest_turns() {
        let mut o = out((0..100)
            .map(|i| turn("user", &format!("{i:03}{}", "x".repeat(997))))
            .collect());
        let before = o.clone();
        let mut small = out(vec![turn("user", "hola")]);
        fit(&mut small, MAX_READ);
        assert!(!small.truncated);
        // No cabe, pero no hay turnos que quitar ni texto que cortar: no se dice que se recortó.
        let mut no_turns = out(vec![]);
        fit(&mut no_turns, 5);
        assert!(!no_turns.truncated);
        assert!(
            serde_json::to_value(&small)
                .unwrap()
                .get("truncated")
                .is_none()
        );

        fit(&mut o, MAX_READ);
        let len = serde_json::to_vec(&o).unwrap().len();
        assert!(len <= MAX_READ, "{len}");
        assert!(o.truncated);
        assert_eq!(serde_json::to_value(&o).unwrap()["truncated"], true);
        // Los que quedan son los últimos, enteros y en orden; y no sobra sitio para otro más.
        assert_eq!(o.turns.last(), before.turns.last());
        let first = before.turns.len() - o.turns.len();
        assert_eq!(o.turns[..], before.turns[first..]);
        let one_more = json_len(&o) + json_len(&before.turns[first - 1]) + 1;
        assert!(one_more > MAX_READ);
    }

    #[test]
    fn fit_cuts_a_single_huge_turn() {
        // Con caracteres que el JSON escapa y otros de varios bytes.
        let text = "á\"\n".repeat(30_000);
        let mut o = out(vec![turn("user", "antes"), turn("assistant", &text)]);
        fit(&mut o, MAX_READ);
        assert!(json_len(&o) <= MAX_READ);
        assert!(o.truncated);
        assert_eq!(o.turns.len(), 1);
        assert_eq!(o.turns[0].role, "assistant");
        assert!(o.turns[0].text.starts_with("á\"\n"));
        assert!(o.turns[0].text.ends_with(CUT));
        // Y se aprovecha el sitio: no se corta de más.
        assert!(json_len(&o) > MAX_READ - 16, "{}", json_len(&o));
    }

    #[test]
    fn json_width_is_what_serde_writes() {
        for c in [
            'a', 'á', '€', '😀', '"', '\\', '\n', '\t', '\u{1}', '\u{1f}', '\u{7f}', '\u{8}',
        ] {
            let s = c.to_string();
            assert_eq!(json_width(c) + 2, json_len(&s), "{c:?}");
        }
    }

    #[test]
    fn cursors_per_reader_and_target() {
        let c = Cursors::default();
        let p = PathBuf::from("/t/a.jsonl");
        assert_eq!(c.get("padre", "hija", &p), 0);
        assert!(c.advance("padre", "hija", &p, 0, 120));
        assert_eq!(c.get("padre", "hija", &p), 120);
        assert_eq!(c.get("otra", "hija", &p), 0, "cada una lleva la suya");
        // Otra lectura a la vez, que empezó en 0: llega tarde y no lo mueve.
        assert!(!c.advance("padre", "hija", &p, 0, 120));
        assert!(c.advance("padre", "hija", &p, 120, 300));
        assert_eq!(c.get("padre", "hija", &p), 300);
        // Otra transcripción (tras `/clear`): desde el principio.
        let b = Path::new("/t/b.jsonl");
        assert_eq!(c.get("padre", "hija", b), 0);
        assert!(c.advance("padre", "hija", b, 0, 10));
        c.forget("hija");
        assert_eq!(c.get("padre", "hija", b), 0);
    }

    #[test]
    fn remote_readers_are_capped() {
        let c = Cursors::default();
        let p = PathBuf::from("/t/a.jsonl");
        // Las de aquí no tienen tope.
        for i in 0..(MAX_REMOTE_READERS + 10) {
            assert!(c.advance(&format!("r{i}"), "hija", &p, 0, 5));
        }
        // Las de otros equipos, sí: fuera las más antiguas.
        for i in 0..(MAX_REMOTE_READERS + 2) {
            assert!(c.advance(&format!("mac/r{i}"), "hija", &p, 0, 7));
        }
        assert_eq!(c.get("mac/r0", "hija", &p), 0);
        assert_eq!(c.get("mac/r1", "hija", &p), 0);
        assert_eq!(c.get("mac/r2", "hija", &p), 7);
        assert_eq!(
            c.get(&format!("mac/r{}", MAX_REMOTE_READERS + 1), "hija", &p),
            7
        );
        assert_eq!(c.get("r0", "hija", &p), 5);
        // La que se vuelve a usar ya no es de las antiguas.
        assert!(c.advance("mac/r2", "hija", &p, 7, 9));
        assert!(c.advance("mac/nueva", "hija", &p, 0, 1));
        assert_eq!(c.get("mac/r2", "hija", &p), 9);
        assert_eq!(c.get("mac/r3", "hija", &p), 0);
    }

    fn entry_n(i: usize) -> Entry {
        Entry {
            session: format!("Portatil/{i:08}"),
            machine: "Portatil".into(),
            machine_id: "k123".into(),
            id: format!("{i:08}"),
            name: format!("sesion-{i}"),
            cwd: PathBuf::from(format!("/home/ana/proyectos/{}", "carpeta".repeat(20))),
            state: Status::Idle,
            since: None,
            opened_by: Some("mac-id/padre".into()),
            mine: false,
            channel: true,
            channel_error: None,
            delivery_error: None,
        }
    }

    #[test]
    fn sessions_for_another_machine_fit_a_budget() {
        let few = fit_sessions((0..3).map(entry_n).collect(), MAX_READ);
        assert_eq!(few["sessions"].as_array().unwrap().len(), 3);
        assert!(few.get("truncated").is_none(), "{few}");
        let many = fit_sessions((0..2000).map(entry_n).collect(), MAX_READ);
        let len = serde_json::to_vec(&many).unwrap().len();
        assert!(len <= MAX_READ && MAX_READ < e2e::PEER_ROOM, "{len}");
        assert_eq!(many["truncated"], true);
        let kept = many["sessions"].as_array().unwrap();
        assert!(kept.len() > 10, "{}", kept.len());
        // Las primeras, en orden, y no cabía otra más.
        assert_eq!(kept[0]["id"], "00000000");
        assert_eq!(kept.last().unwrap()["id"], format!("{:08}", kept.len() - 1));
        assert!(len + json_len(&entry_n(kept.len())) + 1 > MAX_READ);
    }

    fn assistant_line(id: usize, text: &str) -> String {
        json!({"type":"assistant","uuid":format!("a{id}"),"message":{"content":[{"type":"text","text":text}]}})
            .to_string()
    }

    #[test]
    fn last_turns_reads_only_the_tail() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        // Turnos grandes: los 3 últimos no caben en el primer trozo y hay que ir más atrás.
        let big = "x".repeat(200 * 1024);
        let lines: Vec<String> = (0..6)
            .map(|i| assistant_line(i, &format!("{i}{big}")))
            .collect();
        std::fs::write(&path, lines.join("\n") + "\n").unwrap();
        let t = last_turns(&path, 3).unwrap();
        let firsts: Vec<char> = t.iter().map(|t| t.text.chars().next().unwrap()).collect();
        assert_eq!(firsts, ['3', '4', '5']);
        assert!(t.iter().all(|t| t.text.len() == big.len() + 1), "enteros");
        // Más de los que hay: todos.
        assert_eq!(last_turns(&path, 99).unwrap().len(), 6);
        assert_eq!(last_turns(&path, 0).unwrap(), vec![]);
        // Una línea a medias al final tampoco cuenta.
        std::fs::write(
            &path,
            assistant_line(0, "a") + "\n" + &assistant_line(1, "b"),
        )
        .unwrap();
        assert_eq!(last_turns(&path, 5).unwrap(), vec![turn("assistant", "a")]);
    }

    #[test]
    fn line_start_skips_half_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t");
        std::fs::write(&path, "abc\ndef\n").unwrap();
        assert_eq!(line_start(&path, 0).unwrap(), 0);
        assert_eq!(line_start(&path, 1).unwrap(), 4);
        assert_eq!(line_start(&path, 4).unwrap(), 4, "ya es principio de línea");
        assert_eq!(line_start(&path, 5).unwrap(), 8);
    }

    #[test]
    fn wait_timeout_default_and_cap() {
        assert_eq!(wait_timeout(None), Duration::from_secs(600));
        assert_eq!(wait_timeout(Some(5)), Duration::from_secs(5));
        assert_eq!(wait_timeout(Some(99_999)), Duration::from_secs(1800));
    }

    fn dest(alive: bool, channel: bool, started: Option<u64>, mine: bool) -> Dest {
        Dest {
            alive,
            channel,
            started: started.map(Duration::from_secs),
            mine,
            refused: None,
        }
    }

    fn decided(d: Dest) -> Result<(Delivery, Option<String>), StatusCode> {
        decide(&d)
            .map(|x| (x.status, x.note.map(crate::i18n::english)))
            .map_err(|f| f.0)
    }

    #[test]
    fn send_decision_table() {
        use Delivery::*;
        let note = |s: &str| Some(s.to_owned());
        // Su pantalla dijo que no tiene channel: no, aunque acabe de arrancar, y con el porqué.
        let refused = |why| {
            let d = Dest {
                refused: Some(why),
                ..dest(true, false, Some(0), true)
            };
            decide(&d)
                .map(|_| ())
                .map_err(|f| (f.0, crate::i18n::english(&f.1)))
        };
        assert_eq!(
            refused(inbox::Refusal::Blocked),
            Err((
                StatusCode::CONFLICT,
                "this session does not accept messages (no channel): the organization of its \
                 Claude account does not allow channels (channelsEnabled)"
                    .to_owned()
            ))
        );
        assert_eq!(
            refused(inbox::Refusal::NoServer),
            Err((
                StatusCode::CONFLICT,
                "this session does not accept messages (no channel): its Claude does not have \
                 nido's MCP server"
                    .to_owned()
            ))
        );
        assert_eq!(
            refused(inbox::Refusal::NoClaude),
            Err((
                StatusCode::CONFLICT,
                "this session does not accept messages (no channel): its Claude is not running"
                    .to_owned()
            ))
        );
        // Parada, a la cola: al volver a lanzarla puede tenerlo.
        let d = Dest {
            refused: Some(inbox::Refusal::Blocked),
            ..dest(false, false, None, true)
        };
        assert_eq!(decide(&d).unwrap().status, Queued);
        // Parada: a la cola, se entrega al arrancar (lo que diga su channel no cuenta).
        assert_eq!(
            decided(dest(false, false, None, true)),
            Ok((
                Queued,
                note("queued; it will be delivered when the session starts")
            ))
        );
        assert_eq!(
            decided(dest(false, true, Some(9999), false)),
            Ok((
                Queued,
                note(
                    "queued; it will be delivered when the session starts, and the session \
                     decides whether to act on it"
                )
            ))
        );
        // Viva con channel: entregado; si no es suya, decide ella.
        assert_eq!(
            decided(dest(true, true, Some(9999), true)),
            Ok((Sent, None))
        );
        assert_eq!(
            decided(dest(true, true, None, false)),
            Ok((
                Sent,
                note("delivered; the session decides whether to act on it")
            ))
        );
        // Viva, sin channel y recién arrancada: a la cola.
        assert_eq!(
            decided(dest(true, false, Some(0), true)),
            Ok((
                Queued,
                note("queued; it will be delivered when the session has finished starting")
            ))
        );
        let (status, n) = decided(dest(true, false, Some(119), false)).unwrap();
        assert_eq!(status, Queued);
        assert!(n.unwrap().ends_with("decides whether to act on it"));
        // Arrancada hace rato, pero `nido serve` se acaba de reiniciar: aún puede volver.
        let up = Duration::from_secs(30);
        let holder = Some(Duration::from_secs(5000));
        assert_eq!(started(holder, up), up);
        assert_eq!(started(None, up), up);
        assert_eq!(
            started(Some(Duration::from_secs(7)), up),
            Duration::from_secs(7)
        );
        let after_restart = Dest {
            started: Some(started(holder, up)),
            ..dest(true, false, None, true)
        };
        assert_eq!(decided(after_restart).unwrap().0, Queued);
        let long_up = Dest {
            started: Some(started(holder, Duration::from_secs(600))),
            ..after_restart
        };
        assert_eq!(decided(long_up), Err(StatusCode::CONFLICT));
        // Sin channel desde hace rato (o sin saber cuándo arrancó): no admite mensajes.
        for d in [
            dest(true, false, Some(120), true),
            dest(true, false, Some(5000), false),
            dest(true, false, None, true),
        ] {
            assert_eq!(decided(d), Err(StatusCode::CONFLICT), "{d:?}");
        }
    }

    #[test]
    fn sending_to_yourself() {
        let m = grouped(Some("k123"), &[("mac-id", "Mac")]);
        for a in [
            "padre1",
            "Portatil/padre1",
            "k123/padre1",
            " portatil/padre1 ",
        ] {
            assert!(to_self(&m, a, "padre1"), "{a}");
        }
        assert!(!to_self(&m, "hija1", "padre1"));
        assert!(!to_self(&m, "Sobremesa/padre1", "padre1"));
        assert!(!to_self(&m, "Mac/padre1", "padre1"));
    }

    #[test]
    fn picked_up_when_taken_and_on_it() {
        use Status::*;
        assert!(!picked_up(false, Working, true), "aún en la bandeja");
        assert!(picked_up(true, Working, false));
        assert!(picked_up(true, Question, false), "fue directa a preguntar");
        assert!(!picked_up(true, Idle, false), "aún no se ha puesto");
        assert!(picked_up(true, Idle, true), "contestó enseguida");
    }

    fn seen(state: Status, taken: bool, replied: bool) -> Result<Seen, Fail> {
        Ok(Seen {
            state,
            taken,
            replied,
        })
    }

    fn down() -> Result<Seen, Fail> {
        Err(fail(
            StatusCode::BAD_GATEWAY,
            "sin conexión o versión antigua",
        ))
    }

    /// Lo que devuelve la espera de `send` (`pickup`) o de `read` (sin él) con lo que va viendo
    /// `status` (cada vuelta, el siguiente; el último se repite): cuántas veces se miró, cuántas
    /// se leyó y lo leído (`{"after": vueltas}`) o el error.
    async fn replay(
        seen: &[Result<Seen, Fail>],
        pickup_ms: Option<u64>,
        total_ms: u64,
    ) -> (usize, usize, Result<Value, (StatusCode, String)>) {
        let polls = std::cell::Cell::new(0);
        let reads = std::cell::Cell::new(0);
        // Un 5xx, del canal (como los que da `status_at` de otro equipo); lo demás, no.
        let again = |r: &Result<Seen, Fail>| match r {
            Ok(s) => Ok(*s),
            Err(f) if passing(f) => Err(Miss::Channel(Fail(f.0, f.1.clone()))),
            Err(f) => Err(Miss::Fatal(Fail(f.0, f.1.clone()))),
        };
        let r = wait_then_read(
            || {
                let i = polls.get();
                polls.set(i + 1);
                std::future::ready(again(&seen[i.min(seen.len() - 1)]))
            },
            || {
                reads.set(reads.get() + 1);
                std::future::ready(Ok(json!({ "after": polls.get() })))
            },
            pickup_ms.map(Duration::from_millis),
            Duration::from_millis(1),
            Duration::from_millis(total_ms),
        )
        .await;
        (polls.get(), reads.get(), r.map_err(|f| (f.0, f.1)))
    }

    #[tokio::test]
    async fn send_waits_for_pickup_then_for_the_answer() {
        let _limit = crate::watchdog::limit(std::time::Duration::from_secs(60));
        use Status::*;
        // Ya trabajaba en otra cosa (sin recogerlo), lo recoge, sigue trabajando y termina.
        let r = replay(
            &[
                seen(Working, false, false),
                seen(Idle, false, false),
                seen(Working, true, false),
                seen(Working, true, true),
                seen(Idle, true, true),
            ],
            Some(5000),
            10_000,
        )
        .await;
        assert_eq!(r, (5, 1, Ok(json!({"after": 5}))));
        // Recogido pero aún sin ponerse: se sigue esperando a que se ponga.
        let r = replay(
            &[
                seen(Idle, true, false),
                seen(Working, true, false),
                seen(Idle, true, true),
            ],
            Some(5000),
            10_000,
        )
        .await;
        assert_eq!(r.0, 3);
        // Contesta tan rápido que entre dos vueltas ya terminó: no se esperan los 30 s.
        let start = std::time::Instant::now();
        let r = replay(
            &[seen(Working, false, false), seen(Idle, true, true)],
            Some(5000),
            10_000,
        )
        .await;
        assert_eq!(r.0, 2);
        // Directa a preguntar: también.
        let r = replay(
            &[seen(Working, false, false), seen(Question, true, false)],
            Some(5000),
            10_000,
        )
        .await;
        assert_eq!(r.0, 2);
        assert!(start.elapsed() < Duration::from_secs(5));
        // Pregunta tras trabajar: también se acaba la espera.
        let r = replay(
            &[seen(Working, true, false), seen(Question, true, true)],
            Some(5000),
            10_000,
        )
        .await;
        assert_eq!(r.0, 2);
        // Parada: no se espera a que lo recoja.
        assert_eq!(
            replay(&[seen(Stopped, false, false)], Some(5000), 10_000)
                .await
                .0,
            1
        );
        // No lo recoge en el tiempo de recogida: lo que haya.
        let start = std::time::Instant::now();
        let (_, reads, _) = replay(&[seen(Idle, false, false)], Some(50), 10_000).await;
        assert_eq!(reads, 1);
        assert!(start.elapsed() < Duration::from_secs(5));
        // Se acaba la espera entera trabajando: se lee lo que haya (y dirá «working»).
        let start = std::time::Instant::now();
        let (_, reads, r) = replay(&[seen(Working, true, false)], Some(5000), 80).await;
        assert_eq!((reads, r.is_ok()), (1, true));
        let took = start.elapsed();
        assert!(
            took >= Duration::from_millis(80) && took < Duration::from_secs(5),
            "{took:?}"
        );
    }

    #[tokio::test]
    async fn send_wait_needs_a_reply_not_just_the_message() {
        let _limit = crate::watchdog::limit(std::time::Duration::from_secs(60));
        use Status::*;
        // El fallo de antes: el mensaje entra en la conversación (crece) y la sesión aún está
        // parada; eso no es la respuesta. Se espera a que Claude escriba.
        let start = std::time::Instant::now();
        let mut seq = vec![seen(Idle, false, false)];
        seq.extend((0..20).map(|_| seen(Idle, true, false)));
        seq.push(seen(Idle, true, true));
        let (polls, reads, _) = replay(&seq, Some(5000), 10_000).await;
        assert_eq!((polls, reads), (22, 1));
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    #[tokio::test]
    async fn read_waits_until_it_is_not_working() {
        let _limit = crate::watchdog::limit(std::time::Duration::from_secs(60));
        use Status::*;
        // Termina: con lo que haya dicho al final (los turnos los da la lectura).
        let r = replay(
            &[
                seen(Working, false, false),
                seen(Working, false, false),
                seen(Idle, false, false),
            ],
            None,
            10_000,
        )
        .await;
        assert_eq!(r, (3, 1, Ok(json!({"after": 3}))));
        // Pregunta: también.
        assert_eq!(
            replay(
                &[seen(Working, false, false), seen(Question, false, false)],
                None,
                10_000
            )
            .await
            .0,
            2
        );
        // Ya parada o sin trabajar: enseguida (aquí no hay mensaje que recoger).
        assert_eq!(
            replay(&[seen(Stopped, false, false)], None, 10_000).await.0,
            1
        );
        assert_eq!(replay(&[seen(Idle, false, false)], None, 10_000).await.0, 1);
        // Se acaba el tiempo trabajando: se lee igual.
        let (_, reads, r) = replay(&[seen(Working, false, false)], None, 50).await;
        assert_eq!((reads, r.is_ok()), (1, true));
    }

    #[tokio::test]
    async fn waits_survive_the_channel_failing_now_and_then() {
        let _limit = crate::watchdog::limit(std::time::Duration::from_secs(60));
        use Status::*;
        // Falla a ratos (el hub tarda, el otro no contesta): se sigue esperando.
        let r = replay(
            &[
                seen(Working, true, false),
                down(),
                down(),
                seen(Idle, true, true),
            ],
            Some(5000),
            10_000,
        )
        .await;
        assert_eq!(r, (4, 1, Ok(json!({"after": 4}))));
        // Si se acaba la espera tras un fallo suelto, se lee igual: alguna vez contestó.
        let (polls, reads, r) = replay(
            &[
                seen(Working, false, false),
                down(),
                seen(Working, false, false),
            ],
            None,
            50,
        )
        .await;
        assert!(polls > 3, "{polls}");
        assert_eq!((reads, r.is_ok()), (1, true));
        // Un error que no es del canal (ya no existe, no se puede) corta la espera.
        let gone = [
            seen(Working, false, false),
            Err(fail(StatusCode::NOT_FOUND, "no existe esa sesión")),
        ];
        let (polls, reads, r) = replay(&gone, Some(5000), 10_000).await;
        assert_eq!((polls, reads), (2, 0));
        assert_eq!(r.unwrap_err().0, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn waits_give_up_on_a_dead_channel() {
        let _limit = crate::watchdog::limit(std::time::Duration::from_secs(60));
        use Status::*;
        let no_peer = Err((
            StatusCode::BAD_GATEWAY,
            "sin conexión o versión antigua".to_owned(),
        ));
        // Caído desde el principio: el error enseguida, sin esperar al tope.
        for pickup in [None, Some(5000)] {
            let start = std::time::Instant::now();
            let (polls, reads, r) = replay(&[down()], pickup, 10_000).await;
            assert_eq!((polls, reads), (1, 0), "{pickup:?}");
            assert_eq!(r, no_peer);
            assert!(start.elapsed() < Duration::from_secs(5));
        }
        // Tres fallos seguidos acaban la espera (sin leer).
        let start = std::time::Instant::now();
        let (polls, reads, r) = replay(
            &[
                seen(Working, true, false),
                down(),
                down(),
                down(),
                seen(Idle, true, true),
            ],
            Some(5000),
            10_000,
        )
        .await;
        assert_eq!((polls, reads), (4, 0));
        assert_eq!(r, no_peer);
        assert!(start.elapsed() < Duration::from_secs(5));
        // Dos seguidos, no: la cuenta vuelve a empezar con cada respuesta.
        let (polls, reads, r) = replay(
            &[
                seen(Working, false, false),
                down(),
                down(),
                seen(Working, false, false),
                down(),
                down(),
                seen(Idle, false, false),
            ],
            None,
            10_000,
        )
        .await;
        assert_eq!((polls, reads, r.is_ok()), (7, 1, true));
    }

    fn user_line(id: usize, text: &str) -> String {
        json!({"type":"user","uuid":format!("u{id}"),"message":{"content":text}}).to_string()
    }

    fn push_lines(path: &Path, lines: &[String]) {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        for l in lines {
            writeln!(f, "{l}").unwrap();
        }
    }

    #[test]
    fn replied_only_with_claude_writing_after_the_mark() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.jsonl");
        push_lines(
            &a,
            &[user_line(0, "antes"), assistant_line(0, "respuesta vieja")],
        );
        let mark = std::fs::metadata(&a).unwrap().len();
        // Nada nuevo.
        assert_eq!(replied_since(Some(&a), mark, &a).unwrap(), (false, mark));
        // Solo el propio mensaje (la línea del usuario): no ha contestado.
        push_lines(&a, &[user_line(1, "¿qué tal?")]);
        let (yes, upto) = replied_since(Some(&a), mark, &a).unwrap();
        assert!(!yes);
        assert_eq!(upto, std::fs::metadata(&a).unwrap().len());
        // Un turno de Claude sin texto (solo herramientas) tampoco.
        push_lines(
            &a,
            &[json!({"type":"assistant","uuid":"t","message":{"content":[
                {"type":"tool_use","id":"t-1","name":"Bash","input":{"command":"ls"}}]}})
            .to_string()],
        );
        assert!(!replied_since(Some(&a), upto, &a).unwrap().0);
        push_lines(&a, &[assistant_line(1, "Bien.")]);
        assert!(replied_since(Some(&a), mark, &a).unwrap().0);
        assert!(
            replied_since(Some(&a), upto, &a).unwrap().0,
            "desde lo ya mirado"
        );
        // La marca a media línea: esa línea es de antes.
        let b = dir.path().join("b.jsonl");
        let half = assistant_line(2, "a medias");
        std::fs::write(&b, &half[..10]).unwrap();
        let mark = 10;
        std::fs::write(&b, format!("{half}\n{}\n", user_line(3, "hola"))).unwrap();
        assert!(!replied_since(Some(&b), mark, &b).unwrap().0);
        // Otra transcripción (tras `/clear`) o sin ella al encolarlo: cuenta lo que haya.
        let c = dir.path().join("c.jsonl");
        push_lines(&c, &[user_line(0, "hola"), assistant_line(0, "Hecho.")]);
        assert!(replied_since(Some(&a), 3, &c).unwrap().0);
        assert!(replied_since(None, 0, &c).unwrap().0);
        // Más corta que la marca (se rehízo): desde el principio.
        assert!(replied_since(Some(&c), 99_999, &c).unwrap().0);
        let d = dir.path().join("d.jsonl");
        push_lines(&d, &[user_line(0, "solo esto")]);
        assert!(!replied_since(Some(&a), 0, &d).unwrap().0);
    }

    #[test]
    fn old_marks_are_not_read() {
        let m = Marks::default();
        let old = Instant::now()
            .checked_sub(MARK_TTL + Duration::from_secs(1))
            .expect("el reloj lleva en marcha más de media hora");
        m.put("m1", "hija", None, 0, old);
        assert!(m.get("m1", "hija").is_none());
    }

    #[test]
    fn peer_errors_keep_only_error_codes() {
        for (code, want) in [
            (403, StatusCode::FORBIDDEN),
            (404, StatusCode::NOT_FOUND),
            (504, StatusCode::GATEWAY_TIMEOUT),
            (200, StatusCode::BAD_GATEWAY),
            (302, StatusCode::BAD_GATEWAY),
            (399, StatusCode::BAD_GATEWAY),
            (600, StatusCode::BAD_GATEWAY),
            (0, StatusCode::BAD_GATEWAY),
        ] {
            let f = peer_fail((code, "x".into()));
            assert_eq!((f.0, f.1.as_str()), (want, "x"), "{code}");
        }
    }

    #[test]
    fn marks_are_per_message_and_session_and_expire() {
        let m = Marks::default();
        let t0 = Instant::now();
        let p = PathBuf::from("/t/a.jsonl");
        m.put("m1", "hija", Some(p.clone()), 10, t0);
        let k = m.get("m1", "hija").unwrap();
        assert_eq!(
            (k.path.as_deref(), k.from, k.replied),
            (Some(&*p), 10, false)
        );
        assert!(m.get("m1", "otra").is_none(), "de otra sesión");
        assert!(m.get("m2", "hija").is_none());
        m.seen("m1", PathBuf::from("/t/b.jsonl"), 40, true);
        let k = m.get("m1", "hija").unwrap();
        assert_eq!((k.from, k.replied), (40, true));
        m.seen("m1", PathBuf::from("/t/b.jsonl"), 50, false);
        assert!(m.get("m1", "hija").unwrap().replied, "contestó: se queda");
        // Pasada la media hora, fuera (al apuntar otra).
        m.put(
            "m2",
            "hija",
            None,
            0,
            t0 + MARK_TTL + Duration::from_secs(1),
        );
        assert!(m.get("m1", "hija").is_none());
        assert!(m.get("m2", "hija").is_some());
    }

    #[test]
    fn parents_are_remembered_for_a_while() {
        let p = Parents::default();
        let t0 = Instant::now();
        assert_eq!(p.get("mac-id/padre", t0), None);
        p.put("mac-id/padre", true, t0);
        assert_eq!(
            p.get("mac-id/padre", t0 + Duration::from_secs(59)),
            Some(true)
        );
        assert_eq!(p.get("mac-id/padre", t0 + PARENT_TTL), None);
    }

    #[test]
    fn relabel_writes_other_sessions_as_here() {
        let m = grouped(Some("k123"), &[("win-id", "Portatil"), ("mac-id", "Mac")]);
        let e = json!({"session": "Portatil/ab", "machine": "Portatil", "machine_id": "x",
            "id": "ab", "name": "web"});
        let r = relabel(&m, &m.others[0], e);
        assert_eq!(r["session"], "win-id/ab");
        assert_eq!(r["machine"], "Portatil");
        assert_eq!(r["machine_id"], "win-id");
        assert_eq!(r["name"], "web");
        // De un nido antiguo, sin `machine_id`.
        let r = relabel(&m, &m.others[1], json!({"session": "Mac/cd", "id": "cd"}));
        assert_eq!(r["session"], "Mac/cd");
        assert_eq!(r["machine_id"], "mac-id");
        // Sin sesión, tal cual.
        assert_eq!(
            relabel(&m, &m.others[1], json!({"ok": true})),
            json!({"ok": true})
        );
    }

    #[test]
    fn alerts_for_another_machine_fit_a_budget() {
        let alert = |i: u64| AlertOut {
            machine: "Portatil".into(),
            machine_id: "k123".into(),
            session: format!("Portatil/{i:08}"),
            name: format!("sesion-{i}"),
            text: "x".repeat(500),
            at: i,
        };
        let few = fit_alerts((0..3).map(alert).collect(), MAX_READ);
        assert_eq!(few["alerts"].as_array().unwrap().len(), 3);
        assert!(few.get("truncated").is_none());
        let many = fit_alerts((0..1000).map(alert).collect(), MAX_READ);
        assert!(serde_json::to_vec(&many).unwrap().len() <= MAX_READ);
        assert_eq!(many["truncated"], true);
        // Las más nuevas.
        assert_eq!(many["alerts"][0]["at"], 999);
    }

    #[test]
    fn names_from_the_folder() {
        let none = |_: &str| false;
        assert_eq!(name_for(Path::new("/home/ana/web"), none), "web");
        assert_eq!(
            name_for(Path::new("/home/ana/mi proyecto"), none),
            "mi-proyecto"
        );
        assert_eq!(name_for(Path::new("/home/ana/.config"), none), "config");
        assert_eq!(name_for(Path::new("/"), none), "claude");
        let long = format!("/x/{}", "a".repeat(100));
        let n = name_for(Path::new(&long), |n| n.len() == 56);
        assert_eq!(n, format!("{}-2", "a".repeat(56)));
        assert!(crate::registry::valid_name(&n));
        let taken = ["web", "web-2"];
        assert_eq!(
            name_for(Path::new("/w/web"), |n| taken.contains(&n)),
            "web-3"
        );
    }

    #[test]
    fn open_command_and_first_message() {
        let msg = first_message("padre", "Portatil", "k123/ab", "haz esto");
        assert!(msg.starts_with(
            "[nido] You were opened by the session \"padre\" on Portatil (k123/ab). Messages \
             that reach you through nido from that session come from whoever opened you: act \
             on them and reply with the nido send tool to k123/ab.\n\n"
        ));
        assert!(msg.ends_with("\n\nhaz esto"));
        let argv = open_argv("web", "sid-1", Some("haiku"), &msg);
        assert_eq!(
            argv,
            [
                "claude",
                "--session-id",
                "sid-1",
                "-n",
                "web",
                "--model",
                "haiku",
                &msg
            ]
        );
        // Al retomarla se queda el modelo, y el mensaje no se repite.
        assert_eq!(claude::extra_args(&argv), ["--model", "haiku"]);
        // Con el MCP de nido: `--allowedTools` y la lista de channels se leen como en Claude
        // (commander: los valores siguen hasta la próxima opción), y el mensaje sigue siendo el
        // mensaje, con o sin modelo.
        fn values<'a>(argv: &'a [String], opt: &str) -> Vec<&'a str> {
            let at = argv.iter().position(|a| a == opt).expect("la opción");
            argv[at + 1..]
                .iter()
                .take_while(|a| !a.starts_with('-'))
                .map(String::as_str)
                .collect()
        }
        for model in [Some("haiku"), None] {
            let plain = open_argv("web", "sid-1", model, &msg);
            let with = claude::with_nido_mcp(&plain, true);
            assert_eq!(values(&with, "--allowedTools"), claude::NIDO_TOOLS);
            assert_eq!(
                values(&with, "--dangerously-load-development-channels"),
                ["server:nido"]
            );
            assert_eq!(with.last(), Some(&msg));
            assert_eq!(with.len(), plain.len() + 10, "{with:?}");
            assert_eq!(claude::extra_args(&with), claude::extra_args(&plain));
        }
        assert_eq!(
            open_argv("web", "s", None, "m"),
            ["claude", "--session-id", "s", "-n", "web", "m"]
        );
    }

    #[test]
    fn errors_go_out_in_english() {
        let en = crate::i18n::english;
        assert_eq!(
            en("no se llamó desde una sesión de nido"),
            "not called from a nido session"
        );
        assert_eq!(en("no existe esa sesión"), "no such session");
        assert_eq!(
            en("solo puedes contestar las sesiones que abriste"),
            "you can only answer sessions you opened"
        );
        assert_eq!(
            en("solo puedes cerrar las sesiones que abriste"),
            "you can only close sessions you opened"
        );
        assert_eq!(
            en("no hay ninguna pregunta pendiente"),
            "no question is pending"
        );
        assert_eq!(
            en("la pregunta no admite texto libre"),
            "the pending question has no free-text field"
        );
        assert_eq!(
            en("falta la opción o el texto"),
            "the option or the text is missing"
        );
        assert_eq!(en("falta el mensaje"), "the prompt is missing");
        assert_eq!(
            en("el mensaje pasa de 24 KB"),
            "the prompt is longer than 24 KB"
        );
        assert_eq!(en("modelo no válido: \"-x\""), "invalid model: \"-x\"");
        assert_eq!(en("argumentos no válidos: x"), "invalid arguments: x");
        assert_eq!(
            en("no hay ninguna herramienta \"send\""),
            "there is no tool \"send\""
        );
        assert_eq!(
            en("la API del MCP solo va con el token de este equipo"),
            "the MCP API only takes this machine's token"
        );
        assert_eq!(
            en("el modelo pasa de 128 caracteres"),
            "the model is longer than 128 characters"
        );
        assert_eq!(en("falta el texto"), "the text is missing");
        assert_eq!(
            en("el texto pasa de 24 KB"),
            "the text is longer than 24 KB"
        );
        assert_eq!(
            en("no puedes mandarte mensajes a ti misma"),
            "you cannot send a message to yourself"
        );
        assert_eq!(
            en("esa sesión no admite mensajes (no tiene channel)"),
            "this session does not accept messages (no channel)"
        );
        assert_eq!(
            en("la bandeja de esa sesión está llena"),
            "the inbox of that session is full"
        );
        assert_eq!(
            en("«alert» no se puede pedir desde otro equipo"),
            "“alert” cannot be requested from another machine"
        );
        assert_eq!(
            en("falta la sesión que llama"),
            "the calling session is missing"
        );
        assert_eq!(
            en("sesión que llama no válida: \"a/b\""),
            "invalid calling session: \"a/b\""
        );
        assert_eq!(en("«Mac» no es este equipo"), "“Mac” is not this machine");
        assert_eq!(
            en("«Mac» no es ningún equipo del grupo"),
            "“Mac” is not a machine in the group"
        );
        assert_eq!(
            en("el otro equipo no contesta"),
            "the other machine does not answer"
        );
        assert_eq!(
            en("respuesta no válida del otro equipo"),
            "invalid reply from the other machine"
        );
    }
}
