//! La API local del MCP de nido: `POST /api/v1/mcp/<herramienta>`, lo que piden las
//! herramientas de `nido mcp` (ver `mcp.rs`) desde una sesión de este equipo.
//!
//! Quien llama va con el token principal y dice qué sesión es en `X-Nido-Session` (su id). El
//! cuerpo son los argumentos de la herramienta y la respuesta, lo que lee el modelo. Los errores
//! se escriben en español, como en el resto de nido, y salen en inglés: `nido mcp` no manda
//! `Accept-Language`.
//!
//! De momento, solo las sesiones de este equipo: las de otro dan error.

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path as UrlPath, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::prompt::{self, Menu};
use crate::proto::{Hello, Role};
use crate::registry::{ClaudeLink, Registry, Session, new_id, now};
use crate::server::{self, AppState, SessionView, error};
use crate::transcript::{self, ChatItem};
use crate::{auth, claude, e2e, launch};

/// Lo más que ocupa la respuesta de `read` (el JSON entero): fuera antes los turnos más antiguos.
pub const MAX_READ: usize = 48 * 1024;
/// Con `wait`, cada cuánto se mira la sesión.
pub const WAIT_STEP: Duration = Duration::from_secs(2);
/// Con `wait`, cuánto se espera si no se dice, y como mucho (s).
pub const WAIT_DEFAULT_S: u64 = 600;
pub const WAIT_MAX_S: u64 = 1800;
/// Lo más que puede ocupar el mensaje inicial de `open` (va en la orden de Claude).
pub const MAX_PROMPT: usize = 24 * 1024;
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

/// Hasta dónde leyó cada sesión la conversación de otra: (la que lee, la leída) → (su
/// transcripción, posición). Solo en memoria: al reiniciar nido, se vuelve a leer todo.
#[derive(Default)]
pub struct Cursors(std::sync::Mutex<HashMap<(String, String), (PathBuf, u64)>>);

impl Cursors {
    /// Dónde seguir leyendo: 0 si no la leyó nunca o la transcripción es otra (tras `/clear`).
    pub fn get(&self, reader: &str, target: &str, path: &Path) -> u64 {
        self.0
            .lock()
            .expect("cursores")
            .get(&(reader.to_owned(), target.to_owned()))
            .filter(|(p, _)| p == path)
            .map_or(0, |(_, o)| *o)
    }

    /// Lo lleva de `from` a `to`, si sigue en `from` (lo que daría `get`). Si no, otra lectura
    /// de la misma sesión se adelantó y ya devolvió esos turnos: `false`, y no se toca.
    pub fn advance(&self, reader: &str, target: &str, path: &Path, from: u64, to: u64) -> bool {
        let mut map = self.0.lock().expect("cursores");
        let key = (reader.to_owned(), target.to_owned());
        let now = map
            .get(&key)
            .filter(|(p, _)| p == path)
            .map_or(0, |(_, o)| *o);
        if now != from {
            return false;
        }
        map.insert(key, (path.to_owned(), to));
        true
    }

