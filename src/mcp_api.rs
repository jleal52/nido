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
//! Las de aquí, de momento, solo llegan a las sesiones de este equipo: las de otro dan error.

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path as UrlPath, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::inbox::Msg;
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
/// Cuántas sesiones abiertas con `open` desde un mismo equipo puede haber aquí en marcha a la vez,
/// y apuntadas (en marcha o no): un equipo del grupo no llena este de Claudes. Las paradas cuentan
/// solo para el segundo tope, más holgado: las huérfanas no dejan fuera al equipo.
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

// ---------- Equipos y direcciones ----------

/// Este equipo: su nombre y, si está conectado al hub, su id allí.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Machine {
    pub name: String,
    pub key: Option<String>,
}

impl Machine {
    pub fn of(st: &AppState) -> Self {
        Self {
            name: <AppState as e2e::Host>::host_name(st),
            key: st.hub.pair_info().map(|(_, id)| id),
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
}

/// Dónde está una sesión, según cómo la escribió quien llama.
#[derive(Debug, PartialEq, Eq)]
pub enum Where<'a> {
    /// En este equipo, con este id.
    Here(&'a str),
    /// En otro equipo.
    Elsewhere { machine: &'a str, id: &'a str },
}

/// `equipo/id` (el equipo por su nombre o por su id en el hub) o solo `id`: este equipo.
pub fn locate<'a>(me: &Machine, address: &'a str) -> Where<'a> {
    let address = address.trim();
    match address.rsplit_once('/') {
        Some((m, id)) if !m.trim().is_empty() && !me.is_me(m) => Where::Elsewhere {
            machine: m.trim(),
            id: id.trim(),
        },
        Some((_, id)) => Where::Here(id.trim()),
        None => Where::Here(address),
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
    opened_by.is_some_and(|o| locate(me, o) == Where::Here(caller))
}

// ---------- Estado, turnos y pregunta ----------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
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

/// Pregunta con `poll` cada `step` hasta que `done`, o hasta que pase `timeout`; devuelve lo
/// último que vio. Es la espera de `read` con `wait` (y sirve igual para sesiones de otro equipo).
/// `poll` da un futuro nuevo cada vez (con un cierre `async`, el futuro no se puede probar `Send`
/// y axum lo exige).
pub async fn wait_until<T, F: Future<Output = T>>(
    mut poll: impl FnMut() -> F,
    done: impl Fn(&T) -> bool,
    step: Duration,
    timeout: Duration,
) -> T {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let v = poll().await;
        let now = tokio::time::Instant::now();
        if done(&v) || now >= deadline {
            return v;
        }
        tokio::time::sleep(step.min(deadline - now)).await;
    }
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
/// está parada, o si su conversación ha crecido desde que se mandó (contestó enseguida).
pub fn picked_up(taken: bool, state: Status, grown: bool) -> bool {
    taken && (state != Status::Idle || grown)
}

/// ¿Se manda el mensaje a sí misma la sesión `caller`?
pub fn to_self(me: &Machine, address: &str, caller: &str) -> bool {
    locate(me, address) == Where::Here(caller)
}

/// Qué hacer con un mensaje para `d`. Parada, a la cola (se entrega al arrancar); viva con
/// channel, se entrega; viva sin channel pero recién arrancada, a la cola; viva sin channel desde
/// hace rato, no admite mensajes (y nunca se escribe en su consola).
pub fn decide(d: &Dest) -> Result<Decision, Fail> {
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

/// La espera de `send` con `wait`: primero, hasta `pickup`, a que la sesión recoja el mensaje y
/// se ponga a trabajar (`picked`); luego, hasta que pase `total` desde el principio, a que se
/// asiente. Si no llega a ponerse a trabajar (o se para), no se sigue esperando. Devuelve lo
/// último que vio `poll`. No sabe de dónde sale lo que mira: sirve también para otro equipo.
pub async fn wait_reply<T, F: Future<Output = T>>(
    mut poll: impl FnMut() -> F,
    picked: impl Fn(&T) -> bool,
    state: impl Fn(&T) -> Status,
    step: Duration,
    pickup: Duration,
    total: Duration,
) -> T {
    let start = tokio::time::Instant::now();
    let v = wait_until(
        &mut poll,
        |v| picked(v) || state(v) == Status::Stopped,
        step,
        pickup.min(total),
    )
    .await;
    if !picked(&v) {
        return v;
    }
    let left = total.saturating_sub(start.elapsed());
    wait_until(poll, |v| state(v).settled(), step, left).await
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

/// Una sesión de Claude en la lista de `sessions` (y lo que devuelve `open`).
#[derive(Debug, Serialize)]
pub struct Entry {
    /// `<nombre del equipo>/<id>`: lo que se pasa a las demás herramientas.
    pub session: String,
    pub machine: String,
    pub id: String,
    pub name: String,
    pub cwd: PathBuf,
    pub state: Status,
    /// Quién la abrió con `open` (`<equipo>/<id>`); `null`: el usuario.
    pub opened_by: Option<String>,
    /// La abrió quien llama: puede contestarla y cerrarla.
    pub mine: bool,
    /// Recibe mensajes de `send`: su `nido mcp` está recogiendo la bandeja.
    pub channel: bool,
}

fn entry(st: &AppState, me: &Machine, caller: &Caller, v: &SessionView, state: Status) -> Entry {
    Entry {
        session: format!("{}/{}", me.name, v.id),
        machine: me.name.clone(),
        id: v.id.clone(),
        name: v.name.clone(),
        cwd: v.cwd.clone(),
        state,
        opened_by: v.opened_by.clone(),
        mine: caller.owns(me, v.opened_by.as_deref()),
        channel: st.inbox.channel(&v.id),
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

/// Las herramientas que se pueden pedir desde otro equipo. `alert` no: una alerta va siempre al
/// nido de quien la manda.
pub const PEER_TOOLS: [&str; 6] = ["sessions", "send", "read", "open", "answer", "close"];

/// Una herramienta (menos `alert`), para quien llame: de aquí o de otro equipo.
async fn run_tool(st: &AppState, me: &Machine, caller: &Caller, tool: &str, args: Value) -> Reply {
    match tool {
        "sessions" => tool_sessions(st, me, caller, args).await,
        "send" => tool_send(st, me, caller, args).await,
        "read" => tool_read(st, me, caller, args).await,
        "open" => tool_open(st, me, caller, args).await,
        "answer" => tool_answer(st, me, caller, args).await,
        "close" => tool_close(st, me, caller, args).await,
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
    let caller = Caller::remote(&me, member, caller).map_err(code)?;
    run_tool(st, &me, &caller, method, params)
        .await
        .map_err(code)
}

/// `GET /api/v1/mcp/inbox`: lo que espera en la bandeja de la sesión que llama, del más antiguo
/// al más nuevo (`{messages: [...]}`). Si no hay nada, espera a que llegue algo (25 s como mucho;
/// luego, vacío). Lo no confirmado vuelve a salir en la siguiente.
pub async fn api_inbox(State(st): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let caller = match authorize(&st, &headers).await {
        Ok(s) => s,
        Err(f) => return f.into_response(),
    };
    let messages: Vec<Msg> = st.inbox.poll(&caller.id).await;
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
    let removed = st.inbox.ack(&caller.id, &a.ids);
    Json(json!({ "ok": true, "removed": removed })).into_response()
}

/// Con el token principal (las sesiones van con el de la consola, no con el de un teléfono) y
/// desde una sesión de aquí: cuál.
async fn authorize(st: &AppState, headers: &HeaderMap) -> Result<Session, Fail> {
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
    caller(st, headers).await
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

/// La sesión de Claude a la que se refiere `address`, si es de este equipo. A otro equipo no se
/// pasa nada de lo que pide otro equipo: solo se le atiende con las sesiones de aquí.
async fn target(
    st: &AppState,
    me: &Machine,
    caller: &Caller,
    address: &str,
) -> Result<Session, Fail> {
    match locate(me, address) {
        Where::Elsewhere { .. } if !caller.local => Err(fail(
            StatusCode::BAD_REQUEST,
            "esa sesión no es de este equipo",
        )),
        Where::Elsewhere { .. } => Err(fail(
            StatusCode::BAD_REQUEST,
            "la sesión está en otro equipo (aún no se puede)",
        )),
        Where::Here(id) => st
            .registry
            .lock()
            .await
            .get(id)
            .filter(|s| s.claude.is_some())
            .cloned()
            .ok_or_else(|| fail(StatusCode::NOT_FOUND, "no existe esa sesión")),
    }
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

async fn tool_sessions(st: &AppState, me: &Machine, caller: &Caller, args: Value) -> Reply {
    let NoArgs {} = args_of(args)?;
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
    let list: Vec<Entry> = views
        .iter()
        .zip(states)
        .map(|(v, (s, _))| entry(st, me, caller, v, s))
        .collect();
    if caller.local {
        return to_json(&list);
    }
    // A otro equipo va en un solo mensaje Noise: con tope, y diciendo si se recortó.
    Ok(fit_sessions(list, MAX_READ))
}

/// La lista de `sessions` para otro equipo: `{sessions: [...]}` con las primeras que quepan en
/// `max` bytes de JSON y, si no cupieron todas, `truncated: true`.
pub fn fit_sessions(mut list: Vec<Entry>, max: usize) -> Value {
    let mut used = r#"{"sessions":[],"truncated":true}"#.len();
    let mut keep = 0;
    for e in &list {
        let n = json_len(e) + 1;
        if used + n > max {
            break;
        }
        used += n;
        keep += 1;
    }
    let truncated = keep < list.len();
    list.truncate(keep);
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

/// Cómo está la sesión `id` ahora. Hace un `scan()` entero (todas las sesiones): de sobra para
/// una espera, pero hay que revisarlo en la tarea 9, con esperas de otros equipos.
async fn poll_status(st: &AppState, id: &str) -> (Status, Option<Menu>) {
    match view(st, id).await {
        Some(v) => status_of(st, &v).await,
        None => (Status::Stopped, None),
    }
}

/// Lo que ocupa ahora la conversación de la sesión `id` (0 si aún no tiene).
async fn transcript_len(st: &AppState, id: &str) -> u64 {
    let link = st
        .registry
        .lock()
        .await
        .get(id)
        .and_then(|s| s.claude.clone());
    link.and_then(|l| server::transcript_of(st, &l))
        .and_then(|p| std::fs::metadata(p).ok())
        .map_or(0, |m| m.len())
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
    let mut out = ReadOut {
        state,
        turns,
        question: menu.as_ref().map(Question::from),
        truncated: false,
    };
    fit(&mut out, MAX_READ);
    Ok(out)
}

async fn tool_read(st: &AppState, me: &Machine, caller: &Caller, args: Value) -> Reply {
    let a: ReadArgs = args_of(args)?;
    let s = target(st, me, caller, &a.session).await?;
    let id = s.id.as_str();
    let poll = || poll_status(st, id);
    // Para otro equipo no se espera aquí: el ida y vuelta por el hub dura 15 s como mucho. Espera
    // el nido de quien llama, preguntando cada poco.
    let (state, menu) = if a.wait && caller.local {
        wait_until(
            poll,
            |(s, _)| s.settled(),
            WAIT_STEP,
            wait_timeout(a.timeout_s),
        )
        .await
    } else {
        poll().await
    };
    to_json(&read_out(st, &caller.reader(), &s, state, menu, a.last).await?)
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
    // equipo no está aquí: su dirección ya da error en `target`.
    if caller.local && to_self(me, &a.session, &caller.id) {
        return Err(bad_request("no puedes mandarte mensajes a ti misma".into()));
    }
    let s = target(st, me, caller, &a.session).await?;
    let alive = view(st, &s.id).await.is_some_and(|v| v.alive);
    let holder = launch::holder_started(&st.paths, &s.id)
        .map(|t| SystemTime::now().duration_since(t).unwrap_or_default());
    let d = decide(&Dest {
        alive,
        channel: st.inbox.channel(&s.id),
        started: Some(started(holder, st.inbox.uptime())),
        mine: caller.owns(me, s.opened_by.as_deref()),
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
    // Como en `read`: para otro equipo, sin esperar aquí.
    let wait = a.wait.filter(|&w| w > 0 && caller.local);
    // Lo que ocupa su conversación antes de mandarlo: si crece, ya contestó.
    let before = match wait {
        Some(_) => transcript_len(st, &s.id).await,
        None => 0,
    };
    st.inbox
        .push(&s.id, msg)
        .map_err(|_| fail(StatusCode::CONFLICT, "la bandeja de esa sesión está llena"))?;
    let mut out = json!({ "status": d.status });
    if let Some(note) = d.note {
        out["note"] = i18n::tr(note).into();
    }
    let Some(secs) = wait else {
        return Ok(out);
    };
    let (id, msg_id) = (s.id.as_str(), msg_id.as_str());
    let poll = || async move {
        let (state, menu) = poll_status(st, id).await;
        let taken = !st.inbox.has(id, msg_id);
        (state, menu, taken, transcript_len(st, id).await > before)
    };
    let (state, menu, _, _) = wait_reply(
        poll,
        |&(state, _, taken, grown)| picked_up(taken, state, grown),
        |(state, _, _, _)| *state,
        WAIT_STEP,
        PICKUP,
        wait_timeout(Some(secs)),
    )
    .await;
    out["read"] = to_json(&read_out(st, &caller.reader(), &s, state, menu, None).await?)?;
    Ok(out)
}

async fn tool_open(st: &AppState, me: &Machine, caller: &Caller, args: Value) -> Reply {
    let a: OpenArgs = args_of(args)?;
    if !me.is_me(&a.machine) && !caller.local {
        return Err(bad_request(format!(
            "«{}» no es este equipo",
            a.machine.trim()
        )));
    }
    if !me.is_me(&a.machine) {
        return Err(bad_request(format!(
            "«{}» es otro equipo (aún no se puede)",
            a.machine.trim()
        )));
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
    let name = a.name.as_deref().map(str::trim).map(str::to_owned);
    let created = {
        let mut reg = st.registry.lock().await;
        if !caller.local {
            let from = format!("{}/", caller.machine_key);
            let alive = reg
                .sessions()
                .iter()
                .filter(|s| s.opened_by.as_deref().is_some_and(|o| o.starts_with(&from)))
                .map(|s| launch::is_alive(&st.paths, &s.id));
            if !open_allowed(alive) {
                return Err(fail(
                    StatusCode::CONFLICT,
                    "demasiadas sesiones abiertas desde ese equipo; cierra alguna",
                ));
            }
        }
        let session = opened_session(&reg, cwd, name, model.as_deref(), &message, opener);
        reg.add(session).map_err(Fail::from)?
    };
    if let Err(e) = launch::start(&st.paths, st.launcher, &created.id) {
        // Que no quede apuntada una sesión que no arrancó.
        let _ = st.registry.lock().await.remove(&created.id);
        return Err(fail(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("no se pudo arrancar: {e}"),
        ));
    }
    server::wait_alive(st, &created.id).await;
    st.refresh.notify_one();
    let Some(v) = view(st, &created.id).await else {
        return Err(fail(StatusCode::NOT_FOUND, "no existe esa sesión"));
    };
    let (state, _) = status_of(st, &v).await;
    to_json(&entry(st, me, caller, &v, state))
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
        seen: 0,
        opened_by: Some(opened_by),
        alerts: vec![],
    }
}

async fn tool_answer(st: &AppState, me: &Machine, caller: &Caller, args: Value) -> Reply {
    let a: AnswerArgs = args_of(args)?;
    let s = target(st, me, caller, &a.session).await?;
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
    let a: SessionArg = args_of(args)?;
    let s = target(st, me, caller, &a.session).await?;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn me(key: Option<&str>) -> Machine {
        Machine {
            name: "Portatil".into(),
            key: key.map(str::to_owned),
        }
    }

    #[test]
    fn addresses() {
        let m = me(Some("k123"));
        assert_eq!(locate(&m, "ab12cd34"), Where::Here("ab12cd34"));
        assert_eq!(locate(&m, "Portatil/ab12cd34"), Where::Here("ab12cd34"));
        assert_eq!(locate(&m, "PORTATIL/ab12cd34"), Where::Here("ab12cd34"));
        assert_eq!(locate(&m, "k123/ab12cd34"), Where::Here("ab12cd34"));
        assert_eq!(locate(&m, " Portatil/ab12cd34 "), Where::Here("ab12cd34"));
        assert_eq!(
            locate(&m, "Sobremesa/ab12cd34"),
            Where::Elsewhere {
                machine: "Sobremesa",
                id: "ab12cd34"
            }
        );
        // El id en el hub tal cual: otro id es otro equipo.
        assert_eq!(
            locate(&m, "K123/ab12cd34"),
            Where::Elsewhere {
                machine: "K123",
                id: "ab12cd34"
            }
        );
        // Sin conexión con el hub, solo el nombre.
        assert!(matches!(
            locate(&me(None), "k123/ab12cd34"),
            Where::Elsewhere { .. }
        ));
    }

    #[test]
    fn canonical_address_is_the_hub_id_or_the_name() {
        assert_eq!(me(Some("k123")).address("ab"), "k123/ab");
        assert_eq!(me(None).address("ab"), "Portatil/ab");
    }

    #[test]
    fn opened_by_compares_addresses() {
        let m = me(Some("k123"));
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
            id: format!("{i:08}"),
            name: format!("sesion-{i}"),
            cwd: PathBuf::from(format!("/home/ana/proyectos/{}", "carpeta".repeat(20))),
            state: Status::Idle,
            opened_by: Some("mac-id/padre".into()),
            mine: false,
            channel: true,
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

    #[tokio::test]
    async fn wait_until_stops_when_done_or_on_timeout() {
        let mut n = 0;
        let got = wait_until(
            || {
                n += 1;
                std::future::ready(n)
            },
            |&v| v == 3,
            Duration::from_millis(1),
            Duration::from_secs(10),
        )
        .await;
        assert_eq!(got, 3);
        let start = std::time::Instant::now();
        let got = wait_until(
            || std::future::ready(Status::Working),
            |s| s.settled(),
            Duration::from_millis(10),
            Duration::from_millis(50),
        )
        .await;
        assert_eq!(got, Status::Working);
        let took = start.elapsed();
        assert!(
            took >= Duration::from_millis(50) && took < Duration::from_secs(2),
            "{took:?}"
        );
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
        let m = me(Some("k123"));
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

    /// Lo que va viendo `wait_reply`: cada vuelta, el siguiente estado, si ya lo recogió y si su
    /// conversación creció; el último se repite.
    async fn replay(
        seen: &[(Status, bool, bool)],
        pickup_ms: u64,
        total_ms: u64,
    ) -> (usize, Status) {
        let polls = std::cell::Cell::new(0);
        let (state, _, _) = wait_reply(
            || {
                let i = polls.get();
                polls.set(i + 1);
                std::future::ready(seen[i.min(seen.len() - 1)])
            },
            |&(s, taken, grown)| picked_up(taken, s, grown),
            |&(s, _, _)| s,
            Duration::from_millis(1),
            Duration::from_millis(pickup_ms),
            Duration::from_millis(total_ms),
        )
        .await;
        (polls.get(), state)
    }

    #[tokio::test]
    async fn send_waits_for_pickup_then_for_the_answer() {
        use Status::*;
        // Ya trabajaba en otra cosa (sin recogerlo), lo recoge, sigue trabajando y termina.
        let seen = [
            (Working, false, false),
            (Idle, false, false),
            (Working, true, true),
            (Working, true, true),
            (Idle, true, true),
        ];
        assert_eq!(replay(&seen, 5000, 10_000).await, (5, Idle));
        // Recogido pero aún sin ponerse: se sigue esperando a que se ponga.
        let seen = [
            (Idle, true, false),
            (Working, true, false),
            (Idle, true, true),
        ];
        assert_eq!(replay(&seen, 5000, 10_000).await, (3, Idle));
        // Contesta tan rápido que entre dos vueltas ya terminó: no se esperan los 30 s.
        let start = std::time::Instant::now();
        let seen = [(Working, false, false), (Idle, true, true)];
        assert_eq!(replay(&seen, 5000, 10_000).await, (3, Idle));
        // Directa a preguntar: también.
        let seen = [(Working, false, false), (Question, true, false)];
        assert_eq!(replay(&seen, 5000, 10_000).await, (3, Question));
        assert!(start.elapsed() < Duration::from_secs(2));
        // Pregunta tras trabajar: también se acaba la espera.
        let seen = [(Working, true, false), (Question, true, true)];
        assert_eq!(replay(&seen, 5000, 10_000).await, (2, Question));
        // Parada: no se espera a que lo recoja.
        assert_eq!(
            replay(&[(Stopped, false, false)], 5000, 10_000).await,
            (1, Stopped)
        );
        // No lo recoge en el tiempo de recogida: lo que haya.
        let start = std::time::Instant::now();
        let (_, state) = replay(&[(Idle, false, false)], 50, 10_000).await;
        assert_eq!(state, Idle);
        assert!(start.elapsed() < Duration::from_secs(2));
        // Se acaba la espera entera trabajando: «working».
        let start = std::time::Instant::now();
        let (_, state) = replay(&[(Working, true, false)], 5000, 80).await;
        assert_eq!(state, Working);
        let took = start.elapsed();
        assert!(
            took >= Duration::from_millis(80) && took < Duration::from_secs(2),
            "{took:?}"
        );
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
            en("la sesión está en otro equipo (aún no se puede)"),
            "session is on another machine (not supported yet)"
        );
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
        assert_eq!(
            en("«Sobremesa» es otro equipo (aún no se puede)"),
            "“Sobremesa” is another machine (not supported yet)"
        );
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
    }
}
