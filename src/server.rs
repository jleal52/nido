//! Gestor: registro, restauración, estado de cada sesión y API web.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router, middleware};
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, Notify, watch};

use crate::claude::{self, Procfs};
use crate::i18n::{self, tr};
use crate::launch::{self, Launcher, Paths};
use crate::proto::{self, Frame, Hello, HistoryRequest, Role, Size};
use crate::registry::{
    AlertRec, ClaudeLink, NoteKind, Registry, RegistryError, Session, new_id, now, now_ms,
};
use crate::{accounts, auth, e2e, hub, prompt, upload};

pub struct AppState {
    pub paths: Paths,
    pub launcher: Launcher,
    pub registry: Mutex<Registry>,
    /// `None` con `--no-auth`.
    pub token: Option<String>,
    /// Solo se escucha en loopback: se exige `Host` local (anti DNS-rebinding).
    pub loopback_only: bool,
    pub uploads: PathBuf,
    pub views: watch::Sender<Listing>,
    pub refresh: Notify,
    /// Último estado de Claude visto en cada sesión, para notar cuándo termina de trabajar.
    last_status: std::sync::Mutex<std::collections::HashMap<String, Option<String>>>,
    /// Las cuentas de Claude de este equipo y su uso.
    accounts: Arc<accounts::Accounts>,
    /// Contestar un menú de Claude es teclear paso a paso: de uno en uno en cada sesión (ver
    /// `turn`).
    turns: std::sync::Mutex<std::collections::HashMap<String, Arc<Mutex<()>>>>,
    /// Hasta dónde ha leído cada sesión la conversación de otra (la herramienta `read` del MCP).
    pub(crate) mcp_cursors: crate::mcp_api::Cursors,
    /// Los mensajes que esperan a cada sesión (la herramienta `send`) y qué sesiones tienen
    /// channel.
    pub inbox: crate::inbox::Inbox,
    /// Lo que cada sesión de Claude tiene en segundo plano (subagentes, órdenes), según su
    /// pantalla; lo pone al día `spawn_background` cada pocos segundos.
    background: std::sync::Mutex<std::collections::HashMap<String, serde_json::Value>>,
    /// Cuándo se avisó por última vez de que terminó cada sesión (para no avisar dos veces si
    /// termina, sigue y vuelve a terminar en poco tiempo).
    finished_pushed: std::sync::Mutex<std::collections::HashMap<String, std::time::Instant>>,
    /// El freno de las alertas que mandan las sesiones: un push por sesión cada 30 s.
    alert_throttle: std::sync::Mutex<AlertThrottle>,
    /// La conexión con el hub: móviles sin VPN y avisos.
    pub hub: Arc<hub::Hub>,
    /// Lo que `scan` ve pasar (terminó, te necesita), para avisar al móvil.
    alerts: tokio::sync::mpsc::UnboundedSender<Alert>,
    alerts_rx: std::sync::Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<Alert>>>,
    /// Canal cifrado con los teléfonos: identidad, QR en vigor y teléfonos emparejados.
    identity: e2e::Identity,
    pairing: e2e::Pairing,
    pub phones: std::sync::Mutex<e2e::Phones>,
    /// La escucha interna en loopback a la que van los flujos del túnel.
    pub tunnel_addr: std::sync::OnceLock<std::net::SocketAddr>,
    /// Direcciones por las que se llega a nido desde fuera (para el QR).
    pub direct: std::sync::OnceLock<Vec<String>>,
    /// El grupo de equipos y el aviso de que hay que sincronizarlo.
    pub group: crate::group::Group,
    group_dirty: Notify,
    /// Las peticiones de otros equipos ya atendidas (`e2e::MODE_PEER`), para no repetirlas.
    peer_replays: e2e::PeerReplays,
    /// Dónde estaba la conversación de cada sesión al encolarle un mensaje (`send`), para saber
    /// si ya contestó (`status`).
    pub(crate) mcp_marks: crate::mcp_api::Marks,
    /// Si vive la sesión madre de otro equipo de una abierta con `open` (un rato).
    pub(crate) mcp_parents: crate::mcp_api::Parents,
    /// Los avisos que esperan, callados, a que muera la madre de su sesión: uno por sesión y
    /// tipo (los que lleguen mientras, se juntan con ese).
    quiet: std::sync::Mutex<std::collections::HashSet<(String, AlertKind)>>,
    /// Cómo se llega a otro equipo del grupo: sin nada, por el hub (`peer_member`); en las
    /// pruebas, lo que se ponga con `set_peer_link`.
    peer_link: std::sync::OnceLock<PeerLink>,
    /// Las sesiones que se acaban de arrancar, para mirar su pantalla de arranque (ver
    /// `spawn_startup_watch`), con su bandeja ya retenida mientras tanto.
    launched: tokio::sync::mpsc::UnboundedSender<Launched>,
    launched_rx: std::sync::Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<Launched>>>,
    /// Las sesiones de las que ya se avisó, en este arranque, que esperan a que el usuario
    /// confíe en su carpeta (ver `trust_waiting`).
    trust_told: std::sync::Mutex<std::collections::HashSet<String>>,
    /// Los secretos de este equipo (`secret`).
    pub secrets: Arc<crate::secrets::Store>,
    /// Las preguntas de secretos en curso.
    pub asks: crate::secrets::Asks,
    /// Lo que las sesiones mandan al usuario (`clipboard`, `send_file`).
    pub outbox: Arc<crate::outbox::Outbox>,
    /// El plan de fases y tareas de cada sesión (`plan_*` del MCP).
    pub plans: Arc<crate::plan::Plans>,
    /// La memoria de la voz (`/api/v1/voice/memory`), en `<state>/voice-memory.json`.
    pub voice_memory: Arc<crate::voice_memory::VoiceMemory>,
    /// Los modelos que se pueden elegir, como los enseñó el menú de `/model` la última vez que
    /// nido lo abrió (`<state>/models.json`); si nunca, `DEFAULT_MODELS`.
    models: std::sync::Mutex<Vec<String>>,
    /// El modelo de cada sesión según su transcripción: lo pone al día `refresh_models`, fuera
    /// del cerrojo del registro, leyendo solo lo nuevo.
    model_seen: Arc<std::sync::Mutex<std::collections::HashMap<String, ModelSeen>>>,
    /// Que `refresh_models` vaya de uno en uno.
    model_refreshing: Mutex<()>,
    /// Cuánto se tarda en volver a buscar una transcripción que no se encontró (buscar es
    /// recorrer `projects/`).
    model_missing_ttl: std::sync::Mutex<Duration>,
    /// El modelo que nido le puso a cada sesión con `/model`, hasta que Claude conteste.
    model_chosen: std::sync::Mutex<std::collections::HashMap<String, ModelChosen>>,
    /// Los índices de las conversaciones para buscar (`/api/v1/search`).
    pub search: Arc<crate::search::Indexes>,
    /// Que las búsquedas vayan de una en una (la primera hace los índices: las que lleguen
    /// mientras esperan y aprovechan lo hecho; las que se cancelan, ni empiezan). Lo tiene el
    /// hilo que busca: aunque se cancele la petición, la siguiente espera a que acabe.
    searching: Arc<Mutex<()>>,
    /// La cola de Claude de cada transcripción, leída poco a poco (ver `transcript::Queue`).
    queues: std::sync::Mutex<std::collections::HashMap<PathBuf, crate::transcript::Queue>>,
}

/// Los modelos que se ofrecen si nido aún no ha visto el menú de `/model` (también valen como
/// petición: casan con la primera opción que empiece así).
pub const DEFAULT_MODELS: [&str; 4] = ["Opus", "Fable", "Sonnet", "Haiku"];

/// Lo último leído del modelo en la transcripción de una sesión.
#[derive(Default, Clone)]
struct ModelSeen {
    session_id: String,
    path: Option<PathBuf>,
    /// Hasta dónde se ha leído: el principio de la primera línea que aún no estaba entera.
    pos: u64,
    /// El modelo de la última respuesta (tal cual) y dónde empieza esa línea.
    last: Option<(String, u64)>,
    /// No se encontró la transcripción: no se vuelve a buscar hasta entonces.
    missing_until: Option<std::time::Instant>,
}

/// Cuánto se espera, por defecto, a volver a buscar una transcripción que no estaba.
const MODEL_MISSING_TTL: Duration = Duration::from_secs(30);

/// Pone al día el modelo de cada sesión de `targets` (id y su Claude) con lo nuevo de su
/// transcripción. E/S síncrona: en `spawn_blocking`. Devuelve si cambió alguno.
fn refresh_seen(
    seen: &std::sync::Mutex<std::collections::HashMap<String, ModelSeen>>,
    home: &std::path::Path,
    targets: Vec<(String, ClaudeLink)>,
    missing_ttl: Duration,
) -> bool {
    let mut changed = false;
    for (id, link) in targets {
        let old = {
            let mut all = seen.lock().expect("modelo visto");
            let e = all.entry(id.clone()).or_default();
            if e.session_id != link.session_id {
                *e = ModelSeen {
                    session_id: link.session_id.clone(),
                    ..Default::default()
                };
            }
            e.clone()
        };
        let mut e = old.clone();
        if e.path.as_ref().is_none_or(|p| !p.is_file()) {
            if e.missing_until
                .is_some_and(|t| std::time::Instant::now() < t)
            {
                continue;
            }
            let config = link
                .config_dir
                .clone()
                .unwrap_or_else(|| claude::default_config_dir(home));
            e = ModelSeen {
                session_id: e.session_id,
                ..Default::default()
            };
            match crate::transcript::find(&config, &link.session_id) {
                Some(p) => e.path = Some(p),
                None => e.missing_until = Some(std::time::Instant::now() + missing_ttl),
            }
        }
        if let Some(path) = e.path.clone() {
            let len = std::fs::metadata(&path).map_or(0, |m| m.len());
            // Más corta que lo leído: es otra (reescrita); otra vez desde el principio.
            if len < e.pos {
                e.pos = 0;
                e.last = None;
            }
            if e.pos == 0 && len > 0 {
                // La primera vez, desde el final hacia atrás (las hay de cientos de megas).
                e.last = crate::transcript::last_model(&path);
                e.pos = crate::transcript::whole_lines_end(&path, len);
            } else if len > e.pos {
                let (found, pos) = crate::transcript::models_since(&path, e.pos);
                if found.is_some() {
                    e.last = found;
                }
                e.pos = pos;
            }
        }
        changed |= e.last != old.last;
        let mut all = seen.lock().expect("modelo visto");
        // Si entretanto cambió de conversación, lo leído ya no vale.
        if all.get(&id).is_some_and(|x| x.session_id == e.session_id) {
            all.insert(id, e);
        }
    }
    changed
}

/// Un modelo puesto por nido: vale mientras Claude no conteste después (`at`: lo que medía la
/// transcripción al cambiarlo) en esa conversación y siga corriendo (es solo para esta sesión).
struct ModelChosen {
    session_id: String,
    model: String,
    at: u64,
}

fn load_models(path: &std::path::Path) -> Vec<String> {
    std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice::<Vec<String>>(&b).ok())
        .filter(|m| !m.is_empty())
        .unwrap_or_else(|| DEFAULT_MODELS.map(str::to_owned).to_vec())
}

/// Una sesión recién arrancada y, si hay quien mire los arranques, su bandeja retenida.
type Launched = (String, Option<crate::inbox::Hold>);

/// Otra forma de llegar a los demás equipos del grupo (en vez del hub), para las pruebas: recibe
/// el miembro, el método y los parámetros, y devuelve lo que contestaría ese equipo.
pub type PeerLink = Box<
    dyn Fn(
            crate::group::Member,
            String,
            serde_json::Value,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = e2e::PeerResult> + Send>>
        + Send
        + Sync,
>;

impl e2e::Host for AppState {
    fn identity(&self) -> &e2e::Identity {
        &self.identity
    }
    fn pairing(&self) -> &e2e::Pairing {
        &self.pairing
    }
    fn phones(&self) -> &std::sync::Mutex<e2e::Phones> {
        &self.phones
    }
    fn local_addr(&self) -> Option<std::net::SocketAddr> {
        self.tunnel_addr.get().copied()
    }
    fn hub_info(&self) -> Option<(String, String)> {
        self.hub.pair_info()
    }
    fn group(&self) -> Option<&crate::group::Group> {
        Some(&self.group)
    }
    fn group_changed(&self) {
        self.group_dirty.notify_one();
    }
    fn sync_group_now(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
        Box::pin(self.sync_group())
    }
    fn peer_call<'a>(
        &'a self,
        member: &'a crate::group::Member,
        method: &'a str,
        params: serde_json::Value,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = e2e::PeerResult> + Send + 'a>> {
        Box::pin(crate::mcp_api::peer_call(self, member, method, params))
    }
    fn peer_replays(&self) -> Option<&e2e::PeerReplays> {
        Some(&self.peer_replays)
    }
    fn host_name(&self) -> String {
        if let Some(n) = std::env::var("NIDO_NAME")
            .ok()
            .filter(|n| !n.trim().is_empty())
        {
            return n.trim().to_owned();
        }
        // El del sistema (en macOS no hay /etc/hostname), sin el dominio (Mac.local, Mac.casa…).
        let name = crate::sys::hostname().unwrap_or_default().trim().to_owned();
        let name = name.split('.').next().unwrap_or_default().to_owned();
        if name.is_empty() { "nido".into() } else { name }
    }
}

/// Un cambio de una sesión que merece aviso.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum AlertKind {
    /// Espera tu respuesta (un permiso, un menú).
    Waiting,
    /// Terminó de trabajar.
    Finished,
    /// La sesión mandó una alerta (`alert`): sale al momento.
    Requested,
}

#[derive(Debug, Clone)]
struct Alert {
    session: String,
    kind: AlertKind,
}

/// Antes de avisar se espera un poco: si en ese tiempo la ves (en la web, en la app) o sigue
/// sola, no hace falta.
const ALERT_DELAY: Duration = Duration::from_secs(6);
/// «Terminó» espera más: a menudo enseguida le llega algo (un subagente, otra sesión) y sigue.
const FINISHED_DELAY: Duration = Duration::from_secs(20);
/// Como mucho un push de alertas por sesión en este tiempo.
const ALERT_EVERY: Duration = Duration::from_secs(30);
/// El texto de una alerta, como mucho (en caracteres).
const ALERT_MAX_CHARS: usize = 500;

/// Qué hacer con una alerta según el freno.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Offer {
    /// Sale ya (con las que esperaban).
    Now,
    /// Es la primera que espera: sale con las que se junten cuando pase este tiempo.
    Schedule(Duration),
    /// Ya hay otras esperando: sale con ellas.
    Joined,
}

/// Lo que se juntó de una sesión para el siguiente push: cuántas, la última y si todas eran
/// alertas.
#[derive(Debug, Clone, PartialEq)]
struct Held {
    count: usize,
    /// El tipo de la última (`None`: una alerta).
    kind: Option<NoteKind>,
    text: String,
    only_alerts: bool,
}

/// El freno de las notas que mandan las sesiones (alertas y envíos): cuándo salió el último push
/// de cada sesión y las que se juntaron mientras tanto.
#[derive(Debug, Default)]
struct AlertThrottle {
    last: std::collections::HashMap<String, std::time::Instant>,
    held: std::collections::HashMap<String, Held>,
}

impl AlertThrottle {
    /// Una nota nueva: se apunta y dice si sale ya, si hay que poner el reloj o si se junta.
    fn offer(
        &mut self,
        session: &str,
        kind: Option<NoteKind>,
        text: &str,
        now: std::time::Instant,
    ) -> Offer {
        // Antes de mirar el freno: si sale ya, se lleva también las que esperaban, y si no,
        // `pending` dice si es la primera (la que pone el reloj).
        self.hold(session, kind, text);
        if self.allow(session, now) {
            Offer::Now
        } else if self.pending(session) == 1 {
            Offer::Schedule(self.remaining(session, now))
        } else {
            Offer::Joined
        }
    }

    /// Si puede salir un push ahora; si puede, cuenta desde ahora.
    fn allow(&mut self, session: &str, now: std::time::Instant) -> bool {
        if self
            .last
            .get(session)
            .is_some_and(|t| now.saturating_duration_since(*t) < ALERT_EVERY)
        {
            return false;
        }
        self.last.insert(session.to_owned(), now);
        true
    }

    /// Cuánto falta para que pueda salir el siguiente.
    fn remaining(&self, session: &str, now: std::time::Instant) -> Duration {
        self.last.get(session).map_or(Duration::ZERO, |t| {
            ALERT_EVERY.saturating_sub(now.saturating_duration_since(*t))
        })
    }

    /// Apunta una para el siguiente push.
    fn hold(&mut self, session: &str, kind: Option<NoteKind>, text: &str) {
        let h = self.held.entry(session.to_owned()).or_insert(Held {
            count: 0,
            kind: None,
            text: String::new(),
            only_alerts: true,
        });
        h.count += 1;
        h.only_alerts &= kind.is_none();
        h.kind = kind;
        h.text = text.to_owned();
    }

    /// Cuántas hay juntas esperando.
    fn pending(&self, session: &str) -> usize {
        self.held.get(session).map_or(0, |h| h.count)
    }

    /// Las que se juntaron, y queda vacío.
    fn take(&mut self, session: &str) -> Option<Held> {
        self.held.remove(session)
    }

    /// Salió un push de la sesión por otro lado (el de un secreto): cuenta para el freno.
    fn stamp(&mut self, session: &str, now: std::time::Instant) {
        self.last.insert(session.to_owned(), now);
    }

    /// Pasó el tiempo de las que esperaban: `None` si salen ya (o ya no queda ninguna); si
    /// entre tanto salió otro push de la sesión (el de un secreto), lo que falta.
    fn due(&mut self, session: &str, now: std::time::Instant) -> Option<Duration> {
        if self.pending(session) == 0 || self.allow(session, now) {
            None
        } else {
            Some(self.remaining(session, now))
        }
    }
}

/// El título y el texto del push de lo que se juntó de una sesión: una alerta, «🔔 sesión
/// (equipo)» y su texto; un texto para copiar o un fichero, con 📋 o 📎 y su título; varias,
/// cuántas («N alertas: <la última>» o, si no todas eran alertas, «N envíos de <sesión>»).
fn push_text(name: &str, host: &str, h: &Held) -> (String, String) {
    let icon = match (h.count, h.kind) {
        (1, Some(NoteKind::Clipboard)) => "📋",
        (1, Some(NoteKind::File)) => "📎",
        _ => "🔔",
    };
    let body = match h.count {
        1 => h.text.clone(),
        n if h.only_alerts => format!("{n} alertas: {}", h.text),
        n => format!("{n} envíos de {name}"),
    };
    (format!("{icon} {name} ({host})"), body)
}

/// El texto de una alerta, sin caracteres de control (salvo saltos de línea y tabuladores: ni
/// secuencias de escape, que pintarían en la terminal de `nido alerts`), sin espacios alrededor y
/// recortado. `None` si queda vacío.
pub(crate) fn alert_text(text: &str) -> Option<String> {
    let t = without_controls(text);
    let t: String = t.trim().chars().take(ALERT_MAX_CHARS).collect();
    let t = t.trim_end();
    (!t.is_empty()).then(|| t.to_owned())
}

/// `s` sin caracteres de control (C0, DEL y C1) menos `\n` y `\t`. Las secuencias de escape van
/// enteras: CSI (`ESC [ … final`), OSC (`ESC ] … BEL` o `ESC \`) y las de un carácter.
fn without_controls(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\n' | '\t' => out.push(c),
            '\u{1b}' => match chars.next() {
                // CSI: parámetros e intermedios hasta el final (@ a ~).
                Some('[') => {
                    for c in chars.by_ref() {
                        if ('@'..='~').contains(&c) {
                            break;
                        }
                    }
                }
                // OSC, DCS, SOS, PM, APC: hasta BEL o ST (`ESC \`).
                Some(']' | 'P' | 'X' | '^' | '_') => {
                    while let Some(c) = chars.next() {
                        if c == '\u{7}' {
                            break;
                        }
                        if c == '\u{1b}' {
                            if chars.peek() == Some(&'\\') {
                                chars.next();
                            }
                            break;
                        }
                    }
                }
                _ => {}
            },
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

/// La hora de parada que se guarda tras un escaneo (ms; 0: no se sabe). Se apunta al ver parada
/// una que estaba viva en el escaneo de antes (`was_alive`). En marcha se olvida: si luego se
/// para sin que nido lo vea, no se sabe desde cuándo, y la de antes ya no vale.
fn stop_time(stopped: u64, alive: bool, was_alive: bool, now: u64) -> u64 {
    if alive {
        0
    } else if was_alive {
        now
    } else {
        stopped
    }
}

/// Las alertas que el usuario aún no ha visto.
fn unread_alerts(alerts: &[AlertRec], seen: u64) -> Vec<AlertRec> {
    alerts.iter().filter(|a| a.at > seen).cloned().collect()
}

/// Lo que la web recibe por el WebSocket de eventos: las sesiones (en su orden) y el orden de
/// las carpetas.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Listing {
    pub sessions: Vec<SessionView>,
    pub folders: Vec<String>,
}

/// Lo que la web ve de una sesión.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SessionView {
    pub id: String,
    pub name: String,
    pub cwd: PathBuf,
    pub argv: Vec<String>,
    pub autostart: bool,
    pub created: u64,
    /// Cuándo se paró (ms; 0: no se sabe). Solo vale si no está viva.
    #[serde(skip_serializing_if = "crate::registry::is_zero")]
    pub stopped: u64,
    pub alive: bool,
    pub claude: Option<ClaudeView>,
    pub folder: Option<String>,
    /// Terminó de trabajar y el usuario aún no la ha abierto.
    pub unread: bool,
    /// Quién la abrió con el MCP de nido (`equipo/id`); `None`: el usuario.
    pub opened_by: Option<String>,
    /// Las alertas que mandó y el usuario aún no ha visto.
    pub alerts: Vec<AlertRec>,
    /// Cuándo cambiaron por última vez sus envíos (ver `Outbox::rev`; 0: no desde que arrancó
    /// nido): si cambia, la web y la app vuelven a pedirlos.
    #[serde(skip_serializing_if = "crate::registry::is_zero")]
    pub items_rev: u64,
    /// El secreto que espera que le den (ver `secrets::Asks`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub secret: Option<crate::secrets::AskView>,
    /// El resumen de su plan, si tiene (ver `plan::Summary`): si cambia su `rev`, la web y la app
    /// vuelven a pedirlo (`GET /api/v1/sessions/{id}/plan`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan: Option<crate::plan::Summary>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ClaudeView {
    /// Subagentes y órdenes en segundo plano (`agents`, `shells`, y desde cuándo: `since`, en ms).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub background: Option<serde_json::Value>,
    pub session_id: String,
    /// Corriendo ahora (si no, es la última conversación conocida, para retomarla).
    pub running: bool,
    pub status: Option<String>,
    pub title: Option<String>,
    pub account: Option<String>,
    /// El modelo, para enseñarlo («Opus 5.5»): el de su última respuesta o el que le puso nido.
    pub model: Option<String>,
    /// Los modelos que se pueden elegir (`POST /api/v1/sessions/{id}/model`).
    pub models: Vec<String>,
}

impl AppState {
    pub fn new(
        paths: Paths,
        launcher: Launcher,
        token: Option<String>,
        loopback_only: bool,
    ) -> Result<Arc<Self>, RegistryError> {
        let registry = Registry::load(&paths.registry())?;
        let uploads = paths.state.join("uploads");
        let hub = hub::Hub::new(paths.config.join("hub.json"));
        let (alerts, alerts_rx) = tokio::sync::mpsc::unbounded_channel();
        let (launched, launched_rx) = tokio::sync::mpsc::unbounded_channel();
        let paths_config_pair = paths.config.join("pair-demo.json");
        let inbox = crate::inbox::Inbox::load(paths.state.join("inbox.json"));
        let identity = e2e::Identity::load_or_create(&paths.config.join("hub.key"))
            .map_err(|e| RegistryError::Io(std::io::Error::other(e)))?;
        let phones = e2e::Phones::load(paths.data.join("phones.json"));
        let group = crate::group::Group::load(paths.data.join("group.json"), identity.public);
        // Los dispositivos emparejados antes de haber grupo, también en él.
        for p in phones.list() {
            if !group.snapshot().devices.iter().any(|d| d.key == p.key)
                && let Ok(k) =
                    base64::Engine::decode(&base64::engine::general_purpose::STANDARD, &p.key)
            {
                group.add_device(&k, &p.name);
            }
        }
        let accounts = accounts::Accounts::new(&paths.home, &paths.data);
        let secrets_dir = paths.state.join("secrets");
        let outbox = Arc::new(crate::outbox::Outbox::new(
            paths.state.join("outbox"),
            secrets_dir.clone(),
        ));
        let secrets = Arc::new(crate::secrets::Store::new(secrets_dir));
        let plans = Arc::new(crate::plan::Plans::new(paths.state.clone()));
        let voice_memory = Arc::new(crate::voice_memory::VoiceMemory::new(paths.state.clone()));
        let models = load_models(&paths.state.join("models.json"));
        Ok(Arc::new(Self {
            paths,
            launcher,
            registry: Mutex::new(registry),
            token,
            loopback_only,
            uploads,
            views: watch::Sender::new(Listing::default()),
            refresh: Notify::new(),
            last_status: Default::default(),
            accounts,
            turns: Default::default(),
            mcp_cursors: Default::default(),
            inbox,
            background: Default::default(),
            finished_pushed: Default::default(),
            alert_throttle: Default::default(),
            hub,
            alerts,
            alerts_rx: std::sync::Mutex::new(Some(alerts_rx)),
            identity,
            pairing: e2e::Pairing::load(paths_config_pair),
            phones: std::sync::Mutex::new(phones),
            tunnel_addr: Default::default(),
            direct: Default::default(),
            group,
            group_dirty: Notify::new(),
            peer_replays: Default::default(),
            mcp_marks: Default::default(),
            mcp_parents: Default::default(),
            quiet: Default::default(),
            peer_link: Default::default(),
            launched,
            launched_rx: std::sync::Mutex::new(Some(launched_rx)),
            trust_told: Default::default(),
            secrets,
            asks: Default::default(),
            outbox,
            plans,
            voice_memory,
            models: std::sync::Mutex::new(models),
            model_seen: Default::default(),
            model_refreshing: Mutex::new(()),
            model_missing_ttl: std::sync::Mutex::new(MODEL_MISSING_TTL),
            model_chosen: Default::default(),
            search: Default::default(),
            searching: Default::default(),
            queues: Default::default(),
        }))
    }

    /// Mira qué sesiones viven y qué Claude corre en cada una; guarda en el registro la
    /// conversación si cambió (p. ej. tras `/clear`) y publica la lista.
    pub async fn scan(&self) {
        let procfs = Procfs::default();
        let mut reg = self.registry.lock().await;
        let mut views = Vec::with_capacity(reg.sessions().len());
        let sessions: Vec<Session> = reg.sessions().to_vec();
        // Las que estaban vivas en el escaneo de antes: si ya no, se acaban de parar.
        let was_alive: std::collections::HashSet<String> = self
            .views
            .borrow()
            .sessions
            .iter()
            .filter(|v| v.alive)
            .map(|v| v.id.clone())
            .collect();
        let models = self.models();
        for s in sessions {
            let alive = launch::is_alive(&self.paths, &s.id);
            let stopped = stop_time(s.stopped, alive, was_alive.contains(&s.id), now_ms());
            if stopped != s.stopped {
                let _ = reg.update(&s.id, |x| x.stopped = stopped);
            }
            let found = alive
                .then(|| launch::holder_pid(&self.paths, &s.id))
                .flatten()
                .and_then(|pid| u32::try_from(pid).ok())
                .and_then(|pid| claude::find_claude(&procfs, pid, &self.paths.home));
            let mut link = s.claude.clone();
            if let Some(f) = &found {
                let inside_shell = !claude::is_claude(&s.argv);
                let account = link.as_ref().and_then(|l| l.account.clone());
                let fresh = ClaudeLink {
                    session_id: f.info.session_id.clone(),
                    config_dir: f.config_dir.clone(),
                    account,
                    inside_shell,
                };
                if link.as_ref() != Some(&fresh) {
                    link = Some(fresh);
                }
            } else if alive && link.as_ref().is_some_and(|l| l.inside_shell) {
                // Salió de Claude y sigue en la shell: al restaurar, solo la shell.
                link = None;
            }
            if link != s.claude {
                let _ = reg.update(&s.id, |x| x.claude = link.clone());
            }
            // Ha terminado de trabajar (de «busy» a cualquier otro estado): queda «sin leer»
            // hasta que el usuario la abra.
            let status = found.as_ref().and_then(|f| f.info.status.clone());
            let prev = self
                .last_status
                .lock()
                .expect("estado")
                .insert(s.id.clone(), status.clone())
                .flatten();
            let mut finished = s.finished;
            if prev.as_deref() == Some("busy") && status.as_deref() != Some("busy") {
                finished = now_ms();
                let _ = reg.update(&s.id, |x| x.finished = finished);
                let kind = if status.as_deref() == Some("waiting") {
                    AlertKind::Waiting
                } else {
                    AlertKind::Finished
                };
                let _ = self.alerts.send(Alert {
                    session: s.id.clone(),
                    kind,
                });
            } else if status.as_deref() == Some("waiting") && prev.as_deref() != Some("waiting") {
                let _ = self.alerts.send(Alert {
                    session: s.id.clone(),
                    kind: AlertKind::Waiting,
                });
            }
            // La pregunta de una sesión que ya no vive se cierra: su Claude no puede recibir la
            // ruta.
            if !alive {
                self.asks.cancel_session(&s.id);
            }
            views.push(SessionView {
                id: s.id.clone(),
                name: s.name.clone(),
                cwd: s.cwd.clone(),
                argv: s.argv.clone(),
                autostart: s.autostart,
                created: s.created,
                stopped,
                folder: s.folder.clone(),
                unread: finished > s.seen,
                opened_by: s.opened_by.clone(),
                alerts: unread_alerts(&s.alerts, s.seen),
                items_rev: self.outbox.rev(&s.id),
                secret: self.asks.view_for(&s.id),
                plan: self.plans.summary(&s.id),
                alive,
                claude: link.map(|l| ClaudeView {
                    model: self.model_of(&s.id, &l, found.is_some()),
                    models: models.clone(),
                    background: found.as_ref().and_then(|_| {
                        self.background
                            .lock()
                            .expect("segundo plano")
                            .get(&s.id)
                            .cloned()
                    }),
                    session_id: l.session_id,
                    running: found.is_some(),
                    status: found.as_ref().and_then(|f| f.info.status.clone()),
                    title: found.as_ref().and_then(|f| f.info.name.clone()),
                    account: l.account,
                }),
            });
        }
        let targets: Vec<(String, ClaudeLink)> = reg
            .sessions()
            .iter()
            .filter_map(|s| s.claude.clone().map(|l| (s.id.clone(), l)))
            .collect();
        let listing = Listing {
            sessions: views,
            folders: reg.folders().to_vec(),
        };
        // Publicada antes de soltar el registro: el siguiente escaneo lee de ella qué estaba vivo
        // (`was_alive`), y con la de antes apuntaría otra vez la misma parada.
        self.views.send_if_modified(|cur| {
            let changed = *cur != listing;
            if changed {
                *cur = listing;
            }
            changed
        });
        drop(reg);
        self.refresh_models(targets).await;
    }

