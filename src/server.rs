//! Gestor: registro, restauración, estado de cada sesión y API web.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

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
    /// Contestar un menú de Claude es teclear paso a paso: de uno en uno.
    pub(crate) answering: Mutex<()>,
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
    /// El secreto que espera que le den (ver `secrets::Asks`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub secret: Option<crate::secrets::AskView>,
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
            answering: Mutex::new(()),
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
                secret: self.asks.view_for(&s.id),
                alive,
                claude: link.map(|l| ClaudeView {
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
                let _one = st.answering.lock().await;
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
        .route(
            "/api/v1/sessions/{id}/items/{item}",
            get(crate::deliver::api_item),
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
        .route("/api/v1/sessions/{id}/stop", post(api_stop))
        .route("/api/v1/sessions/{id}/restart", post(api_restart))
        .route("/api/v1/sessions/{id}/history", get(api_history))
        .route("/api/v1/sessions/{id}/chat", get(api_chat))
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
        let _one = st.answering.lock().await;
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
    let s = match find_session(&st, &id).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    let _one = st.answering.lock().await;
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
    let (dir, secrets, outbox) = (
        st.paths.session_state(id),
        st.secrets.clone(),
        st.outbox.clone(),
    );
    let id_ = id.to_owned();
    let _ = tokio::task::spawn_blocking(move || {
        let _ = std::fs::remove_dir_all(dir);
        secrets.forget_session(&id_);
        outbox.forget(&id_);
    })
    .await;
    st.refresh.notify_one();
    res.map(|_| ())
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
}

/// La transcripción de la conversación de Claude, si ya la ha empezado a escribir.
pub(crate) fn transcript_of(st: &AppState, link: &ClaudeLink) -> Option<PathBuf> {
    let config = link
        .config_dir
        .clone()
        .unwrap_or_else(|| claude::default_config_dir(&st.paths.home));
    crate::transcript::find(&config, &link.session_id)
}

/// La conversación de Claude de la sesión, leída de su transcripción, por trozos.
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
    Json(serde_json::json!({
        "uri": offer.uri,
        "expires": offer.expires,
        "reusable": days.is_some(),
        "direct": direct,
        "hub": hub.map(|(u, _)| u),
    }))
    .into_response()
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
                        let menu = prompt::parse(&text);
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
                        let msg = serde_json::json!({ "type": "screen", "text": text, "menu": menu_v, "suggestion": suggestion, "form": form, "background": background });
                        if socket.send(Message::Text(msg.to_string().into())).await.is_err() {
                            return;
                        }
                        last_screen = Some(text);
                    }
                }
                Ok(Some(Frame::Exit(_))) | Ok(None) | Err(_) => {
                    holder = None;
                    screen_pending = false;
                    let msg = serde_json::json!({ "type": "screen", "text": null });
                    let _ = socket.send(Message::Text(msg.to_string().into())).await;
                    last_screen = None;
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
            secret: None,
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
    async fn deleting_a_session_drops_its_secrets_items_and_ask() {
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
        delete_session(&st, &id).await.unwrap();
        assert_eq!(waiter.wait().await.0, crate::secrets::Outcome::Closed);
        assert!(!own.exists() && !own.parent().unwrap().exists());
        assert!(team.exists());
        assert!(st.outbox.list(&id).is_empty());
        assert!(!st.paths.state.join("outbox").join(&id).exists());
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
}
