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
use tokio::net::UnixStream;
use tokio::sync::{Mutex, Notify, watch};

use crate::claude::{self, Procfs};
use crate::launch::{self, Launcher, Paths};
use crate::proto::{self, Frame, Hello, HistoryRequest, Role, Size};
use crate::registry::{ClaudeLink, Registry, RegistryError, Session, new_id, now, now_ms};
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
    /// Uso de las cuentas de cswap (con caché).
    accounts: accounts::Cache,
    /// Contestar un menú de Claude es teclear paso a paso: de uno en uno.
    answering: Mutex<()>,
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
}

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
    fn host_name(&self) -> String {
        if let Some(n) = std::env::var("NIDO_NAME")
            .ok()
            .filter(|n| !n.trim().is_empty())
        {
            return n.trim().to_owned();
        }
        // El del sistema (en macOS no hay /etc/hostname), sin el dominio (Mac.local, Mac.casa…).
        let mut buf = [0u8; 256];
        // SAFETY: gethostname escribe como mucho `len` bytes en el buffer.
        let n = if unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) } == 0 {
            buf.iter().position(|&b| b == 0).unwrap_or(buf.len())
        } else {
            0
        };
        let name = String::from_utf8_lossy(&buf[..n]).trim().to_owned();
        let name = name.split('.').next().unwrap_or_default().to_owned();
        if name.is_empty() { "nido".into() } else { name }
    }
}

/// Un cambio de una sesión que merece aviso.
#[derive(Debug, Clone, Copy, PartialEq)]
enum AlertKind {
    /// Espera tu respuesta (un permiso, un menú).
    Waiting,
    /// Terminó de trabajar.
    Finished,
}

#[derive(Debug, Clone)]
struct Alert {
    session: String,
    kind: AlertKind,
}