    /// Al arrancar el equipo: vuelve a abrir las sesiones con autoarranque que no estén vivas.
    /// Solo una vez por arranque del equipo: si se reinicia nido (una actualización), las que
    /// estén paradas se quedan paradas.
    pub async fn restore(&self) {
        let marker = self.paths.state.join("restored-boot");
        let boot = crate::sys::boot_id();
        if let (Some(b), Ok(seen)) = (&boot, std::fs::read_to_string(&marker))
            && crate::sys::same_boot(b, seen.trim())
        {
            return;
        }
        if let Some(b) = &boot {
            let _ = std::fs::create_dir_all(&self.paths.state);
            if let Err(e) = std::fs::write(&marker, b) {
                eprintln!("nido: no se pudo apuntar el arranque restaurado: {e}");
            }
        }
        let sessions: Vec<Session> = self.registry.lock().await.sessions().to_vec();
        for s in sessions.iter().filter(|s| s.autostart) {
            if !launch::is_alive(&self.paths, &s.id) {
                let hold = self.launching(&s.id);
                match launch::start(&self.paths, self.launcher, &s.id) {
                    Ok(()) => self.launched(&s.id, hold),
                    Err(e) => eprintln!("nido: no se pudo restaurar {}: {e}", s.name),
                }
            }
        }
    }

    /// El turno para teclear en la sesión `id` (contestar un menú, cambiar el modo o el modelo,
    /// aceptar un aviso): de uno en uno en cada sesión, y sin esperar a las demás (una secuencia
    /// puede durar decenas de segundos).
    pub(crate) async fn turn(&self, id: &str) -> tokio::sync::OwnedMutexGuard<()> {
        let one = {
            let mut all = self.turns.lock().expect("turnos");
            // Los que no tiene nadie, fuera (si no, uno por cada sesión que haya existido).
            all.retain(|_, m| Arc::strong_count(m) > 1);
            all.entry(id.to_owned()).or_default().clone()
        };
        one.lock_owned().await
    }

    /// Se va a arrancar la sesión `id`: su bandeja, retenida ya, antes de que su `nido mcp`
    /// pueda recoger (ver `Inbox::hold`), si alguien va a mirar su arranque
    /// (`spawn_startup_watch`) y a soltarla. Se le pasa a `launched`; si no arranca, se tira.
    pub(crate) fn launching(&self, id: &str) -> Option<crate::inbox::Hold> {
        let watched = self.launched_rx.lock().expect("arranques").is_none();
        watched.then(|| self.inbox.hold(id))
    }

    /// Se acaba de arrancar la sesión `id` (con lo que dio `launching`): si es un Claude con el
    /// channel de nido, que se mire su pantalla de arranque.
    pub(crate) fn launched(&self, id: &str, hold: Option<crate::inbox::Hold>) {
        // Lo que se sabía de su channel (rechazo, su Claude, parada) era del arranque anterior.
        self.inbox.relaunched(id);
        self.trust_told.lock().expect("confianza").remove(id);
        let _ = self.launched.send((id.to_owned(), hold));
    }
}

impl AppState {
    /// Los modelos que se pueden elegir (ver `models`).
    pub fn models(&self) -> Vec<String> {
        self.models.lock().expect("modelos").clone()
    }

    /// Lo que enseñó el menú de `/model`: se guarda si cambió.
    /// Lo que hay en la cola de Claude según la transcripción `path` (sin los avisos del
    /// sistema), y hasta dónde está leída. Lee solo lo nuevo desde la vez anterior; bloquea.
    fn queue_of(&self, path: &std::path::Path) -> (Vec<String>, u64) {
        let mut queues = self.queues.lock().expect("colas");
        let q = queues.entry(path.to_owned()).or_default();
        if q.read(path).is_err() {
            *q = Default::default();
        }
        (q.shown(), q.offset())
    }

    fn remember_models(&self, models: Vec<String>) {
        let mut cur = self.models.lock().expect("modelos");
        if *cur == models || models.is_empty() {
            return;
        }
        *cur = models;
        let path = self.paths.state.join("models.json");
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Err(e) = std::fs::write(&path, serde_json::to_vec(&*cur).unwrap_or_default()) {
            eprintln!("nido: no se pudieron guardar los modelos: {e}");
        }
    }

    /// Para las pruebas: cuánto se tarda en volver a buscar una transcripción que no estaba.
    pub fn set_model_missing_ttl(&self, ttl: Duration) {
        *self.model_missing_ttl.lock().expect("ttl") = ttl;
    }

    /// Pone al día el modelo de las sesiones `targets` con lo nuevo de sus transcripciones, en
    /// un hilo aparte (fuera del cerrojo del registro y del de tokio); si alguno cambió, que se
    /// vuelva a escanear. De uno en uno: si ya hay otro, nada.
    async fn refresh_models(&self, targets: Vec<(String, ClaudeLink)>) {
        let Ok(_one) = self.model_refreshing.try_lock() else {
            return;
        };
        let (seen, home) = (self.model_seen.clone(), self.paths.home.clone());
        let ttl = *self.model_missing_ttl.lock().expect("ttl");
        let changed = tokio::task::spawn_blocking(move || refresh_seen(&seen, &home, targets, ttl))
            .await
            .unwrap_or(false);
        if changed {
            self.refresh.notify_one();
        }
    }

    /// El modelo de la sesión `id` para enseñarlo: el que le puso nido, si Claude no ha
    /// contestado después; si no, el de su última respuesta (lo último que leyó
    /// `refresh_models`; aquí no se lee nada).
    fn model_of(&self, id: &str, link: &ClaudeLink, running: bool) -> Option<String> {
        let last = self
            .model_seen
            .lock()
            .expect("modelo visto")
            .get(id)
            .filter(|e| e.session_id == link.session_id)
            .and_then(|e| e.last.clone());
        let mut chosen = self.model_chosen.lock().expect("modelo elegido");
        if let Some(c) = chosen.get(id) {
            let answered = last.as_ref().is_some_and(|(_, at)| *at >= c.at);
            if running && c.session_id == link.session_id && !answered {
                return Some(c.model.clone());
            }
            chosen.remove(id);
        }
        last.map(|(m, _)| crate::transcript::model_name(&m))
    }
}

/// Al arrancar `nido serve`, con el MCP encendido: que Claude tenga `nido mcp` en su
/// configuración de usuario, en `~/.claude` y en cada perfil (`CLAUDE_CONFIG_DIR`) que usen las
/// sesiones (ver `claude::ensure_mcp_registered`). Sin `claude` en este equipo, nada. Los fallos
/// se dicen una vez y no paran nada.
pub async fn register_mcp(st: Arc<AppState>) {
    if !launch::mcp_on(&st.paths) {
        return;
    }
    if !registers_mcp(
        std::env::var_os("NIDO_HOME"),
        std::env::var("NIDO_REGISTER_MCP").ok(),
    ) {
        eprintln!(
            "nido: con NIDO_HOME no se registra el MCP de nido en Claude (NIDO_REGISTER_MCP=1 para hacerlo)"
        );
        return;
    }
    let path = std::env::var_os("PATH");
    let Some(program) = claude::find_program(path.as_deref(), &st.paths.home) else {
        eprintln!("nido: no encuentro claude: el MCP de nido no se registra");
        return;
    };
    let nido = match launch::nido_exe() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("nido: no se pudo registrar el MCP en Claude: sin la ruta de nido ({e})");
            return;
        }
    };
    let dirs = {
        let reg = st.registry.lock().await;
        claude::config_dirs(
            reg.sessions().iter().filter_map(|s| s.claude.as_ref()),
            &st.paths.home,
        )
    };
    // En una carpeta neutra: el `.mcp.json` de donde se arrancó nido no cuenta.
    let neutral = st.paths.state.clone();
    let _ = std::fs::create_dir_all(&neutral);
    let cli = claude::Program {
        path: program,
        home: None,
        cwd: Some(neutral),
    };
    let done =
        tokio::task::spawn_blocking(move || claude::ensure_mcp_registered(&cli, &nido, &dirs))
            .await
            .unwrap_or_default();
    for (dir, r) in done {
        let profile = dir.map_or_else(|| "~/.claude".to_owned(), |d| d.display().to_string());
        match r {
            claude::Registration::Already => {}
            claude::Registration::Added => {
                eprintln!("nido: MCP de nido registrado en Claude ({profile})")
            }
            claude::Registration::Replaced => {
                eprintln!(
                    "nido: MCP de nido registrado de nuevo en Claude ({profile}): apuntaba a otro nido"
                )
            }
            claude::Registration::OtherScope(scope) => {
                eprintln!(
                    "nido: Claude ya tiene un MCP «nido» de otro ámbito ({profile}: {scope}); no se toca"
                )
            }
            claude::Registration::Failed(e) => {
                eprintln!("nido: no se pudo registrar el MCP de nido en Claude ({profile}): {e}")
            }
        }
    }
}

/// ¿Registra este nido su MCP en Claude? No si es uno de pruebas o de demostración (con
/// `NIDO_HOME`): el Claude es el de verdad del usuario, y lo dejaría apuntando a ese nido. Con
/// `NIDO_REGISTER_MCP=1`, sí.
fn registers_mcp(nido_home: Option<std::ffi::OsString>, flag: Option<String>) -> bool {
    nido_home.is_none() || flag.as_deref() == Some("1")
}

/// Cómo acabó la espera al registro del MCP antes de restaurar las sesiones.
#[derive(Debug, PartialEq, Eq)]
pub enum Registered {
    /// Antes del aviso.
    InTime,
    /// Tardó más que el aviso, pero acabó antes del tope.
    Late,
    /// Ni con el tope: se sigue sin él (cuando acabe, aún dice cómo fue).
    GaveUp,
}

/// Espera a que acabe de verdad el registro (`done`, cuando termina su tarea): si pasa de `warn`
/// se dice y se sigue esperando, hasta `cap` en total.
pub async fn registration_done(
    done: impl std::future::Future<Output = ()>,
    warn: Duration,
    cap: Duration,
) -> Registered {
    tokio::pin!(done);
    if tokio::time::timeout(warn, &mut done).await.is_ok() {
        return Registered::InTime;
    }
    eprintln!(
        "nido: registrar el MCP en Claude tarda más de {} s; se espera hasta {} s",
        warn.as_secs(),
        cap.as_secs()
    );
    match tokio::time::timeout(cap.saturating_sub(warn), done).await {
        Ok(()) => Registered::Late,
        Err(_) => Registered::GaveUp,
    }
}

/// Cuánto tiene que estar la caja de Claude en pantalla para dar por pintado su saludo.
const REPL_SETTLE: Duration = Duration::from_secs(1);

/// Cuánto se mira la pantalla de un Claude recién lanzado con el channel de nido.
pub const STARTUP_WINDOW: Duration = Duration::from_secs(60);
/// Lo más que se alarga mientras el usuario contesta si confía en la carpeta: este número de
/// plazos (con el de 60 s, 5 minutos).
const STARTUP_CAP: u32 = 5;

/// Mira la pantalla de cada Claude que se lanza con el channel de nido (ver `watch_startup`).
pub fn spawn_startup_watch(state: Arc<AppState>) {
    let Some(mut rx) = state.launched_rx.lock().expect("arranques").take() else {
        return;
    };
    tokio::spawn(async move {
        // Sin channel (o ya borrada), su bandeja se suelta al tirar `hold`.
        while let Some((id, hold)) = rx.recv().await {
            let Some(s) = state.registry.lock().await.get(&id).cloned() else {
                continue;
            };
            // Fuera del runtime: puede preguntarle a Claude su versión (`claude --version`, ver
            // `claude::channels_supported`).
            let (channel, old) = {
                let (paths, s) = (state.paths.clone(), s.clone());
                tokio::task::spawn_blocking(move || {
                    let channel = launch::holder_config(&paths, &s, 80, 24).channel;
                    let old = !channel
                        && launch::channel_launch(&paths, &s.argv)
                            == launch::ChannelLaunch::OldClaude;
                    (channel, old)
                })
                .await
                .unwrap_or((false, false))
            };
            if !channel {
                // Con un Claude Code de antes de los channels: sin channel, y que se sepa.
                if old {
                    state.inbox.refuse(&id, crate::inbox::Refusal::OldClaude);
                }
                // Sin channel no hay arranque que mirar; pero una hija de `open` puede quedarse
                // en la pregunta de confiar en la carpeta, y eso es del usuario.
                if s.opened_by.is_some() && claude::is_claude(&s.argv) {
                    let st = state.clone();
                    tokio::spawn(async move { watch_trust(&st, &id).await });
                }
                continue;
            }
            let st = state.clone();
            let hold = hold.unwrap_or_else(|| st.inbox.hold(&id));
            tokio::spawn(async move { watch_held(&st, &id, STARTUP_WINDOW, hold).await });
        }
    });
}

/// La pantalla de arranque de un Claude lanzado con el channel de nido, como visor y durante
/// `window`. El aviso de los channels de desarrollo se acepta (un Enter) solo si lista
/// exactamente `server:nido`; con otra lista no se toca. Si Claude dice que no tiene el servidor
/// `nido`, o que la organización de su cuenta no permite channels, la sesión queda sin channel.
/// Lo demás (la pregunta de confiar en la carpeta) es del
/// usuario: se sigue mirando, y cuando se va el plazo vuelve a empezar (como mucho
/// `STARTUP_CAP` plazos en total). Devuelve en qué quedó: `Accept` si se aceptó, `Nothing` si no salió
/// nada (o se cerró la sesión).
pub async fn watch_startup(st: &Arc<AppState>, id: &str, window: Duration) -> prompt::Startup {
    watch_held(st, id, window, st.inbox.hold(id)).await
}