    /// Una sesión que ya no está: fuera lo suyo, como lectora y como leída.
    pub fn forget(&self, id: &str) {
        self.0
            .lock()
            .expect("cursores")
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
}

fn entry(me: &Machine, caller: &str, v: &SessionView, state: Status) -> Entry {
    Entry {
        session: format!("{}/{}", me.name, v.id),
        machine: me.name.clone(),
        id: v.id.clone(),
        name: v.name.clone(),
        cwd: v.cwd.clone(),
        state,
        opened_by: v.opened_by.clone(),
        mine: opened_by(me, v.opened_by.as_deref(), caller),
    }
}

type Reply = Result<Value, Fail>;

/// Un error de una herramienta: el código y el mensaje, en español (sale como `server::error`,
/// en el idioma de la petición).
pub struct Fail(StatusCode, String);

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
    // Las sesiones van con el token de la consola, no con el de un teléfono.
    if let Some(token) = &st.token {
        let given = headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "));
        if !given.is_some_and(|g| auth::ct_eq(g, token)) {
            return error(
                StatusCode::FORBIDDEN,
                "la API del MCP solo va con el token de este equipo",
            );
        }
    }
    let caller = match caller(&st, &headers).await {
        Ok(s) => s,
        Err(f) => return f.into_response(),
    };
    let args: Value = if body.iter().all(u8::is_ascii_whitespace) {
        json!({})
    } else {
        match serde_json::from_slice(&body) {
            Ok(v) => v,
            Err(e) => {
                return error(
                    StatusCode::BAD_REQUEST,
                    format!("argumentos no válidos: {e}"),
                );
            }
        }
    };
    let me = Machine::of(&st);
    let reply = match tool.as_str() {
        "sessions" => tool_sessions(&st, &me, &caller, args).await,
        "read" => tool_read(&st, &me, &caller, args).await,
        "open" => tool_open(&st, &me, &caller, args).await,
        "answer" => tool_answer(&st, &me, &caller, args).await,
        "close" => tool_close(&st, &me, &caller, args).await,
        "alert" => tool_alert(&st, &caller, args).await,
        _ => Err(fail(
            StatusCode::NOT_FOUND,
            format!("no hay ninguna herramienta {tool:?}"),
        )),
    };
    match reply {
        Ok(v) => Json(v).into_response(),
        Err(f) => f.into_response(),
    }
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

/// La sesión de Claude a la que se refiere `address`, si es de este equipo.
async fn target(st: &AppState, me: &Machine, address: &str) -> Result<Session, Fail> {
    match locate(me, address) {
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

async fn tool_sessions(st: &AppState, me: &Machine, caller: &Session, args: Value) -> Reply {
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
        .map(|(v, (s, _))| entry(me, &caller.id, v, s))
        .collect();
    to_json(&list)
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

async fn tool_read(st: &AppState, me: &Machine, caller: &Session, args: Value) -> Reply {
    let a: ReadArgs = args_of(args)?;
    let s = target(st, me, &a.session).await?;
    let id = s.id.as_str();
    // Cada vuelta hace un `scan()` entero (todas las sesiones): de sobra para una espera, pero hay
    // que revisarlo en la tarea 9, con esperas de otros equipos.
    let poll = || async move {
        match view(st, id).await {
            Some(v) => status_of(st, &v).await,
            None => (Status::Stopped, None),
        }
    };
    let (state, menu) = if a.wait {
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
    // Mientras se esperaba pudo empezar otra conversación (`/clear`).
    let link = st
        .registry
        .lock()
        .await
        .get(&s.id)
        .and_then(|x| x.claude.clone())
        .or(s.claude)
        .expect("sesión de Claude");
    let turns = unread_turns(st, &caller.id, &s.id, &link, a.last).await?;
    let mut out = ReadOut {
        state,
        turns,
        question: menu.as_ref().map(Question::from),
        truncated: false,
    };
    fit(&mut out, MAX_READ);
    to_json(&out)
}

async fn tool_open(st: &AppState, me: &Machine, caller: &Session, args: Value) -> Reply {
    let a: OpenArgs = args_of(args)?;
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
    if a.prompt.trim().is_empty() {
        return Err(bad_request("falta el mensaje".into()));
    }
    if a.prompt.trim().len() > MAX_PROMPT {
        return Err(bad_request(format!(
            "el mensaje pasa de {} KB",
            MAX_PROMPT / 1024
        )));
    }
    let message = first_message(
        &caller.name,
        &me.name,
        &me.address(&caller.id),
        a.prompt.trim(),
    );
    let name = a.name.as_deref().map(str::trim).map(str::to_owned);
    let opener = me.address(&caller.id);
    let created = {
        let mut reg = st.registry.lock().await;
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
    to_json(&entry(me, &caller.id, &v, state))
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

async fn tool_answer(st: &AppState, me: &Machine, caller: &Session, args: Value) -> Reply {
    let a: AnswerArgs = args_of(args)?;
    let s = target(st, me, &a.session).await?;
    if !opened_by(me, s.opened_by.as_deref(), &caller.id) {
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

async fn tool_close(st: &AppState, me: &Machine, caller: &Session, args: Value) -> Reply {
    let a: SessionArg = args_of(args)?;
    let s = target(st, me, &a.session).await?;
    if !opened_by(me, s.opened_by.as_deref(), &caller.id) {
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
    }
}