/// Antes de avisar se espera un poco: si en ese tiempo la ves (en la web, en la app) o sigue
/// sola, no hace falta.
const ALERT_DELAY: Duration = Duration::from_secs(6);

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
    pub alive: bool,
    pub claude: Option<ClaudeView>,
    pub folder: Option<String>,
    /// Terminó de trabajar y el usuario aún no la ha abierto.
    pub unread: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ClaudeView {
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
            accounts: Default::default(),
            answering: Mutex::new(()),
            hub,
            alerts,
            alerts_rx: std::sync::Mutex::new(Some(alerts_rx)),
            identity,
            pairing: Default::default(),
            phones: std::sync::Mutex::new(phones),
            tunnel_addr: Default::default(),
            direct: Default::default(),
            group,
            group_dirty: Notify::new(),
        }))
    }

    /// Mira qué sesiones viven y qué Claude corre en cada una; guarda en el registro la
    /// conversación si cambió (p. ej. tras `/clear`) y publica la lista.
    pub async fn scan(&self) {
        let procfs = Procfs::default();
        let mut reg = self.registry.lock().await;
        let mut views = Vec::with_capacity(reg.sessions().len());
        let sessions: Vec<Session> = reg.sessions().to_vec();
        for s in sessions {
            let alive = launch::is_alive(&self.paths, &s.id);
            let found = alive
                .then(|| launch::holder_pid(&self.paths, &s.id))
                .flatten()
                .and_then(|pid| u32::try_from(pid).ok())
                .and_then(|pid| claude::find_claude(&procfs, pid, &self.paths.home));
            let mut link = s.claude.clone();
            if let Some(f) = &found {
                let inside_shell = !runs_claude_directly(&s.argv);
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
            views.push(SessionView {
                id: s.id.clone(),
                name: s.name.clone(),
                cwd: s.cwd.clone(),
                argv: s.argv.clone(),
                autostart: s.autostart,
                created: s.created,
                folder: s.folder.clone(),
                unread: finished > s.seen,
                alive,
                claude: link.map(|l| ClaudeView {
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
        drop(reg);
        self.views.send_if_modified(|cur| {
            let changed = *cur != listing;
            if changed {
                *cur = listing;
            }
            changed
        });
    }

    /// Al arrancar: vuelve a abrir las sesiones con autoarranque que no estén vivas.
    pub async fn restore(&self) {
        let sessions: Vec<Session> = self.registry.lock().await.sessions().to_vec();
        for s in sessions.iter().filter(|s| s.autostart) {
            if !launch::is_alive(&self.paths, &s.id)
                && let Err(e) = launch::start(&self.paths, self.launcher, &s.id)
            {
                eprintln!("nido: no se pudo restaurar {}: {e}", s.name);
            }
        }
    }
}

fn runs_claude_directly(argv: &[String]) -> bool {
    matches!(argv.first().map(String::as_str), Some("claude" | "cswap"))
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
    }
}

/// Las que piden atención (el número sobre el icono de la app).
fn attention(sessions: &[SessionView]) -> usize {
    sessions
        .iter()
        .filter(|v| still_alerting(v, AlertKind::Waiting) || still_alerting(v, AlertKind::Finished))
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

    async fn alert(&self, a: Alert) {
        tokio::time::sleep(ALERT_DELAY).await;
        let (view, badge) = {
            let l = self.views.borrow();
            (
                l.sessions.iter().find(|v| v.id == a.session).cloned(),
                attention(&l.sessions),
            )
        };
        let Some(v) = view.filter(|v| still_alerting(v, a.kind)) else {
            return;
        };
        let body = match a.kind {
            AlertKind::Waiting => "Te necesita".to_owned(),
            AlertKind::Finished => match v.claude.as_ref().and_then(|c| c.title.clone()) {
                Some(t) if t != v.name => format!("Terminó · {t}"),
                _ => "Terminó".to_owned(),
            },
        };
        self.push_all(&v.name, &body, &v.id, badge).await;
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

/// Cada 2 s (o al pedirlo) vuelve a mirar el estado.
pub fn spawn_scanner(state: Arc<AppState>) {
    spawn_group(state.clone());
    // Los avisos al móvil, según los cambios que vea `scan`.
    if let Some(mut rx) = state.alerts_rx.lock().expect("avisos").take() {
        let st = state.clone();
        tokio::spawn(async move {
            while let Some(a) = rx.recv().await {
                let any = st
                    .phones
                    .lock()
                    .expect("teléfonos")
                    .list()
                    .iter()
                    .any(|p| p.handle.is_some());
                if !any || !st.hub.status.borrow().connected {
                    continue;
                }
                let st = st.clone();
                tokio::spawn(async move { st.alert(a).await });
            }
        });
    }
    // La conexión con el hub (si hay uno configurado).
    hub::spawn(state.hub.clone(), state.clone());
    // El uso de las cuentas, pedido ya: la primera vez cswap puede tardar decenas de segundos.
    let warm = state.clone();
    tokio::spawn(async move {
        warm.accounts.get().await;
    });
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
        .route("/api/v1/accounts", get(api_accounts))
        .route("/api/v1/accounts/switch", post(api_switch_account))
        .route("/api/v1/e2e", get(ws_e2e))
        .route("/api/v1/pair", post(api_pair))
        .route("/api/v1/group", get(api_group))
        .route("/api/v1/group/invite", post(api_group_invite))
        .route("/api/v1/group/join", post(api_group_join))
        .route(
            "/api/v1/group/members/{id}",
            axum::routing::delete(api_group_remove),
        )
        .route("/api/v1/phones", get(api_phones))
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
        .route("/api/v1/sessions/{id}/answer", post(api_answer))
        .route("/api/v1/sessions/{id}/stop", post(api_stop))
        .route("/api/v1/sessions/{id}/restart", post(api_restart))
        .route("/api/v1/sessions/{id}/history", get(api_history))
        .route("/api/v1/sessions/{id}/chat", get(api_chat))
        .route("/api/v1/sessions/{id}/live", get(ws_live))
        .route("/api/v1/sessions/{id}/term", get(ws_term))
        .route("/api/v1/events", get(ws_events))
        .route(
            "/api/v1/upload",
            post(upload::api_upload).layer(DefaultBodyLimit::max(upload::MAX_BYTES)),
        )
        .layer(middleware::from_fn_with_state(state.clone(), auth::guard))
        .with_state(state)
}

pub fn error(status: StatusCode, msg: impl Into<String>) -> Response {
    (status, Json(serde_json::json!({ "error": msg.into() }))).into_response()
}

fn registry_error(e: RegistryError) -> Response {
    let status = match e {
        RegistryError::NotFound(_) => StatusCode::NOT_FOUND,
        RegistryError::Io(_) => StatusCode::INTERNAL_SERVER_ERROR,
        _ => StatusCode::BAD_REQUEST,
    };
    error(status, e.to_string())
}

async fn api_info(State(st): State<Arc<AppState>>) -> Response {
    Json(serde_json::json!({
        "name": env!("CARGO_PKG_NAME"),
        "version": env!("CARGO_PKG_VERSION"),
        "api": 1,
        "auth": st.token.is_some(),
        "home": st.paths.home,
    }))
    .into_response()
}

async fn api_accounts(State(st): State<Arc<AppState>>) -> Response {
    Json(st.accounts.get().await).into_response()
}

/// Cómo va la conexión con el hub.
async fn api_hub(State(st): State<Arc<AppState>>) -> Response {
    Json(st.hub.status.borrow().clone()).into_response()
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
        Ok(()) => Json(st.hub.status.borrow().clone()).into_response(),
        Err(e) => error(StatusCode::BAD_REQUEST, e),
    }
}

async fn api_push_test(State(st): State<Arc<AppState>>) -> Response {
    let badge = attention(&st.views.borrow().sessions);
    let failed = st.push_all("nido", "Aviso de prueba", "", badge).await;
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
        Ok(r) => {
            // Las sesiones paradas por el límite de la cuenta anterior siguen con la nueva.
            let resumed = if r.switched { resume_limited(&st).await } else { Vec::new() };
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
        let link = st.registry.lock().await.get(&s.id).and_then(|x| x.claude.clone());
        let stopped = link
            .and_then(|l| {
                let config = l.config_dir.clone().unwrap_or_else(|| claude::default_config_dir(&st.paths.home));
                crate::transcript::find(&config, &l.session_id)
            })
            .is_some_and(|p| crate::transcript::stopped_by_limit(&p));
        let hello = Hello { role: Role::Viewer, cols: 80, rows: 24 };
        let Ok(stream) = holder_connect(st, &s.id, hello).await else { continue };
        let (rd, wr) = stream.into_split();
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
    /// Cuenta de cswap para Claude (`cswap run <cuenta> -- claude …`).
    account: Option<String>,
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
fn uuid_v4() -> String {
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
fn expand_home(home: &std::path::Path, cwd: Option<PathBuf>) -> PathBuf {
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
async fn api_conversations(State(st): State<Arc<AppState>>, Query(q): Query<ConversationsQuery>) -> Response {
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
    let account = req.account.filter(|a| !a.trim().is_empty());
    let (argv, claude) = if let Some(sid) = req.claude_session_id {
        // Una conversación existente: el guardián la retoma (`claude --resume`).
        let link = ClaudeLink {
            session_id: sid,
            config_dir: req.config_dir,
            account: account.clone(),
            inside_shell: false,
        };
        (vec!["claude".to_owned()], Some(link))
    } else if req.command.is_empty() {
        let sid = uuid_v4();
        let argv = claude::new_argv(&req.name, &sid, account.as_deref());
        let link = ClaudeLink {
            session_id: sid,
            config_dir: None,
            account,
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
        seen: 0,
    };
    let created = match st.registry.lock().await.add(session) {
        Ok(s) => s,
        Err(e) => return registry_error(e),
    };
    if req.start {
        if let Err(e) = launch::start(&st.paths, st.launcher, &created.id) {
            return error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("no se pudo arrancar: {e}"),
            );
        }
        wait_alive(&st, &created.id).await;
    }
    st.refresh.notify_one();
    (StatusCode::CREATED, Json(created)).into_response()
}

async fn wait_alive(st: &AppState, id: &str) -> bool {
    for _ in 0..60 {
        if launch::is_alive(&st.paths, id) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

async fn find_session(st: &AppState, key: &str) -> Result<Session, Response> {
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

async fn stop_and_wait(st: &AppState, id: &str) {
    let _ = launch::stop(&st.paths, st.launcher, id);
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
    let hello = Hello { role: Role::Viewer, cols: 80, rows: 24 };
    let Ok(stream) = holder_connect(&st, &s.id, hello).await else {
        return error(StatusCode::CONFLICT, "la sesión no está en marcha");
    };
    let (rd, wr) = stream.into_split();
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

async fn api_start(State(st): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    let s = match find_session(&st, &id).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    if !launch::is_alive(&st.paths, &s.id) {
        if let Err(e) = launch::start(&st.paths, st.launcher, &s.id) {
            return error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
        }
        wait_alive(&st, &s.id).await;
    }
    st.refresh.notify_one();
    Json(serde_json::json!({ "ok": true })).into_response()
}

async fn api_stop(State(st): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    let s = match find_session(&st, &id).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    stop_and_wait(&st, &s.id).await;
    st.refresh.notify_one();
    Json(serde_json::json!({ "ok": true })).into_response()
}

async fn api_restart(State(st): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    let s = match find_session(&st, &id).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    // Antes de pararla, que el registro tenga la conversación actual para retomarla.
    st.scan().await;
    stop_and_wait(&st, &s.id).await;
    if let Err(e) = launch::start(&st.paths, st.launcher, &s.id) {
        return error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
    }
    wait_alive(&st, &s.id).await;
    st.refresh.notify_one();
    Json(serde_json::json!({ "ok": true })).into_response()
}

async fn api_delete(State(st): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    let s = match find_session(&st, &id).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    stop_and_wait(&st, &s.id).await;
    let res = st.registry.lock().await.remove(&s.id);
    let _ = std::fs::remove_dir_all(st.paths.session_state(&s.id));
    st.refresh.notify_one();
    match res {
        Ok(_) => Json(serde_json::json!({ "ok": true })).into_response(),
        Err(e) => registry_error(e),
    }
}

#[derive(Deserialize)]
struct HistoryQuery {
    #[serde(default)]
    all: bool,
    /// Solo la pantalla actual (la franja en vivo de la vista chat).
    #[serde(default)]
    screen: bool,
}

async fn holder_connect(st: &AppState, id: &str, hello: Hello) -> std::io::Result<UnixStream> {
    let mut stream = UnixStream::connect(st.paths.socket(id)).await?;
    proto::write_frame(&mut stream, &Frame::Hello(hello)).await?;
    Ok(stream)
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
    let config = link
        .config_dir
        .clone()
        .unwrap_or_else(|| claude::default_config_dir(&st.paths.home));
    let Some(path) = crate::transcript::find(&config, &link.session_id) else {
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
async fn api_pair(State(st): State<Arc<AppState>>) -> Response {
    let direct = st.direct.get().cloned().unwrap_or_default();
    let hub = st.hub.pair_info();
    let offer = st.pairing.offer(
        &st.identity,
        &direct,
        hub.as_ref().map(|(u, id)| (u.as_str(), id.as_str())),
    );
    Json(serde_json::json!({ "uri": offer.uri, "expires": offer.expires, "direct": direct, "hub": hub.map(|(u, _)| u) }))
        .into_response()
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

async fn api_remove_phone(State(st): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    // Quitarlo aquí es quitarlo en todo el grupo.
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
    match st.phones.lock().expect("teléfonos").remove(&id) {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => error(StatusCode::NOT_FOUND, "ese teléfono no está emparejado"),
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
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
    ws.on_upgrade(move |socket| term(socket, st, s.id, q))
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
                reason: "la sesión no está en marcha".into(),
            };
            let _ = socket.send(Message::Close(Some(frame))).await;
            return;
        }
    };
    let (mut rd, mut wr) = stream.into_split();
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
        || path.components().any(|c| matches!(c, std::path::Component::ParentDir))
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
    ws.on_upgrade(move |socket| live(socket, st, s.id, q))
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

    let mut holder: Option<(
        tokio::net::unix::OwnedReadHalf,
        tokio::net::unix::OwnedWriteHalf,
    )> = None;
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
                            holder = Some(stream.into_split());
                            screen_due = Some(tokio::time::Instant::now());
                        }
                    }
                }
                // La pantalla, agrupando los cambios rápidos.
                if let (Some(due), Some((_, wr))) = (screen_due, holder.as_mut())
                    && !screen_pending
                    && tokio::time::Instant::now() >= due
                {
                    let req = Frame::HistoryRequest(HistoryRequest { all: false, screen: true });
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
                        let msg = serde_json::json!({ "type": "screen", "text": text, "menu": menu_v, "suggestion": suggestion, "form": form });
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