/// La pantalla de arranque de un Claude sin channel abierto con `open`, como visor: si sale la
/// pregunta de confiar en la carpeta, se avisa al usuario (`AppState::trust_waiting`). Hasta ver
/// a Claude esperando peticiones, que se cierre, o `STARTUP_WINDOW` por `STARTUP_CAP`.
async fn watch_trust(st: &Arc<AppState>, id: &str) {
    let until = tokio::time::Instant::now() + STARTUP_WINDOW * STARTUP_CAP;
    let mut scr: Option<HolderScreen> = None;
    while tokio::time::Instant::now() < until {
        if scr.is_none() {
            let hello = Hello {
                role: Role::Viewer,
                cols: 80,
                rows: 24,
            };
            match holder_connect(st, id, hello).await {
                Ok(stream) => {
                    let (rd, wr) = crate::ipc::split(stream);
                    scr = Some(prompt::Screen::new(rd, wr));
                }
                Err(_) if !launch::is_alive(&st.paths, id) => return,
                Err(_) => {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    continue;
                }
            }
        }
        let Some(s) = scr.as_mut() else { return };
        match s.look().await {
            Ok(text) if prompt::trust_screen(&text) => {
                st.trust_waiting(id).await;
                return;
            }
            Ok(text) if prompt::repl(&text) => return,
            Ok(_) => {}
            Err(prompt::Unread::Slow) => scr = None,
            Err(prompt::Unread::Closed) => return,
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// `watch_startup`, con la bandeja ya retenida (`hold`, ver `AppState::launching`).
async fn watch_held(
    st: &Arc<AppState>,
    id: &str,
    window: Duration,
    hold: crate::inbox::Hold,
) -> prompt::Startup {
    use prompt::Startup;
    let started = tokio::time::Instant::now();
    let mut deadline = started + window;
    // La pregunta de confiar en la carpeta alarga el plazo, hasta este tope.
    let cap = started + window * STARTUP_CAP;
    let mut trusting = false;
    let name = st
        .registry
        .lock()
        .await
        .get(id)
        .map_or_else(|| id.to_owned(), |s| s.name.clone());
    let mut scr: Option<HolderScreen> = None;
    let mut accepted = false;
    // Hasta ver a Claude ya esperando peticiones sin que diga que no tiene channel (con el aviso
    // aceptado), su bandeja no da nada (ver `Inbox::hold`).
    let mut hold = Some(hold);
    let mut repl_since: Option<tokio::time::Instant> = None;
    // El guardián de este arranque (ver `launch::HolderId`), desde que se conecta: si se
    // reinicia, lo de este arranque ya no vale para el nuevo.
    let mut holder: Option<launch::HolderId> = None;
    while tokio::time::Instant::now() < deadline {
        if scr.is_none() {
            if holder.is_some() && launch::holder_id(&st.paths, id) != holder {
                return Startup::Nothing;
            }
            let hello = Hello {
                role: Role::Viewer,
                cols: 80,
                rows: 24,
            };
            match holder_connect(st, id, hello).await {
                Ok(stream) => {
                    let (rd, wr) = crate::ipc::split(stream);
                    scr = Some(prompt::Screen::new(rd, wr));
                    // Escribe su pid antes de atender.
                    holder = holder.or_else(|| launch::holder_id(&st.paths, id));
                }
                // Aún no atiende (al restaurar no se espera a que arranque).
                Err(_) => {
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    continue;
                }
            }
        }
        let Some(s) = scr.as_mut() else { break };
        let text = match s.look().await {
            Ok(t) => t,
            // Tarda: otra conexión, y se vuelve a mirar.
            Err(prompt::Unread::Slow) => {
                scr = None;
                continue;
            }
            Err(prompt::Unread::Closed) => break,
        };
        // Mientras está, y al irse, el plazo empieza de nuevo.
        let trust = prompt::trust_screen(&text);
        if trust && !trusting {
            st.trust_waiting(id).await;
        }
        if trust || trusting {
            deadline = (tokio::time::Instant::now() + window)
                .min(cap)
                .max(deadline);
        }
        trusting = trust;
        match prompt::startup(&text) {
            Startup::Accept if !accepted => {
                // De uno en uno, y mirando otra vez ya con el turno: otro que mire la misma
                // sesión puede haberlo aceptado entretanto.
                let _one = st.turn(id).await;
                let Ok(text) = s.screen().await else {
                    break;
                };
                if prompt::startup(&text) == Startup::Accept {
                    if s.keys(b"\r").await.is_err() {
                        break;
                    }
                    eprintln!("nido: {name}: aceptado el aviso del channel de nido");
                    // Con el turno hasta que se vaya el aviso: así nadie lo ve aún y pulsa otra vez.
                    let gone = tokio::time::Instant::now() + Duration::from_secs(3);
                    while tokio::time::Instant::now() < gone {
                        match s.screen().await {
                            Ok(t) if prompt::startup(&t) == Startup::Accept => {
                                tokio::time::sleep(Duration::from_millis(100)).await
                            }
                            _ => break,
                        }
                    }
                }
                accepted = true;
            }
            Startup::Foreign(list) => {
                eprintln!(
                    "nido: {name}: el aviso de channels lista otros ({}); no se toca",
                    list.join(" ")
                );
                // Retenida hasta que el usuario lo conteste (ver `keep_held`).
                if let Some(h) = hold.take() {
                    tokio::spawn(keep_held(
                        st.paths.clone(),
                        holder,
                        scr.take(),
                        h,
                        name.clone(),
                        id.to_owned(),
                    ));
                }
                return Startup::Foreign(list);
            }
            r @ (Startup::NoServer | Startup::Blocked) => {
                let why = refusal_of(&r);
                st.inbox.refuse(id, why);
                log_refusal(why, &name, id);
                return r;
            }
            _ => {}
        }
        if hold.is_some() {
            match ready(s, &text, &mut repl_since).await {
                Ready::Wait => {}
                // Se sigue mirando hasta el plazo.
                Ready::Release => hold = None,
                // Retenida aún; con otra conexión, se vuelve a mirar.
                Ready::Reconnect => scr = None,
                Ready::Refuse(r) => {
                    let why = refusal_of(&r);
                    st.inbox.refuse(id, why);
                    log_refusal(why, &name, id);
                    return r;
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    // Se acabó el plazo sin haberla soltado: con el aviso o la pregunta en pantalla, en otra
    // (iniciar sesión, aprobar un MCP…), o sin que su guardián contestara. Claude podría tirar lo
    // que le llegue: retenida, y se sigue mirando (ver `keep_held`).
    if let Some(h) = hold.take() {
        tokio::spawn(keep_held(
            st.paths.clone(),
            holder,
            scr.take(),
            h,
            name,
            id.to_owned(),
        ));
    }
    if accepted {
        Startup::Accept
    } else {
        Startup::Nothing
    }
}

type HolderScreen = prompt::Screen<crate::ipc::ReadHalf, crate::ipc::WriteHalf>;

/// Cada cuánto mira `keep_held` (`elapsed`: desde que empezó): cada segundo el primer minuto, y
/// luego cada cinco (al restaurar puede haber muchas retenidas).
fn keep_pace(elapsed: Duration) -> Duration {
    if elapsed < Duration::from_secs(60) {
        Duration::from_secs(1)
    } else {
        Duration::from_secs(5)
    }
}

/// Cada cuánto mira `keep_held` si sigue el Claude de la sesión: recorrer los procesos cuesta
/// (más en Windows), y al restaurar puede haber muchas retenidas.
const CLAUDE_EVERY: Duration = Duration::from_secs(5);

/// Si sigue el Claude de una sesión, con su guardián de este arranque.
#[derive(Debug, PartialEq, Eq)]
enum Present {
    Claude,
    NoClaude,
    /// Ya no está ese guardián: se cerró o se reinició la sesión.
    Replaced,
    /// No se pudo mirar.
    Unknown,
}

/// ¿Sigue habiendo un Claude (ver `claude::session_claude`) con `holder`, el guardián de la
/// sesión `id` en este arranque?
async fn claude_with(paths: &Paths, id: &str, holder: launch::HolderId) -> Present {
    let paths = paths.clone();
    let id = id.to_owned();
    tokio::task::spawn_blocking(move || {
        let found = u32::try_from(holder.0)
            .is_ok_and(|h| claude::session_claude(&Procfs::default(), h, &paths.home).is_some());
        // Después de mirar: si entretanto se reinició, lo visto no vale para el nuevo.
        if launch::holder_id(&paths, &id) != Some(holder) || !launch::is_alive(&paths, &id) {
            Present::Replaced
        } else if found {
            Present::Claude
        } else {
            Present::NoClaude
        }
    })
    .await
    .unwrap_or(Present::Unknown)
}

/// Tras tantas veces seguidas sin que su guardián conteste (con `keep_held`, un minuto o más), se
/// dice.
const SLOW_SAY: u32 = 12;

/// Las veces seguidas que el guardián de una sesión tardó en contestar.
#[derive(Default)]
struct SlowCount {
    n: u32,
    said: bool,
}

impl SlowCount {
    /// Otra vez sin contestar: `true` si toca decirlo (la primera vez que llega a `SLOW_SAY`).
    fn slow(&mut self) -> bool {
        self.n += 1;
        let say = self.n >= SLOW_SAY && !self.said;
        self.said |= say;
        say
    }

    /// Contestó.
    fn ok(&mut self) {
        self.n = 0;
    }
}

/// Si ya puede soltarse la bandeja de una sesión que arranca.
#[derive(Debug, PartialEq, Eq)]
enum Ready {
    Wait,
    Release,
    /// Su guardián tardó en dar lo que ha sacado: retenida aún, y otra conexión.
    Reconnect,
    /// Lo que ha sacado desde que arrancó dice que no tiene channel (`Blocked` o `NoServer`).
    Refuse(prompt::Startup),
}

/// Con la caja de Claude un rato en pantalla (`REPL_SETTLE`; `since`: desde cuándo), se mira todo
/// lo que ha sacado desde que arrancó (ver `prompt::launch_refusal`): retomada con mucha
/// conversación, su saludo ya no está en pantalla.
async fn ready<R, W>(
    s: &mut prompt::Screen<R, W>,
    text: &str,
    since: &mut Option<tokio::time::Instant>,
) -> Ready
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    if !prompt::repl(text) {
        *since = None;
        return Ready::Wait;
    }
    let t = *since.get_or_insert_with(tokio::time::Instant::now);
    if t.elapsed() < REPL_SETTLE {
        return Ready::Wait;
    }
    match s.output().await {
        Ok(out) => prompt::launch_refusal(&out).map_or(Ready::Release, Ready::Refuse),
        // Sin mirarlo no se suelta: Claude podría estar tirando el channel.
        Err(prompt::Unread::Slow) => Ready::Reconnect,
        // Se cerró: nada que retener.
        Err(prompt::Unread::Closed) => Ready::Release,
    }
}

/// Por qué no tiene channel, según lo que se vio (`Blocked` o `NoServer`).
fn refusal_of(r: &prompt::Startup) -> crate::inbox::Refusal {
    match r {
        prompt::Startup::Blocked => crate::inbox::Refusal::Blocked,
        _ => crate::inbox::Refusal::NoServer,
    }
}

fn log_refusal(why: crate::inbox::Refusal, name: &str, id: &str) {
    match why {
        crate::inbox::Refusal::Blocked => eprintln!(
            "nido: la organización de la cuenta de Claude no permite channels \
             (channelsEnabled): la sesión {name} ({id}) no recibe mensajes"
        ),
        crate::inbox::Refusal::NoServer => {
            eprintln!("nido: {name}: Claude no tiene el MCP de nido; queda sin channel")
        }
        // Ya lo dijo `claude::channels_supported`, una vez por programa.
        crate::inbox::Refusal::OldClaude => {}
        crate::inbox::Refusal::NoClaude => eprintln!(
            "nido: {name} ({id}): su Claude ya no está en marcha; queda sin channel hasta que se \
             vuelva a lanzar"
        ),
    }
}

/// Pasado el plazo sin haber soltado la bandeja (el aviso de los channels o la pregunta de
/// confiar aún en pantalla, otra pantalla, o su guardián sin contestar): se sigue mirando (sin
/// pulsar nada) hasta que Claude espere peticiones, y entonces se suelta (o se apunta que no
/// tiene channel). Solo se suelta sin mirar si se cierra la sesión (nadie atiende en su socket).
/// Si su Claude se va y la sesión sigue (p. ej. una shell de login que no le hace `exec`), sin
/// channel hasta que se vuelva a lanzar, y se deja de mirar. Si su guardián lleva mucho sin
/// contestar, se dice una vez. Mira cada vez menos (ver `keep_pace`; su Claude, cada
/// `CLAUDE_EVERY`). Es de un arranque (`holder`: su guardián, si se sabe): si la sesión se
/// reinicia, acaba sin tocar nada.
async fn keep_held(
    paths: Paths,
    mut holder: Option<launch::HolderId>,
    mut scr: Option<HolderScreen>,
    hold: crate::inbox::Hold,
    name: String,
    id: String,
) {
    use prompt::Startup;
    let socket = paths.socket(&id);
    let begun = tokio::time::Instant::now();
    let mut since = None;
    let mut slow = SlowCount::default();
    // Si se ha visto su Claude: sin verlo nunca no se sabe si se fue (instalado con npm su
    // proceso es `node`, y no se encuentra).
    let mut seen = false;
    let mut claude_at = begun;
    loop {
        tokio::time::sleep(keep_pace(begun.elapsed())).await;
        // Otro guardián (reiniciada) o ninguno (se cerró): no es ya este arranque.
        let now = launch::holder_id(&paths, &id);
        match holder {
            Some(h) if now != Some(h) => return,
            Some(_) => {}
            None => holder = now,
        }
        // Antes que la pantalla: sin Claude, la de la shell no dice nada.
        if let Some(h) = holder
            && tokio::time::Instant::now() >= claude_at
        {
            claude_at = tokio::time::Instant::now() + CLAUDE_EVERY;
            match claude_with(&paths, &id, h).await {
                Present::Claude => seen = true,
                Present::NoClaude if seen => {
                    hold.refuse(crate::inbox::Refusal::NoClaude);
                    log_refusal(crate::inbox::Refusal::NoClaude, &name, &id);
                    return;
                }
                Present::Replaced => return,
                Present::NoClaude | Present::Unknown => {}
            }
        }
        if scr.is_none() {
            let Ok(s) = viewer_at(&socket).await else {
                return;
            };
            scr = Some(s);
        }
        let Some(s) = scr.as_mut() else { return };
        let text = match s.look().await {
            Ok(t) => t,
            Err(prompt::Unread::Slow) => {
                scr = None;
                said_slow(&mut slow, &name, &id);
                continue;
            }
            Err(prompt::Unread::Closed) => return,
        };
        let r = match prompt::startup(&text) {
            r @ (Startup::NoServer | Startup::Blocked) => r,
            _ => match ready(s, &text, &mut since).await {
                Ready::Wait => {
                    slow.ok();
                    continue;
                }
                Ready::Release => return,
                Ready::Reconnect => {
                    scr = None;
                    said_slow(&mut slow, &name, &id);
                    continue;
                }
                Ready::Refuse(r) => r,
            },
        };
        let why = refusal_of(&r);
        hold.refuse(why);
        log_refusal(why, &name, &id);
        return;
    }
}

/// Otra vez sin que conteste el guardián de `keep_held`: tras `SLOW_SAY` seguidas, se dice (una
/// vez).
fn said_slow(slow: &mut SlowCount, name: &str, id: &str) {
    if slow.slow() {
        eprintln!(
            "nido: {name} ({id}): su guardián no contesta; la bandeja sigue retenida hasta que \
             conteste o se cierre"
        );
    }
}

/// ¿Sigue mereciendo aviso? Si no la has visto y sigue igual.
fn still_alerting(v: &SessionView, kind: AlertKind) -> bool {
    let waiting = v
        .claude
        .as_ref()
        .is_some_and(|c| c.running && c.status.as_deref() == Some("waiting"));
    match kind {
        AlertKind::Waiting => waiting,
        AlertKind::Finished => v.unread && !waiting,
        AlertKind::Requested => !v.alerts.is_empty(),
    }
}

/// Un aviso callado que espera (ver `AppState::alert_push`): mientras viva, otro igual de la
/// misma sesión no espera aparte. Al acabar la espera (o cancelarse), fuera.
struct Quiet<'a> {
    set: &'a std::sync::Mutex<std::collections::HashSet<(String, AlertKind)>>,
    key: (String, AlertKind),
}

impl<'a> Quiet<'a> {
    /// `None` si ya hay uno con esa clave. Se crea con el cerrojo ya suelto: uno que no entra,
    /// al tirarlo, lo cogería otra vez (y se quedaría colgado) y quitaría la clave del que espera.
    fn take(
        set: &'a std::sync::Mutex<std::collections::HashSet<(String, AlertKind)>>,
        key: (String, AlertKind),
    ) -> Option<Self> {
        let new = set.lock().expect("callados").insert(key.clone());
        new.then(|| Self { set, key })
    }
}

impl Drop for Quiet<'_> {
    fn drop(&mut self) {
        self.set.lock().expect("callados").remove(&self.key);
    }
}

/// Las que piden atención (el número sobre el icono de la app).
fn attention(sessions: &[SessionView]) -> usize {
    sessions
        .iter()
        .filter(|v| {
            still_alerting(v, AlertKind::Waiting)
                || still_alerting(v, AlertKind::Finished)
                || still_alerting(v, AlertKind::Requested)
                || v.secret.is_some()
        })
        .count()
}

impl AppState {
    /// Manda un aviso, cifrado para cada uno, a los teléfonos emparejados que pueden recibirlos
    /// (los que tienen handle en el hub). Olvida los que ya no existen. Devuelve los fallos.
    async fn push_all(&self, title: &str, body: &str, session: &str, badge: usize) -> Vec<String> {
        let phones: Vec<e2e::Phone> = self
            .phones
            .lock()
            .expect("teléfonos")
            .list()
            .iter()
            .filter(|p| p.handle.is_some())
            .cloned()
            .collect();
        let content =
            serde_json::json!({ "title": title, "body": body, "session": session, "badge": badge });
        // Agrupa en la pantalla de bloqueo los de la misma sesión, sin decirle al hub cuál es.
        let collapse = format!("{:x}", {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            session.hash(&mut h);
            h.finish()
        });
        let mut failed = Vec::new();
        for p in phones {
            let (Some(handle), Some(e)) = (p.handle.as_deref(), hub::seal(&p.push_key, &content))
            else {
                continue;
            };
            match self.hub.push(handle, e, &collapse).await {
                Ok(()) => {}
                Err(hub::PushError::Gone(r)) => {
                    eprintln!("nido: {} ya no recibe avisos ({r})", p.name);
                    let _ = self.phones.lock().expect("teléfonos").forget_handle(handle);
                    failed.push(format!("{}: ya no recibe avisos", p.name));
                }
                Err(hub::PushError::Other(r)) => {
                    eprintln!("nido: aviso a {} no enviado: {r}", p.name);
                    failed.push(format!("{}: {r}", p.name));
                }
            }
        }
        failed
    }

    /// La vista de una sesión y el número del icono, según la última lista.
    fn view_and_badge(&self, id: &str) -> (Option<SessionView>, usize) {
        let l = self.views.borrow();
        (
            l.sessions.iter().find(|v| v.id == id).cloned(),
            attention(&l.sessions),
        )
    }

    async fn alert(&self, a: Alert) {
        tokio::time::sleep(match a.kind {
            AlertKind::Waiting => ALERT_DELAY,
            AlertKind::Finished => FINISHED_DELAY,
            // Van por `session_alert`/`flush_alerts`.
            AlertKind::Requested => return,
        })
        .await;
        if let Some((title, body, badge)) = self.alert_push(&a).await {
            self.push_all(&title, &body, &a.session, badge).await;
        }
    }

    /// Lo que hay que mandar por `a`, ya pasado su retraso (el título, el texto y el número del
    /// icono), o nada si ya no hace falta.
    async fn alert_push(&self, a: &Alert) -> Option<(String, String, usize)> {
        let (view, _) = self.view_and_badge(&a.session);
        let v = view.filter(|v| still_alerting(v, a.kind))?;
        // De una sesión abierta con `open`, es para su madre mientras viva: al usuario no se le
        // avisa. Se vuelve a mirar cada rato (sin lo recordado) mientras siga haciendo falta: si
        // la madre ya no está, sale el aviso.
        if Self::quiet_while_parent(a.kind)
            && let Some(by) = v.opened_by.as_deref()
        {
            // Ya espera otro aviso igual de esta sesión: sale (o no) con ese.
            let _quiet = Quiet::take(&self.quiet, (a.session.clone(), a.kind))?;
            let mut fresh = false;
            while crate::mcp_api::parent_alive(self, by, &v.id, &v.name, fresh).await {
                tokio::time::sleep(self.mcp_parents.ttl()).await;
                fresh = true;
                let (view, _) = self.view_and_badge(&a.session);
                view.filter(|v| still_alerting(v, a.kind))?;
            }
        }
        let (view, badge) = self.view_and_badge(&a.session);
        let v = view.filter(|v| still_alerting(v, a.kind))?;
        // «Terminó» solo si de verdad ha terminado: no ha vuelto a trabajar y no le quedan
        // subagentes en marcha (mirando su pantalla ahora). Sin leer sigue igual en la web y la app.
        if a.kind == AlertKind::Finished {
            if v.claude
                .as_ref()
                .is_some_and(|c| c.status.as_deref() == Some("busy"))
            {
                return None;
            }
            if self.has_subagents(&v.id).await {
                return None;
            }
            let mut pushed = self.finished_pushed.lock().expect("avisos");
            if pushed
                .get(&v.id)
                .is_some_and(|t| t.elapsed() < FINISHED_DELAY)
            {
                return None;
            }
            pushed.insert(v.id.clone(), std::time::Instant::now());
        }
        let body = match a.kind {
            AlertKind::Waiting => "Te necesita".to_owned(),
            AlertKind::Finished => match v.claude.as_ref().and_then(|c| c.title.clone()) {
                Some(t) if t != v.name => format!("Terminó · {t}"),
                _ => "Terminó".to_owned(),
            },
            AlertKind::Requested => return None,
        };
        Some((v.name.clone(), body, badge))
    }

    /// La sesión `id` espera a que el usuario confíe en su carpeta (la pregunta de Claude al
    /// arrancar). Si la abrió otra sesión con `open`, eso solo lo puede hacer el usuario (ni nido
    /// ni su madre: `answer` se niega): se le avisa con una alerta (la 🔔 y un push), aunque viva su
    /// madre, una vez por arranque.
    pub(crate) async fn trust_waiting(self: &Arc<Self>, id: &str) {
        let Some(s) = self.registry.lock().await.get(id).cloned() else {
            return;
        };
        if s.opened_by.is_none()
            || !self
                .trust_told
                .lock()
                .expect("confianza")
                .insert(id.to_owned())
        {
            return;
        }
        let text = format!("Espera a que confíes en la carpeta {}", s.cwd.display());
        if let Some(text) = alert_text(&text) {
            let _ = self.session_alert(id, &text).await;
        }
    }

    /// Quiere decir algo al usuario que, de una sesión abierta con `open`, es para su madre
    /// mientras viva (que pregunta o que terminó; la madre lo sabe por su espera). Las alertas que
    /// manda la propia sesión (`alert`), nunca.
    fn quiet_while_parent(kind: AlertKind) -> bool {
        matches!(kind, AlertKind::Waiting | AlertKind::Finished)
    }

    /// Si hay a quién mandar avisos: algún teléfono con handle y el hub conectado.
    fn can_push(&self) -> bool {
        let connected = self.hub.status.borrow().connected;
        connected
            && self
                .phones
                .lock()
                .expect("teléfonos")
                .list()
                .iter()
                .any(|p| p.handle.is_some())
    }

    /// Una sesión manda una alerta al usuario (`text` ya limpio, de `alert_text`); ver
    /// [`AppState::session_note`].
    pub async fn session_alert(
        self: &Arc<Self>,
        id: &str,
        text: &str,
    ) -> Result<(), RegistryError> {
        self.session_note(id, None, text, None).await
    }

    /// Una nota de la sesión para el usuario (`text` ya limpio): se apunta, se redibuja la lista
    /// y sale un push a los teléfonos (como mucho uno cada 30 s por sesión; las demás se juntan
    /// en el siguiente). «Sin contestar» (`NoteKind::Secret`) no lleva push: ya se avisó al
    /// preguntar. No espera al push.
    pub async fn session_note(
        self: &Arc<Self>,
        id: &str,
        kind: Option<NoteKind>,
        text: &str,
        item: Option<&str>,
    ) -> Result<(), RegistryError> {
        let id = {
            let mut reg = self.registry.lock().await;
            let id = reg
                .find(id)
                .map(|s| s.id.clone())
                .ok_or_else(|| RegistryError::NotFound(id.to_owned()))?;
            reg.add_note(&id, text, now_ms(), kind, item.map(str::to_owned))?;
            id
        };
        self.scan().await;
        if kind == Some(NoteKind::Secret) || !self.can_push() {
            return Ok(());
        }
        let offer = self.alert_throttle.lock().expect("freno").offer(
            &id,
            kind,
            text,
            std::time::Instant::now(),
        );
        let st = self.clone();
        match offer {
            Offer::Now => {
                tokio::spawn(async move { st.flush_alerts(&id).await });
            }
            Offer::Schedule(mut d) => {
                tokio::spawn(async move {
                    // Si entre tanto salió el push de un secreto, otra espera.
                    loop {
                        tokio::time::sleep(d).await;
                        let now = std::time::Instant::now();
                        match st.alert_throttle.lock().expect("freno").due(&id, now) {
                            Some(more) => d = more,
                            None => break,
                        }
                    }
                    st.flush_alerts(&id).await;
                });
            }
            Offer::Joined => {}
        }
        Ok(())
    }

    /// Manda en un push las notas apartadas de una sesión, si aún no se han leído.
    async fn flush_alerts(&self, id: &str) {
        let Some(held) = self.alert_throttle.lock().expect("freno").take(id) else {
            return;
        };
        if !self.can_push() {
            return;
        }
        let (view, badge) = self.view_and_badge(id);
        let Some(v) = view.filter(|v| still_alerting(v, AlertKind::Requested)) else {
            return;
        };
        let host = <Self as e2e::Host>::host_name(self);
        let (title, body) = push_text(&v.name, &host, &held);
        self.push_all(&title, &body, id, badge).await;
    }

    /// Una sesión pide un secreto nuevo: push al momento («🔑 sesión (equipo)» / «pide
    /// «nombre»»), que cuenta para el freno de la sesión. No espera al push.
    pub(crate) async fn secret_push(self: &Arc<Self>, id: &str, secret: &str) {
        self.scan().await;
        if !self.can_push() {
            return;
        }
        self.alert_throttle
            .lock()
            .expect("freno")
            .stamp(id, std::time::Instant::now());
        let (view, badge) = self.view_and_badge(id);
        let Some(v) = view else {
            return;
        };
        let host = <Self as e2e::Host>::host_name(self);
        let title = format!("🔑 {} ({host})", v.name);
        let body = format!("pide «{secret}»");
        let (st, id) = (self.clone(), id.to_owned());
        tokio::spawn(async move {
            st.push_all(&title, &body, &id, badge).await;
        });
    }

    /// Si hay algún dispositivo (teléfono o navegador) que pueda contestar: emparejado aquí o
    /// en otro equipo del grupo (entra por el grupo).
    pub(crate) fn has_devices(&self) -> bool {
        self.group.snapshot().devices.iter().any(|d| !d.removed)
    }
}

impl AppState {
    /// Si la sesión tiene ahora subagentes en marcha (según su pantalla).
    async fn has_subagents(&self, id: &str) -> bool {
        let hello = Hello {
            role: Role::Viewer,
            cols: 80,
            rows: 24,
        };
        let Ok(stream) = holder_connect(self, id, hello).await else {
            return false;
        };
        let (rd, wr) = crate::ipc::split(stream);
        let mut scr = prompt::Screen::new(rd, wr);
        let Ok(Ok(text)) = tokio::time::timeout(Duration::from_secs(3), scr.screen()).await else {
            return false;
        };
        prompt::background(&text).is_some_and(|b| !b.agents.is_empty())
    }
}

impl AppState {
    /// Lo que el grupo ha dado de baja, fuera también de aquí.
    fn apply_group(&self) {
        for key in self.group.removed_devices() {
            if let Ok(true) = self.phones.lock().expect("teléfonos").remove_by_key(&key) {
                eprintln!("nido: un dispositivo quitado en el grupo, quitado aquí");
            }
        }
    }

    /// Cuenta el estado del grupo a cada equipo y junta lo que contesten. Si alguno sabía algo
    /// nuevo, se repite una vez para que lo sepan todos.
    async fn sync_group(&self) {
        for _ in 0..2 {
            let mut learned = false;
            for m in self.group.others() {
                let Ok(key) =
                    base64::Engine::decode(&base64::engine::general_purpose::STANDARD, &m.key)
                else {
                    continue;
                };
                let Ok(key) = <[u8; 32]>::try_from(key) else {
                    continue;
                };
                let ours = serde_json::to_vec(&self.group.snapshot()).unwrap_or_default();
                let Ok((dial, first)) = e2e::Dial::start(&self.identity, &key, None, &ours) else {
                    continue;
                };
                let reply = match self.hub.exchange(&m.id, first).await {
                    Ok(r) => r,
                    Err(_) => continue, // apagado o sin conexión: ya se pondrá al día
                };
                if let Ok(payload) = dial.finish(&reply)
                    && let Ok(theirs) = serde_json::from_slice::<crate::group::State>(&payload)
                    && self.group.merge(&theirs)
                {
                    learned = true;
                }
            }
            self.apply_group();
            if !learned {
                break;
            }
        }
    }

    /// Pide `method` (una herramienta del MCP, con quien llama en `params.caller`) al nido de otro
    /// equipo del grupo, por su id en el hub o su nombre, por el canal «peer» (`e2e::MODE_PEER`).
    /// Devuelve lo que conteste: lo de la herramienta o su error (en español), con el estado HTTP
    /// que tendría en la API local. Por el nombre, si hay dos equipos que se llaman igual, hay
    /// que dar el id.
    pub async fn peer(
        &self,
        member: &str,
        method: &str,
        params: serde_json::Value,
    ) -> e2e::PeerResult {
        let m = self.member(member)?;
        self.peer_member(&m, method, params).await
    }

    /// El equipo del grupo (sin este) con ese id en el hub o, si no, con ese nombre.
    pub fn member(&self, member: &str) -> Result<crate::group::Member, (u16, String)> {
        let member = member.trim();
        let others = self.group.others();
        if let Some(m) = others.iter().find(|m| m.id == member) {
            return Ok(m.clone());
        }
        let named: Vec<_> = others
            .iter()
            .filter(|m| m.name.eq_ignore_ascii_case(member))
            .collect();
        match named[..] {
            [m] => Ok(m.clone()),
            [] => Err(unknown_member(member)),
            _ => Err((400, ambiguous_member(member))),
        }
    }

    /// Como `peer`, a un miembro ya elegido.
    pub async fn peer_member(
        &self,
        m: &crate::group::Member,
        method: &str,
        params: serde_json::Value,
    ) -> e2e::PeerResult {
        if let Some(link) = self.peer_link.get() {
            return link(m.clone(), method.to_owned(), params).await;
        }
        let key = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, &m.key)
            .ok()
            .and_then(|k| <[u8; 32]>::try_from(k).ok())
            .ok_or_else(|| unknown_member(&m.id))?;
        e2e::peer_request(&self.identity, &key, method, params, |first| {
            self.hub.exchange(&m.id, first)
        })
        .await
    }

    /// Para las pruebas: a los demás equipos se llega con `link` y no por el hub. Solo una vez:
    /// `false` si ya había otro.
    pub fn set_peer_link(&self, link: PeerLink) -> bool {
        self.peer_link.set(link).is_ok()
    }

    /// Unirse al grupo de otro equipo con su código de `nido group invite`.
    pub async fn join_group(&self, code: &str) -> Result<Vec<crate::group::MemberView>, String> {
        let c = crate::group::parse_invite(code)?;
        let Some((hub, self_id)) = self.hub.pair_info() else {
            return Err("este equipo no está conectado al hub (nido hub)".into());
        };
        let host = |u: &str| {
            u.split("://")
                .nth(1)
                .unwrap_or(u)
                .trim_end_matches('/')
                .to_owned()
        };
        if host(&hub) != host(&c.hub) {
            return Err(format!(
                "el otro equipo usa otro hub ({}); este usa {hub}",
                c.hub
            ));
        }
        let name = <Self as e2e::Host>::host_name(self);
        let hello =
            serde_json::json!({ "id": self_id, "name": name, "state": self.group.snapshot() });
        let (dial, first) = e2e::Dial::start(
            &self.identity,
            &c.key,
            Some(&c.secret),
            hello.to_string().as_bytes(),
        )?;
        let reply = self.hub.exchange(&c.id, first).await?;
        let payload = dial.finish(&reply)?;
        let theirs: crate::group::State =
            serde_json::from_slice(&payload).map_err(|e| e.to_string())?;
        self.group.merge(&theirs);
        self.group_dirty.notify_one();
        Ok(self.group.members())
    }
}

/// El grupo: apuntarse al conectar con el hub y sincronizar cuando cambia algo (y cada 5 min).
fn spawn_group(state: Arc<AppState>) {
    let st = state.clone();
    tokio::spawn(async move {
        let mut status = st.hub.status.subscribe();
        loop {
            let s = status.borrow_and_update().clone();
            if s.connected
                && let Some(id) = &s.id
            {
                st.group
                    .set_self(id, &<AppState as e2e::Host>::host_name(&st));
                st.group_dirty.notify_one();
            }
            if status.changed().await.is_err() {
                return;
            }
        }
    });
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(300));
        loop {
            tokio::select! {
                _ = tick.tick() => {}
                _ = state.group_dirty.notified() => {
                    // Los cambios suelen venir juntos: un momento y se manda todo.
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
            }
            if state.hub.status.borrow().connected && !state.group.others().is_empty() {
                state.sync_group().await;
            } else {
                state.apply_group();
            }
        }
    });
}

/// Lo de segundo plano como se manda: cada cosa con desde cuándo (ms) en vez de cuánto lleva.
pub fn background_json(bg: &prompt::Background) -> serde_json::Value {
    let now = now_ms();
    let since = |secs: u64| now.saturating_sub(secs * 1000);
    let agents: Vec<serde_json::Value> = bg
        .agents
        .iter()
        .map(|a| serde_json::json!({ "kind": a.kind, "task": a.task, "since": since(a.elapsed) }))
        .collect();
    let oldest = bg.agents.iter().map(|a| a.elapsed).max();
    serde_json::json!({ "agents": agents, "shells": bg.shells, "since": oldest.map(since) })
}

/// Cada 10 s mira la pantalla de las sesiones con Claude en marcha: si tiene subagentes u órdenes
/// en segundo plano (aunque él esté esperando, siguen trabajando).
fn spawn_background(state: Arc<AppState>) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(10));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tick.tick().await;
            let sessions = state.views.borrow().sessions.clone();
            let mut seen = std::collections::HashMap::new();
            for s in sessions {
                if !s.alive || !s.claude.as_ref().is_some_and(|c| c.running) {
                    continue;
                }
                let hello = Hello {
                    role: Role::Viewer,
                    cols: 80,
                    rows: 24,
                };
                let Ok(stream) = holder_connect(&state, &s.id, hello).await else {
                    continue;
                };
                let (rd, wr) = crate::ipc::split(stream);
                let mut scr = prompt::Screen::new(rd, wr);
                let Ok(Ok(text)) = tokio::time::timeout(Duration::from_secs(3), scr.screen()).await
                else {
                    continue;
                };
                if let Some(bg) = prompt::background(&text) {
                    seen.insert(s.id.clone(), background_json(&bg));
                }
            }
            let changed = {
                let mut cur = state.background.lock().expect("segundo plano");
                // Sin cambios de qué hay (el «desde» se mueve un poco en cada lectura): no se avisa.
                let shape = |m: &std::collections::HashMap<String, serde_json::Value>| {
                    let mut v: Vec<String> = m
                        .iter()
                        .map(|(k, b)| {
                            let tasks: Vec<&str> = b["agents"]
                                .as_array()
                                .map(|a| a.iter().filter_map(|x| x["task"].as_str()).collect())
                                .unwrap_or_default();
                            format!("{k}:{}:{}", b["shells"], tasks.join("|"))
                        })
                        .collect();
                    v.sort();
                    v
                };
                let changed = shape(&cur) != shape(&seen);
                if changed {
                    *cur = seen;
                }
                changed
            };
            if changed {
                state.refresh.notify_one();
            }
        }
    });
}

/// Cada cuánto se mira si lo dado a los `nido mcp` está ya en la transcripción de su sesión.
const DELIVERY_TICK: Duration = Duration::from_secs(2);

/// Cada `DELIVERY_TICK`, en las sesiones con mensajes dados que aún no se ven en su
/// transcripción (y solo en ellas), mira lo nuevo de la transcripción y vuelve a dar lo que toque
/// (ver `mcp_api::check_delivery`).
pub fn spawn_delivery_watch(state: Arc<AppState>) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(DELIVERY_TICK);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut last = tokio::time::Instant::now();
        loop {
            tick.tick().await;
            let elapsed = last.elapsed();
            last = tokio::time::Instant::now();
            for id in state.inbox.outstanding() {
                crate::mcp_api::check_delivery(&state, &id, Some(elapsed)).await;
            }
        }
    });
}

/// Cada cuánto se borra lo caducado de secretos y envíos.
const CLEANUP_EVERY: Duration = Duration::from_secs(10 * 60);

/// Al arrancar y cada `CLEANUP_EVERY`: secretos recordados vencidos, sin recordar de más de
/// 24 h, envíos de más de 7 días o por encima del tope, y lo de sesiones que ya no existen.
fn spawn_cleanup(state: Arc<AppState>) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(CLEANUP_EVERY);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tick.tick().await;
            let st = state.clone();
            let _ = tokio::task::spawn_blocking(move || {
                // Con el registro cogido mientras se limpia: una sesión que se cree a la vez no
                // puede guardar nada antes de estar en él, así que no se le borra lo recién
                // guardado por no estar en una lista vieja.
                let reg = st.registry.blocking_lock();
                let ids: std::collections::HashSet<&str> =
                    reg.sessions().iter().map(|s| s.id.as_str()).collect();
                let now = now_ms();
                st.secrets.prune(now, |id| ids.contains(id));
                st.outbox.prune(now, |id| ids.contains(id));
            })
            .await;
        }
    });
}

/// Cada 2 s (o al pedirlo) vuelve a mirar el estado.
pub fn spawn_scanner(state: Arc<AppState>) {
    spawn_group(state.clone());
    spawn_background(state.clone());
    spawn_startup_watch(state.clone());
    spawn_delivery_watch(state.clone());
    spawn_cleanup(state.clone());
    // Los avisos al móvil, según los cambios que vea `scan`.
    if let Some(mut rx) = state.alerts_rx.lock().expect("avisos").take() {
        let st = state.clone();
        tokio::spawn(async move {
            while let Some(a) = rx.recv().await {
                if !st.can_push() {
                    continue;
                }
                let st = st.clone();
                tokio::spawn(async move { st.alert(a).await });
            }
        });
    }
    // La conexión con el hub (si hay uno configurado).
    hub::spawn(state.hub.clone(), state.clone());
    // Las cuentas de Claude: lo que gasta cada una, mirado de vez en cuando.
    state.accounts.spawn(state.paths.home.clone());
    // Tras un cambio automático de cuenta, las sesiones paradas por el límite siguen con la nueva.
    {
        let st = state.clone();
        tokio::spawn(async move {
            loop {
                st.accounts.auto_switched().notified().await;
                let resumed = resume_limited(&st).await;
                if !resumed.is_empty() {
                    eprintln!(
                        "nido: retomadas tras el cambio de cuenta: {}",
                        resumed.join(", ")
                    );
                }
            }
        });
    }
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(2));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = tick.tick() => {}
                _ = state.refresh.notified() => {}
            }
            state.scan().await;
        }
    });
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/api/v1/info", get(api_info))
        .route("/api/v1/browse", get(api_browse))
        .route("/api/v1/accounts", get(api_accounts))
        .route("/api/v1/accounts/switch", post(api_switch_account))
        .route("/api/v1/accounts/login", post(api_login_start))
        .route("/api/v1/accounts/login/finish", post(api_login_finish))
        .route("/api/v1/accounts/auto", post(api_accounts_auto))
        .route(
            "/api/v1/accounts/{number}",
            axum::routing::delete(api_remove_account),
        )
        .route("/api/v1/e2e", get(ws_e2e))
        .route("/api/v1/pair", post(api_pair).delete(api_pair_stop))
        .route("/api/v1/group", get(api_group))
        .route("/api/v1/group/invite", post(api_group_invite))
        .route("/api/v1/group/join", post(api_group_join))
        .route(
            "/api/v1/group/members/{id}",
            axum::routing::delete(api_group_remove),
        )
        .route("/api/v1/phones", get(api_phones))
        .route("/api/v1/phones/me", axum::routing::delete(api_remove_me))
        .route(
            "/api/v1/phones/{id}",
            axum::routing::delete(api_remove_phone),
        )
        .route("/api/v1/hub", get(api_hub).post(api_set_hub))
        .route("/api/v1/push/test", post(api_push_test))
        .route("/api/v1/voice/token", post(api_voice_token))
        // La memoria de la voz: cada ruta tiene que mirar `paired_device` antes que nada y estar
        // en `memory_routes()` de las pruebas, que comprueban que el token de este equipo no vale.
        .route("/api/v1/voice/memory", get(api_voice_memory))
        .route(
            "/api/v1/voice/memory/notes",
            post(api_voice_note_add).layer(DefaultBodyLimit::max(MEMORY_BODY)),
        )
        .route(
            "/api/v1/voice/memory/notes/{id}",
            axum::routing::delete(api_voice_note_delete),
        )
        .route(
            "/api/v1/voice/memory/calls",
            post(api_voice_call_put).layer(DefaultBodyLimit::max(MEMORY_BODY)),
        )
        .route("/api/v1/sessions", get(api_list).post(api_create))
        .route("/api/v1/conversations", get(api_conversations))
        .route(
            "/api/v1/sessions/{id}",
            axum::routing::patch(api_update).delete(api_delete),
        )
        .route("/api/v1/folders/rename", post(api_rename_folder))
        .route("/api/v1/folders/order", post(api_order_folders))
        .route("/api/v1/sessions/order", post(api_order_sessions))
        .route("/api/v1/sessions/{id}/start", post(api_start))
        .route("/api/v1/sessions/{id}/seen", post(api_seen))
        .route("/api/v1/sessions/{id}/alert", post(api_alert))
        .route(
            "/api/v1/sessions/{id}/items",
            get(crate::deliver::api_items),
        )
        .route("/api/v1/sessions/{id}/plan", get(api_plan))
        .route(
            "/api/v1/sessions/{id}/items/{item}",
            get(crate::deliver::api_item).delete(crate::deliver::api_dismiss_item),
        )
        .route(
            "/api/v1/sessions/{id}/secret",
            post(crate::deliver::api_secret_answer)
                .layer(DefaultBodyLimit::max(crate::deliver::MAX_ANSWER)),
        )
        .route("/api/v1/secrets", get(crate::deliver::api_secrets))
        .route(
            "/api/v1/secrets/{name}",
            axum::routing::delete(crate::deliver::api_forget_secret),
        )
        .route("/api/v1/sessions/{id}/answer", post(api_answer))
        .route("/api/v1/sessions/{id}/mode", post(api_mode))
        .route("/api/v1/sessions/{id}/model", post(api_model))
        .route("/api/v1/sessions/{id}/queue/send", post(api_queue_send))
        .route("/api/v1/sessions/{id}/queue/edit", post(api_queue_edit))
        .route("/api/v1/sessions/{id}/stop", post(api_stop))
        .route("/api/v1/sessions/{id}/restart", post(api_restart))
        .route("/api/v1/sessions/{id}/history", get(api_history))
        .route("/api/v1/sessions/{id}/chat", get(api_chat))
        .route("/api/v1/search", get(api_search))
        .route("/api/v1/sessions/{id}/live", get(ws_live))
        .route("/api/v1/sessions/{id}/term", get(ws_term))
        .route("/api/v1/events", get(ws_events))
        .route("/api/v1/mcp/inbox", get(crate::mcp_api::api_inbox))
        .route("/api/v1/mcp/inbox/ack", post(crate::mcp_api::api_inbox_ack))
        .route("/api/v1/mcp/alerts", get(crate::mcp_api::api_alerts))
        .route("/api/v1/mcp/{tool}", post(crate::mcp_api::api_mcp))
        .route(
            "/api/v1/upload",
            post(upload::api_upload).layer(DefaultBodyLimit::max(upload::MAX_BYTES)),
        )
        .layer(middleware::from_fn_with_state(state.clone(), auth::guard))
        // Por fuera de todo (también de `guard`): el idioma de los mensajes, según la petición.
        .layer(middleware::from_fn(i18n::lang_layer))
        .with_state(state)
}

fn unknown_member(member: &str) -> (u16, String) {
    (404, format!("«{member}» no es ningún equipo del grupo"))
}

/// Hay más de un equipo con ese nombre: hay que dar su id.
pub(crate) fn ambiguous_member(member: &str) -> String {
    format!("hay varios equipos «{member}» en el grupo: usa su id")
}

/// Un error para la app o la web, en el idioma de la petición.
pub fn error(status: StatusCode, msg: impl Into<String>) -> Response {
    (
        status,
        Json(serde_json::json!({ "error": tr(&msg.into()) })),
    )
        .into_response()
}

fn registry_error(e: RegistryError) -> Response {
    error(registry_status(&e), e.to_string())
}

/// El código HTTP de un error del registro.
pub(crate) fn registry_status(e: &RegistryError) -> StatusCode {
    match e {
        RegistryError::NotFound(_) => StatusCode::NOT_FOUND,
        RegistryError::Io(_) => StatusCode::INTERNAL_SERVER_ERROR,
        _ => StatusCode::BAD_REQUEST,
    }
}

async fn api_info(State(st): State<Arc<AppState>>) -> Response {
    Json(serde_json::json!({
        "name": env!("CARGO_PKG_NAME"),
        "version": env!("CARGO_PKG_VERSION"),
        "api": 1,
        "auth": st.token.is_some(),
        "home": st.paths.home,
        // Para la web y la app: cómo se escriben aquí las rutas y qué órdenes proponer.
        "os": std::env::consts::OS,
        "sep": std::path::MAIN_SEPARATOR.to_string(),
        "roots": crate::browse::roots(),
        "commands": crate::browse::commands(),
        // Los modelos que se pueden elegir con `POST /api/v1/sessions/{id}/model`.
        "models": st.models(),
    }))
    .into_response()
}

#[derive(Deserialize)]
struct BrowseQuery {
    #[serde(default)]
    path: String,
}

/// Las carpetas que hay dentro de una (para elegir dónde abrir una sesión). Solo carpetas y
/// solo sus nombres: nada de ficheros ni de su contenido.
async fn api_browse(State(st): State<Arc<AppState>>, Query(q): Query<BrowseQuery>) -> Response {
    let home = st.paths.home.clone();
    match tokio::task::spawn_blocking(move || crate::browse::list(&home, &q.path)).await {
        Ok(Ok(l)) => Json(l).into_response(),
        Ok(Err(e)) => error(StatusCode::NOT_FOUND, e),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

/// La vista se rehace de fondo, en español: se traduce al mandarla.
fn accounts_view(mut view: accounts::View) -> Response {
    for a in &mut view.accounts {
        a.problem = a.problem.as_deref().map(tr);
    }
    view.error = view.error.as_deref().map(tr);
    Json(view).into_response()
}

async fn api_accounts(State(st): State<Arc<AppState>>) -> Response {
    accounts_view(st.accounts.view())
}

#[derive(Deserialize)]
struct AutoReq {
    enabled: bool,
}

/// Enciende o apaga el cambio automático de cuenta de este equipo; devuelve las cuentas.
async fn api_accounts_auto(State(st): State<Arc<AppState>>, Json(req): Json<AutoReq>) -> Response {
    match st.accounts.set_auto(req.enabled).await {
        Ok(v) => accounts_view(v),
        Err(e) => error(StatusCode::BAD_REQUEST, e),
    }
}

#[derive(Deserialize)]
struct LoginStartReq {
    /// Para volver a entrar en una cuenta: su correo (la página de Claude ya lo trae puesto).
    #[serde(default)]
    email: Option<String>,
}

/// Empieza a entrar en una cuenta de Claude: el enlace que hay que abrir (en cualquier
/// navegador) y el estado para acabar con el código que enseñe la página.
async fn api_login_start(
    State(st): State<Arc<AppState>>,
    body: Option<Json<LoginStartReq>>,
) -> Response {
    let hint = body
        .and_then(|Json(b)| b.email)
        .filter(|e| accounts::valid_target(e));
    let (url, state) = st.accounts.login_start(hint.as_deref());
    Json(serde_json::json!({ "url": url, "state": state })).into_response()
}

#[derive(Deserialize)]
struct LoginFinishReq {
    #[serde(default)]
    state: Option<String>,
    code: String,
}

async fn api_login_finish(
    State(st): State<Arc<AppState>>,
    Json(req): Json<LoginFinishReq>,
) -> Response {
    match st
        .accounts
        .login_finish(req.state.as_deref(), &req.code)
        .await
    {
        Ok(r) => Json(r).into_response(),
        Err(e) => error(StatusCode::BAD_REQUEST, e),
    }
}

async fn api_remove_account(State(st): State<Arc<AppState>>, Path(number): Path<u32>) -> Response {
    match st.accounts.remove(number).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => error(StatusCode::BAD_REQUEST, e),
    }
}

/// Cómo va la conexión con el hub (su error, en el idioma de la petición).
fn hub_status(st: &AppState) -> hub::HubStatus {
    let mut s = st.hub.status.borrow().clone();
    s.error = s.error.as_deref().map(tr);
    s
}

async fn api_hub(State(st): State<Arc<AppState>>) -> Response {
    Json(hub_status(&st)).into_response()
}

#[derive(Deserialize)]
struct HubReq {
    url: Option<String>,
    #[serde(default)]
    address: Option<String>,
}

/// Pone (o quita, con `url: null`) el hub y reconecta.
async fn api_set_hub(State(st): State<Arc<AppState>>, Json(req): Json<HubReq>) -> Response {
    match st.hub.set_url(req.url, req.address) {
        Ok(()) => Json(hub_status(&st)).into_response(),
        Err(e) => error(StatusCode::BAD_REQUEST, e),
    }
}

async fn api_push_test(State(st): State<Arc<AppState>>) -> Response {
    let badge = attention(&st.views.borrow().sessions);
    let failed: Vec<String> = st
        .push_all("nido", "Aviso de prueba", "", badge)
        .await
        .iter()
        .map(|f| tr(f))
        .collect();
    let phones = st
        .phones
        .lock()
        .expect("teléfonos")
        .list()
        .iter()
        .filter(|p| p.handle.is_some())
        .count();
    Json(serde_json::json!({ "phones": phones, "failed": failed })).into_response()
}

/// `POST /api/v1/voice/token`: un token de corta vida para que la web o la app hablen con el
/// modelo de voz directamente (el audio no pasa por nido ni por el hub). Lo da el hub; solo lo
/// pide un dispositivo emparejado, no el token de este equipo, para que cada pedido sea de
/// alguien.
async fn api_voice_token(
    State(st): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
) -> Response {
    if !paired_device(&st, &headers) {
        return error(
            StatusCode::FORBIDDEN,
            "solo un teléfono o una web emparejados pueden llamar",
        );
    }
    match st.hub.voice_token().await {
        Ok(v) => Json(v).into_response(),
        Err(e) => {
            let status = match e {
                hub::VoiceError::NoHub => StatusCode::SERVICE_UNAVAILABLE,
                hub::VoiceError::NotOffered => StatusCode::NOT_IMPLEMENTED,
                hub::VoiceError::Limit => StatusCode::TOO_MANY_REQUESTS,
                hub::VoiceError::Other(_) => StatusCode::BAD_GATEWAY,
            };
            error(status, e.message())
        }
    }
}

/// Si pide un dispositivo emparejado (con su token), no el token de este equipo.
fn paired_device(st: &AppState, headers: &axum::http::HeaderMap) -> bool {
    crate::auth::bearer(headers)
        .and_then(|t| st.phones.lock().expect("teléfonos").id_by_token(t))
        .is_some()
}

/// La memoria de la voz es de quien habla: solo de los dispositivos emparejados.
const MEMORY_DENIED: &str =
    "solo un teléfono o una web emparejados pueden usar la memoria de la voz";
const NO_SUCH_NOTE: &str = "no hay esa nota";
/// Lo más que cabe en una petición de la memoria: un texto de llamada (4000 caracteres, hasta
/// 4 bytes cada uno, o 6 escapado en JSON) y algo más; lo demás, 413 sin leerlo entero.
const MEMORY_BODY: usize = 64 * 1024;
const BAD_CALL_KIND: &str = "kind tiene que ser «resumen» o «bitácora»";

/// El código HTTP de un error de la memoria de la voz.
fn memory_status(e: &crate::voice_memory::MemoryError) -> StatusCode {
    use crate::voice_memory::MemoryError as E;
    match e {
        E::EmptyNote | E::EmptyText | E::BadCallId => StatusCode::BAD_REQUEST,
        E::Full => StatusCode::CONFLICT,
        E::Read(_) | E::Save(_) => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

/// Una operación de la memoria de la voz, que lee y escribe su fichero: fuera del runtime. El
/// error se traduce aquí, en la tarea de la petición (la de `spawn_blocking` no sabe el idioma).
async fn with_memory<T: Send + 'static>(
    st: &Arc<AppState>,
    f: impl FnOnce(&crate::voice_memory::VoiceMemory) -> Result<T, crate::voice_memory::MemoryError>
    + Send
    + 'static,
) -> Result<T, Response> {
    let memory = st.voice_memory.clone();
    match tokio::task::spawn_blocking(move || f(&memory)).await {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => Err(error(memory_status(&e), e.to_string())),
        Err(e) => Err(error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
    }
}

/// `GET /api/v1/voice/memory`: `{notes, calls}` (ver `voice_memory::Snapshot`).
async fn api_voice_memory(
    State(st): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
) -> Response {
    if !paired_device(&st, &headers) {
        return error(StatusCode::FORBIDDEN, MEMORY_DENIED);
    }
    match with_memory(&st, |m| m.get()).await {
        Ok(snapshot) => Json(snapshot).into_response(),
        Err(r) => r,
    }
}

#[derive(Deserialize)]
struct NoteReq {
    text: String,
}

/// `POST /api/v1/voice/memory/notes` `{text}`: `{id}`; con las notas llenas, 409.
async fn api_voice_note_add(
    State(st): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    body: Bytes,
) -> Response {
    if !paired_device(&st, &headers) {
        return error(StatusCode::FORBIDDEN, MEMORY_DENIED);
    }
    let req: NoteReq = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => return bad_body(&e),
    };
    match with_memory(&st, move |m| m.add_note(&req.text)).await {
        Ok(id) => Json(serde_json::json!({ "id": id })).into_response(),
        Err(r) => r,
    }
}

/// `DELETE /api/v1/voice/memory/notes/{id}`: 204, o 404 si no hay esa nota (tampoco si el id no
/// es un número: no hay ninguna así).
async fn api_voice_note_delete(
    State(st): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if !paired_device(&st, &headers) {
        return error(StatusCode::FORBIDDEN, MEMORY_DENIED);
    }
    let Ok(id) = id.parse::<u64>() else {
        return error(StatusCode::NOT_FOUND, NO_SUCH_NOTE);
    };
    match with_memory(&st, move |m| m.delete_note(id)).await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => error(StatusCode::NOT_FOUND, NO_SUCH_NOTE),
        Err(r) => r,
    }
}

#[derive(Deserialize)]
struct CallReq {
    id: String,
    kind: String,
    text: String,
}

/// `POST /api/v1/voice/memory/calls` `{id, kind, text}`: guarda el resumen o la bitácora de una
/// llamada (204).
async fn api_voice_call_put(
    State(st): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    body: Bytes,
) -> Response {
    use crate::voice_memory::CallKind;
    if !paired_device(&st, &headers) {
        return error(StatusCode::FORBIDDEN, MEMORY_DENIED);
    }
    let req: CallReq = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => return bad_body(&e),
    };
    // Como en el fichero: «resumen» o «bitácora» (también sin tilde).
    let Ok(kind) = serde_json::from_value::<CallKind>(serde_json::Value::String(req.kind)) else {
        return error(StatusCode::BAD_REQUEST, BAD_CALL_KIND);
    };
    match with_memory(&st, move |m| m.put_call(&req.id, kind, &req.text)).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(r) => r,
    }
}

#[derive(Deserialize)]
struct SwitchReq {
    account: String,
}

async fn api_switch_account(
    State(st): State<Arc<AppState>>,
    Json(req): Json<SwitchReq>,
) -> Response {
    let target = req.account.trim();
    if !accounts::valid_target(target) {
        return error(StatusCode::BAD_REQUEST, "cuenta no válida");
    }
    match st.accounts.switch(target).await {
        Ok(mut r) => {
            // Las sesiones paradas por el límite de la cuenta anterior siguen con la nueva.
            let resumed = if r.switched {
                resume_limited(&st).await
            } else {
                Vec::new()
            };
            r.message = r.message.as_deref().map(tr);
            r.warnings = r.warnings.iter().map(|w| tr(w)).collect();
            let mut v = serde_json::to_value(&r).unwrap_or_default();
            v["resumed"] = resumed.into();
            Json(v).into_response()
        }
        Err(e) => error(StatusCode::BAD_GATEWAY, e),
    }
}

/// Las sesiones de Claude que van con la cuenta por defecto y se quedaron paradas por el límite
/// (lo dice su transcripción, o el aviso en pantalla): quitar el aviso y decirles que sigan.
/// Devuelve sus nombres.
async fn resume_limited(st: &AppState) -> Vec<String> {
    let sessions = st.views.borrow().sessions.clone();
    let mut resumed = Vec::new();
    for s in sessions {
        let Some(c) = &s.claude else { continue };
        if !s.alive || !c.running || c.account.is_some() {
            continue;
        }
        let link = st
            .registry
            .lock()
            .await
            .get(&s.id)
            .and_then(|x| x.claude.clone());
        let stopped = link
            .and_then(|l| {
                let config = l
                    .config_dir
                    .clone()
                    .unwrap_or_else(|| claude::default_config_dir(&st.paths.home));
                crate::transcript::find(&config, &l.session_id)
            })
            .is_some_and(|p| crate::transcript::stopped_by_limit(&p));
        let hello = Hello {
            role: Role::Viewer,
            cols: 80,
            rows: 24,
        };
        let Ok(stream) = holder_connect(st, &s.id, hello).await else {
            continue;
        };
        let (rd, wr) = crate::ipc::split(stream);
        let mut scr = prompt::Screen::new(rd, wr);
        let _one = st.turn(&s.id).await;
        match prompt::resume_after_limit(&mut scr, stopped).await {
            Ok(true) => resumed.push(s.name.clone()),
            Ok(false) => {}
            Err(e) => eprintln!("nido: {} no se pudo retomar tras el límite: {e}", s.name),
        }
    }
    resumed
}

async fn api_list(State(st): State<Arc<AppState>>) -> Response {
    st.scan().await;
    Json(st.views.borrow().sessions.clone()).into_response()
}

#[derive(Deserialize)]
struct CreateRequest {
    name: String,
    cwd: Option<PathBuf>,
    /// Orden y argumentos; vacía o ausente: Claude.
    #[serde(default)]
    command: Vec<String>,
    #[serde(default = "yes")]
    autostart: bool,
    /// Retomar una conversación que ya existe (importar desde tmux).
    claude_session_id: Option<String>,
    /// Su `CLAUDE_CONFIG_DIR` (perfil de cswap), si no es `~/.claude`.
    config_dir: Option<PathBuf>,
    /// Carpeta lógica (vacía o ausente: ninguna).
    folder: Option<String>,
    /// Arrancarla ya (por defecto) o solo registrarla.
    #[serde(default = "yes")]
    start: bool,
}

fn yes() -> bool {
    true
}

/// Un uuid v4 para `claude --session-id`.
pub(crate) fn uuid_v4() -> String {
    let mut b = [0u8; 16];
    getrandom::fill(&mut b).expect("sin aleatoriedad del sistema");
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &h[0..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..32]
    )
}

/// «~» es la carpeta de inicio de este equipo (cada uno del grupo tiene la suya).
pub(crate) fn expand_home(home: &std::path::Path, cwd: Option<PathBuf>) -> PathBuf {
    match cwd {
        Some(c) if c == std::path::Path::new("~") => home.to_owned(),
        Some(c) => match c.strip_prefix("~") {
            Ok(rest) => home.join(rest),
            Err(_) => c,
        },
        None => home.to_owned(),
    }
}

#[derive(Deserialize)]
struct ConversationsQuery {
    cwd: Option<PathBuf>,
}

/// Las conversaciones de Claude guardadas en una carpeta, para retomar una en una sesión nueva.
/// Cada una dice en qué sesión de nido está ya, si está en alguna.
async fn api_conversations(
    State(st): State<Arc<AppState>>,
    Query(q): Query<ConversationsQuery>,
) -> Response {
    let cwd = expand_home(&st.paths.home, q.cwd);
    let dir = crate::transcript::project_dir(&claude::default_config_dir(&st.paths.home), &cwd);
    let list = tokio::task::spawn_blocking(move || crate::transcript::conversations(&dir, 30))
        .await
        .unwrap_or_default();
    let by_sid: std::collections::HashMap<String, String> = st
        .registry
        .lock()
        .await
        .sessions()
        .iter()
        .filter_map(|s| Some((s.claude.as_ref()?.session_id.clone(), s.name.clone())))
        .collect();
    let out: Vec<serde_json::Value> = list
        .into_iter()
        .map(|c| {
            let mut v = serde_json::to_value(&c).unwrap_or_default();
            v["session"] = by_sid.get(&c.id).cloned().into();
            v
        })
        .collect();
    Json(serde_json::json!({ "cwd": cwd, "conversations": out })).into_response()
}

async fn api_create(State(st): State<Arc<AppState>>, Json(req): Json<CreateRequest>) -> Response {
    let cwd = expand_home(&st.paths.home, req.cwd);
    if !cwd.is_dir() {
        return error(
            StatusCode::BAD_REQUEST,
            format!("no existe la carpeta {}", cwd.display()),
        );
    }
    let (argv, claude) = if let Some(sid) = req.claude_session_id {
        // Una conversación existente: el guardián la retoma (`claude --resume`).
        let link = ClaudeLink {
            session_id: sid,
            config_dir: req.config_dir,
            account: None,
            inside_shell: false,
        };
        (vec!["claude".to_owned()], Some(link))
    } else if req.command.is_empty() {
        let sid = uuid_v4();
        let argv = claude::new_argv(&req.name, &sid);
        let link = ClaudeLink {
            session_id: sid,
            config_dir: None,
            account: None,
            inside_shell: false,
        };
        (argv, Some(link))
    } else {
        (req.command, None)
    };
    let session = Session {
        id: new_id(),
        name: req.name,
        cwd,
        argv,
        env: vec![],
        autostart: req.autostart,
        created: now(),
        claude,
        folder: req
            .folder
            .map(|f| f.trim().to_owned())
            .filter(|f| !f.is_empty()),
        finished: 0,
        stopped: 0,
        seen: 0,
        opened_by: None,
        alerts: vec![],
    };
    let created = match st.registry.lock().await.add(session) {
        Ok(s) => s,
        Err(e) => return registry_error(e),
    };
    if req.start {
        let hold = st.launching(&created.id);
        if let Err(e) = launch::start(&st.paths, st.launcher, &created.id) {
            return error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("no se pudo arrancar: {e}"),
            );
        }
        wait_alive(&st, &created.id).await;
        st.launched(&created.id, hold);
    }
    st.refresh.notify_one();
    (StatusCode::CREATED, Json(created)).into_response()
}

pub(crate) async fn wait_alive(st: &AppState, id: &str) -> bool {
    for _ in 0..60 {
        if launch::is_alive(&st.paths, id) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

pub(crate) async fn find_session(st: &AppState, key: &str) -> Result<Session, Response> {
    st.registry.lock().await.find(key).cloned().ok_or_else(|| {
        error(
            StatusCode::NOT_FOUND,
            format!("no hay ninguna sesión {key:?}"),
        )
    })
}

#[derive(Deserialize)]
struct UpdateRequest {
    name: Option<String>,
    autostart: Option<bool>,
    /// Carpeta lógica; cadena vacía para sacarla de su carpeta.
    folder: Option<String>,
}

async fn api_update(
    State(st): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(req): Json<UpdateRequest>,
) -> Response {
    let s = match find_session(&st, &id).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    let res = st.registry.lock().await.update(&s.id, |x| {
        if let Some(n) = req.name {
            x.name = n;
        }
        if let Some(a) = req.autostart {
            x.autostart = a;
        }
        if let Some(f) = req.folder {
            let f = f.trim().to_owned();
            x.folder = (!f.is_empty()).then_some(f);
        }
    });
    st.refresh.notify_one();
    match res {
        Ok(s) => Json(s).into_response(),
        Err(e) => registry_error(e),
    }
}

#[derive(Deserialize)]
struct RenameFolder {
    from: String,
    /// Vacío: las sesiones se quedan sin carpeta.
    to: String,
}

/// Renombra una carpeta: cambia la de todas sus sesiones.
async fn api_rename_folder(
    State(st): State<Arc<AppState>>,
    Json(req): Json<RenameFolder>,
) -> Response {
    let res = st.registry.lock().await.rename_folder(&req.from, &req.to);
    st.refresh.notify_one();
    match res {
        Ok(n) => Json(serde_json::json!({ "ok": true, "sessions": n })).into_response(),
        Err(e) => registry_error(e),
    }
}

#[derive(Deserialize)]
struct SessionOrder {
    ids: Vec<String>,
}

/// Nuevo orden de las sesiones (las que no vengan, detrás como estaban).
async fn api_order_sessions(
    State(st): State<Arc<AppState>>,
    Json(req): Json<SessionOrder>,
) -> Response {
    let res = st.registry.lock().await.reorder(&req.ids);
    st.refresh.notify_one();
    match res {
        Ok(()) => Json(serde_json::json!({ "ok": true })).into_response(),
        Err(e) => registry_error(e),
    }
}

#[derive(Deserialize)]
struct FolderOrder {
    folders: Vec<String>,
}

async fn api_order_folders(
    State(st): State<Arc<AppState>>,
    Json(req): Json<FolderOrder>,
) -> Response {
    let res = st.registry.lock().await.set_folder_order(req.folders);
    st.refresh.notify_one();
    match res {
        Ok(()) => Json(serde_json::json!({ "ok": true })).into_response(),
        Err(e) => registry_error(e),
    }
}

pub(crate) async fn stop_and_wait(st: &AppState, id: &str) {
    // Lo que le llegue a su `nido mcp` mientras se muere se perdería.
    st.inbox.stopped(id);
    // Bloquea un momento (en Windows espera a que el guardián recoja la orden): fuera del hilo
    // de tokio.
    let (paths, launcher, sid) = (st.paths.clone(), st.launcher, id.to_owned());
    let _ = tokio::task::spawn_blocking(move || launch::stop(&paths, launcher, &sid)).await;
    // Hasta que el proceso termine del todo, no solo hasta que deje el socket.
    for _ in 0..240 {
        if !launch::is_alive(&st.paths, id) && !launch::holder_running(&st.paths, id) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Contesta el menú que Claude tiene en pantalla (sus preguntas, un permiso, el plan…) pulsando
/// las teclas que tocan; ver [`prompt::answer`].
async fn api_answer(
    State(st): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(req): Json<prompt::Request>,
) -> Response {
    if let Err(e) = req.check_expect() {
        return error(StatusCode::BAD_REQUEST, e);
    }
    let s = match find_session(&st, &id).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    let _one = st.turn(&s.id).await;
    let hello = Hello {
        role: Role::Viewer,
        cols: 80,
        rows: 24,
    };
    let Ok(stream) = holder_connect(&st, &s.id, hello).await else {
        return error(StatusCode::CONFLICT, "la sesión no está en marcha");
    };
    let (rd, wr) = crate::ipc::split(stream);
    let mut scr = prompt::Screen::new(rd, wr);
    match prompt::answer(&mut scr, &req).await {
        Ok(()) => Json(serde_json::json!({ "ok": true })).into_response(),
        Err(e) => error(StatusCode::CONFLICT, e),
    }
}

/// El cuerpo de una petición, como JSON: si no lo es, 400 con `{"error"}` (como el resto de
/// errores de la API).
fn json_body(body: &Bytes) -> Result<serde_json::Value, serde_json::Error> {
    serde_json::from_slice(body)
}

fn bad_body(e: &serde_json::Error) -> Response {
    error(
        StatusCode::BAD_REQUEST,
        format!("el cuerpo no es JSON válido: {e}"),
    )
}

/// Una secuencia de teclas en la sesión `id`, en una tarea aparte con el turno de la sesión: si
/// se corta la petición, la secuencia sigue hasta el final (a medias dejaría `/model` en la caja
/// o un menú abierto).
async fn typing<T: Send + 'static>(
    st: &Arc<AppState>,
    id: &str,
    steps: impl FnOnce(
        Arc<AppState>,
        HolderScreen,
    ) -> futures_util::future::BoxFuture<'static, Result<T, String>>
    + Send
    + 'static,
) -> Result<T, Response> {
    let (st, id) = (st.clone(), id.to_owned());
    let task = tokio::spawn(async move {
        let _one = st.turn(&id).await;
        let Some(scr) = session_screen(&st, &id).await else {
            return Err("la sesión no está en marcha".to_owned());
        };
        steps(st.clone(), scr).await
    });
    match task.await {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => Err(error(StatusCode::CONFLICT, e)),
        Err(e) => Err(error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
    }
}

/// Cambia el modo de permisos de Claude (`{"mode": "plan"}`) con shift+tab; ver
/// [`prompt::set_mode`].
async fn api_mode(
    State(st): State<Arc<AppState>>,
    Path(id): Path<String>,
    body: Bytes,
) -> Response {
    let body = match json_body(&body) {
        Ok(b) => b,
        Err(e) => return bad_body(&e),
    };
    let Some(want) = body.get("mode").and_then(serde_json::Value::as_str) else {
        return error(StatusCode::BAD_REQUEST, "falta el modo");
    };
    let Some(mode) = prompt::Mode::from_name(want) else {
        return error(
            StatusCode::BAD_REQUEST,
            format!("modo desconocido: «{want}»"),
        );
    };
    let s = match find_session(&st, &id).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    let done = typing(&st, &s.id, move |_, mut scr| {
        Box::pin(async move { prompt::set_mode(&mut scr, mode).await })
    })
    .await;
    match done {
        Ok(m) => Json(serde_json::json!({ "ok": true, "mode": m.name() })).into_response(),
        Err(r) => r,
    }
}

/// Cambia el modelo de Claude solo para esta sesión (`{"model": "Opus 5.5"}`) con su menú de
/// `/model`; ver [`prompt::set_model`].
async fn api_model(
    State(st): State<Arc<AppState>>,
    Path(id): Path<String>,
    body: Bytes,
) -> Response {
    let body = match json_body(&body) {
        Ok(b) => b,
        Err(e) => return bad_body(&e),
    };
    let Some(want) = body
        .get("model")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .map(str::to_owned)
    else {
        return error(StatusCode::BAD_REQUEST, "falta el modelo");
    };
    let s = match find_session(&st, &id).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    let Some(link) = s.claude.clone() else {
        return error(StatusCode::CONFLICT, "en esta sesión no hay ningún Claude");
    };
    let busy = st.views.borrow().sessions.iter().any(|v| {
        v.id == s.id
            && v.claude
                .as_ref()
                .is_some_and(|c| c.status.as_deref() == Some("busy"))
    });
    if busy {
        return error(
            StatusCode::CONFLICT,
            "Claude está trabajando: espera a que termine",
        );
    }
    let sid = s.id.clone();
    // Todo, también apuntar lo que se vio y lo elegido, en la tarea: si se corta la petición,
    // el modelo cambia igual y queda apuntado.
    let done = typing(&st, &s.id, move |st, mut scr| {
        Box::pin(async move {
            // Lo que mide la transcripción ahora: lo que Claude conteste después dirá su modelo.
            let at = {
                let (st, link) = (st.clone(), link.clone());
                tokio::task::spawn_blocking(move || {
                    transcript_of(&st, &link)
                        .and_then(|p| std::fs::metadata(p).ok())
                        .map_or(0, |m| m.len())
                })
                .await
                .unwrap_or(0)
            };
            let mut seen = None;
            let result = prompt::set_model(&mut scr, &want, &mut seen).await;
            if let Some(models) = seen {
                st.remember_models(models);
            }
            let model = result?;
            st.model_chosen.lock().expect("modelo elegido").insert(
                sid,
                ModelChosen {
                    session_id: link.session_id,
                    model: model.clone(),
                    at,
                },
            );
            st.refresh.notify_one();
            Ok(model)
        })
    })
    .await;
    match done {
        Ok(model) => Json(serde_json::json!({ "ok": true, "model": model })).into_response(),
        Err(r) => r,
    }
}

/// Manda ya a Claude lo que tiene en cola (ctrl+x ctrl+s); ver [`prompt::send_queue`].
async fn api_queue_send(State(st): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    let s = match find_session(&st, &id).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    let done = typing(&st, &s.id, move |_, mut scr| {
        Box::pin(async move { prompt::send_queue(&mut scr).await })
    })
    .await;
    match done {
        Ok(()) => Json(serde_json::json!({ "ok": true })).into_response(),
        Err(r) => r,
    }
}

/// Saca de la cola de Claude lo que tiene (↑) y lo borra de su caja, para editarlo en el chat:
/// devuelve el texto, tal como lo apuntó la transcripción. Ver [`prompt::edit_queue`].
async fn api_queue_edit(State(st): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    let s = match find_session(&st, &id).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    let Some(link) = s.claude.clone() else {
        return error(StatusCode::CONFLICT, "en esta sesión no hay ningún Claude");
    };
    let done = typing(&st, &s.id, move |st, mut scr| {
        Box::pin(async move {
            // La cola de ahora y hasta dónde está leída la transcripción: lo que saque ↑ se
            // apunta después.
            let (path, queued, from) = tokio::task::spawn_blocking(move || {
                let path = transcript_of(&st, &link);
                let (queued, from) = path.as_deref().map(|p| st.queue_of(p)).unwrap_or_default();
                (path, queued, from)
            })
            .await
            .map_err(|e| e.to_string())?;
            let popped = move || {
                path.as_deref()
                    .map(|p| crate::transcript::popped_since(p, from))
                    .unwrap_or_default()
            };
            prompt::edit_queue(&mut scr, &queued, popped).await
        })
    })
    .await;
    match done {
        Ok(text) => Json(serde_json::json!({ "ok": true, "text": text })).into_response(),
        Err(r) => r,
    }
}

/// La pantalla de la sesión, como visor de su guardián (para leerla y teclear); `None` si no
/// está en marcha.
async fn session_screen(st: &AppState, id: &str) -> Option<HolderScreen> {
    if !launch::is_alive(&st.paths, id) {
        return None;
    }
    let hello = Hello {
        role: Role::Viewer,
        cols: 80,
        rows: 24,
    };
    let stream = holder_connect(st, id, hello).await.ok()?;
    let (rd, wr) = crate::ipc::split(stream);
    Some(prompt::Screen::new(rd, wr))
}

/// El usuario ha visto la sesión: deja de estar «sin leer».
async fn api_seen(State(st): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    let s = match find_session(&st, &id).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    let res = st
        .registry
        .lock()
        .await
        .update(&s.id, |x| x.seen = now_ms());
    st.refresh.notify_one();
    match res {
        Ok(_) => Json(serde_json::json!({ "ok": true })).into_response(),
        Err(e) => registry_error(e),
    }
}

#[derive(Deserialize)]
struct AlertRequest {
    text: String,
}

/// La sesión avisa al usuario (la herramienta `alert` del MCP de nido).
async fn api_alert(
    State(st): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(req): Json<AlertRequest>,
) -> Response {
    let Some(text) = alert_text(&req.text) else {
        return error(StatusCode::BAD_REQUEST, "la alerta no tiene texto");
    };
    match st.session_alert(&id, &text).await {
        Ok(()) => Json(serde_json::json!({ "ok": true })).into_response(),
        Err(e) => registry_error(e),
    }
}

/// Arrancarla o pararla a mano decide si vuelve a arrancar al reiniciar el equipo o nido: se
/// queda como la dejaste.
pub(crate) async fn remember_running(st: &AppState, id: &str, running: bool) {
    let mut reg = st.registry.lock().await;
    if reg.get(id).is_some_and(|s| s.autostart != running)
        && let Err(e) = reg.update(id, |s| s.autostart = running)
    {
        eprintln!("nido: no se pudo guardar el autoarranque de {id}: {e}");
    }
}

/// Arranca la sesión si no está viva (retoma su conversación) y espera a que lo esté (como
/// mucho lo de `wait_alive`). `Ok(true)`: ya está en marcha.
pub(crate) async fn start_session(st: &AppState, id: &str) -> std::io::Result<bool> {
    let mut alive = launch::is_alive(&st.paths, id);
    if !alive {
        let hold = st.launching(id);
        launch::start(&st.paths, st.launcher, id)?;
        alive = wait_alive(st, id).await;
        st.launched(id, hold);
    }
    remember_running(st, id, true).await;
    st.refresh.notify_one();
    Ok(alive)
}

async fn api_start(State(st): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    let s = match find_session(&st, &id).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    if let Err(e) = start_session(&st, &s.id).await {
        return error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
    }
    Json(serde_json::json!({ "ok": true })).into_response()
}

/// Para la sesión y apunta que se queda parada (no vuelve a arrancar con el equipo).
pub(crate) async fn stop_session(st: &AppState, id: &str) {
    stop_and_wait(st, id).await;
    remember_running(st, id, false).await;
    st.refresh.notify_one();
}

async fn api_stop(State(st): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    let s = match find_session(&st, &id).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    stop_session(&st, &s.id).await;
    Json(serde_json::json!({ "ok": true })).into_response()
}

async fn api_restart(State(st): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    let s = match find_session(&st, &id).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    // Antes de pararla, que el registro tenga la conversación actual para retomarla.
    st.scan().await;
    // Retenida ya antes de pararla: el `nido mcp` de antes aún recoge.
    let hold = st.launching(&s.id);
    stop_and_wait(&st, &s.id).await;
    if let Err(e) = launch::start(&st.paths, st.launcher, &s.id) {
        return error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
    }
    wait_alive(&st, &s.id).await;
    st.launched(&s.id, hold);
    remember_running(&st, &s.id, true).await;
    st.refresh.notify_one();
    Json(serde_json::json!({ "ok": true })).into_response()
}

async fn api_delete(State(st): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    let s = match find_session(&st, &id).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    match delete_session(&st, &s.id).await {
        Ok(()) => Json(serde_json::json!({ "ok": true })).into_response(),
        Err(e) => registry_error(e),
    }
}

/// La para, la quita del registro y borra su estado (historial, registro del guardián).
pub(crate) async fn delete_session(st: &AppState, id: &str) -> Result<(), RegistryError> {
    stop_and_wait(st, id).await;
    let res = st.registry.lock().await.remove(id);
    st.mcp_cursors.forget(id);
    st.inbox.forget(id);
    st.asks.cancel_session(id);
    // Sus ficheros, sus secretos y sus envíos: se cerró. Borrar puede tardar: fuera del hilo.
    let (dir, secrets, outbox, plans) = (
        st.paths.session_state(id),
        st.secrets.clone(),
        st.outbox.clone(),
        st.plans.clone(),
    );
    let id_ = id.to_owned();
    let _ = tokio::task::spawn_blocking(move || {
        // Antes de borrar la carpeta: espera al plan que se esté guardando, y los cambios que
        // lleguen después fallan en vez de volver a crearla.
        plans.forget(&id_);
        let _ = std::fs::remove_dir_all(dir);
        secrets.forget_session(&id_);
        outbox.forget(&id_);
    })
    .await;
    st.refresh.notify_one();
    res.map(|_| ())
}

/// `GET /api/v1/sessions/{id}/plan`: el plan de la sesión, con sus horas (ms desde 1970); 404 si
/// no tiene.
async fn api_plan(State(st): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    let s = match find_session(&st, &id).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    // La primera vez se lee del disco: fuera del hilo de tokio.
    let plan = tokio::task::spawn_blocking(move || st.plans.get(&s.id)).await;
    match plan {
        Ok(Some(plan)) => Json(plan).into_response(),
        Ok(None) => error(StatusCode::NOT_FOUND, "esta sesión no tiene plan"),
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

/// Ha cambiado el plan de una sesión: la lista, otra vez (con el `rev` nuevo de su resumen).
pub fn plan_changed(st: &AppState) {
    st.refresh.notify_one();
}

#[derive(Deserialize)]
struct HistoryQuery {
    #[serde(default)]
    all: bool,
    /// Solo la pantalla actual (la franja en vivo de la vista chat).
    #[serde(default)]
    screen: bool,
}

pub(crate) async fn holder_connect(
    st: &AppState,
    id: &str,
    hello: Hello,
) -> std::io::Result<crate::ipc::Stream> {
    let mut stream = crate::ipc::connect(&st.paths.socket(id)).await?;
    proto::write_frame(&mut stream, &Frame::Hello(hello)).await?;
    Ok(stream)
}

/// Como visor (80×24), al guardián que atiende en `socket`.
async fn viewer_at(socket: &std::path::Path) -> std::io::Result<HolderScreen> {
    let mut stream = crate::ipc::connect(socket).await?;
    let hello = Hello {
        role: Role::Viewer,
        cols: 80,
        rows: 24,
    };
    proto::write_frame(&mut stream, &Frame::Hello(hello)).await?;
    let (rd, wr) = crate::ipc::split(stream);
    Ok(prompt::Screen::new(rd, wr))
}

async fn api_history(
    State(st): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(q): Query<HistoryQuery>,
    headers: axum::http::HeaderMap,
) -> Response {
    let s = match find_session(&st, &id).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    let text = if launch::is_alive(&st.paths, &s.id) {
        let hello = Hello {
            role: Role::Viewer,
            cols: 80,
            rows: 24,
        };
        let fetch = async {
            let mut stream = holder_connect(&st, &s.id, hello).await?;
            proto::write_frame(
                &mut stream,
                &Frame::HistoryRequest(HistoryRequest {
                    all: q.all,
                    screen: q.screen,
                    launch: None,
                }),
            )
            .await?;
            loop {
                match proto::read_frame(&mut stream).await? {
                    Some(Frame::History(b)) => return Ok(String::from_utf8_lossy(&b).into_owned()),
                    Some(_) => continue,
                    None => return Err(std::io::Error::other("el guardián cerró")),
                }
            }
        };
        match tokio::time::timeout(Duration::from_secs(5), fetch).await {
            Ok(Ok(t)) => t,
            Ok(Err(e)) => return error(StatusCode::BAD_GATEWAY, e.to_string()),
            Err(_) => return error(StatusCode::GATEWAY_TIMEOUT, "el guardián no respondió"),
        }
    } else {
        // Parada: lo que quedó en disco (y pantalla, ninguna).
        match crate::history::History::open_read_only(&st.paths.history(&s.id))
            .ok()
            .filter(|_| !q.screen)
        {
            Some(h) => h.text(q.all),
            None => String::new(),
        }
    };
    let mut hasher = std::hash::DefaultHasher::new();
    std::hash::Hash::hash(&text, &mut hasher);
    let etag = format!("\"{:016x}\"", std::hash::Hasher::finish(&hasher));
    // Se pide cada segundo y medio y casi siempre es igual: un 304 no lleva cuerpo.
    let cache = [
        (header::CACHE_CONTROL, "no-cache".to_owned()),
        (header::ETAG, etag.clone()),
    ];
    let unchanged = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v == etag);
    if unchanged {
        return (StatusCode::NOT_MODIFIED, cache).into_response();
    }
    (
        cache,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        text,
    )
        .into_response()
}

#[derive(Deserialize)]
struct ChatQuery {
    /// Dónde se quedó la consulta anterior (0: desde el principio).
    #[serde(default)]
    offset: u64,
    /// Al empezar, solo los últimos N mensajes.
    limit: Option<usize>,
    /// En vez de lo anterior, los mensajes alrededor de este punto (el `at` de un mensaje).
    around: Option<u64>,
    /// Con `around`: cuántos mensajes antes y desde ese punto.
    before: Option<usize>,
    after: Option<usize>,
    /// Con `around`: la conversación de ese `at`; si ya no es la actual, 409.
    sid: Option<String>,
}

/// Con `around`, cuántos mensajes a cada lado si no se dice, y como mucho.
const AROUND_DEFAULT: usize = 30;
const AROUND_MAX: usize = 1000;

/// La transcripción de la conversación de Claude, si ya la ha empezado a escribir.
pub(crate) fn transcript_of(st: &AppState, link: &ClaudeLink) -> Option<PathBuf> {
    let config = link
        .config_dir
        .clone()
        .unwrap_or_else(|| claude::default_config_dir(&st.paths.home));
    crate::transcript::find(&config, &link.session_id)
}

/// La conversación de Claude de la sesión, leída de su transcripción, por trozos; o, con
/// `around`, la parte alrededor de un mensaje.
async fn api_chat(
    State(st): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(q): Query<ChatQuery>,
) -> Response {
    let s = match find_session(&st, &id).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    let Some(link) = s.claude else {
        return error(StatusCode::NOT_FOUND, "en esta sesión no hay ningún Claude");
    };
    if let Some(around) = q.around {
        if q.sid.as_ref().is_some_and(|sid| *sid != link.session_id) {
            return error(
                StatusCode::CONFLICT,
                "esa conversación ya no es la actual de la sesión",
            );
        }
        let before = q.before.unwrap_or(AROUND_DEFAULT).min(AROUND_MAX);
        let after = q.after.unwrap_or(AROUND_DEFAULT).min(AROUND_MAX);
        let st2 = st.clone();
        let read = tokio::task::spawn_blocking(move || match transcript_of(&st2, &link) {
            Some(path) => crate::transcript::read_around(&path, around, before, after)
                .map(|w| (link.session_id, w)),
            None => Ok((link.session_id, crate::transcript::Window::default())),
        })
        .await;
        return match read {
            Ok(Ok((sid, w))) => {
                let more_before = w.more_before;
                let mut v = serde_json::to_value(&w).unwrap_or_default();
                localize_chunk(&mut v);
                v["session_id"] = sid.into();
                v["truncated"] = more_before.into();
                v["reset"] = false.into();
                Json(v).into_response()
            }
            Ok(Err(e)) => error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
            Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
        };
    }
    let Some(path) = transcript_of(&st, &link) else {
        // Un Claude recién creado aún no ha escrito nada.
        let empty = serde_json::json!({ "session_id": link.session_id, "offset": 0, "items": [], "results": [], "truncated": false });
        return Json(empty).into_response();
    };
    let limit = Some(q.limit.unwrap_or(300));
    match tokio::task::spawn_blocking(move || crate::transcript::read_chunk(&path, q.offset, limit))
        .await
    {
        Ok(Ok(chunk)) => {
            let mut v = serde_json::to_value(&chunk).unwrap_or_default();
            localize_chunk(&mut v);
            v["session_id"] = serde_json::Value::String(link.session_id);
            Json(v).into_response()
        }
        Ok(Err(e)) => error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

#[derive(Deserialize)]
struct SearchQuery {
    #[serde(default)]
    q: String,
    limit: Option<usize>,
}

/// Busca en las conversaciones de las sesiones de este equipo: las sesiones cuyo nombre,
/// carpeta o directorio casan, y los mensajes (tuyos y de Claude) que tienen todas las
/// palabras, lo más reciente primero.
async fn api_search(State(st): State<Arc<AppState>>, Query(q): Query<SearchQuery>) -> Response {
    let Some(terms) = crate::search::terms(&q.q) else {
        return error(
            StatusCode::BAD_REQUEST,
            "hay que buscar al menos 2 caracteres",
        );
    };
    let limit = q
        .limit
        .unwrap_or(crate::search::DEFAULT_LIMIT)
        .clamp(1, crate::search::MAX_LIMIT);
    let sessions: Vec<String> = st
        .views
        .borrow()
        .sessions
        .iter()
        .filter(|s| {
            let cwd = s.cwd.to_string_lossy();
            let fields = [s.name.as_str(), s.folder.as_deref().unwrap_or(""), &cwd];
            crate::search::matches_any(&fields, &terms)
        })
        .map(|s| s.id.clone())
        .collect();
    let targets: Vec<crate::search::Target> = st
        .registry
        .lock()
        .await
        .sessions()
        .iter()
        .filter_map(|s| {
            let link = s.claude.as_ref()?;
            Some(crate::search::Target {
                id: s.id.clone(),
                session_id: link.session_id.clone(),
                config_dir: link
                    .config_dir
                    .clone()
                    .unwrap_or_else(|| claude::default_config_dir(&st.paths.home)),
            })
        })
        .collect();
    let one = st.searching.clone().lock_owned().await;
    let index = st.search.clone();
    let search = move || {
        let found = index.search(targets, &terms, limit);
        drop(one);
        found
    };
    match tokio::task::spawn_blocking(search).await {
        Ok((messages, truncated)) => Json(serde_json::json!({
            "sessions": sessions,
            "messages": messages,
            "truncated": truncated,
        }))
        .into_response(),
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

/// El canal cifrado con un teléfono por la LAN o la VPN (sin hub). No pide token: la
/// identificación es la de Noise (ver `auth::guard`).
async fn ws_e2e(ws: WebSocketUpgrade, State(st): State<Arc<AppState>>) -> Response {
    ws.max_message_size(e2e::MAX_MSG + 1)
        .on_upgrade(move |socket| e2e_socket(socket, st))
}

async fn e2e_socket(socket: WebSocket, st: Arc<AppState>) {
    use futures_util::{SinkExt, StreamExt};
    let (mut ws_tx, mut ws_rx) = socket.split();
    let (in_tx, in_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(32);
    let (out_tx, mut out_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(32);
    let reader = tokio::spawn(async move {
        while let Some(Ok(m)) = ws_rx.next().await {
            match m {
                Message::Binary(b) => {
                    if in_tx.send(b.to_vec()).await.is_err() {
                        break;
                    }
                }
                Message::Close(_) => break,
                _ => {}
            }
        }
    });
    let writer = tokio::spawn(async move {
        while let Some(m) = out_rx.recv().await {
            if ws_tx.send(Message::Binary(m.into())).await.is_err() {
                break;
            }
        }
        let _ = ws_tx.close().await;
    });
    e2e::serve(st, in_rx, out_tx).await;
    reader.abort();
    let _ = writer.await;
}

/// Un QR nuevo para emparejar un teléfono (el anterior deja de valer).
#[derive(Deserialize, Default)]
struct PairReq {
    /// Un código reutilizable que vale estos días (para demos), en vez del QR de una vez.
    #[serde(default)]
    days: Option<u64>,
}

async fn api_pair(State(st): State<Arc<AppState>>, body: Option<Json<PairReq>>) -> Response {
    let direct = st.direct.get().cloned().unwrap_or_default();
    let hub = st.hub.pair_info();
    let hub_ref = hub.as_ref().map(|(u, id)| (u.as_str(), id.as_str()));
    let days = body.and_then(|Json(b)| b.days);
    let offer = match days {
        Some(d) => match st.pairing.offer_standing(&st.identity, &direct, hub_ref, d) {
            Ok(o) => o,
            Err(e) => {
                return error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("no se pudo guardar el código: {e}"),
                );
            }
        },
        None => st.pairing.offer(&st.identity, &direct, hub_ref),
    };
    let link = hub.as_ref().map(|(u, _)| pair_link(u, &offer.uri));
    let mut r = serde_json::json!({
        "uri": offer.uri,
        "expires": offer.expires,
        "reusable": days.is_some(),
        "direct": direct,
        "hub": hub.map(|(u, _)| u),
    });
    // Solo con hub: sin él no hay web que abra el enlace.
    if let Some(link) = link {
        r["link"] = link.into();
    }
    Json(r).into_response()
}

/// El enlace del QR de `nido pair` con hub: la web del hub con el código en el `#` (que el
/// navegador no manda al servidor). Lo abre la cámara del móvil y lo leen la web y la app.
fn pair_link(hub: &str, uri: &str) -> String {
    format!(
        "{}/app#nido={}",
        hub.trim_end_matches('/'),
        encode_uri_component(uri)
    )
}

/// Como `encodeURIComponent` de JS: todo menos `A-Za-z0-9-_.!~*'()` va como `%XX` (en UTF-8).
fn encode_uri_component(s: &str) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&b) {
            out.push(b as char);
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

/// Retira el código reutilizable.
async fn api_pair_stop(State(st): State<Arc<AppState>>) -> Response {
    match st.pairing.stop_standing() {
        Ok(had) => Json(serde_json::json!({ "stopped": had })).into_response(),
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

async fn api_phones(State(st): State<Arc<AppState>>) -> Response {
    Json(st.phones.lock().expect("teléfonos").views()).into_response()
}

/// Los equipos del grupo (este incluido).
async fn api_group(State(st): State<Arc<AppState>>) -> Response {
    Json(serde_json::json!({ "members": st.group.members(), "connected": st.hub.status.borrow().connected }))
        .into_response()
}

async fn api_group_invite(State(st): State<Arc<AppState>>) -> Response {
    let (Some(hub), Some(id)) = (st.hub.url(), st.group.self_id()) else {
        return error(
            StatusCode::CONFLICT,
            "para formar un grupo, este equipo tiene que estar conectado al hub (nido hub set …)",
        );
    };
    let (uri, expires) = st.group.invite(&hub, &id);
    Json(serde_json::json!({ "uri": uri, "expires": expires })).into_response()
}

#[derive(Deserialize)]
struct JoinReq {
    code: String,
}

async fn api_group_join(State(st): State<Arc<AppState>>, Json(req): Json<JoinReq>) -> Response {
    match st.join_group(&req.code).await {
        Ok(members) => Json(serde_json::json!({ "members": members })).into_response(),
        Err(e) => error(StatusCode::BAD_REQUEST, e),
    }
}

async fn api_group_remove(State(st): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    if st.group.self_id().as_deref() == Some(id.as_str()) {
        return error(
            StatusCode::BAD_REQUEST,
            "para sacar este equipo, hazlo desde otro del grupo",
        );
    }
    if st.group.remove_member(&id) {
        st.group_dirty.notify_one();
        StatusCode::NO_CONTENT.into_response()
    } else {
        error(StatusCode::NOT_FOUND, "ese equipo no está en el grupo")
    }
}

/// Quita un dispositivo de este equipo y de todo el grupo (por su clave).
fn remove_phone(st: &AppState, id: &str) -> Response {
    let key = st
        .phones
        .lock()
        .expect("teléfonos")
        .list()
        .iter()
        .find(|p| p.id == id)
        .map(|p| p.key.clone());
    if let Some(k) = &key {
        st.group.remove_device(k);
        st.group_dirty.notify_one();
    }
    match st.phones.lock().expect("teléfonos").remove(id) {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => error(StatusCode::NOT_FOUND, "ese teléfono no está emparejado"),
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

async fn api_remove_phone(State(st): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    // Quitarlo aquí es quitarlo en todo el grupo.
    remove_phone(&st, &id)
}

/// `DELETE /api/v1/phones/me`: el dispositivo que llama (por su token) se da de baja: «Cerrar
/// sesión» en la web. Con el token principal no hay dispositivo que quitar.
async fn api_remove_me(
    State(st): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
) -> Response {
    let id = crate::auth::bearer(&headers)
        .and_then(|t| st.phones.lock().expect("teléfonos").id_by_token(t));
    match id {
        Some(id) => remove_phone(&st, &id),
        None => error(
            StatusCode::BAD_REQUEST,
            "solo un teléfono o una web emparejados pueden cerrar su sesión",
        ),
    }
}

async fn ws_events(ws: WebSocketUpgrade, State(st): State<Arc<AppState>>) -> Response {
    ws.on_upgrade(move |socket| events(socket, st))
}

async fn events(mut socket: WebSocket, st: Arc<AppState>) {
    let mut rx = st.views.subscribe();
    st.scan().await;
    loop {
        let json = serde_json::to_string(&*rx.borrow_and_update()).unwrap_or_default();
        if socket.send(Message::Text(json.into())).await.is_err() {
            return;
        }
        tokio::select! {
            r = rx.changed() => if r.is_err() { return },
            msg = socket.recv() => match msg {
                Some(Ok(Message::Close(_)) | Err(_)) | None => return,
                Some(Ok(_)) => {}
            },
        }
    }
}

#[derive(Deserialize)]
struct TermQuery {
    #[serde(default = "driver")]
    role: Role,
    cols: Option<u16>,
    rows: Option<u16>,
}

fn driver() -> Role {
    Role::Driver
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum Control {
    Resize { cols: u16, rows: u16 },
}

async fn ws_term(
    ws: WebSocketUpgrade,
    State(st): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(q): Query<TermQuery>,
) -> Response {
    let s = match find_session(&st, &id).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    // El WebSocket sigue en otra tarea: se lleva el idioma de la petición.
    let lang = i18n::lang();
    ws.on_upgrade(move |socket| i18n::scope(lang, term(socket, st, s.id, q)))
}

/// Puente entre el navegador y el guardián: bytes en binario; control en JSON.
async fn term(mut socket: WebSocket, st: Arc<AppState>, id: String, q: TermQuery) {
    let hello = Hello {
        role: q.role,
        cols: q.cols.unwrap_or(80),
        rows: q.rows.unwrap_or(24),
    };
    let stream = match holder_connect(&st, &id, hello).await {
        Ok(s) => s,
        Err(_) => {
            let frame = axum::extract::ws::CloseFrame {
                code: 4004,
                reason: tr("la sesión no está en marcha").into(),
            };
            let _ = socket.send(Message::Close(Some(frame))).await;
            return;
        }
    };
    let (mut rd, mut wr) = crate::ipc::split(stream);
    loop {
        tokio::select! {
            frame = proto::read_frame(&mut rd) => match frame {
                Ok(Some(Frame::Output(b) | Frame::Snapshot(b))) => {
                    if socket.send(Message::Binary(b.into())).await.is_err() {
                        break;
                    }
                }
                Ok(Some(Frame::Exit(e))) => {
                    let msg = serde_json::json!({ "type": "exit", "code": e.code }).to_string();
                    let _ = socket.send(Message::Text(msg.into())).await;
                    st.refresh.notify_one();
                    break;
                }
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => break,
            },
            msg = socket.recv() => match msg {
                Some(Ok(Message::Binary(data))) => {
                    if proto::write_frame(&mut wr, &Frame::Input(data.to_vec())).await.is_err() {
                        break;
                    }
                }
                Some(Ok(Message::Text(text))) => {
                    if let Ok(Control::Resize { cols, rows }) = serde_json::from_str(text.as_str()) {
                        let _ = proto::write_frame(&mut wr, &Frame::Resize(Size { cols, rows })).await;
                    }
                }
                Some(Ok(Message::Close(_)) | Err(_)) | None => break,
                Some(Ok(_)) => {}
            },
        }
    }
    let _ = socket.send(Message::Close(None)).await;
}

#[derive(Deserialize)]
struct LiveQuery {
    /// La conversación que tiene en caché el cliente; si ya no es la actual, se empieza de cero.
    sid: Option<String>,
    /// Hasta dónde tiene leída la transcripción.
    #[serde(default)]
    offset: u64,
    /// Al empezar de cero, solo los últimos N mensajes.
    limit: Option<usize>,
}

/// Los avisos que nido pone en la conversación (p. ej. «Claude resumió la conversación…»), en el
/// idioma de la petición. Lo demás es de Claude o tuyo: tal cual.
fn localize_chunk(v: &mut serde_json::Value) {
    for item in v["items"].as_array_mut().into_iter().flatten() {
        if item["kind"] == "notice"
            && let Some(t) = item["text"].as_str()
        {
            item["text"] = tr(t).into();
        }
    }
}

/// El plan que Claude pide aprobar: el fichero que nombra el menú (`~/.claude/plans/<nombre>.md`).
/// Solo se lee si es un `.md` dentro de una carpeta `plans`.
fn plan_text(home: &std::path::Path, m: &prompt::Menu) -> Option<String> {
    if m.kind != prompt::Kind::Plan {
        return None;
    }
    let hint = m.hint.as_deref()?;
    let token = hint
        .split(|c: char| c.is_whitespace() || c == '·')
        .find(|t| t.contains("/plans/") && t.ends_with(".md"))?;
    let path = match token.strip_prefix("~/") {
        Some(rest) => home.join(rest),
        None => PathBuf::from(token),
    };
    if !path.is_absolute()
        || path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
        || path.parent()?.file_name()? != "plans"
    {
        return None;
    }
    let text = std::fs::read_to_string(&path).ok()?;
    Some(text.chars().take(30_000).collect())
}

async fn ws_live(
    ws: WebSocketUpgrade,
    State(st): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(q): Query<LiveQuery>,
) -> Response {
    let s = match find_session(&st, &id).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    let lang = i18n::lang();
    ws.on_upgrade(move |socket| i18n::scope(lang, live(socket, st, s.id, q)))
}

/// Lo que la vista chat necesita, en cuanto cambia: los mensajes nuevos de la transcripción de
/// Claude y la pantalla actual (para la franja en vivo). Sustituye a preguntar cada 1,5 s.
async fn live(mut socket: WebSocket, st: Arc<AppState>, id: String, q: LiveQuery) {
    let limit = q.limit.unwrap_or(300);
    let mut sid = q.sid.clone();
    let mut offset = q.offset;
    let mut path: Option<PathBuf> = None;
    let mut last_len: Option<u64> = None;
    let mut first = true;

    let mut holder: Option<(crate::ipc::ReadHalf, crate::ipc::WriteHalf)> = None;
    let mut screen_due: Option<tokio::time::Instant> = Some(tokio::time::Instant::now());
    let mut screen_pending = false;
    let mut last_screen: Option<String> = None;
    // Lo que había en cola en el último `screen` mandado.
    let mut last_queued: Vec<String> = Vec::new();
    // La última AskUserQuestion de la transcripción: las vistas previas de sus opciones. Si se
    // sigue desde más allá de ella (`offset`), se busca una vez al ver una pregunta (`ask_looked`).
    let mut last_ask: Option<serde_json::Value> = None;
    let mut ask_looked = false;

    let mut tick = tokio::time::interval(Duration::from_millis(50));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut n: u64 = 0;

    loop {
        tokio::select! {
            _ = tick.tick() => {
                n += 1;
                // Cada 250 ms: ¿ha crecido la transcripción (o cambió la conversación)?
                if first || n.is_multiple_of(5) {
                    let link = st.registry.lock().await.get(&id).and_then(|s| s.claude.clone());
                    if let Some(link) = link {
                        let mut reset = false;
                        if sid.as_deref() != Some(link.session_id.as_str()) {
                            // Otra conversación (/clear) o el cliente no tenía ninguna: de cero.
                            reset = sid.is_some();
                            sid = Some(link.session_id.clone());
                            offset = 0;
                            path = None;
                            last_len = None;
                        }
                        if path.is_none() {
                            let config = link
                                .config_dir
                                .clone()
                                .unwrap_or_else(|| claude::default_config_dir(&st.paths.home));
                            path = crate::transcript::find(&config, &link.session_id);
                        }
                        let len = path.as_ref().and_then(|p| std::fs::metadata(p).ok()).map(|m| m.len());
                        // Con la cola en pantalla, la transcripción puede apuntar lo encolado
                        // después de pintarlo: si cambia lo que hay en cola, se manda otra vez.
                        if len != last_len
                            && let Some(p) = path.clone()
                            && last_screen.as_deref().is_some_and(prompt::queue_shown)
                        {
                            let st2 = st.clone();
                            let queued = tokio::task::spawn_blocking(move || st2.queue_of(&p).0).await.unwrap_or_default();
                            // Como se mandó: los que se ven en la pantalla.
                            let queued = prompt::queued_on_screen(last_screen.as_deref().unwrap_or_default(), queued);
                            if queued != last_queued {
                                last_screen = None;
                                screen_due.get_or_insert_with(tokio::time::Instant::now);
                            }
                        }
                        if first || reset || len != last_len {
                            let chunk = match &path {
                                Some(p) => {
                                    let p = p.clone();
                                    let o = offset;
                                    tokio::task::spawn_blocking(move || crate::transcript::read_chunk(&p, o, Some(limit)))
                                        .await
                                        .ok()
                                        .and_then(Result::ok)
                                }
                                None => Some(crate::transcript::Chunk::default()),
                            };
                            if let Some(chunk) = chunk {
                                last_len = len;
                                if reset || chunk.reset {
                                    last_ask = None;
                                    ask_looked = false;
                                }
                                let ask = chunk.items.iter().rev().find_map(|i| match i {
                                    crate::transcript::ChatItem::Tool { name, ask: Some(a), .. }
                                        if name == "AskUserQuestion" => Some(a.clone()),
                                    _ => None,
                                });
                                if ask.is_some() {
                                    last_ask = ask;
                                    // La pregunta pudo salir en pantalla antes: otra vez, con ellas.
                                    last_screen = None;
                                    screen_due.get_or_insert_with(tokio::time::Instant::now);
                                }
                                let send = first || reset || chunk.reset || !chunk.items.is_empty() || !chunk.results.is_empty();
                                offset = chunk.offset;
                                if send {
                                    let mut v = serde_json::to_value(&chunk).unwrap_or_default();
                                    localize_chunk(&mut v);
                                    v["type"] = "chat".into();
                                    v["session_id"] = link.session_id.clone().into();
                                    v["reset"] = (reset || chunk.reset).into();
                                    if socket.send(Message::Text(v.to_string().into())).await.is_err() {
                                        return;
                                    }
                                }
                                first = false;
                            }
                        }
                    } else if first {
                        let msg = serde_json::json!({ "type": "chat", "session_id": null, "items": [], "results": [], "offset": 0, "reset": true, "truncated": false });
                        if socket.send(Message::Text(msg.to_string().into())).await.is_err() {
                            return;
                        }
                        first = false;
                    }
                    // La sesión pudo arrancar después: engancharse al guardián.
                    if holder.is_none() && launch::is_alive(&st.paths, &id) {
                        let hello = Hello { role: Role::Viewer, cols: 80, rows: 24 };
                        if let Ok(stream) = holder_connect(&st, &id, hello).await {
                            holder = Some(crate::ipc::split(stream));
                            screen_due = Some(tokio::time::Instant::now());
                        }
                    }
                }
                // La pantalla, agrupando los cambios rápidos.
                if let (Some(due), Some((_, wr))) = (screen_due, holder.as_mut())
                    && !screen_pending
                    && tokio::time::Instant::now() >= due
                {
                    let req = Frame::HistoryRequest(HistoryRequest {
                        all: false,
                        screen: true,
                        launch: None,
                    });
                    if proto::write_frame(wr, &req).await.is_err() {
                        holder = None;
                    } else {
                        screen_pending = true;
                        screen_due = None;
                    }
                }
            }
            frame = async {
                match holder.as_mut() {
                    Some((rd, _)) => proto::read_frame(rd).await,
                    None => std::future::pending().await,
                }
            } => match frame {
                Ok(Some(Frame::Output(_) | Frame::Snapshot(_))) => {
                    // Algo cambió en la pantalla: pedirla dentro de 50 ms (si no está ya pedida).
                    screen_due.get_or_insert_with(|| tokio::time::Instant::now() + Duration::from_millis(50));
                }
                Ok(Some(Frame::History(b))) => {
                    screen_pending = false;
                    let text = String::from_utf8_lossy(&b).into_owned();
                    if last_screen.as_deref() != Some(text.as_str()) {
                        // El menú que haya (preguntas, permisos…), para contestarlo con botones.
                        let mut menu = prompt::parse(&text);
                        if last_ask.is_none()
                            && !ask_looked
                            && menu.as_ref().is_some_and(|m| m.kind == prompt::Kind::Question)
                            && let Some(p) = path.clone()
                        {
                            ask_looked = true;
                            last_ask = tokio::task::spawn_blocking(move || crate::transcript::last_ask(&p))
                                .await
                                .ok()
                                .flatten();
                        }
                        if let (Some(m), Some(ask)) = (menu.as_mut(), last_ask.as_ref()) {
                            m.fill_previews(ask);
                        }
                        let mut menu_v = serde_json::to_value(&menu).unwrap_or_default();
                        // Para aprobar el plan, el plan en sí (el de la transcripción puede ser viejo).
                        if let Some(plan) = menu.as_ref().and_then(|m| plan_text(&st.paths.home, m)) {
                            menu_v["plan"] = plan.into();
                        }
                        // La siguiente petición que propone Claude, para mandarla con un botón.
                        let suggestion = if menu.is_none() { prompt::suggestion(&text) } else { None };
                        // Un servidor MCP pide confirmación o datos (con su formulario).
                        let form = prompt::form(&text);
                        // Subagentes y órdenes en segundo plano.
                        let background = prompt::background(&text).map(|b| background_json(&b));
                        // El modo de permisos, según el pie.
                        let mode = prompt::mode(&text);
                        // Lo que hay en cola, si Claude lo enseña (si no, nada: un Esc o algo que
                        // no se entienda en la transcripción no deja mensajes fantasma), y no más
                        // de los que enseña: los últimos.
                        let queued = match path.clone() {
                            Some(p) if prompt::queue_shown(&text) => {
                                let st2 = st.clone();
                                let queued = tokio::task::spawn_blocking(move || st2.queue_of(&p).0).await.unwrap_or_default();
                                prompt::queued_on_screen(&text, queued)
                            }
                            _ => Vec::new(),
                        };
                        let msg = serde_json::json!({ "type": "screen", "text": text, "menu": menu_v, "suggestion": suggestion, "form": form, "background": background, "mode": mode, "queued": queued });
                        if socket.send(Message::Text(msg.to_string().into())).await.is_err() {
                            return;
                        }
                        last_screen = Some(text);
                        last_queued = queued;
                    }
                }
                Ok(Some(Frame::Exit(_))) | Ok(None) | Err(_) => {
                    holder = None;
                    screen_pending = false;
                    let msg = serde_json::json!({ "type": "screen", "text": null, "mode": null, "queued": [] });
                    let _ = socket.send(Message::Text(msg.to_string().into())).await;
                    last_screen = None;
                    last_queued.clear();
                }
                Ok(Some(_)) => {}
            },
            msg = socket.recv() => match msg {
                Some(Ok(Message::Close(_)) | Err(_)) | None => return,
                Some(Ok(_)) => {}
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Instant;

    #[test]
    fn a_stop_time_from_before_the_last_run_is_forgotten() {
        let since = crate::mcp_api::since;
        // Parada mientras nido miraba: su hora.
        assert_eq!(stop_time(0, false, true, 1_000), 1_000);
        assert_eq!(stop_time(1_000, false, false, 2_000), 1_000);
        // Vuelve a estar en marcha: esa hora ya no vale.
        let t1 = stop_time(1_000, true, false, 3_000);
        assert_eq!(t1, 0);
        // Se para sin que nido lo vea (p. ej. mientras se reiniciaba): no se sabe, no la de antes.
        let after = stop_time(t1, false, false, 4_000);
        assert_eq!(since(false, None, after), None);
    }

    // ---------- El enlace del QR de `nido pair` ----------

    #[test]
    fn the_pair_code_is_encoded_like_encode_uri_component() {
        // Lo que no toca `encodeURIComponent` pasa tal cual.
        let keep = "AZaz09-_.!~*'()";
        assert_eq!(encode_uri_component(keep), keep);
        assert_eq!(
            encode_uri_component("nido://pair?k=a+b/c=&x=1 2#ñ"),
            "nido%3A%2F%2Fpair%3Fk%3Da%2Bb%2Fc%3D%26x%3D1%202%23%C3%B1"
        );
    }

    #[test]
    fn the_pair_link_goes_to_the_hub_web_with_the_code_after_the_hash() {
        let uri = "nido://pair?k=a+b";
        let want = "https://hub.example.org/app#nido=nido%3A%2F%2Fpair%3Fk%3Da%2Bb";
        assert_eq!(pair_link("https://hub.example.org", uri), want);
        assert_eq!(pair_link("https://hub.example.org/", uri), want);
        // Con una ruta, la web cuelga de ella.
        assert_eq!(
            pair_link("https://hub.example.org/nido/", uri),
            "https://hub.example.org/nido/app#nido=nido%3A%2F%2Fpair%3Fk%3Da%2Bb"
        );
    }

    /// Lo que contesta `POST /api/v1/pair` (con `days` o sin él).
    async fn pair_reply(st: &Arc<AppState>, days: Option<u64>) -> serde_json::Value {
        let body = days.map(|d| Json(PairReq { days: Some(d) }));
        let r = api_pair(State(st.clone()), body).await;
        assert_eq!(r.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(r.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn pair_gives_a_link_only_with_the_hub() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let paths = Paths {
            runtime: dir.join("run"),
            data: dir.join("data"),
            state: dir.join("state"),
            config: dir.join("config"),
            home: dir.to_owned(),
            server: None,
        };
        let st = AppState::new(paths, Launcher::Detached, None, true).unwrap();

        // Sin hub: ni `link` ni `hub`.
        let r = pair_reply(&st, None).await;
        assert!(r["uri"].as_str().unwrap().starts_with("nido://pair?"));
        assert!(r.get("link").is_none(), "{r}");
        assert_eq!(r["hub"], serde_json::Value::Null);

        // Conectado al hub: el enlace a su web, con el código de esta misma respuesta.
        st.hub.status.send_modify(|s| {
            s.url = Some("wss://hub.example.org/nido".into());
            s.id = Some("este".into());
            s.connected = true;
        });
        for days in [None, Some(3)] {
            let r = pair_reply(&st, days).await;
            let uri = r["uri"].as_str().unwrap();
            let link = r["link"].as_str().unwrap();
            assert_eq!(link, pair_link("https://hub.example.org/nido", uri));
            assert!(
                link.starts_with("https://hub.example.org/nido/app#nido=nido%3A%2F%2Fpair%3F"),
                "{link}"
            );
            assert_eq!(r["reusable"], days.is_some());
        }

        // Si se cae la conexión, otra vez sin enlace.
        st.hub.status.send_modify(|s| s.connected = false);
        assert!(pair_reply(&st, None).await.get("link").is_none());
    }

    /// `expect` con un mensaje (o una propuesta, o un formulario): 400, sin buscar la sesión.
    #[tokio::test]
    async fn an_answer_with_expect_and_a_message_is_a_bad_request() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let paths = Paths {
            runtime: dir.join("run"),
            data: dir.join("data"),
            state: dir.join("state"),
            config: dir.join("config"),
            home: dir.to_owned(),
            server: None,
        };
        let st = AppState::new(paths, Launcher::Detached, None, true).unwrap();
        let req: prompt::Request = serde_json::from_value(json!({
            "message": "hola", "expect": {"kind": "permission"}
        }))
        .unwrap();
        let r = api_answer(State(st.clone()), Path("no-existe".into()), Json(req)).await;
        assert_eq!(r.status(), StatusCode::BAD_REQUEST);
        let bytes = axum::body::to_bytes(r.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["error"], "expect only works with option or questions");
    }

    // ---------- El token de voz ----------

    /// Lo que contesta `POST /api/v1/voice/token` con ese bearer.
    async fn voice_reply(st: &Arc<AppState>, bearer: &str) -> (StatusCode, serde_json::Value) {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            format!("Bearer {bearer}").parse().unwrap(),
        );
        let r = api_voice_token(State(st.clone()), headers).await;
        let status = r.status();
        let bytes = axum::body::to_bytes(r.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    #[tokio::test]
    async fn only_a_paired_device_asks_for_a_voice_token() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let paths = Paths {
            runtime: dir.join("run"),
            data: dir.join("data"),
            state: dir.join("state"),
            config: dir.join("config"),
            home: dir.to_owned(),
            server: None,
        };
        let st = AppState::new(paths, Launcher::Detached, Some("local".into()), true).unwrap();

        // El token de este equipo no vale: cada pedido tiene que ser de un dispositivo.
        let (status, body) = voice_reply(&st, "local").await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(
            body["error"],
            "only a paired phone or browser can make a call"
        );

        // Un teléfono emparejado sí, pero sin hub no hay token.
        let phone = st
            .phones
            .lock()
            .unwrap()
            .add(&[1u8; 32], "tel", None)
            .unwrap();
        let (status, body) = voice_reply(&st, &phone.token).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["error"], "not connected to the hub");
    }

    /// Con el hub: lo que dé, tal cual (sin `req` ni `ok`), y cada error con su código. Un hub
    /// que no contesta (uno antiguo) es que no ofrece voz.
    #[tokio::test]
    async fn the_voice_token_answers_what_the_hub_says() {
        use hub::fake::Reply;
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let paths = Paths {
            runtime: dir.join("run"),
            data: dir.join("data"),
            state: dir.join("state"),
            config: dir.join("config"),
            home: dir.to_owned(),
            server: None,
        };
        let st = AppState::new(paths, Launcher::Detached, Some("local".into()), true).unwrap();
        let phone = st
            .phones
            .lock()
            .unwrap()
            .add(&[1u8; 32], "tel", None)
            .unwrap();
        // Los pedidos van numerados desde 1: cada uno, una respuesta.
        let addr = hub::fake::hub(|req| {
            let fail = |error: &str| {
                Reply::Send(json!({ "voiced": { "req": req, "ok": false, "error": error } }))
            };
            match req {
                1 => Reply::Send(json!({ "voiced": {
                    "req": req, "ok": true, "token": "token-de-prueba", "expires": 3000,
                    "project": "proyecto", "location": "eu", "host": "voz.example",
                    "model": "modelo-voz",
                }})),
                2 => fail("límite de voz"),
                3 => fail("invalid_grant"),
                4 => fail("el hub no ofrece voz"),
                _ => Reply::Silent,
            }
        })
        .await;
        hub::fake::connect(&st.hub, st.clone(), addr).await;

        let (status, body) = voice_reply(&st, &phone.token).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            json!({
                "token": "token-de-prueba", "expires": 3000, "project": "proyecto",
                "location": "eu", "host": "voz.example", "model": "modelo-voz",
            })
        );
        let (status, body) = voice_reply(&st, &phone.token).await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(body["error"], "voice limit reached");
        let (status, body) = voice_reply(&st, &phone.token).await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert_eq!(body["error"], "invalid_grant");
        let (status, body) = voice_reply(&st, &phone.token).await;
        assert_eq!(status, StatusCode::NOT_IMPLEMENTED);
        assert_eq!(body["error"], "the hub does not offer voice");
        // Un hub antiguo no contesta: lo mismo, al acabarse la espera (corta en las pruebas).
        let (status, body) = voice_reply(&st, &phone.token).await;
        assert_eq!(status, StatusCode::NOT_IMPLEMENTED);
        assert_eq!(body["error"], "the hub does not offer voice");
    }

    // ---------- La memoria de la voz ----------

    /// Un nido con la API escuchando en loopback, como la escucha interna a la que van los flujos
    /// del túnel cifrado, y un teléfono emparejado (su token).
    async fn memory_server(dir: &std::path::Path) -> (Arc<AppState>, std::net::SocketAddr, String) {
        let paths = Paths {
            runtime: dir.join("run"),
            data: dir.join("data"),
            state: dir.join("state"),
            config: dir.join("config"),
            home: dir.to_owned(),
            server: None,
        };
        let st = AppState::new(paths, Launcher::Detached, Some("local".into()), true).unwrap();
        let phone = st
            .phones
            .lock()
            .unwrap()
            .add(&[1u8; 32], "tel", None)
            .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = router(st.clone()).into_make_service_with_connect_info::<std::net::SocketAddr>();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (st, addr, phone.token)
    }

    /// Una petición con ese bearer (sin `Accept-Language`: en inglés, salvo `lang`).
    async fn memory_call(
        addr: std::net::SocketAddr,
        bearer: &str,
        method: &str,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> (u16, serde_json::Value) {
        memory_call_in(addr, bearer, method, path, body, None).await
    }

    async fn memory_call_in(
        addr: std::net::SocketAddr,
        bearer: &str,
        method: &str,
        path: &str,
        body: Option<serde_json::Value>,
        lang: Option<&'static str>,
    ) -> (u16, serde_json::Value) {
        let (bearer, method, path) = (bearer.to_owned(), method.to_owned(), path.to_owned());
        tokio::task::spawn_blocking(move || {
            let headers: Vec<(&str, &str)> =
                lang.map(|l| ("Accept-Language", l)).into_iter().collect();
            crate::client::Api {
                addr,
                token: Some(bearer),
            }
            .request_with(
                &method,
                &path,
                body.as_ref(),
                &headers,
                Some(Duration::from_secs(30)),
            )
            .unwrap()
        })
        .await
        .unwrap()
    }

    /// Las horas (`at`) a 0, tras ver que son números: lo demás se compara tal cual.
    fn without_times(mut v: serde_json::Value) -> serde_json::Value {
        fn walk(v: &mut serde_json::Value) {
            match v {
                serde_json::Value::Object(m) => {
                    if let Some(at) = m.get_mut("at") {
                        assert!(at.is_u64(), "{at}");
                        *at = json!(0);
                    }
                    m.values_mut().for_each(walk);
                }
                serde_json::Value::Array(a) => a.iter_mut().for_each(walk),
                _ => {}
            }
        }
        walk(&mut v);
        v
    }

    /// Las rutas de la memoria, con un cuerpo que valdría.
    fn memory_routes() -> Vec<(&'static str, &'static str, Option<serde_json::Value>)> {
        vec![
            ("GET", "/api/v1/voice/memory", None),
            (
                "POST",
                "/api/v1/voice/memory/notes",
                Some(json!({"text": "hola"})),
            ),
            ("DELETE", "/api/v1/voice/memory/notes/1", None),
            (
                "POST",
                "/api/v1/voice/memory/calls",
                Some(json!({"id": "c1", "kind": "resumen", "text": "hola"})),
            ),
        ]
    }

    /// Como el token de voz: el token de este equipo no vale, ni con un cuerpo que no se entiende
    /// (se mira quién pide antes que lo que pide), y no se toca nada: ni la nota 1, que existe.
    #[tokio::test(flavor = "multi_thread")]
    async fn only_a_paired_device_uses_the_voice_memory() {
        let tmp = tempfile::tempdir().unwrap();
        let (st, addr, _) = memory_server(tmp.path()).await;
        assert_eq!(st.voice_memory.add_note("guardada"), Ok(1));
        let file = st.paths.state.join("voice-memory.json");
        let before = std::fs::read(&file).unwrap();
        let denied = "only a paired phone or browser can use the voice memory";
        for (method, path, body) in memory_routes() {
            for body in [body.clone(), Some(json!("no es lo que se espera"))] {
                let (code, r) = memory_call(addr, "local", method, path, body).await;
                assert_eq!(
                    (code, r["error"].as_str()),
                    (403, Some(denied)),
                    "{method} {path}"
                );
            }
        }
        // Sin token, ni llega: el guardián.
        let (code, _) = memory_call(addr, "", "GET", "/api/v1/voice/memory", None).await;
        assert_eq!(code, 401);
        assert_eq!(std::fs::read(&file).unwrap(), before);
    }

    /// Cada ruta, con sus códigos, y lo que enseña `GET`.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_voice_memory_api() {
        let tmp = tempfile::tempdir().unwrap();
        let (st, addr, phone) = memory_server(tmp.path()).await;
        let call = |method: &'static str, path: &'static str, body: Option<serde_json::Value>| {
            let phone = phone.clone();
            async move { memory_call(addr, &phone, method, path, body).await }
        };
        const MEMORY: &str = "/api/v1/voice/memory";
        const NOTES: &str = "/api/v1/voice/memory/notes";
        const CALLS: &str = "/api/v1/voice/memory/calls";

        // Vacía.
        let (code, r) = call("GET", MEMORY, None).await;
        assert_eq!((code, r), (200, json!({"notes": [], "calls": []})));

        // Las notas.
        let (code, r) = call("POST", NOTES, Some(json!({"text": "  hola  "}))).await;
        assert_eq!((code, r), (200, json!({"id": 1})));
        let (code, r) = call("POST", NOTES, Some(json!({"text": "segunda"}))).await;
        assert_eq!((code, r), (200, json!({"id": 2})));
        let (code, r) = call("POST", NOTES, Some(json!({"text": " \n "}))).await;
        assert_eq!(
            (code, r["error"].as_str()),
            (400, Some("the note is empty"))
        );
        for body in [json!({}), json!({"text": 3}), json!("hola")] {
            let (code, r) = call("POST", NOTES, Some(body.clone())).await;
            assert_eq!(code, 400, "{body}: {r}");
            assert!(r["error"].as_str().is_some(), "{r}");
        }
        let (code, _) = call("DELETE", "/api/v1/voice/memory/notes/1", None).await;
        assert_eq!(code, 204);
        for gone in ["1", "99", "abc", "-1"] {
            let path = format!("{NOTES}/{gone}");
            let (code, r) = memory_call(addr, &phone, "DELETE", &path, None).await;
            assert_eq!(
                (code, r["error"].as_str()),
                (404, Some("no such note")),
                "{gone}"
            );
        }
        // En español, si se pide.
        let (code, r) = memory_call_in(
            addr,
            &phone,
            "DELETE",
            "/api/v1/voice/memory/notes/1",
            None,
            Some("es"),
        )
        .await;
        assert_eq!((code, r["error"].as_str()), (404, Some("no hay esa nota")));

        // Las llamadas: la bitácora, el resumen que la sustituye y la bitácora tardía que no lo
        // pisa.
        let put = |kind: &str, text: &str| Some(json!({"id": "c1", "kind": kind, "text": text}));
        let (code, _) = call("POST", CALLS, put("bitácora", "dijo hola")).await;
        assert_eq!(code, 204);
        let (code, _) = call("POST", CALLS, put("resumen", "el resumen")).await;
        assert_eq!(code, 204);
        let (code, _) = call("POST", CALLS, put("bitácora", "tarde")).await;
        assert_eq!(code, 204);
        // Sin tilde también vale (y tampoco lo pisa).
        let (code, _) = call("POST", CALLS, put("bitacora", "más tarde")).await;
        assert_eq!(code, 204);
        let unknown = "kind must be “resumen” or “bitácora”";
        for kind in ["otro", "", "Resumen", "BITÁCORA"] {
            let (code, r) = call("POST", CALLS, put(kind, "x")).await;
            assert_eq!((code, r["error"].as_str()), (400, Some(unknown)), "{kind}");
        }
        for body in [
            json!({"id": "c2", "text": "x"}),
            json!({"id": "c2", "kind": 1, "text": "x"}),
            json!({"kind": "resumen", "text": "x"}),
            json!({"id": "c2", "kind": "resumen"}),
        ] {
            let (code, r) = call("POST", CALLS, Some(body.clone())).await;
            assert_eq!(code, 400, "{body}: {r}");
        }
        let (code, r) = call(
            "POST",
            CALLS,
            Some(json!({"id": "mal id", "kind": "resumen", "text": "x"})),
        )
        .await;
        assert_eq!(code, 400);
        assert!(
            r["error"].as_str().unwrap().starts_with("invalid call id"),
            "{r}"
        );
        let (code, r) = call(
            "POST",
            CALLS,
            Some(json!({"id": "c2", "kind": "resumen", "text": "  "})),
        )
        .await;
        assert_eq!(
            (code, r["error"].as_str()),
            (400, Some("the text is empty"))
        );

        // Todo junto, sin nada de más (ni `next_note`).
        let (code, r) = call("GET", MEMORY, None).await;
        assert_eq!(code, 200);
        assert_eq!(
            without_times(r),
            json!({
                "notes": [{"id": 2, "text": "segunda", "at": 0}],
                "calls": [{"id": "c1", "at": 0, "kind": "resumen", "text": "el resumen"}],
            })
        );

        // El handle para retomar lo guarda el hub, no nido: esa ruta no existe.
        for method in ["PUT", "DELETE", "GET"] {
            let (code, _) = call(method, "/api/v1/voice/memory/resume", None).await;
            assert_eq!(code, 404, "{method}");
        }

        // Borrar el fichero la borra, ya.
        std::fs::remove_file(st.paths.state.join("voice-memory.json")).unwrap();
        let (code, r) = call("GET", MEMORY, None).await;
        assert_eq!((code, r), (200, json!({"notes": [], "calls": []})));
    }

    /// Un cuerpo enorme ni se lee (413); el de un texto de llamada largo y escapado, sí.
    #[tokio::test(flavor = "multi_thread")]
    async fn memory_bodies_have_a_cap() {
        let tmp = tempfile::tempdir().unwrap();
        let (_st, addr, phone) = memory_server(tmp.path()).await;
        let huge = "x".repeat(MEMORY_BODY);
        for (method, path, body) in [
            (
                "POST",
                "/api/v1/voice/memory/notes",
                json!({ "text": huge }),
            ),
            (
                "POST",
                "/api/v1/voice/memory/calls",
                json!({"id": "c1", "kind": "resumen", "text": huge}),
            ),
        ] {
            let (code, _) = memory_call(addr, &phone, method, path, Some(body)).await;
            assert_eq!(code, 413, "{method} {path}");
        }
        // 4000 letras de dos bytes, cada una tras un carácter de control (6 bytes en JSON).
        let long = json!({"id": "largo", "kind": "resumen", "text": "\u{1}ñ".repeat(4000)});
        assert!(long.to_string().len() > 30 * 1024);
        let (code, r) = memory_call(
            addr,
            &phone,
            "POST",
            "/api/v1/voice/memory/calls",
            Some(long),
        )
        .await;
        assert_eq!(code, 204, "{r}");
        let (_, r) = memory_call(addr, &phone, "GET", "/api/v1/voice/memory", None).await;
        assert_eq!(r["calls"][0]["text"], "ñ".repeat(4000));
    }

    /// La nota 51 no cabe (409) hasta que se olvide alguna.
    #[tokio::test(flavor = "multi_thread")]
    async fn fifty_notes_and_no_more() {
        let tmp = tempfile::tempdir().unwrap();
        let (_st, addr, phone) = memory_server(tmp.path()).await;
        const NOTES: &str = "/api/v1/voice/memory/notes";
        for i in 1..=50u64 {
            let body = json!({ "text": format!("nota {i}") });
            let (code, r) = memory_call(addr, &phone, "POST", NOTES, Some(body)).await;
            assert_eq!((code, r), (200, json!({ "id": i })));
        }
        let body = json!({"text": "una más"});
        let (code, r) = memory_call(addr, &phone, "POST", NOTES, Some(body.clone())).await;
        assert_eq!(
            (code, r["error"].as_str()),
            (409, Some("there are 50 notes: forget one"))
        );
        let (code, _) = memory_call(addr, &phone, "DELETE", &format!("{NOTES}/7"), None).await;
        assert_eq!(code, 204);
        let (code, r) = memory_call(addr, &phone, "POST", NOTES, Some(body)).await;
        assert_eq!((code, r), (200, json!({"id": 51})));
    }

    /// Un fichero que no se puede leer: 500 con el error, también `GET` (y no se pisa).
    #[tokio::test(flavor = "multi_thread")]
    async fn the_voice_memory_says_when_it_cannot_read_its_file() {
        let tmp = tempfile::tempdir().unwrap();
        let (st, addr, phone) = memory_server(tmp.path()).await;
        let file = st.paths.state.join("voice-memory.json");
        std::fs::create_dir_all(&file).unwrap();
        let (code, r) = memory_call(
            addr,
            &phone,
            "POST",
            "/api/v1/voice/memory/notes",
            Some(json!({"text": "hola"})),
        )
        .await;
        assert_eq!(code, 500);
        assert!(
            r["error"]
                .as_str()
                .unwrap()
                .starts_with("could not read the memory: "),
            "{r}"
        );
        assert!(file.is_dir());
        let (code, r) = memory_call(addr, &phone, "GET", "/api/v1/voice/memory", None).await;
        assert_eq!(code, 500);
        assert!(
            r["error"]
                .as_str()
                .unwrap()
                .starts_with("could not read the memory: "),
            "{r}"
        );
    }

    fn view(id: &str) -> SessionView {
        SessionView {
            id: id.into(),
            name: id.into(),
            cwd: PathBuf::from("/tmp"),
            argv: vec!["claude".into()],
            autostart: false,
            created: 0,
            stopped: 0,
            alive: true,
            claude: None,
            folder: None,
            unread: false,
            opened_by: None,
            alerts: vec![],
            items_rev: 0,
            secret: None,
            plan: None,
        }
    }

    /// Un nido en `dir` con un equipo «Mac» en el grupo que contesta a `status` lo que diga la
    /// prueba, y cuántas veces se le preguntó. Las madres se vuelven a mirar cada 50 ms.
    type Answer = Arc<std::sync::Mutex<e2e::PeerResult>>;
    fn with_mac(dir: &std::path::Path) -> (Arc<AppState>, Answer, Arc<AtomicUsize>) {
        let paths = Paths {
            runtime: dir.join("run"),
            data: dir.join("data"),
            state: dir.join("state"),
            config: dir.join("config"),
            home: dir.to_owned(),
            server: None,
        };
        let st = AppState::new(paths, Launcher::Detached, None, true).unwrap();
        st.group.add_member("mac-id", "bWFj", "Mac");
        st.mcp_parents.set_ttl(Duration::from_millis(50));
        let answer: Answer = Arc::new(std::sync::Mutex::new(Ok(json!({"state": "working"}))));
        let asked = Arc::new(AtomicUsize::new(0));
        let (a, n) = (answer.clone(), asked.clone());
        assert!(st.set_peer_link(Box::new(move |_, method, _| {
            let r = if method == "status" {
                n.fetch_add(1, Ordering::SeqCst);
                a.lock().unwrap().clone()
            } else {
                Err((502, "sin conexión o versión antigua".into()))
            };
            Box::pin(async move { r })
        })));
        (st, answer, asked)
    }

    // ---------- El modo y el modelo ----------

    /// Un nido en `dir`, con su API en un puerto propio y una sesión cuyo guardián es un Claude
    /// de mentira (`prompt::fake`). Devuelve también cómo parar ese guardián.
    #[cfg(unix)]
    async fn with_fake_claude(
        dir: &std::path::Path,
    ) -> (
        Arc<AppState>,
        std::net::SocketAddr,
        String,
        Arc<std::sync::Mutex<prompt::fake::Claude>>,
        tokio::task::JoinHandle<()>,
    ) {
        let paths = Paths {
            runtime: dir.join("run"),
            data: dir.join("data"),
            state: dir.join("state"),
            config: dir.join("config"),
            home: dir.to_owned(),
            server: None,
        };
        let st = AppState::new(paths, Launcher::Detached, None, true).unwrap();
        let session = Session {
            id: new_id(),
            name: "c".into(),
            cwd: dir.to_owned(),
            argv: vec!["claude".into()],
            env: vec![],
            autostart: false,
            created: now(),
            claude: Some(ClaudeLink {
                session_id: "conv".into(),
                config_dir: Some(dir.join("claude")),
                account: None,
                inside_shell: false,
            }),
            folder: None,
            finished: 0,
            stopped: 0,
            seen: 0,
            opened_by: None,
            alerts: vec![],
        };
        let id = st.registry.lock().await.add(session).unwrap().id;
        let claude = Arc::new(std::sync::Mutex::new(prompt::fake::Claude::default()));
        let mut holder = crate::ipc::Listener::bind(&st.paths.socket(&id))
            .await
            .unwrap();
        let c = claude.clone();
        let guard = tokio::spawn(async move {
            while let Ok(stream) = holder.accept().await {
                tokio::spawn(prompt::fake::serve(stream, c.clone()));
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = router(st.clone()).into_make_service_with_connect_info::<std::net::SocketAddr>();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (st, addr, id, claude, guard)
    }

    #[cfg(unix)]
    async fn post(
        addr: std::net::SocketAddr,
        path: String,
        body: serde_json::Value,
    ) -> (u16, serde_json::Value) {
        tokio::task::spawn_blocking(move || {
            crate::client::Api { addr, token: None }
                .request_with("POST", &path, Some(&body), &[], None)
                .unwrap()
        })
        .await
        .unwrap()
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn the_mode_route() {
        let _limit = crate::watchdog::limit(Duration::from_secs(60));
        let tmp = tempfile::tempdir().unwrap();
        let (st, addr, id, claude, guard) = with_fake_claude(tmp.path()).await;
        let url = format!("/api/v1/sessions/{id}/mode");
        // Lo que no es un modo: 400, sin tocar nada.
        for body in [json!({}), json!({"mode": "turbo"}), json!({"mode": 3})] {
            let (code, r) = post(addr, url.clone(), body).await;
            assert_eq!(code, 400, "{r}");
        }
        let (code, r) = post(addr, url.clone(), json!({"mode": "turbo"})).await;
        assert_eq!(
            (code, r["error"].as_str()),
            (400, Some("unknown mode: “turbo”"))
        );
        let (code, _) = post(
            addr,
            "/api/v1/sessions/nadie/mode".into(),
            json!({"mode": "plan"}),
        )
        .await;
        assert_eq!(code, 404);
        assert!(claude.lock().unwrap().keys.is_empty());
        // Al modo pedido.
        let (code, r) = post(addr, url.clone(), json!({"mode": "plan"})).await;
        assert_eq!(
            (code, r.clone()),
            (200, json!({"ok": true, "mode": "plan"})),
            "{r}"
        );
        assert_eq!(
            prompt::mode(&claude.lock().unwrap().screen()),
            Some(prompt::Mode::Plan)
        );
        // Inalcanzable, o con un menú en pantalla: 409.
        let (code, r) = post(addr, url.clone(), json!({"mode": "bypassPermissions"})).await;
        assert_eq!(code, 409, "{r}");
        claude.lock().unwrap().permission = true;
        let (code, r) = post(addr, url.clone(), json!({"mode": "auto"})).await;
        assert_eq!(code, 409, "{r}");
        // Parada: 409.
        guard.abort();
        let _ = guard.await;
        std::fs::remove_file(st.paths.socket(&id)).unwrap();
        let (code, r) = post(addr, url, json!({"mode": "auto"})).await;
        assert_eq!(
            (code, r["error"].as_str()),
            (409, Some("the session is not running"))
        );
    }

    /// La transcripción de la sesión de `with_fake_claude`, con estas líneas.
    #[cfg(unix)]
    fn fake_transcript(dir: &std::path::Path, lines: &[serde_json::Value]) {
        let project = dir.join("claude/projects/-proyecto");
        std::fs::create_dir_all(&project).unwrap();
        let text: String = lines.iter().map(|l| format!("{l}\n")).collect();
        std::fs::write(project.join("conv.jsonl"), text).unwrap();
    }

    #[cfg(unix)]
    fn enqueue(content: &str) -> serde_json::Value {
        json!({
            "type": "queue-operation",
            "operation": "enqueue",
            "timestamp": "2026-11-13T06:33:48.508Z",
            "sessionId": "conv",
            "content": content,
        })
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn the_queue_routes() {
        let _limit = crate::watchdog::limit(Duration::from_secs(60));
        let tmp = tempfile::tempdir().unwrap();
        let (st, addr, id, claude, guard) = with_fake_claude(tmp.path()).await;
        fake_transcript(
            tmp.path(),
            &[
                enqueue("primero"),
                enqueue("<task-notification>\n<task-id>b1</task-id>\n</task-notification>"),
                enqueue("segundo\ncon dos líneas"),
            ],
        );
        for what in ["send", "edit"] {
            let url = format!("/api/v1/sessions/{id}/queue/{what}");
            // Sesión que no existe: 404.
            let (code, _) = post(
                addr,
                format!("/api/v1/sessions/nadie/queue/{what}"),
                json!({}),
            )
            .await;
            assert_eq!(code, 404);
            // Sin cola: 409.
            let (code, r) = post(addr, url.clone(), json!({})).await;
            assert_eq!(
                (code, r["error"].as_str()),
                (409, Some("Claude has no queued messages"))
            );
            // Con un permiso en pantalla: 409.
            {
                let mut c = claude.lock().unwrap();
                c.working = true;
                c.queue = vec!["primero".into(), "segundo\ncon dos líneas".into()];
                c.permission = true;
            }
            let (code, r) = post(addr, url.clone(), json!({})).await;
            assert_eq!(code, 409, "{r}");
            assert!(claude.lock().unwrap().keys.is_empty());
            let mut c = claude.lock().unwrap();
            c.permission = false;
            c.queue.clear();
        }
        // Con un borrador en la caja, editar: 409 sin tocar nada.
        {
            let mut c = claude.lock().unwrap();
            c.queue = vec!["primero".into(), "segundo\ncon dos líneas".into()];
            c.input = "a medio".into();
        }
        let (code, r) = post(addr, format!("/api/v1/sessions/{id}/queue/edit"), json!({})).await;
        assert_eq!(code, 409, "{r}");
        assert!(claude.lock().unwrap().keys.is_empty());
        claude.lock().unwrap().input.clear();
        // Editar: el texto, como lo apuntó la transcripción; la caja y la cola, vacías.
        let (code, r) = post(addr, format!("/api/v1/sessions/{id}/queue/edit"), json!({})).await;
        assert_eq!(
            (code, r.clone()),
            (
                200,
                json!({"ok": true, "text": "primero\nsegundo\ncon dos líneas"})
            ),
            "{r}"
        );
        {
            let mut c = claude.lock().unwrap();
            assert!(c.queue.is_empty() && c.input.is_empty() && c.sent_now.is_empty());
            // Otra vez en cola, para mandarla ya.
            c.queue = vec!["tercero".into()];
            c.keys.clear();
        }
        let (code, r) = post(addr, format!("/api/v1/sessions/{id}/queue/send"), json!({})).await;
        assert_eq!((code, r.clone()), (200, json!({"ok": true})), "{r}");
        {
            let c = claude.lock().unwrap();
            assert_eq!(c.keys, [b"\x18".to_vec(), b"\x13".to_vec()]);
            assert_eq!(c.sent_now, ["tercero"]);
        }
        // Que no se va: 409.
        {
            let mut c = claude.lock().unwrap();
            c.queue = vec!["cuarto".into()];
            c.send_now_ignored = true;
        }
        let (code, r) = post(addr, format!("/api/v1/sessions/{id}/queue/send"), json!({})).await;
        assert_eq!(code, 409, "{r}");
        // Parada: 409.
        guard.abort();
        let _ = guard.await;
        std::fs::remove_file(st.paths.socket(&id)).unwrap();
        let (code, r) = post(addr, format!("/api/v1/sessions/{id}/queue/send"), json!({})).await;
        assert_eq!(
            (code, r["error"].as_str()),
            (409, Some("the session is not running"))
        );
    }

    /// El siguiente `screen` de un `/live`.
    #[cfg(unix)]
    async fn next_screen(
        live: &mut (
                 impl futures_util::Stream<
            Item = Result<
                tokio_tungstenite::tungstenite::Message,
                tokio_tungstenite::tungstenite::Error,
            >,
        > + Unpin
             ),
    ) -> serde_json::Value {
        use futures_util::StreamExt;
        loop {
            let msg = live.next().await.unwrap().unwrap();
            if let tokio_tungstenite::tungstenite::Message::Text(t) = msg {
                let v: serde_json::Value = serde_json::from_str(t.as_str()).unwrap();
                if v["type"] == "screen" {
                    return v;
                }
            }
        }
    }

    /// Los `screen` de `/live` llevan la cola: la de la transcripción, solo si Claude la enseña.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn the_live_screen_has_the_queue() {
        use std::io::Write;
        let _limit = crate::watchdog::limit(Duration::from_secs(60));
        let tmp = tempfile::tempdir().unwrap();
        let (_st, addr, id, claude, _guard) = with_fake_claude(tmp.path()).await;
        let url = format!("ws://{addr}/api/v1/sessions/{id}/live");
        // En la transcripción hay uno que Claude no enseña (p. ej. tras un Esc): nada.
        fake_transcript(tmp.path(), &[enqueue("primero")]);
        let (mut live, _) = tokio_tungstenite::connect_async(url.clone()).await.unwrap();
        let v = next_screen(&mut live).await;
        assert_eq!(v["queued"], json!([]), "{v}");
        drop(live);
        // Claude enseña dos, pero la transcripción solo ha apuntado el primero.
        {
            let mut c = claude.lock().unwrap();
            c.working = true;
            c.queue = vec!["primero".into(), "segundo".into()];
        }
        let (mut live, _) = tokio_tungstenite::connect_async(url).await.unwrap();
        let v = next_screen(&mut live).await;
        assert_eq!(v["queued"], json!(["primero"]), "{v}");
        assert!(
            v["text"]
                .as_str()
                .unwrap()
                .contains("ctrl+x ctrl+s to send now")
        );
        // Apunta el segundo (y un aviso del sistema, que no se enseña): llega otra vez la
        // pantalla, con los dos, aunque no haya cambiado.
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(tmp.path().join("claude/projects/-proyecto/conv.jsonl"))
            .unwrap();
        for l in [
            enqueue("<task-notification>\n</task-notification>"),
            enqueue("segundo"),
        ] {
            writeln!(f, "{l}").unwrap();
        }
        let v = tokio::time::timeout(Duration::from_secs(20), next_screen(&mut live))
            .await
            .expect("no llegó otra vez la pantalla");
        assert_eq!(v["queued"], json!(["primero", "segundo"]), "{v}");
    }

    /// En una pregunta con vista previa, cada opción lleva la suya (de la transcripción), no solo
    /// la del cursor (que es la que se ve).
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn the_live_question_has_the_previews() {
        let _limit = crate::watchdog::limit(Duration::from_secs(60));
        let tmp = tempfile::tempdir().unwrap();
        let (_st, addr, id, claude, _guard) = with_fake_claude(tmp.path()).await;
        claude.lock().unwrap().raw =
            Some(include_str!("../tests/pantallas/pregunta_vista_previa.txt").to_owned());
        let option = |label: &str, preview: &str| json!({"label": label, "description": "", "preview": preview});
        fake_transcript(
            tmp.path(),
            &[
                json!({"type": "assistant", "uuid": "a1", "message": {"content": [
                {"type": "tool_use", "id": "t-1", "name": "AskUserQuestion", "input": {"questions": [{
                    "question": "¿Cómo lo dejo?", "header": "«estimado»", "multiSelect": false,
                    "options": [
                        option("Tilde en la píldora (Recommended)", "Bandeja: [ ~10 días ]"),
                        option("«estimado» en la píldora", "Bandeja: [ estimado ]"),
                        option("Solo en el detalle", "Detalle: estimado"),
                        option("Dejarlo como está", "Bandeja: [ 10 días ]"),
                    ]}]}}]}}),
            ],
        );
        let end = std::fs::metadata(tmp.path().join("claude/projects/-proyecto/conv.jsonl"))
            .unwrap()
            .len();
        // Desde el principio, y siguiendo desde el final (la app que vuelve con lo que ya tenía).
        for q in [String::new(), format!("?sid=conv&offset={end}")] {
            let url = format!("ws://{addr}/api/v1/sessions/{id}/live{q}");
            let (mut live, _) = tokio_tungstenite::connect_async(url).await.unwrap();
            // La primera pantalla puede llegar antes de leer la transcripción: hasta que las traiga.
            let previews = tokio::time::timeout(Duration::from_secs(20), async {
                loop {
                    let v = next_screen(&mut live).await;
                    let p: Vec<_> = v["menu"]["options"]
                        .as_array()
                        .map(|o| o.iter().map(|o| o["preview"].clone()).collect())
                        .unwrap_or_default();
                    if p.len() == 4 && p[1].is_string() {
                        return p;
                    }
                }
            })
            .await
            .unwrap_or_else(|_| panic!("no llegaron las vistas previas ({q})"));
            assert_eq!(
                previews,
                [
                    json!("Bandeja: [ ~10 días ]"),
                    json!("Bandeja: [ estimado ]"),
                    json!("Detalle: estimado"),
                    json!("Bandeja: [ 10 días ]")
                ]
            );
        }
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn the_model_route() {
        let _limit = crate::watchdog::limit(Duration::from_secs(60));
        let tmp = tempfile::tempdir().unwrap();
        let (st, addr, id, claude, _guard) = with_fake_claude(tmp.path()).await;
        let url = format!("/api/v1/sessions/{id}/model");
        // Antes de ver el menú, los de siempre.
        assert_eq!(st.models(), DEFAULT_MODELS);
        for body in [json!({}), json!({"model": "  "}), json!({"model": 1})] {
            let (code, r) = post(addr, url.clone(), body).await;
            assert_eq!(code, 400, "{r}");
        }
        // Con un borrador en la caja: 409, sin tocar nada.
        claude.lock().unwrap().input = "a medio".into();
        let (code, r) = post(addr, url.clone(), json!({"model": "Opus"})).await;
        assert_eq!(code, 409, "{r}");
        assert!(claude.lock().unwrap().keys.is_empty());
        claude.lock().unwrap().input.clear();
        // Claude trabajando (según su estado): 409.
        st.views.send_replace(Listing {
            sessions: vec![SessionView {
                claude: Some(ClaudeView {
                    background: None,
                    session_id: "conv".into(),
                    running: true,
                    status: Some("busy".into()),
                    title: None,
                    account: None,
                    model: None,
                    models: vec![],
                }),
                ..view(&id)
            }],
            folders: vec![],
        });
        let (code, r) = post(addr, url.clone(), json!({"model": "Opus"})).await;
        assert_eq!(code, 409, "{r}");
        assert!(claude.lock().unwrap().keys.is_empty());
        st.views.send_replace(Listing::default());
        // Bien: solo para esta sesión, y nido apunta los modelos del menú.
        let (code, r) = post(addr, url.clone(), json!({"model": "Opus"})).await;
        assert_eq!(
            (code, r.clone()),
            (200, json!({"ok": true, "model": "Opus 5.5"})),
            "{r}"
        );
        {
            let c = claude.lock().unwrap();
            assert_eq!(c.current, 1);
            assert!(!c.saved_default && !c.effort_changed);
        }
        let menu = [
            "Opus 5.5",
            "Fable 5.1",
            "Fable 5.2",
            "Sonnet 5.5",
            "Haiku 5.5",
        ];
        assert_eq!(st.models(), menu);
        assert_eq!(load_models(&st.paths.state.join("models.json")), menu);
        let info = tokio::task::spawn_blocking(move || {
            crate::client::Api { addr, token: None }
                .request("GET", "/api/v1/info", None)
                .unwrap()
        })
        .await
        .unwrap();
        assert_eq!(info.1["models"], json!(menu));
        let link = st
            .registry
            .lock()
            .await
            .get(&id)
            .unwrap()
            .claude
            .clone()
            .unwrap();
        assert_eq!(st.model_of(&id, &link, true).as_deref(), Some("Opus 5.5"));
        // Una opción que no está: 409 (y el menú, cerrado).
        let (code, r) = post(addr, url.clone(), json!({"model": "Gemini"})).await;
        assert_eq!(code, 409, "{r}");
        assert_eq!(claude.lock().unwrap().menu, None);
    }

    /// Una petición tal cual (el cuerpo sin pasar por JSON), sin esperar la respuesta: el socket.
    #[cfg(unix)]
    fn raw_request(addr: std::net::SocketAddr, path: &str, body: &str) -> std::net::TcpStream {
        use std::io::Write;
        let mut s = std::net::TcpStream::connect(addr).unwrap();
        write!(
            s,
            "POST {path} HTTP/1.0\r\nHost: {addr}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
        s
    }

    /// La respuesta entera de `raw_request`: código y cuerpo.
    #[cfg(unix)]
    async fn raw(addr: std::net::SocketAddr, path: String, body: &'static str) -> (u16, String) {
        tokio::task::spawn_blocking(move || {
            use std::io::Read;
            let mut s = raw_request(addr, &path, body);
            let mut out = String::new();
            s.read_to_string(&mut out).unwrap();
            let code = out.split_whitespace().nth(1).unwrap().parse().unwrap();
            let body = out.split_once("\r\n\r\n").unwrap().1.to_owned();
            (code, body)
        })
        .await
        .unwrap()
    }

    /// Un cuerpo que no es JSON: 400 con `{"error"}`, como el resto de la API.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn mode_and_model_with_a_body_that_is_not_json() {
        let _limit = crate::watchdog::limit(Duration::from_secs(60));
        let tmp = tempfile::tempdir().unwrap();
        let (_st, addr, id, claude, _guard) = with_fake_claude(tmp.path()).await;
        for what in ["mode", "model"] {
            for body in ["esto no", "{\"mode\": ", ""] {
                let (code, r) = raw(addr, format!("/api/v1/sessions/{id}/{what}"), body).await;
                assert_eq!(code, 400, "{what} {body:?}: {r}");
                let r: serde_json::Value = serde_json::from_str(&r).unwrap_or_default();
                assert!(r["error"].is_string(), "{what} {body:?}: {r}");
            }
        }
        assert!(claude.lock().unwrap().keys.is_empty());
    }

    /// (2) Si se corta la petición a mitad de `/model`, la secuencia sigue hasta el final (no
    /// queda `/model` en la caja ni el menú abierto).
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn a_cut_request_does_not_leave_model_half_done() {
        let _limit = crate::watchdog::limit(Duration::from_secs(60));
        let tmp = tempfile::tempdir().unwrap();
        let (st, addr, id, claude, _guard) = with_fake_claude(tmp.path()).await;
        let url = format!("/api/v1/sessions/{id}/model");
        let sock =
            tokio::task::spawn_blocking(move || raw_request(addr, &url, r#"{"model": "Opus"}"#))
                .await
                .unwrap();
        // En cuanto ha escrito `/model`, se va.
        for _ in 0..200 {
            if !claude.lock().unwrap().keys.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        sock.shutdown(std::net::Shutdown::Both).unwrap();
        drop(sock);
        tokio::time::sleep(Duration::from_secs(5)).await;
        {
            let c = claude.lock().unwrap();
            assert_eq!(
                (c.current, c.menu, c.input.as_str()),
                (1, None, ""),
                "{:?}",
                c.keys
            );
            assert!(!c.saved_default);
        }
        // Y lo apuntó: el modelo elegido y los del menú.
        assert_eq!(st.models().len(), 5);
        let link = st
            .registry
            .lock()
            .await
            .get(&id)
            .unwrap()
            .claude
            .clone()
            .unwrap();
        assert_eq!(st.model_of(&id, &link, true).as_deref(), Some("Opus 5.5"));
    }

    #[tokio::test]
    async fn the_model_of_a_session() {
        let tmp = tempfile::tempdir().unwrap();
        let (st, _, _) = with_mac(tmp.path());
        st.set_model_missing_ttl(Duration::ZERO);
        let config = tmp.path().join("claude");
        let dir = config.join("projects").join("-w");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("conv.jsonl");
        let reply = |model: &str| {
            format!(
                "{}\n",
                json!({"type": "assistant", "message": {"model": model, "content": []}})
            )
        };
        let link = ClaudeLink {
            session_id: "conv".into(),
            config_dir: Some(config),
            account: None,
            inside_shell: false,
        };
        let refresh = || st.refresh_models(vec![("s".into(), link.clone())]);
        // Sin transcripción aún: no se sabe.
        refresh().await;
        assert_eq!(st.model_of("s", &link, true), None);
        std::fs::write(&file, reply("claude-fable-5-2")).unwrap();
        refresh().await;
        assert_eq!(st.model_of("s", &link, true).as_deref(), Some("Fable 5.2"));
        // Puesto por nido: ese, hasta que Claude conteste.
        let at = std::fs::metadata(&file).unwrap().len();
        let chosen = |model: &str| ModelChosen {
            session_id: "conv".into(),
            model: model.into(),
            at,
        };
        st.model_chosen
            .lock()
            .unwrap()
            .insert("s".into(), chosen("Haiku 5.5"));
        assert_eq!(st.model_of("s", &link, true).as_deref(), Some("Haiku 5.5"));
        let mut text = std::fs::read_to_string(&file).unwrap();
        text += &reply("claude-haiku-5-5");
        std::fs::write(&file, &text).unwrap();
        refresh().await;
        assert_eq!(st.model_of("s", &link, true).as_deref(), Some("Haiku 5.5"));
        assert!(st.model_chosen.lock().unwrap().is_empty());
        // Si se para, lo puesto por nido ya no vale (era solo para esa vez).
        st.model_chosen.lock().unwrap().insert(
            "s".into(),
            ModelChosen {
                at: u64::MAX,
                ..chosen("Opus 5.5")
            },
        );
        assert_eq!(st.model_of("s", &link, false).as_deref(), Some("Haiku 5.5"));
    }

    /// (11) De la transcripción se lee solo lo nuevo (las líneas enteras), y si no está no se
    /// vuelve a buscar en cada escaneo.
    #[tokio::test]
    async fn the_model_is_read_from_what_is_new() {
        let tmp = tempfile::tempdir().unwrap();
        let (st, _, _) = with_mac(tmp.path());
        let config = tmp.path().join("claude");
        let dir = config.join("projects").join("-w");
        let file = dir.join("conv.jsonl");
        let reply = |model: &str| {
            format!(
                "{}\n",
                json!({"type": "assistant", "message": {"model": model, "content": []}})
            )
        };
        let link = ClaudeLink {
            session_id: "conv".into(),
            config_dir: Some(config),
            account: None,
            inside_shell: false,
        };
        let refresh = || st.refresh_models(vec![("s".into(), link.clone())]);
        // No está: no se vuelve a buscar hasta que pase el rato.
        refresh().await;
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&file, reply("claude-haiku-5-5")).unwrap();
        refresh().await;
        assert_eq!(st.model_of("s", &link, true), None);
        st.set_model_missing_ttl(Duration::ZERO);
        st.model_seen
            .lock()
            .unwrap()
            .get_mut("s")
            .unwrap()
            .missing_until = None;
        refresh().await;
        assert_eq!(st.model_of("s", &link, true).as_deref(), Some("Haiku 5.5"));
        // Lo de antes no se vuelve a leer: cambiado en su sitio (mismo tamaño), no cuenta…
        let old = std::fs::read_to_string(&file).unwrap();
        std::fs::write(&file, old.replace("claude-haiku-5-5", "claude-fable-5-2")).unwrap();
        let user = format!(
            "{}\n",
            json!({"type": "user", "message": {"content": "hola"}})
        );
        std::fs::write(
            &file,
            old.replace("claude-haiku-5-5", "claude-fable-5-2") + &user,
        )
        .unwrap();
        refresh().await;
        assert_eq!(st.model_of("s", &link, true).as_deref(), Some("Haiku 5.5"));
        // …lo nuevo sí, y una línea a medias espera a estar entera.
        let line = reply("claude-opus-5-5");
        let (half, rest) = line.split_at(20);
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&file)
            .unwrap();
        std::io::Write::write_all(&mut f, half.as_bytes()).unwrap();
        refresh().await;
        assert_eq!(st.model_of("s", &link, true).as_deref(), Some("Haiku 5.5"));
        std::io::Write::write_all(&mut f, rest.as_bytes()).unwrap();
        refresh().await;
        assert_eq!(st.model_of("s", &link, true).as_deref(), Some("Opus 5.5"));
        // Otra conversación: desde el principio.
        std::fs::write(dir.join("otra.jsonl"), reply("claude-sonnet-5-5")).unwrap();
        let other = ClaudeLink {
            session_id: "otra".into(),
            ..link.clone()
        };
        st.refresh_models(vec![("s".into(), other.clone())]).await;
        assert_eq!(
            st.model_of("s", &other, true).as_deref(),
            Some("Sonnet 5.5")
        );
        assert_eq!(st.model_of("s", &link, true), None);
    }

    /// (12) El turno de teclear es de cada sesión: una secuencia larga en una no espera a las
    /// demás.
    #[tokio::test]
    async fn each_session_has_its_own_turn() {
        let tmp = tempfile::tempdir().unwrap();
        let (st, _, _) = with_mac(tmp.path());
        let a = st.turn("a").await;
        assert!(
            tokio::time::timeout(Duration::from_secs(1), st.turn("b"))
                .await
                .is_ok()
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(100), st.turn("a"))
                .await
                .is_err()
        );
        drop(a);
        assert!(
            tokio::time::timeout(Duration::from_secs(1), st.turn("a"))
                .await
                .is_ok()
        );
        // Los que no tiene nadie no se acumulan.
        let _c = st.turn("c").await;
        assert_eq!(st.turns.lock().unwrap().len(), 1);
    }

    /// Una hija abierta desde el Mac que pregunta (`Waiting`) o que terminó y no se ha visto.
    fn child(kind: AlertKind, opened_by: Option<&str>) -> SessionView {
        let mut v = view("hija");
        v.opened_by = opened_by.map(str::to_owned);
        v.unread = kind == AlertKind::Finished;
        let status = if kind == AlertKind::Waiting {
            "waiting"
        } else {
            "idle"
        };
        v.claude = Some(ClaudeView {
            background: None,
            session_id: "s".into(),
            running: true,
            status: Some(status.into()),
            title: None,
            account: None,
            model: None,
            models: vec![],
        });
        v
    }

    fn show(st: &AppState, v: SessionView) {
        st.views.send_replace(Listing {
            sessions: vec![v],
            folders: vec![],
        });
    }

    fn spawn_push(
        st: &Arc<AppState>,
        kind: AlertKind,
    ) -> tokio::task::JoinHandle<Option<(String, String, usize)>> {
        let st = st.clone();
        let a = Alert {
            session: "hija".into(),
            kind,
        };
        tokio::spawn(async move { st.alert_push(&a).await })
    }

    #[tokio::test]
    async fn a_child_alerts_once_its_parent_is_gone() {
        let _limit = crate::watchdog::limit(std::time::Duration::from_secs(60));
        for (kind, body) in [
            (AlertKind::Waiting, "Te necesita"),
            (AlertKind::Finished, "Terminó"),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let (st, answer, asked) = with_mac(dir.path());
            show(&st, child(kind, Some("mac-id/madre")));
            let task = spawn_push(&st, kind);
            // La madre vive: no se avisa, y se vuelve a mirar (sin lo recordado) cada poco.
            tokio::time::sleep(Duration::from_millis(400)).await;
            assert!(
                !task.is_finished(),
                "{kind:?}: se calla mientras viva la madre"
            );
            assert!(asked.load(Ordering::SeqCst) >= 3, "{kind:?}");
            // Ya no vive: sale el aviso.
            *answer.lock().unwrap() = Ok(json!({"state": "stopped"}));
            let got = tokio::time::timeout(Duration::from_secs(5), task)
                .await
                .expect("a tiempo")
                .unwrap();
            let (title, text, _) = got.unwrap_or_else(|| panic!("{kind:?}: sin aviso"));
            assert_eq!((title.as_str(), text.as_str()), ("hija", body));
        }
    }

    #[tokio::test]
    async fn one_quiet_wait_per_session_and_kind() {
        let _limit = crate::watchdog::limit(std::time::Duration::from_secs(60));
        let dir = tempfile::tempdir().unwrap();
        let (st, answer, asked) = with_mac(dir.path());
        show(&st, child(AlertKind::Finished, Some("mac-id/madre")));
        // La hija termina dos veces mientras vive la madre: se espera una sola vez.
        let first = spawn_push(&st, AlertKind::Finished);
        tokio::time::sleep(Duration::from_millis(100)).await;
        let second = spawn_push(&st, AlertKind::Finished);
        let got = tokio::time::timeout(Duration::from_secs(5), second).await;
        assert_eq!(got.expect("enseguida").unwrap(), None);
        // Mientras, una pregunta cada rato (50 ms), no dos.
        let start = std::time::Instant::now();
        let before = asked.load(Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(500)).await;
        let n = asked.load(Ordering::SeqCst) - before;
        let most = (start.elapsed().as_millis() / 50) as usize + 1;
        assert!(
            n >= 1 && n <= most,
            "{n} preguntas en {:?}",
            start.elapsed()
        );
        // Muere la madre: un solo aviso.
        *answer.lock().unwrap() = Ok(json!({"state": "stopped"}));
        let got = tokio::time::timeout(Duration::from_secs(5), first).await;
        assert!(got.expect("a tiempo").unwrap().is_some());
        // Ya no espera nadie: el siguiente sale (y no se queda la marca de la espera).
        st.finished_pushed.lock().unwrap().clear();
        let got =
            tokio::time::timeout(Duration::from_secs(5), spawn_push(&st, AlertKind::Finished))
                .await
                .expect("a tiempo")
                .unwrap();
        assert!(got.is_some());
        // Tampoco si se cancela la espera a medias.
        *answer.lock().unwrap() = Ok(json!({"state": "working"}));
        let cut = spawn_push(&st, AlertKind::Finished);
        tokio::time::sleep(Duration::from_millis(100)).await;
        cut.abort();
        let _ = cut.await;
        *answer.lock().unwrap() = Ok(json!({"state": "stopped"}));
        st.finished_pushed.lock().unwrap().clear();
        let got =
            tokio::time::timeout(Duration::from_secs(5), spawn_push(&st, AlertKind::Finished))
                .await
                .expect("a tiempo")
                .unwrap();
        assert!(got.is_some());
    }

    #[tokio::test]
    async fn a_quiet_alert_is_dropped_once_it_is_no_longer_needed() {
        let _limit = crate::watchdog::limit(std::time::Duration::from_secs(60));
        let dir = tempfile::tempdir().unwrap();
        let (st, _, _) = with_mac(dir.path());
        show(&st, child(AlertKind::Waiting, Some("mac-id/madre")));
        let task = spawn_push(&st, AlertKind::Waiting);
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(!task.is_finished());
        // Ya no pregunta (le contestó la madre): no hay nada que avisar.
        let mut busy = child(AlertKind::Waiting, Some("mac-id/madre"));
        busy.claude.as_mut().unwrap().status = Some("busy".into());
        show(&st, busy);
        let got = tokio::time::timeout(Duration::from_secs(5), task).await;
        assert_eq!(got.expect("a tiempo").unwrap(), None);
    }

    #[tokio::test]
    async fn alerts_without_a_live_parent_go_out_at_once() {
        let _limit = crate::watchdog::limit(std::time::Duration::from_secs(60));
        for kind in [AlertKind::Waiting, AlertKind::Finished] {
            // Del usuario: sin preguntar a nadie.
            let dir = tempfile::tempdir().unwrap();
            let (st, answer, asked) = with_mac(dir.path());
            show(&st, child(kind, None));
            let got = spawn_push(&st, kind).await.unwrap();
            assert!(got.is_some(), "{kind:?}");
            assert_eq!(asked.load(Ordering::SeqCst), 0);
            // Madre parada, que no contesta o de un equipo que ya no es del grupo.
            for (by, r) in [
                ("mac-id/madre", Ok(json!({"state": "stopped"}))),
                (
                    "mac-id/otra",
                    Err((502, "sin conexión o versión antigua".into())),
                ),
                ("mac-id/tercera", Err((404, "no existe esa sesión".into()))),
                ("viejo-id/madre", Ok(json!({"state": "working"}))),
            ] {
                *answer.lock().unwrap() = r;
                st.finished_pushed.lock().unwrap().clear();
                show(&st, child(kind, Some(by)));
                let got = tokio::time::timeout(Duration::from_secs(5), spawn_push(&st, kind))
                    .await
                    .expect("a tiempo")
                    .unwrap();
                assert!(got.is_some(), "{kind:?} {by}");
            }
        }
    }

    #[test]
    fn a_sessions_own_alerts_are_never_quiet() {
        assert!(AppState::quiet_while_parent(AlertKind::Waiting));
        assert!(AppState::quiet_while_parent(AlertKind::Finished));
        assert!(!AppState::quiet_while_parent(AlertKind::Requested));
    }

    /// Al borrar una sesión se van su pregunta, sus secretos sin recordar y sus envíos; los
    /// recordados del equipo se quedan.
    #[tokio::test(flavor = "multi_thread")]
    async fn deleting_a_session_drops_its_secrets_items_ask_and_plan() {
        let _limit = crate::watchdog::limit(Duration::from_secs(60));
        let tmp = tempfile::tempdir().unwrap();
        let (st, _, _) = with_mac(tmp.path());
        let session = Session {
            id: new_id(),
            name: "api".into(),
            cwd: tmp.path().to_owned(),
            argv: vec!["true".into()],
            env: vec![],
            autostart: false,
            created: 0,
            claude: None,
            folder: None,
            finished: 0,
            stopped: 0,
            seen: 0,
            opened_by: None,
            alerts: vec![],
        };
        let id = st.registry.lock().await.add(session).unwrap().id;
        let own = st.secrets.save_session(&id, "tok", b"v").unwrap();
        let team = st
            .secrets
            .save_team("gh", b"v", crate::secrets::Remember::Always, "api", 1)
            .unwrap();
        st.outbox.add_text(&id, "hola", None, 1).unwrap();
        let (mut waiter, _) = st.asks.join("npm", "x", &id, 1).unwrap();
        let plan: crate::plan::SetArgs =
            serde_json::from_value(json!({"phases": [{"id": "f1", "name": "Uno"}]})).unwrap();
        st.plans.set(&id, plan, 1).unwrap();
        let plan_file = st.paths.session_state(&id).join("plan.json");
        assert!(plan_file.exists());
        delete_session(&st, &id).await.unwrap();
        assert_eq!(waiter.wait().await.0, crate::secrets::Outcome::Closed);
        assert!(!own.exists() && !own.parent().unwrap().exists());
        assert!(team.exists());
        assert!(st.outbox.list(&id).is_empty());
        assert!(!st.paths.state.join("outbox").join(&id).exists());
        // Su plan, fuera: del disco y de memoria.
        assert!(!plan_file.exists());
        assert!(st.plans.summary(&id).is_none());
        assert!(st.plans.get(&id).is_none());
        // Y un plan_set que llegue tarde no lo vuelve a crear.
        let late: crate::plan::SetArgs =
            serde_json::from_value(json!({"phases": [{"id": "f1", "name": "Uno"}]})).unwrap();
        assert_eq!(
            st.plans.set(&id, late, 2).unwrap_err(),
            "la sesión ya no existe"
        );
        assert!(!st.paths.session_state(&id).exists());
    }

    #[test]
    fn a_pending_secret_counts_for_the_badge() {
        let mut v = view("a1");
        assert_eq!(attention(&[v.clone()]), 0);
        v.secret = Some(crate::secrets::AskView {
            id: "q1".into(),
            name: "tok".into(),
            reason: "x".into(),
            since: 1,
            until: 2,
        });
        assert_eq!(attention(&[v]), 1);
    }

    #[test]
    fn the_session_view_carries_its_pending_secret() {
        let mut v = view("a1");
        let json = serde_json::to_value(&v).unwrap();
        assert!(
            json.get("secret").is_none(),
            "sin pregunta, sin la clave: {json}"
        );
        v.secret = Some(crate::secrets::AskView {
            id: "q1".into(),
            name: "tok".into(),
            reason: "subir".into(),
            since: 1,
            until: 2,
        });
        let json = serde_json::to_value(&v).unwrap();
        assert_eq!(json["secret"]["name"], "tok");
        assert_eq!(json["secret"]["reason"], "subir");
        let keys: Vec<&String> = json["secret"].as_object().unwrap().keys().collect();
        assert_eq!(keys, ["id", "name", "reason", "since", "until"], "{json}");
        assert!(json["secret"].get("value").is_none());
    }

    #[test]
    fn attention_counts_unread_alerts() {
        let mut a = view("a");
        a.alerts.push(AlertRec {
            text: "listo".into(),
            at: 5,
            kind: None,
            item: None,
        });
        let b = view("b");
        assert!(still_alerting(&a, AlertKind::Requested));
        assert!(!still_alerting(&b, AlertKind::Requested));
        assert_eq!(attention(&[a, b]), 1);
    }

    #[test]
    fn only_alerts_after_seen_are_unread() {
        let alerts = vec![
            AlertRec {
                text: "vieja".into(),
                at: 10,
                kind: None,
                item: None,
            },
            AlertRec {
                text: "nueva".into(),
                at: 30,
                kind: None,
                item: None,
            },
        ];
        let unread = unread_alerts(&alerts, 20);
        assert_eq!(unread.len(), 1);
        assert_eq!(unread[0].text, "nueva");
        assert!(unread_alerts(&alerts, 30).is_empty());
    }

    /// Un arranque que se mira en `keep_held` (ya visto su Claude) y se reinicia: el guardián
    /// nuevo aún sin Claude no es «su Claude se fue». Acaba sin tocar la bandeja.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn keep_held_leaves_a_restarted_session_alone() {
        let _limit = crate::watchdog::limit(Duration::from_secs(60));
        left_alone_after_restart(false).await;
    }

    /// Igual con el mismo pid (Windows los reutiliza enseguida): lo distingue cuándo arrancó.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn keep_held_tells_a_restart_with_the_same_pid() {
        let _limit = crate::watchdog::limit(Duration::from_secs(60));
        left_alone_after_restart(true).await;
    }

    /// Ver `keep_held_leaves_a_restarted_session_alone`; `same_pid`: el guardián nuevo tiene el
    /// pid del de antes (y su Claude sigue ahí).
    #[cfg(unix)]
    async fn left_alone_after_restart(same_pid: bool) {
        use std::os::unix::process::CommandExt;
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        let paths = Paths {
            runtime: d.join("run"),
            data: d.join("data"),
            state: d.join("state"),
            config: d.join("config"),
            home: d.to_owned(),
            server: None,
        };
        std::fs::create_dir_all(&paths.runtime).unwrap();
        let id = "s";
        // Su guardián atiende sin contestar (`look` tarda).
        let listener = tokio::net::UnixListener::bind(paths.socket(id)).unwrap();
        let mute = tokio::spawn(async move {
            let mut open = Vec::new();
            while let Ok((c, _)) = listener.accept().await {
                open.push(c);
            }
        });
        // El guardián de antes: esta prueba, con un «claude» debajo (bash por un enlace).
        std::os::unix::fs::symlink("/bin/bash", d.join("claude")).unwrap();
        let mut claude = std::process::Command::new(d.join("claude"))
            .args(["-c", "sleep 60 & wait"])
            .process_group(0)
            .spawn()
            .unwrap();
        std::fs::write(paths.pidfile(id), std::process::id().to_string()).unwrap();
        let inbox = crate::inbox::Inbox::load(d.join("inbox.json"));
        let task = tokio::spawn(keep_held(
            paths.clone(),
            None,
            None,
            inbox.hold(id),
            "s".into(),
            id.into(),
        ));
        // Que vea su Claude.
        tokio::time::sleep(Duration::from_millis(2500)).await;
        // Reiniciada: otro guardián, aún sin Claude; o con el mismo pid, escrito después.
        let mut new = (!same_pid).then(|| {
            std::process::Command::new("sleep")
                .arg("60")
                .spawn()
                .unwrap()
        });
        let pid = new.as_ref().map_or(std::process::id(), |c| c.id());
        let pidfile = paths.pidfile(id);
        let before = std::fs::metadata(&pidfile).unwrap().modified().unwrap();
        std::fs::write(&pidfile, pid.to_string()).unwrap();
        std::fs::File::options()
            .write(true)
            .open(&pidfile)
            .unwrap()
            .set_modified(before + Duration::from_secs(2))
            .unwrap();
        let ended = tokio::time::timeout(Duration::from_secs(20), task).await;
        // Lo que se ve con el de antes tampoco vale (aún atiende, y su Claude sigue): es otro.
        let old = (std::process::id() as i32, Some(before));
        let seen_before = claude_with(&paths, id, old).await;
        let now = launch::holder_id(&paths, id).unwrap();
        let seen_now = claude_with(&paths, id, now).await;
        unsafe {
            libc::kill(-(claude.id() as i32), libc::SIGKILL);
        }
        let _ = claude.wait();
        if let Some(n) = new.as_mut() {
            let _ = n.kill();
            let _ = n.wait();
        }
        mute.abort();
        assert!(ended.is_ok(), "sigue mirando el arranque nuevo");
        assert_eq!(seen_before, Present::Replaced);
        let expected = if same_pid {
            Present::Claude
        } else {
            Present::NoClaude
        };
        assert_eq!(seen_now, expected);
        assert_eq!(inbox.refusal(id), None, "rechazó el arranque nuevo");
        assert!(!inbox.held(id));
    }

    #[test]
    fn keep_held_looks_less_often_after_a_minute() {
        assert_eq!(keep_pace(Duration::ZERO), Duration::from_secs(1));
        assert_eq!(keep_pace(Duration::from_secs(59)), Duration::from_secs(1));
        assert_eq!(keep_pace(Duration::from_secs(60)), Duration::from_secs(5));
        assert_eq!(keep_pace(Duration::from_secs(3600)), Duration::from_secs(5));
    }

    #[tokio::test]
    async fn a_long_silence_is_said_once() {
        let mut c = SlowCount::default();
        for _ in 1..SLOW_SAY {
            assert!(!c.slow());
        }
        assert!(c.slow(), "a la vez {SLOW_SAY}");
        // Una vez, aunque siga.
        for _ in 0..3 * SLOW_SAY {
            assert!(!c.slow());
        }
        // Seguidas: si contesta, se empieza a contar de nuevo (pero ya no se vuelve a decir).
        let mut c = SlowCount::default();
        for _ in 1..SLOW_SAY {
            assert!(!c.slow());
        }
        c.ok();
        for _ in 1..SLOW_SAY {
            assert!(!c.slow());
        }
        assert!(c.slow());
    }

    #[tokio::test]
    async fn the_check_before_releasing_waits_for_a_slow_holder() {
        let _limit = crate::watchdog::limit(Duration::from_secs(60));
        let repl = include_str!("../tests/pantallas/normal.txt");
        let settled = || Some(tokio::time::Instant::now() - Duration::from_secs(5));
        // Su guardián sigue ahí pero no contesta a tiempo: retenida, y otra conexión.
        let (a, b) = tokio::io::duplex(1 << 16);
        let (rd, wr) = tokio::io::split(a);
        let mut s = prompt::Screen::new(rd, wr);
        assert_eq!(ready(&mut s, repl, &mut settled()).await, Ready::Reconnect);
        drop(b);
        // Cerrado de verdad: se suelta.
        let (a, b) = tokio::io::duplex(1 << 16);
        drop(b);
        let (rd, wr) = tokio::io::split(a);
        let mut s = prompt::Screen::new(rd, wr);
        assert_eq!(ready(&mut s, repl, &mut settled()).await, Ready::Release);
        // Sin la caja un rato, ni se pregunta.
        let (a, _b) = tokio::io::duplex(1 << 16);
        let (rd, wr) = tokio::io::split(a);
        let mut s = prompt::Screen::new(rd, wr);
        assert_eq!(ready(&mut s, repl, &mut None).await, Ready::Wait);
    }

    #[tokio::test]
    async fn restore_waits_for_the_registration_to_really_end() {
        let _limit = crate::watchdog::limit(Duration::from_secs(60));
        let ms = Duration::from_millis;
        assert_eq!(
            registration_done(async {}, ms(50), ms(500)).await,
            Registered::InTime
        );
        // Pasa del aviso: se dice, y se sigue esperando a que acabe de verdad.
        let start = std::time::Instant::now();
        let slow = tokio::time::sleep(ms(200));
        assert_eq!(
            registration_done(slow, ms(50), ms(1000)).await,
            Registered::Late
        );
        assert!(start.elapsed() >= ms(200));
        // Hasta un tope.
        let start = std::time::Instant::now();
        let never = std::future::pending::<()>();
        assert_eq!(
            registration_done(never, ms(50), ms(300)).await,
            Registered::GaveUp
        );
        assert!(start.elapsed() >= ms(300) && start.elapsed() < ms(2000));
    }

    #[test]
    fn test_nidos_do_not_register_the_mcp() {
        assert!(registers_mcp(None, None));
        assert!(registers_mcp(None, Some("0".into())));
        // Con `NIDO_HOME` (pruebas, demos), no: tocaría el Claude de verdad del usuario.
        assert!(!registers_mcp(Some("/tmp/nido-a".into()), None));
        assert!(!registers_mcp(Some("/tmp/nido-a".into()), Some("0".into())));
        assert!(registers_mcp(Some("/tmp/nido-a".into()), Some("1".into())));
    }

    #[test]
    fn throttle_one_push_every_30_s() {
        let mut t = AlertThrottle::default();
        let t0 = Instant::now();
        assert_eq!(t.remaining("s", t0), Duration::ZERO);
        assert!(t.allow("s", t0));
        assert!(!t.allow("s", t0 + Duration::from_secs(10)));
        assert_eq!(
            t.remaining("s", t0 + Duration::from_secs(10)),
            Duration::from_secs(20)
        );
        // Otra sesión no espera.
        assert!(t.allow("otra", t0 + Duration::from_secs(10)));
        assert!(!t.allow("s", t0 + Duration::from_secs(29)));
        assert!(t.allow("s", t0 + Duration::from_secs(30)));
        assert!(!t.allow("s", t0 + Duration::from_secs(31)));
    }

    #[test]
    fn throttle_offer_sequence() {
        let mut t = AlertThrottle::default();
        let t0 = Instant::now();
        let at = |s| t0 + Duration::from_secs(s);
        let took = |t: &mut AlertThrottle| t.take("s").map(|h| (h.count, h.text));
        assert_eq!(t.offer("s", None, "una", t0), Offer::Now);
        assert_eq!(took(&mut t), Some((1, "una".to_owned())));
        assert_eq!(
            t.offer("s", None, "dos", at(10)),
            Offer::Schedule(Duration::from_secs(20))
        );
        assert_eq!(t.offer("s", None, "tres", at(12)), Offer::Joined);
        assert_eq!(t.pending("s"), 2);
        // Otra sesión, por su cuenta.
        assert_eq!(t.offer("otra", None, "x", at(12)), Offer::Now);
        assert_eq!(took(&mut t), Some((2, "tres".to_owned())));
        assert_eq!(t.pending("s"), 0);
        assert_eq!(took(&mut t), None);
        assert_eq!(
            t.offer("s", None, "cuatro", at(20)),
            Offer::Schedule(Duration::from_secs(10))
        );
        // Si sale ya, se lleva también las que esperaban.
        assert_eq!(t.offer("s", None, "cinco", at(30)), Offer::Now);
        assert_eq!(took(&mut t), Some((2, "cinco".to_owned())));
    }

    #[test]
    fn mixed_kinds_are_not_only_alerts_and_a_secret_stamps_the_throttle() {
        let mut t = AlertThrottle::default();
        let t0 = Instant::now();
        t.stamp("s", t0);
        assert!(matches!(
            t.offer("s", Some(NoteKind::Clipboard), "orden", t0),
            Offer::Schedule(_)
        ));
        t.offer("s", None, "listo", t0);
        let h = t.take("s").unwrap();
        assert_eq!((h.count, h.only_alerts, h.kind), (2, false, None));
    }

    #[test]
    fn a_secret_push_while_others_wait_puts_them_off() {
        let mut t = AlertThrottle::default();
        let t0 = Instant::now();
        let at = |s| t0 + Duration::from_secs(s);
        assert_eq!(t.offer("s", None, "una", t0), Offer::Now);
        t.take("s");
        assert_eq!(
            t.offer("s", Some(NoteKind::File), "a.pdf", at(10)),
            Offer::Schedule(Duration::from_secs(20))
        );
        // Sale el push de un secreto antes de que le toque: espera otros 30 s, no se pierde.
        t.stamp("s", at(25));
        assert_eq!(t.due("s", at(30)), Some(Duration::from_secs(25)));
        assert_eq!(t.pending("s"), 1);
        assert_eq!(t.due("s", at(55)), None);
        assert_eq!(t.take("s").map(|h| h.count), Some(1));
        // Sin ninguna esperando, nada (y no cuenta como push).
        assert_eq!(t.due("s", at(90)), None);
        assert_eq!(t.offer("s", None, "otra", at(90)), Offer::Now);
    }

    #[test]
    fn push_text_by_kind_and_count() {
        let h = |count, kind, text: &str, only_alerts| Held {
            count,
            kind,
            text: text.into(),
            only_alerts,
        };
        let p = |h: Held| push_text("api", "mac", &h);
        assert_eq!(
            p(h(1, None, "listo", true)),
            ("🔔 api (mac)".into(), "listo".into())
        );
        assert_eq!(
            p(h(3, None, "tres", true)),
            ("🔔 api (mac)".into(), "3 alertas: tres".into())
        );
        assert_eq!(
            p(h(1, Some(NoteKind::Clipboard), "la orden", false)),
            ("📋 api (mac)".into(), "la orden".into())
        );
        assert_eq!(
            p(h(1, Some(NoteKind::File), "informe.pdf", false)),
            ("📎 api (mac)".into(), "informe.pdf".into())
        );
        assert_eq!(
            p(h(3, Some(NoteKind::File), "x", false)),
            ("🔔 api (mac)".into(), "3 envíos de api".into())
        );
    }

    #[test]
    fn alert_text_is_trimmed_and_cut() {
        assert_eq!(alert_text("  hola \n"), Some("hola".to_owned()));
        assert_eq!(alert_text("   "), None);
        let long = "á".repeat(600);
        assert_eq!(alert_text(&long).unwrap().chars().count(), 500);
        // El corte puede dejar un espacio al final: fuera también.
        let cut = format!("{} {}", "a".repeat(499), "b".repeat(10));
        assert_eq!(alert_text(&cut), Some("a".repeat(499)));
    }

    #[test]
    fn alert_text_has_no_control_characters() {
        // Saltos de línea y tabuladores, sí.
        assert_eq!(alert_text("uno\n\tdos"), Some("uno\n\tdos".to_owned()));
        // Colores, títulos de ventana (OSC con BEL o con ST), cursor, borrar pantalla: fuera
        // enteros, sin dejar restos.
        assert_eq!(
            alert_text("\u{1b}[1;31mrojo\u{1b}[0m y \u{1b}]0;título\u{7}nada"),
            Some("rojo y nada".to_owned())
        );
        assert_eq!(
            alert_text("a\u{1b}]8;;https://x\u{1b}\\enlace\u{1b}]8;;\u{1b}\\b"),
            Some("aenlaceb".to_owned())
        );
        assert_eq!(
            alert_text("\u{1b}[2J\u{1b}[H\u{1b}7hola\u{1b}8"),
            Some("hola".to_owned())
        );
        // Otros C0, DEL y C1 (también el CSI de un byte).
        assert_eq!(
            alert_text("a\u{0}b\u{7}c\rd\u{7f}e\u{85}f\u{9b}31mg"),
            Some("abcdef31mg".to_owned())
        );
        // Solo control: vacía.
        assert_eq!(alert_text("\u{1b}[0m\u{7}"), None);
        // Un ESC suelto al final no rompe nada.
        assert_eq!(alert_text("fin\u{1b}"), Some("fin".to_owned()));
    }

    // ---------- Buscar y el chat alrededor de un mensaje ----------

    /// Un nido en `dir` con su API en un puerto propio y una sesión «trabajo» (carpeta «web»)
    /// con un Claude cuya transcripción es `transcript`.
    async fn with_transcript(
        dir: &std::path::Path,
        transcript: &str,
    ) -> (Arc<AppState>, std::net::SocketAddr, String) {
        let paths = Paths {
            runtime: dir.join("run"),
            data: dir.join("data"),
            state: dir.join("state"),
            config: dir.join("config"),
            home: dir.to_owned(),
            server: None,
        };
        let st = AppState::new(paths, Launcher::Detached, None, true).unwrap();
        let project = dir.join("claude").join("projects").join("-w");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join("conv.jsonl"), transcript).unwrap();
        let session = Session {
            id: new_id(),
            name: "trabajo".into(),
            cwd: dir.to_owned(),
            argv: vec!["claude".into()],
            env: vec![],
            autostart: false,
            created: now(),
            claude: Some(ClaudeLink {
                session_id: "conv".into(),
                config_dir: Some(dir.join("claude")),
                account: None,
                inside_shell: false,
            }),
            folder: Some("Web".into()),
            finished: 0,
            stopped: 0,
            seen: 0,
            opened_by: None,
            alerts: vec![],
        };
        let id = st.registry.lock().await.add(session).unwrap().id;
        st.scan().await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = router(st.clone()).into_make_service_with_connect_info::<std::net::SocketAddr>();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (st, addr, id)
    }

    async fn get_json(addr: std::net::SocketAddr, path: &str) -> (u16, serde_json::Value) {
        let path = path.to_owned();
        tokio::task::spawn_blocking(move || {
            crate::client::Api { addr, token: None }
                .request_with("GET", &path, None, &[], None)
                .unwrap()
        })
        .await
        .unwrap()
    }

    /// `DELETE …/items/{item}`: fuera para todos (de la lista y su contenido) y sus notas; la
    /// lista de sesiones lo dice con `items_rev`. Ya quitado, o uno que no existe: 404.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_dismiss_item_route() {
        let _limit = crate::watchdog::limit(Duration::from_secs(60));
        let tmp = tempfile::tempdir().unwrap();
        let line = said_line("user", "u1", "hola", "2026-11-12T10:00:00Z");
        let (st, addr, id) = with_transcript(tmp.path(), &line).await;
        let a = st.outbox.add_text(&id, "uno", None, 1).unwrap();
        let b = st.outbox.add_text(&id, "dos", None, 2).unwrap();
        st.registry
            .lock()
            .await
            .add_note(
                &id,
                "uno",
                now_ms(),
                Some(NoteKind::Clipboard),
                Some(a.id.clone()),
            )
            .unwrap();
        let rev = |l: &serde_json::Value| l[0]["items_rev"].as_u64().unwrap_or(0);
        let (_, list) = get_json(addr, "/api/v1/sessions").await;
        let before = rev(&list);
        assert!(before > 0, "{list}");
        assert_eq!(list[0]["alerts"][0]["item"], a.id.as_str());
        let delete = |path: String| async move {
            tokio::task::spawn_blocking(move || {
                crate::client::Api { addr, token: None }
                    .request_with("DELETE", &path, None, &[], None)
                    .unwrap()
            })
            .await
            .unwrap()
        };
        let url = format!("/api/v1/sessions/{id}/items/{}", a.id);
        assert_eq!(delete(url.clone()).await, (200, json!({"ok": true})));
        let (code, r) = get_json(addr, &format!("/api/v1/sessions/{id}/items")).await;
        assert_eq!(code, 200);
        let ids: Vec<&str> = r["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, [b.id.as_str()]);
        let (code, r) = get_json(addr, &url).await;
        assert_eq!((code, r["error"].as_str()), (404, Some("no such item")));
        // Los demás dispositivos lo saben por la lista de sesiones; y su nota ya no sale.
        let (_, list) = get_json(addr, "/api/v1/sessions").await;
        assert!(rev(&list) > before, "{list}");
        assert!(
            list[0].get("alerts").is_none_or(|a| a == &json!([])),
            "{list}"
        );
        // Otra vez, uno que no existe o de una sesión que no existe: 404.
        let (code, r) = delete(url.clone()).await;
        assert_eq!((code, r["error"].as_str()), (404, Some("no such item")));
        let (code, _) = delete(format!("/api/v1/sessions/{id}/items/0a1b2c3d")).await;
        assert_eq!(code, 404);
        let (code, _) = delete(format!("/api/v1/sessions/nadie/items/{}", b.id)).await;
        assert_eq!(code, 404);
        // Una ruta que solo se pide (como `…/items/{item}` en un nido de antes): 405. La web y la
        // app lo esconden entonces solo en ese dispositivo.
        let (code, _) = delete(format!("/api/v1/sessions/{id}/items")).await;
        assert_eq!(code, 405);
        // El otro sigue ahí.
        let (code, r) = get_json(addr, &format!("/api/v1/sessions/{id}/items/{}", b.id)).await;
        assert_eq!((code, r), (200, json!("dos")));
    }

    // ---------- El plan de la sesión ----------

    /// El resumen del plan va en la lista de sesiones, con un `rev` que sube con cada cambio; el
    /// plan entero, con sus horas, en `GET …/plan`; y tras reiniciar nido sigue ahí.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_plan_in_the_list_and_its_route() {
        let _limit = crate::watchdog::limit(Duration::from_secs(60));
        let tmp = tempfile::tempdir().unwrap();
        let line = said_line("user", "u1", "hola", "2026-11-12T10:00:00Z");
        let (st, addr, id) = with_transcript(tmp.path(), &line).await;
        // Sin plan: no va el campo, y el plan da 404.
        let (_, list) = get_json(addr, "/api/v1/sessions").await;
        assert!(list[0].get("plan").is_none(), "{list}");
        let url = format!("/api/v1/sessions/{id}/plan");
        let (code, r) = get_json(addr, &url).await;
        assert_eq!(
            (code, r["error"].as_str()),
            (404, Some("this session has no plan"))
        );
        let (code, r) = get_json(addr, "/api/v1/sessions/nadie/plan").await;
        assert_eq!(
            (code, r["error"].as_str()),
            (404, Some("there is no session \"nadie\""))
        );

        let args: crate::plan::SetArgs = serde_json::from_value(json!({
            "title": "Migrar",
            "phases": [
                {"id": "f1", "name": "Uno", "tasks": [
                    {"id": "t1", "name": "Leer", "status": "done"},
                    {"id": "t2", "name": "Escribir", "status": "in_progress"}]},
                {"id": "f2", "name": "Dos", "tasks": [{"id": "t3", "name": "Probar"}]}]
        }))
        .unwrap();
        st.plans.set(&id, args, 1000).unwrap();
        plan_changed(&st);
        let (_, list) = get_json(addr, "/api/v1/sessions").await;
        let plan = &list[0]["plan"];
        assert_eq!(plan["title"], "Migrar", "{list}");
        assert_eq!(
            [&plan["done"], &plan["total"], &plan["active"]],
            [1, 3, 1],
            "{list}"
        );
        assert_eq!([&plan["blocked"], &plan["failed"]], [0, 0]);
        assert_eq!(plan["current"], "Escribir");
        let rev = plan["rev"].as_u64().unwrap();

        // Un cambio de una tarea: sube `rev`.
        let t: crate::plan::TaskArgs =
            serde_json::from_value(json!({"id": "t2", "status": "done"})).unwrap();
        st.plans.update_task(&id, t, 2000).unwrap();
        plan_changed(&st);
        let (_, list) = get_json(addr, "/api/v1/sessions").await;
        let plan = &list[0]["plan"];
        assert!(plan["rev"].as_u64().unwrap() > rev, "{list}");
        assert_eq!(plan["done"], 2);
        assert!(plan.get("current").is_none(), "{list}");

        // El plan entero, con horas (ms).
        let (code, r) = get_json(addr, &url).await;
        assert_eq!(code, 200, "{r}");
        assert_eq!(r["title"], "Migrar");
        assert_eq!(r["updated"], 2000);
        let t2 = &r["phases"][0]["tasks"][1];
        assert_eq!(t2["id"], "t2");
        assert_eq!(t2["status"], "done");
        assert_eq!(
            (&t2["started"], &t2["finished"]),
            (&json!(1000), &json!(2000))
        );
        let t3 = &r["phases"][1]["tasks"][0];
        assert_eq!(t3["status"], "pending");
        assert!(t3.get("started").is_none() && t3.get("finished").is_none());

        // Como tras reiniciar `nido serve`: otro estado sobre la misma carpeta lo lee del disco.
        let again = AppState::new(st.paths.clone(), Launcher::Detached, None, true).unwrap();
        again.scan().await;
        let v = again
            .views
            .borrow()
            .sessions
            .iter()
            .find(|v| v.id == id)
            .cloned()
            .unwrap();
        let p = v.plan.expect("el plan, tras reiniciar");
        assert_eq!(
            (p.title.as_deref(), p.done, p.total),
            (Some("Migrar"), 2, 3)
        );
    }

    fn said_line(kind: &str, uuid: &str, text: &str, ts: &str) -> String {
        let v = if kind == "user" {
            json!({"type":"user","uuid":uuid,"timestamp":ts,"message":{"content":text}})
        } else {
            json!({"type":"assistant","uuid":uuid,"timestamp":ts,"message":{"content":[{"type":"text","text":text}]}})
        };
        format!("{v}\n")
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_search_route() {
        let _limit = crate::watchdog::limit(Duration::from_secs(60));
        let tmp = tempfile::tempdir().unwrap();
        let first = said_line(
            "user",
            "u1",
            "¿Dónde está el CÓDIGO?",
            "2026-11-12T10:00:00Z",
        );
        let text = first.clone()
            + &said_line(
                "assistant",
                "a1",
                "El código está en src/",
                "2026-11-12T10:00:05Z",
            );
        let (_st, addr, id) = with_transcript(tmp.path(), &text).await;
        // Menos de 2 caracteres: 400.
        for q in ["", "a", "%20%20x%20", "%C3%A9"] {
            let (code, r) = get_json(addr, &format!("/api/v1/search?q={q}")).await;
            assert_eq!(code, 400, "{q}: {r}");
            assert_eq!(r["error"], "search for at least 2 characters");
        }
        let (code, r) = get_json(addr, "/api/v1/search?q=codigo").await;
        assert_eq!(code, 200, "{r}");
        assert_eq!(r["truncated"], false);
        assert_eq!(r["sessions"], json!([]));
        assert_eq!(
            r["messages"],
            json!([
                {"session": id, "sid": "conv", "at": first.len(),
                 "id": "a1", "ts": crate::search::ts_ms("2026-11-12T10:00:05Z"),
                 "role": "assistant", "text": "El código está en src/", "hits": [[3, 9]]},
                {"session": id, "sid": "conv", "at": 0, "id": "u1",
                 "ts": crate::search::ts_ms("2026-11-12T10:00:00Z"),
                 "role": "user", "text": "¿Dónde está el CÓDIGO?", "hits": [[15, 21]]},
            ])
        );
        // `limit`, y las sesiones por su nombre, carpeta o directorio.
        let (code, r) = get_json(addr, "/api/v1/search?q=C%C3%B3digo&limit=1").await;
        assert_eq!(code, 200);
        assert_eq!(r["messages"].as_array().unwrap().len(), 1);
        assert_eq!(r["messages"][0]["id"], "a1");
        let (_, r) = get_json(addr, "/api/v1/search?q=TRAB").await;
        assert_eq!(
            (r["sessions"].clone(), r["messages"].clone()),
            (json!([id]), json!([]))
        );
        let (_, r) = get_json(addr, "/api/v1/search?q=web").await;
        assert_eq!(r["sessions"], json!([id]));
        // Un `limit` que no es un número: 400.
        let (code, _) = get_json(addr, "/api/v1/search?q=codigo&limit=x").await;
        assert_eq!(code, 400);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_chat_around_a_message() {
        let _limit = crate::watchdog::limit(Duration::from_secs(60));
        let tmp = tempfile::tempdir().unwrap();
        let mut text = String::new();
        let mut starts = Vec::new();
        for i in 0..20 {
            starts.push(text.len() as u64);
            text += &said_line("user", &format!("u{i}"), &format!("mensaje {i}"), "");
        }
        let (_st, addr, id) = with_transcript(tmp.path(), &text).await;
        let chat = |q: String| {
            let id = id.clone();
            async move { get_json(addr, &format!("/api/v1/sessions/{id}/chat?{q}")).await }
        };
        let ids = |r: &serde_json::Value| -> Vec<String> {
            r["items"]
                .as_array()
                .unwrap()
                .iter()
                .map(|i| i["id"].as_str().unwrap().to_owned())
                .collect()
        };
        // Sin `around`, como siempre (y cada mensaje con su `at`).
        let (code, r) = chat("limit=2".into()).await;
        assert_eq!(code, 200);
        assert_eq!(ids(&r), ["u18", "u19"]);
        assert_eq!(r["items"][0]["at"], starts[18]);
        assert_eq!(r["truncated"], true);
        assert_eq!(r["offset"], text.len());
        // Alrededor de «mensaje 10».
        let (code, r) = chat(format!("around={}&before=2&after=3&sid=conv", starts[10])).await;
        assert_eq!(code, 200, "{r}");
        assert_eq!(ids(&r), ["u8", "u9", "u10", "u11", "u12"]);
        assert_eq!(r["session_id"], "conv");
        assert_eq!(r["start"], starts[8]);
        assert_eq!(r["offset"], starts[13]);
        assert_eq!(
            (r["more_before"].clone(), r["more_after"].clone()),
            (json!(true), json!(true))
        );
        assert_eq!(
            (r["truncated"].clone(), r["reset"].clone()),
            (json!(true), json!(false))
        );
        assert_eq!(r["results"], json!([]));
        // Más arriba y más abajo.
        let (_, up) = chat(format!("around={}&before=3&after=0", starts[8])).await;
        assert_eq!(ids(&up), ["u5", "u6", "u7"]);
        let (_, down) = chat(format!("around={}&before=0&after=30", starts[13])).await;
        assert_eq!(ids(&down).len(), 7);
        assert_eq!(down["more_after"], false);
        assert_eq!(down["offset"], text.len());
        // Por defecto, 30 a cada lado.
        let (_, r) = chat(format!("around={}", starts[10])).await;
        assert_eq!(ids(&r).len(), 20);
        // Otra conversación: 409.
        let (code, r) = chat(format!("around={}&sid=otra", starts[10])).await;
        assert_eq!(code, 409, "{r}");
        // Un `at` que no es de ningún mensaje: lo que haya desde ahí.
        let (code, r) = chat(format!("around={}&before=0&after=1", starts[10] + 1)).await;
        assert_eq!((code, ids(&r)), (200, vec!["u11".to_owned()]));
    }
}
