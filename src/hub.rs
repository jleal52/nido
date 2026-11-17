//! La conexión de nido con el hub (`nido-hub`): saliente, para que los móviles lleguen sin
//! abrir puertos, y por donde salen los avisos. Lo que pasa por el hub va cifrado de extremo a
//! extremo (ver `e2e.rs`); el hub solo reenvía. Protocolo: `docs/plans/2026-10-23-hub-design.md`.
//!
//! Configuración: `<config>/hub.json` con `{"url": "wss://hub.ejemplo"}` (`nido hub set`).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use ed25519_dalek::Signer;
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot, watch};
use tokio_tungstenite::tungstenite::Message;

use crate::e2e;

pub const AUTH_CONTEXT: &[u8] = b"nido-hub-v1";
const FRAME_OPEN: u8 = 1;
const FRAME_DATA: u8 = 2;
const FRAME_CLOSE: u8 = 3;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct HubConfig {
    /// `wss://…` (o `ws://` en pruebas); `None`, sin hub.
    #[serde(default)]
    pub url: Option<String>,
    /// Dónde conectar (`ip:puerto`) si el nombre del hub aún no resuelve aquí (DNS recién
    /// cambiado); el certificado se sigue comprobando contra el nombre de `url`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
}

impl HubConfig {
    pub fn load(path: &Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, serde_json::to_vec_pretty(self)?)
    }
}

/// La URL del hub en la forma de WebSocket (`https://` → `wss://`).
pub fn ws_url(url: &str) -> Option<String> {
    let u = url.trim().trim_end_matches('/');
    let rest = u
        .strip_prefix("wss://")
        .map(|r| ("wss://", r))
        .or_else(|| u.strip_prefix("https://").map(|r| ("wss://", r)))
        .or_else(|| u.strip_prefix("ws://").map(|r| ("ws://", r)))
        .or_else(|| u.strip_prefix("http://").map(|r| ("ws://", r)))?;
    (!rest.1.is_empty()).then(|| format!("{}{}", rest.0, rest.1))
}

/// Cómo va la conexión, para la web, la terminal y el QR.
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct HubStatus {
    pub url: Option<String>,
    /// El id de este nido en el hub (lo da el hub al identificarse).
    pub id: Option<String>,
    pub connected: bool,
    pub error: Option<String>,
    /// La cuenta de este nido según el hub (`{"name","lang"}`), lo último que dijo: no se borra
    /// al desconectar, solo si se cambia de hub.
    pub account: Option<serde_json::Value>,
}

#[derive(Debug, PartialEq)]
pub enum PushError {
    /// El teléfono ya no existe para APNs.
    Gone(String),
    Other(String),
}

type Pending = Mutex<HashMap<u64, oneshot::Sender<Result<(), PushError>>>>;

/// Por qué no hay token de voz.
#[derive(Debug, PartialEq)]
pub enum VoiceError {
    /// No hay conexión con el hub.
    NoHub,
    /// El hub no da voz (o no contesta, como uno antiguo, que no sabe qué es `voice`).
    NotOffered,
    /// Este nido ya pidió todos los que le tocan por ahora.
    Limit,
    Other(String),
}

impl VoiceError {
    /// El mensaje para la API (en español; se traduce al responder).
    pub fn message(&self) -> &str {
        match self {
            VoiceError::NoHub => "sin conexión con el hub",
            VoiceError::NotOffered => VOICE_NOT_OFFERED,
            VoiceError::Limit => VOICE_LIMIT,
            VoiceError::Other(e) => e,
        }
    }

    /// El error según el texto que manda el hub.
    fn from_hub(error: &str) -> Self {
        match error {
            VOICE_NOT_OFFERED => VoiceError::NotOffered,
            VOICE_LIMIT => VoiceError::Limit,
            e => VoiceError::Other(e.to_owned()),
        }
    }
}

/// Lo que dice el hub cuando no tiene voz y cuando se pasa del límite.
const VOICE_NOT_OFFERED: &str = "el hub no ofrece voz";
const VOICE_LIMIT: &str = "límite de voz";

/// Cuánto se espera el token: un hub antiguo no contesta nunca.
#[cfg(not(test))]
const VOICE_WAIT: Duration = Duration::from_secs(30);
#[cfg(test)]
const VOICE_WAIT: Duration = Duration::from_secs(1);

/// Por qué no hay sesión del hub para un dispositivo.
#[derive(Debug, PartialEq)]
pub enum SessionError {
    /// No hay conexión con el hub.
    NoHub,
    /// El hub no da sesiones (o no contesta, como uno antiguo, que no sabe qué es `session`).
    NotOffered,
    /// Este nido ya pidió todas las que le tocan por ahora (el freno del hub).
    Limit,
    /// Lo que dijo el hub.
    Hub(String),
}

impl SessionError {
    /// El mensaje para la API (en español; se traduce al responder).
    pub fn message(&self) -> &str {
        match self {
            SessionError::NoHub => "sin conexión con el hub",
            SessionError::NotOffered => SESSION_NOT_OFFERED,
            SessionError::Limit => SESSION_LIMIT,
            SessionError::Hub(e) => e,
        }
    }

    /// El error según el texto que manda el hub.
    fn from_hub(error: &str) -> Self {
        match error {
            SESSION_NOT_OFFERED => SessionError::NotOffered,
            SESSION_LIMIT => SessionError::Limit,
            e => SessionError::Hub(e.to_owned()),
        }
    }
}

/// Lo que dice el hub cuando no da sesiones y cuando se pasa del freno.
const SESSION_NOT_OFFERED: &str = "el hub no ofrece sesiones";
const SESSION_LIMIT: &str = "límite de sesiones";
/// Un `ok` sin vale.
const SESSION_NO_GRANT: &str = "el hub no dio el vale";

/// Cuánto se espera el vale de sesión: un hub antiguo no contesta nunca.
#[cfg(not(test))]
const SESSION_WAIT: Duration = Duration::from_secs(30);
#[cfg(test)]
const SESSION_WAIT: Duration = Duration::from_secs(1);

/// Cuánto se espera la respuesta a una petición del tablero: el hub contesta en 10 s como mucho
/// (con un 503 si MongoDB no responde), y uno antiguo no contesta nunca.
#[cfg(not(test))]
const BOARD_WAIT: Duration = Duration::from_secs(30);
#[cfg(test)]
const BOARD_WAIT: Duration = Duration::from_secs(2);
/// Peticiones del tablero sin contestar a la vez: el hub contesta 429 a la quinta.
pub const BOARD_IN_FLIGHT: usize = 4;
/// El 413 del hub cuando la cuenta ya tiene todos los objetos que caben (el otro 413 es el de un
/// blob demasiado largo).
const BOARD_FULL: &str = "la cuenta ya tiene 2000 objetos";
const BOARD_NO_HUB: &str = "sin conexión con el hub";
const BOARD_SILENT: &str = "el hub no contestó";
/// Una respuesta del tablero que no es como dice el protocolo.
const BOARD_ODD: &str = "respuesta del tablero no válida";

/// Un objeto del tablero tal y como lo guarda el hub: el blob cifrado y lo que hace falta para
/// versionarlo. En una lápida (`gone`), `kid` va vacío y `blob` a `null`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoardItem {
    pub id: String,
    pub v: i64,
    #[serde(default, deserialize_with = "null_empty")]
    pub kid: String,
    #[serde(default)]
    pub blob: Option<String>,
    #[serde(default)]
    pub gone: bool,
    /// La marca de su última escritura (µs, del hub).
    #[serde(default)]
    pub at: i64,
}

/// Un texto que puede venir a `null`: vacío.
fn null_empty<'de, D: serde::Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    Ok(Option::<String>::deserialize(d)?.unwrap_or_default())
}

/// Una página de lo cambiado en el tablero desde una marca.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoardPage {
    /// En orden de `(at, id)`.
    pub items: Vec<BoardItem>,
    /// La marca que se guarda al acabar (con `more: false`) como el `since` de la próxima.
    pub next: i64,
    /// Si hay más: la siguiente se pide con el mismo `since` y `after`.
    pub more: bool,
    /// El cursor de la página siguiente (`"<at>:<id>"`), tal cual.
    pub after: Option<String>,
    /// La marca era más vieja que las lápidas: hay que vaciar la copia y empezar desde 0.
    pub reset: bool,
}

/// Lo que da una escritura que sale bien: la versión nueva y su marca.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BoardStamp {
    pub v: i64,
    pub at: i64,
}

/// Por qué no salió una petición del tablero.
#[derive(Debug, Clone, PartialEq)]
pub enum BoardError {
    /// La versión no es la que hay (409): el objeto como está ahora.
    Conflict(BoardItem),
    /// La cuenta ya tiene todos los objetos que caben (413).
    Full,
    /// El blob es demasiado largo para el hub (413).
    TooBig,
    /// El freno de escrituras, o demasiadas peticiones a la vez (429).
    Busy,
    /// Sin conexión, sin respuesta, un 503 (sin cuenta, MongoDB) o algo que no se esperaba.
    Hub(String),
}

impl std::fmt::Display for BoardError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BoardError::Conflict(c) => write!(f, "el objeto cambió (va por la versión {})", c.v),
            BoardError::Full => write!(f, "el tablero de la cuenta está lleno"),
            BoardError::TooBig => write!(f, "el objeto es demasiado grande para el hub"),
            BoardError::Busy => write!(f, "demasiadas escrituras en el tablero: prueba en un rato"),
            BoardError::Hub(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for BoardError {}

/// Los pedidos que esperan respuesta del hub, por su número.
type Waiters = Mutex<HashMap<u64, oneshot::Sender<serde_json::Value>>>;

/// De qué es un pedido al hub que espera respuesta.
#[derive(Clone, Copy)]
enum Asking {
    Voice,
    Session,
    Board,
}

impl Asking {
    /// La clave del pedido (`{"voice":…}`, `{"session":…}`).
    fn kind(self) -> &'static str {
        match self {
            Self::Voice => "voice",
            Self::Session => "session",
            Self::Board => "board",
        }
    }
}

/// Un texto de control hacia el hub.
struct Control {
    text: String,
    /// Si es un pedido que espera respuesta, con qué saber al sacarlo de la cola si todavía
    /// hace falta: si su plazo venció o ya no lo espera nadie, no sale (lo que diera el hub no le
    /// serviría a nadie y gastaría cupo de su freno).
    ask: Option<Asked>,
}

impl Control {
    /// Un texto que no espera respuesta: sale siempre.
    fn text(msg: serde_json::Value) -> Self {
        Self {
            text: msg.to_string(),
            ask: None,
        }
    }
}

/// Un pedido en la cola de control: de qué, su número y hasta cuándo se espera la respuesta.
struct Asked {
    of: Asking,
    req: u64,
    deadline: tokio::time::Instant,
}

/// Quita un pedido de los que esperan respuesta cuando `ask` acaba, sea como sea: con la
/// respuesta, con el plazo vencido o porque quien lo pidió dejó de esperar.
struct Forget<'a> {
    waiters: &'a Waiters,
    req: u64,
}

impl Drop for Forget<'_> {
    fn drop(&mut self) {
        self.waiters
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.req);
    }
}

/// Por qué un pedido al hub se quedó sin respuesta.
enum Unanswered {
    /// No había conexión, o se cerró con el pedido en marcha.
    NoHub,
    /// Venció el plazo (un hub antiguo no contesta a lo que no conoce).
    Silent,
}

/// El cliente: se reconecta solo y atiende los móviles que llegan por el hub.
pub struct Hub {
    pub status: watch::Sender<HubStatus>,
    config_path: PathBuf,
    config: watch::Sender<HubConfig>,
    /// Lo que va hacia el hub (mientras haya conexión).
    out: Mutex<Option<mpsc::Sender<Message>>>,
    /// Los textos de control hacia el hub (el grupo, los dispositivos, las bajas, los pedidos
    /// de voz y de sesión), mientras haya conexión: una cola aparte, sin límite (son pocos y
    /// pequeños), para que no se pierdan con `out` lleno (los flujos de los móviles pueden
    /// llenarlo) y salgan en el orden en que se mandaron. Los pedidos que ya no hacen falta
    /// (`Control::ask`) se descartan al sacarlos.
    control: Mutex<Option<mpsc::UnboundedSender<Control>>>,
    pending: Pending,
    /// Los pedidos de token de voz que esperan respuesta (lo que vino en `voiced`).
    voice_pending: Waiters,
    /// Los pedidos de sesión de dispositivo que esperan respuesta (lo que vino en `sessioned`).
    session_pending: Waiters,
    /// Las peticiones del tablero que esperan respuesta (lo que vino en `boarded`).
    board_pending: Waiters,
    /// Para no tener más de `BOARD_IN_FLIGHT` peticiones del tablero sin contestar.
    board_slots: tokio::sync::Semaphore,
    /// Los avisos del hub de que el tablero de la cuenta cambió (`{"board":{"at"}}`). El valor es
    /// la marca del aviso, que solo es una pista: se sincroniza siempre desde la guardada.
    board_notice: watch::Sender<i64>,
    /// Lo mandado en esta conexión (se vacía al conectar y al desconectar, así que al reconectar
    /// se manda siempre). Va con `control`: se cogen en ese orden.
    sent: Mutex<Sent>,
    next_req: AtomicU64,
}

impl Hub {
    pub fn new(config_path: PathBuf) -> Arc<Self> {
        let config = HubConfig::load(&config_path);
        Arc::new(Self {
            status: watch::Sender::new(HubStatus {
                url: config.url.clone(),
                ..Default::default()
            }),
            config_path,
            config: watch::Sender::new(config),
            out: Default::default(),
            pending: Default::default(),
            voice_pending: Default::default(),
            session_pending: Default::default(),
            board_pending: Default::default(),
            board_slots: tokio::sync::Semaphore::new(BOARD_IN_FLIGHT),
            board_notice: watch::Sender::new(0),
            control: Default::default(),
            sent: Default::default(),
            next_req: AtomicU64::new(1),
        })
    }

    /// Cambia el hub (o lo quita) y reconecta.
    pub fn set_url(&self, url: Option<String>, address: Option<String>) -> Result<(), String> {
        let url = match url.filter(|u| !u.trim().is_empty()) {
            Some(u) => Some(
                ws_url(&u).ok_or("la dirección del hub tiene que empezar por wss:// o https://")?,
            ),
            None => None,
        };
        let address = address.filter(|a| !a.trim().is_empty());
        if let Some(a) = &address
            && a.parse::<std::net::SocketAddr>().is_err()
        {
            return Err(
                "la dirección tiene que ser ip:puerto (por ejemplo 203.0.113.5:443)".into(),
            );
        }
        let cfg = HubConfig {
            address: address.filter(|_| url.is_some()),
            url,
        };
        cfg.save(&self.config_path).map_err(|e| e.to_string())?;
        self.config.send_replace(cfg);
        Ok(())
    }

    /// La dirección y el id para el QR, si está conectado.
    pub fn pair_info(&self) -> Option<(String, String)> {
        let s = self.status.borrow();
        match (&s.url, &s.id, s.connected) {
            (Some(u), Some(id), true) => Some((
                u.replacen("wss://", "https://", 1)
                    .replacen("ws://", "http://", 1),
                id.clone(),
            )),
            _ => None,
        }
    }

    /// Pide un aviso al hub para un teléfono, con el contenido ya cifrado.
    pub async fn push(&self, handle: &str, e: String, collapse: &str) -> Result<(), PushError> {
        let out = self
            .out
            .lock()
            .expect("hub")
            .clone()
            .ok_or_else(|| PushError::Other("sin conexión con el hub".into()))?;
        let req = self.next_req.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().expect("avisos").insert(req, tx);
        let msg = serde_json::json!({ "push": { "req": req, "device": handle, "e": e, "collapse": collapse } });
        if out
            .send(Message::Text(msg.to_string().into()))
            .await
            .is_err()
        {
            self.pending.lock().expect("avisos").remove(&req);
            return Err(PushError::Other("sin conexión con el hub".into()));
        }
        match tokio::time::timeout(Duration::from_secs(30), rx).await {
            Ok(Ok(r)) => r,
            // Se cerró la conexión con el aviso en marcha (ver `disconnected`).
            Ok(Err(_)) => Err(PushError::Other("sin conexión con el hub".into())),
            Err(_) => {
                self.pending.lock().expect("avisos").remove(&req);
                Err(PushError::Other("el hub no contestó".into()))
            }
        }
    }

    /// Los que esperan respuesta a los pedidos de `of`.
    fn waiters(&self, of: Asking) -> &Waiters {
        match of {
            Asking::Voice => &self.voice_pending,
            Asking::Session => &self.session_pending,
            Asking::Board => &self.board_pending,
        }
    }

    /// Manda `{kind: {"req": N, …}}` (lo de `body`, con su número) y espera, como mucho `wait`,
    /// lo que conteste el hub con ese número, que le entrega el despachador de los textos
    /// (`answered`). Si el plazo vence antes de que el pedido salga, ya no sale.
    async fn ask(
        &self,
        of: Asking,
        mut body: serde_json::Map<String, serde_json::Value>,
        wait: Duration,
    ) -> Result<serde_json::Value, Unanswered> {
        let control = self
            .control
            .lock()
            .expect("hub")
            .clone()
            .ok_or(Unanswered::NoHub)?;
        let deadline = tokio::time::Instant::now() + wait;
        let waiters = self.waiters(of);
        let req = self.next_req.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        waiters.lock().expect("pedidos").insert(req, tx);
        let _forget = Forget { waiters, req };
        body.insert("req".into(), req.into());
        let mut msg = serde_json::Map::new();
        msg.insert(of.kind().into(), body.into());
        let msg = Control {
            text: serde_json::Value::Object(msg).to_string(),
            ask: Some(Asked { of, req, deadline }),
        };
        if control.send(msg).is_err() {
            return Err(Unanswered::NoHub);
        }
        match tokio::time::timeout_at(deadline, rx).await {
            Ok(Ok(v)) => Ok(v),
            // Se cerró la conexión con el pedido en marcha (ver `disconnected`).
            Ok(Err(_)) => Err(Unanswered::NoHub),
            Err(_) => Err(Unanswered::Silent),
        }
    }

    /// Si lo que sale de la cola de control todavía hace falta: un pedido, si no ha vencido su
    /// plazo y alguien espera aún la respuesta; lo demás, siempre. Solo se mira al sacarlo de la
    /// cola: si luego se queda atascado al mandarlo (`tx.send(…).await`), puede salir vencido.
    fn wanted(&self, c: &Control) -> bool {
        let Some(a) = &c.ask else { return true };
        tokio::time::Instant::now() < a.deadline
            && self
                .waiters(a.of)
                .lock()
                .expect("pedidos")
                .get(&a.req)
                .is_some_and(|w| !w.is_closed())
    }

    /// Pide al hub un token de voz para que la web o la app hablen con el modelo directamente.
    /// Devuelve lo que da el hub (`token`, `expires`, `project`, `location`, `host`, `model`).
    pub async fn voice_token(&self) -> Result<serde_json::Value, VoiceError> {
        let mut v = match self
            .ask(Asking::Voice, Default::default(), VOICE_WAIT)
            .await
        {
            Ok(v) => v,
            Err(Unanswered::NoHub) => return Err(VoiceError::NoHub),
            Err(Unanswered::Silent) => return Err(VoiceError::NotOffered),
        };
        if v["ok"] != true {
            return Err(VoiceError::from_hub(
                v["error"].as_str().unwrap_or_default(),
            ));
        }
        strip_reply(&mut v);
        Ok(v)
    }

    /// Pide al hub un vale de sesión para un dispositivo (`device`, su id aquí; sin él, el hub da
    /// una sesión que solo caduca): la web lo canjea en el hub por su cookie. Devuelve lo que da
    /// el hub (`grant`, `expires`).
    pub async fn hub_session(
        &self,
        device: Option<&str>,
    ) -> Result<serde_json::Value, SessionError> {
        let mut body = serde_json::Map::new();
        if let Some(d) = device {
            body.insert("device".into(), d.into());
        }
        let mut v = match self.ask(Asking::Session, body, SESSION_WAIT).await {
            Ok(v) => v,
            Err(Unanswered::NoHub) => return Err(SessionError::NoHub),
            Err(Unanswered::Silent) => return Err(SessionError::NotOffered),
        };
        if v["ok"] != true {
            return Err(SessionError::from_hub(
                v["error"].as_str().unwrap_or_default(),
            ));
        }
        if !v["grant"].is_string() {
            return Err(SessionError::Hub(SESSION_NO_GRANT.into()));
        }
        strip_reply(&mut v);
        Ok(v)
    }

    /// Manda al hub, si hay conexión, los ids de los otros equipos del grupo
    /// (`{"group":{"ids":[…],"at":ms}}`), sin repetidos y ordenados; solo si no son los mismos
    /// que se mandaron la última vez en esta conexión. Sin conexión no se manda nada: al
    /// conectar se mandan.
    pub fn send_group(&self, mut ids: Vec<String>) {
        ids.sort();
        ids.dedup();
        // Con `control` cogido: lo apuntado como mandado va con la conexión por la que salió.
        let control = self.control.lock().expect("hub");
        let Some(control) = control.as_ref() else {
            return;
        };
        let mut sent = self.sent.lock().expect("hub");
        if sent.group.as_ref() == Some(&ids) {
            return;
        }
        let msg = serde_json::json!({ "group": { "ids": ids, "at": crate::group::now_ms() } });
        // Solo falla si la conexión ya se acabó: al reconectar se vuelve a mandar.
        if control.send(Control::text(msg)).is_ok() {
            sent.group = Some(ids);
        }
    }

    /// Manda al hub, una vez por conexión, los ids de los dispositivos emparejados aquí
    /// (`{"devices":{"ids":[…]}}`, ordenados): el hub borra las sesiones de los que no estén. Así
    /// se arregla una baja que no llegó (`unsession` sin conexión). Sin conexión, nada.
    pub fn send_devices(&self, mut ids: Vec<String>) {
        ids.sort();
        ids.dedup();
        let control = self.control.lock().expect("hub");
        let Some(control) = control.as_ref() else {
            return;
        };
        let mut sent = self.sent.lock().expect("hub");
        if sent.devices {
            return;
        }
        let msg = serde_json::json!({ "devices": { "ids": ids } });
        if control.send(Control::text(msg)).is_ok() {
            sent.devices = true;
        }
    }

    /// Avisa al hub de que este dispositivo (su id aquí) ya no está emparejado, para que borre
    /// sus sesiones (`{"unsession":{"device":…}}`). Sin conexión no se manda: la lista de
    /// dispositivos que va al conectar (`send_devices`) lo arregla.
    pub fn unsession(&self, device: &str) {
        let control = self.control.lock().expect("hub").clone();
        let Some(control) = control else { return };
        let msg = serde_json::json!({ "unsession": { "device": device } });
        let _ = control.send(Control::text(msg));
    }

    /// Si es una respuesta `{"voiced":{"req":N,…}}`, se la entrega a quien la espera. Devuelve
    /// si lo era. Con `get_mut` y no con `v["voiced"]`: el índice mutable de un `Value` mete la
    /// clave que falta (y entra en pánico si no es un objeto), y por aquí pasan todos los textos.
    fn voiced(&self, v: &mut serde_json::Value) -> bool {
        v.get_mut("voiced")
            .is_some_and(|r| answered(&self.voice_pending, r))
    }

    /// Lo mismo con `{"sessioned":{"req":N,…}}`.
    fn sessioned(&self, v: &mut serde_json::Value) -> bool {
        v.get_mut("sessioned")
            .is_some_and(|r| answered(&self.session_pending, r))
    }

    /// Lo mismo con `{"boarded":{"req":N,…}}`, las respuestas del tablero.
    fn boarded(&self, v: &mut serde_json::Value) -> bool {
        v.get_mut("boarded")
            .is_some_and(|r| answered(&self.board_pending, r))
    }

    /// Si es el aviso `{"board":{"at":…}}` (el tablero de la cuenta cambió): despierta a quien
    /// escuche `board_notices`, aunque la marca sea la misma que la anterior. Devuelve si lo era.
    /// El nido no manda `board` sin `req`, y el hub no contesta con `board`: un `board` que llega
    /// es siempre un aviso.
    fn board_noticed(&self, v: &serde_json::Value) -> bool {
        let Some(b) = v.get("board").and_then(|b| b.as_object()) else {
            return false;
        };
        let at = b.get("at").and_then(|a| a.as_i64());
        self.board_notice.send_modify(|n| *n = at.unwrap_or(*n));
        true
    }

    /// Los avisos de cambio del tablero de la cuenta. Llegan solo a una conexión que ya ha hecho
    /// alguna petición del tablero: tras cada reconexión hay que pedir algo (`board_get`).
    pub fn board_notices(&self) -> watch::Receiver<i64> {
        self.board_notice.subscribe()
    }

    /// Manda una petición del tablero y espera su respuesta: con 200, lo que da el hub; con otro
    /// estado, su error. Antes coge un hueco de los `BOARD_IN_FLIGHT` (el plazo empieza a contar
    /// al encolarla, así que no se gasta esperando el hueco).
    async fn board_ask(
        &self,
        body: serde_json::Map<String, serde_json::Value>,
    ) -> Result<serde_json::Value, BoardError> {
        let _slot = self
            .board_slots
            .acquire()
            .await
            .map_err(|_| BoardError::Hub(BOARD_NO_HUB.into()))?;
        let v = match self.ask(Asking::Board, body, BOARD_WAIT).await {
            Ok(v) => v,
            Err(Unanswered::NoHub) => return Err(BoardError::Hub(BOARD_NO_HUB.into())),
            Err(Unanswered::Silent) => return Err(BoardError::Hub(BOARD_SILENT.into())),
        };
        let error = || v["error"].as_str().unwrap_or_default().to_owned();
        match v["status"].as_u64() {
            Some(200) => Ok(v),
            Some(409) => match serde_json::from_value::<BoardItem>(v["current"].clone()) {
                Ok(current) => Err(BoardError::Conflict(current)),
                Err(_) => Err(BoardError::Hub(BOARD_ODD.into())),
            },
            Some(413) if v["error"] == BOARD_FULL => Err(BoardError::Full),
            Some(413) => Err(BoardError::TooBig),
            Some(429) => Err(BoardError::Busy),
            Some(_) if !error().is_empty() => Err(BoardError::Hub(error())),
            _ => Err(BoardError::Hub(BOARD_ODD.into())),
        }
    }

    /// Una página de lo cambiado en el tablero de la cuenta desde `since` (µs; 0, todo). La
    /// primera de una sincronización va sin `after`; las siguientes, con el mismo `since` y el
    /// `after` de la anterior, tal cual.
    pub async fn board_get(
        &self,
        since: i64,
        after: Option<&str>,
    ) -> Result<BoardPage, BoardError> {
        let mut body = serde_json::Map::new();
        body.insert("op".into(), "get".into());
        body.insert("since".into(), since.into());
        body.insert("after".into(), after.into());
        let v = self.board_ask(body).await?;
        let odd = || BoardError::Hub(BOARD_ODD.into());
        let items: Vec<BoardItem> =
            serde_json::from_value(v["items"].clone()).map_err(|_| odd())?;
        let more = v["more"].as_bool().ok_or_else(odd)?;
        let after = v["after"].as_str().map(str::to_owned);
        if more && after.is_none() {
            return Err(odd());
        }
        Ok(BoardPage {
            items,
            next: v["next"].as_i64().ok_or_else(odd)?,
            more,
            after,
            reset: v["reset"].as_bool().unwrap_or(false),
        })
    }

    /// Escribe el objeto `id` si su versión es `v` (0: si es nuevo).
    pub async fn board_put(
        &self,
        id: &str,
        v: i64,
        kid: &str,
        blob: &str,
    ) -> Result<BoardStamp, BoardError> {
        let mut body = serde_json::Map::new();
        body.insert("op".into(), "put".into());
        body.insert("id".into(), id.into());
        body.insert("v".into(), v.into());
        body.insert("kid".into(), kid.into());
        body.insert("blob".into(), blob.into());
        stamp(&self.board_ask(body).await?)
    }

    /// Borra el objeto `id` (deja una lápida) si su versión es `v`.
    pub async fn board_del(&self, id: &str, v: i64) -> Result<BoardStamp, BoardError> {
        let mut body = serde_json::Map::new();
        body.insert("op".into(), "del".into());
        body.insert("id".into(), id.into());
        body.insert("v".into(), v.into());
        stamp(&self.board_ask(body).await?)
    }

    /// Si es `{"account":{"name","lang"}}`, la cuenta de este nido: al estado (y, si cambió, avisa
    /// a quien lo escuche). Devuelve si lo era. Un nombre o un idioma que no valen se quedan en
    /// `null`: van a la información de los dispositivos, donde uno enorme no cabría y dejaría sin
    /// emparejar ni conectar a nadie.
    fn account(&self, v: &serde_json::Value) -> bool {
        let Some(a) = v.get("account").and_then(|a| a.as_object()) else {
            return false;
        };
        let text = |k: &str, valid: fn(&str) -> bool| {
            a.get(k)
                .and_then(|x| x.as_str())
                .filter(|x| valid(x))
                .map(str::to_owned)
        };
        let account = serde_json::json!({
            "name": text("name", account_name),
            "lang": text("lang", account_lang),
        });
        self.status.send_if_modified(|s| {
            if s.account.as_ref() == Some(&account) {
                return false;
            }
            s.account = Some(account);
            true
        });
        true
    }

    /// Se acabó la conexión: ya no se manda nada y los que esperan respuesta dejan de esperar
    /// (sus `oneshot` se cierran), en vez de esperar a que venza el plazo. Lo que se mande con
    /// un `out` de antes ya no llega a nadie: falla al mandarlo.
    fn disconnected(&self) {
        *self.out.lock().expect("hub") = None;
        {
            let mut control = self.control.lock().expect("hub");
            *self.sent.lock().expect("hub") = Sent::default();
            *control = None;
        }
        self.pending.lock().expect("avisos").clear();
        self.voice_pending.lock().expect("pedidos").clear();
        self.session_pending.lock().expect("pedidos").clear();
        self.board_pending.lock().expect("pedidos").clear();
    }

    fn set_status(&self, f: impl FnOnce(&mut HubStatus)) {
        self.status.send_modify(f);
    }
}

/// Si es un nombre de cuenta como los que da y acepta el hub: de 3 a 30 caracteres de
/// `[a-z0-9-]`, sin guion al principio ni al final y sin dos seguidos.
fn account_name(name: &str) -> bool {
    (3..=30).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !name.starts_with('-')
        && !name.ends_with('-')
        && !name.contains("--")
}

/// Los idiomas de una cuenta.
fn account_lang(lang: &str) -> bool {
    matches!(lang, "es" | "en")
}

/// Lo mandado al hub en una conexión.
#[derive(Default)]
struct Sent {
    /// Los ids del grupo de la última vez.
    group: Option<Vec<String>>,
    /// Si ya se mandó la lista de dispositivos.
    devices: bool,
}

/// Le entrega `reply` (lo de dentro de `{"voiced":…}`, `{"sessioned":…}`) a quien espera su
/// número. Devuelve si era una respuesta (con número), la esperase alguien o no.
fn answered(waiters: &Waiters, reply: &mut serde_json::Value) -> bool {
    let Some(req) = reply["req"].as_u64() else {
        return false;
    };
    if let Some(waiter) = waiters.lock().expect("pedidos").remove(&req) {
        let _ = waiter.send(reply.take());
    }
    true
}

/// Lo que da el hub, sin lo del pedido (`req`, `ok`).
fn strip_reply(v: &mut serde_json::Value) {
    if let Some(o) = v.as_object_mut() {
        o.remove("req");
        o.remove("ok");
    }
}

/// La versión y la marca de una escritura del tablero que salió bien.
fn stamp(v: &serde_json::Value) -> Result<BoardStamp, BoardError> {
    match (v["v"].as_i64(), v["at"].as_i64()) {
        (Some(v), Some(at)) => Ok(BoardStamp { v, at }),
        _ => Err(BoardError::Hub(BOARD_ODD.into())),
    }
}

/// Cifra un aviso para un teléfono: `nonce (12) || ChaCha20-Poly1305(clave, nonce, json)`.
pub fn seal(push_key_b64: &str, body: &serde_json::Value) -> Option<String> {
    let key: [u8; 32] = B64.decode(push_key_b64).ok()?.try_into().ok()?;
    let mut nonce = [0u8; 12];
    getrandom::fill(&mut nonce).ok()?;
    let cipher = ChaCha20Poly1305::new(Key::from_slice(&key));
    let ct = cipher
        .encrypt(Nonce::from_slice(&nonce), body.to_string().as_bytes())
        .ok()?;
    let mut out = nonce.to_vec();
    out.extend_from_slice(&ct);
    Some(B64.encode(out))
}

/// Mantiene la conexión con el hub mientras haya uno configurado.
pub fn spawn<H: e2e::Host>(hub: Arc<Hub>, host: Arc<H>) {
    tokio::spawn(async move {
        let mut cfg_rx = hub.config.subscribe();
        let mut wait = Duration::from_secs(2);
        loop {
            let (url, address) = {
                let c = cfg_rx.borrow_and_update();
                (c.url.clone(), c.address.clone())
            };
            hub.set_status(|s| {
                // La cuenta es lo último que se supo de este hub: con otro, ya no vale.
                let account = s.account.take().filter(|_| s.url == url);
                *s = HubStatus {
                    url: url.clone(),
                    account,
                    ..Default::default()
                }
            });
            let Some(url) = url else {
                // Sin hub: esperar a que lo configuren.
                if cfg_rx.changed().await.is_err() {
                    return;
                }
                continue;
            };
            let started = std::time::Instant::now();
            tokio::select! {
                r = run(&hub, &host, &url, address.as_deref()) => {
                    if let Err(e) = r {
                        eprintln!("nido: hub {url}: {e}");
                        hub.set_status(|s| s.error = Some(e));
                    }
                }
                _ = cfg_rx.changed() => {
                    hub.disconnected();
                    continue;
                }
            }
            hub.disconnected();
            hub.set_status(|s| s.connected = false);
            // Si duró, se reintenta pronto; si no, cada vez más espaciado (hasta un minuto).
            if started.elapsed() > Duration::from_secs(60) {
                wait = Duration::from_secs(2);
            }
            tokio::select! {
                _ = tokio::time::sleep(wait) => {}
                _ = cfg_rx.changed() => { wait = Duration::from_secs(2); continue; }
            }
            wait = (wait * 2).min(Duration::from_secs(60));
        }
    });
}

#[derive(Deserialize)]
struct Pushed {
    pushed: PushedBody,
}

#[derive(Deserialize)]
struct PushedBody {
    req: u64,
    ok: bool,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    gone: bool,
}

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Abre un WebSocket con el hub. Con `address`, a esa IP, pero con el nombre del hub para TLS
/// (SNI y certificado) y para Host.
async fn dial(url: &str, address: Option<&str>, path: &str) -> Result<Ws, String> {
    let target = format!("{url}{path}");
    let connect = async {
        match address {
            Some(addr) => {
                let tcp = tokio::net::TcpStream::connect(addr)
                    .await
                    .map_err(|e| format!("{addr}: {e}"))?;
                tokio_tungstenite::client_async_tls(target.as_str(), tcp)
                    .await
                    .map_err(|e| e.to_string())
            }
            None => tokio_tungstenite::connect_async(target.as_str())
                .await
                .map_err(|e| e.to_string()),
        }
    };
    let (ws, _) = tokio::time::timeout(Duration::from_secs(15), connect)
        .await
        .map_err(|_| "el hub no contesta".to_owned())??;
    Ok(ws)
}

impl Hub {
    /// Un ida y vuelta con otro nido a través del hub (como un móvil): manda `first` y devuelve
    /// su respuesta. Para unirse al grupo y sincronizarlo.
    pub async fn exchange(&self, nido_id: &str, first: Vec<u8>) -> Result<Vec<u8>, String> {
        let (url, address) = {
            let c = self.config.borrow();
            (c.url.clone(), c.address.clone())
        };
        let url = url.ok_or("no hay hub configurado")?;
        let mut ws = dial(&url, address.as_deref(), &format!("/v1/app?nido={nido_id}")).await?;
        ws.send(Message::Binary(first.into()))
            .await
            .map_err(|e| e.to_string())?;
        let reply = loop {
            match tokio::time::timeout(Duration::from_secs(15), ws.next()).await {
                Ok(Some(Ok(Message::Binary(b)))) => break b.to_vec(),
                Ok(Some(Ok(Message::Ping(_) | Message::Pong(_)))) => {}
                Ok(Some(Ok(Message::Close(_)))) | Ok(None) => {
                    return Err("el otro equipo cerró sin contestar".into());
                }
                Ok(Some(Err(e))) => return Err(e.to_string()),
                Ok(Some(Ok(_))) => {}
                Err(_) => return Err("el otro equipo no contesta".into()),
            }
        };
        let _ = ws.close(None).await;
        Ok(reply)
    }

    /// La URL del hub configurado (https://…), si hay.
    pub fn url(&self) -> Option<String> {
        self.config.borrow().url.clone().map(|u| {
            u.replacen("wss://", "https://", 1)
                .replacen("ws://", "http://", 1)
        })
    }
}

async fn run<H: e2e::Host>(
    hub: &Arc<Hub>,
    host: &Arc<H>,
    url: &str,
    address: Option<&str>,
) -> Result<(), String> {
    let ws = dial(url, address, "/v1/nido").await?;
    let (mut tx, mut rx) = ws.split();

    // Identificarse: firmar el reto con la clave Ed25519 de nido.
    let challenge = loop {
        match tokio::time::timeout(Duration::from_secs(15), rx.next()).await {
            Ok(Some(Ok(Message::Text(t)))) => {
                let v: serde_json::Value = serde_json::from_str(&t).map_err(|e| e.to_string())?;
                break B64
                    .decode(v["challenge"].as_str().unwrap_or_default())
                    .map_err(|_| "reto no válido".to_owned())?;
            }
            Ok(Some(Ok(Message::Ping(_) | Message::Pong(_)))) => {}
            _ => return Err("el hub no mandó el reto".into()),
        }
    };
    let signing = &host.identity().signing;
    let mut msg = AUTH_CONTEXT.to_vec();
    msg.extend_from_slice(&challenge);
    let reply = serde_json::json!({
        "pub": B64.encode(signing.verifying_key().to_bytes()),
        "sig": B64.encode(signing.sign(&msg).to_bytes()),
    });
    tx.send(Message::Text(reply.to_string().into()))
        .await
        .map_err(|e| e.to_string())?;
    let id = loop {
        match tokio::time::timeout(Duration::from_secs(15), rx.next()).await {
            Ok(Some(Ok(Message::Text(t)))) => {
                let v: serde_json::Value = serde_json::from_str(&t).map_err(|e| e.to_string())?;
                break v["id"].as_str().ok_or("el hub no dio el id")?.to_owned();
            }
            Ok(Some(Ok(Message::Ping(_) | Message::Pong(_)))) => {}
            Ok(Some(Ok(Message::Close(f)))) => {
                return Err(format!(
                    "el hub no aceptó la firma ({})",
                    f.map(|f| f.reason.to_string()).unwrap_or_default()
                ));
            }
            _ => return Err("el hub no dio el id".into()),
        }
    };
    eprintln!("nido: conectado al hub {url} como {id}");

    let (out_tx, mut out_rx) = mpsc::channel::<Message>(256);
    // `control_tx` vive aquí también: la cola no se cierra mientras dure la conexión.
    let (control_tx, mut control_rx) = mpsc::unbounded_channel::<Control>();
    *hub.out.lock().expect("hub") = Some(out_tx.clone());
    {
        // Una conexión nueva: el grupo y los dispositivos se vuelven a mandar.
        let mut control = hub.control.lock().expect("hub");
        *hub.sent.lock().expect("hub") = Sent::default();
        *control = Some(control_tx.clone());
    }
    hub.set_status(|s| {
        s.id = Some(id.clone());
        s.connected = true;
        s.error = None;
    });

    // Cada móvil que llega por el hub: su canal cifrado, como por la LAN.
    let mut phones: HashMap<u32, mpsc::Sender<Vec<u8>>> = HashMap::new();
    let mut ping = tokio::time::interval(Duration::from_secs(25));
    loop {
        tokio::select! {
            _ = ping.tick() => {
                tx.send(Message::Ping(Default::default())).await.map_err(|e| e.to_string())?;
            }
            m = out_rx.recv() => {
                let Some(m) = m else { return Ok(()) };
                tx.send(m).await.map_err(|e| e.to_string())?;
            }
            Some(c) = control_rx.recv() => {
                // Un pedido que ya no hace falta (venció o nadie lo espera) no sale.
                if hub.wanted(&c) {
                    tx.send(Message::Text(c.text.into())).await.map_err(|e| e.to_string())?;
                }
            }
            msg = rx.next() => match msg {
                Some(Ok(Message::Binary(b))) if b.len() >= 5 => {
                    let conn = u32::from_be_bytes(b[1..5].try_into().expect("4 bytes"));
                    match b[0] {
                        FRAME_OPEN => {
                            let (in_tx, in_rx) = mpsc::channel::<Vec<u8>>(64);
                            let (e2e_tx, mut e2e_rx) = mpsc::channel::<Vec<u8>>(64);
                            phones.insert(conn, in_tx);
                            tokio::spawn(e2e::serve(host.clone(), in_rx, e2e_tx));
                            let out = out_tx.clone();
                            tokio::spawn(async move {
                                while let Some(m) = e2e_rx.recv().await {
                                    if out.send(Message::Binary(frame(FRAME_DATA, conn, &m).into())).await.is_err() {
                                        return;
                                    }
                                }
                                let _ = out.send(Message::Binary(frame(FRAME_CLOSE, conn, &[]).into())).await;
                            });
                        }
                        FRAME_DATA => {
                            if let Some(p) = phones.get(&conn)
                                && p.send(b[5..].to_vec()).await.is_err()
                            {
                                phones.remove(&conn);
                            }
                        }
                        FRAME_CLOSE => {
                            phones.remove(&conn);
                        }
                        _ => {}
                    }
                }
                Some(Ok(Message::Text(t))) => {
                    if let Ok(Pushed { pushed: p }) = serde_json::from_str::<Pushed>(&t)
                        && let Some(waiter) = hub.pending.lock().expect("avisos").remove(&p.req)
                    {
                        let r = if p.ok {
                            Ok(())
                        } else if p.gone {
                            Err(PushError::Gone(p.error.unwrap_or_default()))
                        } else {
                            Err(PushError::Other(p.error.unwrap_or_default()))
                        };
                        let _ = waiter.send(r);
                    } else if let Ok(mut v) = serde_json::from_str::<serde_json::Value>(&t) {
                        // Cada uno dice si era suyo; lo que no conoce nadie se ignora.
                        let _ = hub.voiced(&mut v)
                            || hub.sessioned(&mut v)
                            || hub.boarded(&mut v)
                            || hub.board_noticed(&v)
                            || hub.account(&v);
                    }
                }
                Some(Ok(Message::Close(_))) | None => return Ok(()),
                Some(Err(e)) => return Err(e.to_string()),
                Some(Ok(_)) => {}
            }
        }
    }
}

fn frame(kind: u8, conn: u32, data: &[u8]) -> Vec<u8> {
    let mut f = Vec::with_capacity(5 + data.len());
    f.push(kind);
    f.extend_from_slice(&conn.to_be_bytes());
    f.extend_from_slice(data);
    f
}

#[cfg(test)]
mod tests {
    use super::fake::Reply;
    use super::*;

    #[test]
    fn urls() {
        assert_eq!(
            ws_url("https://hub.example/").as_deref(),
            Some("wss://hub.example")
        );
        assert_eq!(
            ws_url("ws://10.0.0.1:7700").as_deref(),
            Some("ws://10.0.0.1:7700")
        );
        assert_eq!(ws_url("hub.example"), None);
        assert_eq!(ws_url("https://"), None);
    }

    #[test]
    fn sealed_push_opens_with_the_key() {
        let key = B64.encode([4u8; 32]);
        let e = seal(&key, &serde_json::json!({"title": "proyecto-api"})).unwrap();
        let raw = B64.decode(e).unwrap();
        let cipher = ChaCha20Poly1305::new(Key::from_slice(&[4u8; 32]));
        let plain = cipher
            .decrypt(Nonce::from_slice(&raw[..12]), &raw[12..])
            .unwrap();
        assert_eq!(plain, br#"{"title":"proyecto-api"}"#);
        assert!(seal("corta", &serde_json::json!({})).is_none());
    }

    // ---------- El token de voz ----------

    /// Un nido conectado al hub de mentira de `addr`.
    async fn connected(dir: &Path, addr: std::net::SocketAddr) -> Arc<Hub> {
        fake::nido(dir, addr).await
    }

    #[tokio::test]
    async fn the_voice_token_comes_from_the_hub() {
        let tmp = tempfile::tempdir().unwrap();
        let addr = fake::hub(|req| {
            Reply::Send(serde_json::json!({ "voiced": {
                "req": req, "ok": true, "token": "token-de-prueba", "expires": 3000,
                "project": "proyecto", "location": "eu", "host": "voz.example",
                "model": "modelo-voz",
            }}))
        })
        .await;
        let hub = connected(tmp.path(), addr).await;
        let v = hub.voice_token().await.unwrap();
        assert_eq!(
            v,
            serde_json::json!({
                "token": "token-de-prueba", "expires": 3000, "project": "proyecto",
                "location": "eu", "host": "voz.example", "model": "modelo-voz",
            })
        );
        // Cada pedido con su número: un segundo también llega.
        assert!(hub.voice_token().await.is_ok());
    }

    #[tokio::test]
    async fn the_hub_errors_become_voice_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let addr = fake::hub(|req| {
            let error = match req % 3 {
                1 => "límite de voz",
                2 => "el hub no ofrece voz",
                _ => "invalid_grant",
            };
            Reply::Send(
                serde_json::json!({ "voiced": { "req": req, "ok": false, "error": error } }),
            )
        })
        .await;
        let hub = connected(tmp.path(), addr).await;
        assert_eq!(hub.voice_token().await, Err(VoiceError::Limit));
        assert_eq!(hub.voice_token().await, Err(VoiceError::NotOffered));
        assert_eq!(
            hub.voice_token().await,
            Err(VoiceError::Other("invalid_grant".into()))
        );
    }

    #[tokio::test]
    async fn a_hub_that_does_not_answer_does_not_offer_voice() {
        let tmp = tempfile::tempdir().unwrap();
        // Un hub antiguo: no sabe qué es `voice` y no contesta.
        let addr = fake::hub(|_| Reply::Silent).await;
        let hub = connected(tmp.path(), addr).await;
        assert_eq!(hub.voice_token().await, Err(VoiceError::NotOffered));
        assert!(hub.voice_pending.lock().unwrap().is_empty());
    }

    /// Si se cae la conexión con el pedido en marcha, no hay hub (no se espera a que venza).
    #[tokio::test]
    async fn a_hub_that_hangs_up_is_no_hub() {
        let tmp = tempfile::tempdir().unwrap();
        let addr = fake::hub(|_| Reply::Close).await;
        let hub = connected(tmp.path(), addr).await;
        let started = std::time::Instant::now();
        assert_eq!(hub.voice_token().await, Err(VoiceError::NoHub));
        assert!(started.elapsed() < VOICE_WAIT, "{:?}", started.elapsed());
        assert!(hub.voice_pending.lock().unwrap().is_empty());
    }

    /// Lo mismo con un aviso: «sin conexión», no «no contestó».
    #[tokio::test]
    async fn a_push_when_the_hub_hangs_up_has_no_hub() {
        let tmp = tempfile::tempdir().unwrap();
        let addr = fake::hub_on_push(Reply::Close).await;
        let hub = connected(tmp.path(), addr).await;
        let r = tokio::time::timeout(
            Duration::from_secs(10),
            hub.push("tel", "cifrado".into(), "c"),
        )
        .await
        .expect("el aviso esperó a que venciera");
        assert_eq!(r, Err(PushError::Other("sin conexión con el hub".into())));
        assert!(hub.pending.lock().unwrap().is_empty());
    }

    // ---------- El grupo, la sesión de dispositivo y la cuenta ----------

    /// Un nido conectado a un hub de mentira que graba lo que recibe.
    async fn recorded(
        dir: &Path,
        answer: impl Fn(&serde_json::Value) -> Option<Reply> + Send + 'static,
    ) -> (Arc<Hub>, fake::Recorder) {
        let rec = fake::recording(answer).await;
        let hub = connected(dir, rec.addr).await;
        (hub, rec)
    }

    #[tokio::test]
    async fn the_group_ids_go_once_per_connection() {
        let tmp = tempfile::tempdir().unwrap();
        let (hub, mut rec) = recorded(tmp.path(), |_| None).await;
        let before = crate::group::now_ms();
        hub.send_group(vec!["b".into(), "a".into(), "b".into()]);
        let g = rec.next("group").await;
        assert_eq!(
            g["ids"],
            serde_json::json!(["a", "b"]),
            "ordenados y sin repetir"
        );
        assert!(g["at"].as_u64().unwrap() >= before, "{g}");
        // La misma lista (en otro orden) no se repite; otra, sí.
        hub.send_group(vec!["b".into(), "a".into()]);
        hub.send_group(vec!["a".into(), "b".into(), "c".into()]);
        assert_eq!(
            rec.next("group").await["ids"],
            serde_json::json!(["a", "b", "c"])
        );
        // Una lista vacía (este equipo solo) también es una lista.
        hub.send_group(vec![]);
        assert_eq!(rec.next("group").await["ids"], serde_json::json!([]));

        // Al reconectar se manda aunque sea la misma.
        hub.send_group(vec!["a".into()]);
        assert_eq!(rec.next("group").await["ids"], serde_json::json!(["a"]));
        rec.tell(fake::Tell::Hangup);
        let mut status = hub.status.subscribe();
        tokio::time::timeout(Duration::from_secs(10), status.wait_for(|s| !s.connected))
            .await
            .unwrap()
            .unwrap();
        rec.next("pub").await;
        tokio::time::timeout(Duration::from_secs(10), status.wait_for(|s| s.connected))
            .await
            .unwrap()
            .unwrap();
        hub.send_group(vec!["a".into()]);
        assert_eq!(rec.next("group").await["ids"], serde_json::json!(["a"]));
    }

    #[tokio::test]
    async fn the_account_from_the_hub_stays_in_the_status() {
        let tmp = tempfile::tempdir().unwrap();
        let (hub, mut rec) = recorded(tmp.path(), |_| None).await;
        assert_eq!(hub.status.borrow().account, None);
        let mut status = hub.status.subscribe();
        status.borrow_and_update();
        // Lo que no se entiende se ignora, sin cortar la conexión.
        for odd in [
            serde_json::json!("hola"),
            serde_json::json!([1, 2]),
            serde_json::json!({ "voiced": 5 }),
            serde_json::json!({ "sessioned": { "req": 99, "ok": true } }),
            serde_json::json!({ "account": "mirlo" }),
            serde_json::json!({ "nuevo": {} }),
        ] {
            rec.tell(fake::Tell::Send(odd));
        }
        rec.tell(fake::Tell::Send(serde_json::json!({
            "account": { "name": "mirlo-sereno", "lang": "es", "otra": 1 }
        })));
        let want = serde_json::json!({ "name": "mirlo-sereno", "lang": "es" });
        tokio::time::timeout(
            Duration::from_secs(10),
            status.wait_for(|s| s.account.as_ref() == Some(&want)),
        )
        .await
        .expect("no llegó la cuenta")
        .unwrap();
        assert!(hub.status.borrow().connected);
        // La misma otra vez no avisa; sin nombre ni idioma (recién creada), nulos.
        status.borrow_and_update();
        rec.tell(fake::Tell::Send(serde_json::json!({
            "account": { "name": "mirlo-sereno", "lang": "es" }
        })));
        rec.tell(fake::Tell::Send(serde_json::json!({ "account": {} })));
        let empty = serde_json::json!({ "name": null, "lang": null });
        tokio::time::timeout(
            Duration::from_secs(10),
            status.wait_for(|s| s.account.as_ref() == Some(&empty)),
        )
        .await
        .unwrap()
        .unwrap();
        rec.tell(fake::Tell::Send(serde_json::json!({
            "account": { "name": "mirlo-sereno", "lang": "en" }
        })));
        let en = serde_json::json!({ "name": "mirlo-sereno", "lang": "en" });
        tokio::time::timeout(
            Duration::from_secs(10),
            status.wait_for(|s| s.account.as_ref() == Some(&en)),
        )
        .await
        .unwrap()
        .unwrap();
        // Al desconectar se queda (es lo último que se supo), también al reconectar.
        rec.tell(fake::Tell::Hangup);
        tokio::time::timeout(Duration::from_secs(10), status.wait_for(|s| !s.connected))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(hub.status.borrow().account.as_ref(), Some(&en));
        rec.next("pub").await;
        tokio::time::timeout(Duration::from_secs(10), status.wait_for(|s| s.connected))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(hub.status.borrow().account.as_ref(), Some(&en));
        // Con otro hub, ya no.
        hub.set_url(Some("ws://127.0.0.1:9".into()), None).unwrap();
        tokio::time::timeout(
            Duration::from_secs(10),
            status.wait_for(|s| s.account.is_none()),
        )
        .await
        .unwrap()
        .unwrap();
    }

    /// El nombre y el idioma de la cuenta, como los valida el hub: lo demás no es un nombre ni un
    /// idioma.
    #[test]
    fn account_names_and_langs() {
        for ok in ["mirlo-sereno", "abc", "a1-b2", &"a".repeat(30)] {
            assert!(account_name(ok), "{ok}");
        }
        for bad in [
            "",
            "ab",
            "-abc",
            "abc-",
            "a--b",
            "Mirlo",
            "mirlo sereno",
            "mirlo_s",
            "ñandú",
            &"a".repeat(31),
        ] {
            assert!(!account_name(bad), "{bad}");
        }
        assert!(account_lang("es") && account_lang("en"));
        for bad in ["", "ES", "fr", "es-ES"] {
            assert!(!account_lang(bad), "{bad}");
        }
    }

    /// Un nombre o un idioma que no valen llegan como `null`: un hub no puede meter en la
    /// información de los dispositivos lo que quiera (un nombre enorme no cabría al emparejar).
    #[tokio::test]
    async fn an_odd_account_from_the_hub_is_null() {
        let tmp = tempfile::tempdir().unwrap();
        let (hub, rec) = recorded(tmp.path(), |_| None).await;
        let mut status = hub.status.subscribe();
        rec.tell(fake::Tell::Send(serde_json::json!({
            "account": { "name": "x".repeat(70_000), "lang": "fr" }
        })));
        let empty = serde_json::json!({ "name": null, "lang": null });
        tokio::time::timeout(
            Duration::from_secs(10),
            status.wait_for(|s| s.account.as_ref() == Some(&empty)),
        )
        .await
        .expect("no llegó la cuenta")
        .unwrap();
        rec.tell(fake::Tell::Send(serde_json::json!({
            "account": { "name": "Mirlo--Sereno", "lang": "en" }
        })));
        let en = serde_json::json!({ "name": null, "lang": "en" });
        tokio::time::timeout(
            Duration::from_secs(10),
            status.wait_for(|s| s.account.as_ref() == Some(&en)),
        )
        .await
        .expect("no llegó la cuenta")
        .unwrap();
    }

    #[tokio::test]
    async fn the_session_grant_comes_from_the_hub() {
        let tmp = tempfile::tempdir().unwrap();
        let (hub, mut rec) = recorded(tmp.path(), |v| {
            v["session"]["req"].as_u64().map(|req| {
                Reply::Send(serde_json::json!({ "sessioned": {
                    "req": req, "ok": true, "grant": "vale-de-prueba", "expires": 60000,
                }}))
            })
        })
        .await;
        // En nombre de un dispositivo: va su id.
        let v = hub.hub_session(Some("a1b2c3")).await.unwrap();
        assert_eq!(
            v,
            serde_json::json!({ "grant": "vale-de-prueba", "expires": 60000 })
        );
        let asked = rec.next("session").await;
        assert_eq!(asked["device"], "a1b2c3");
        assert!(asked["req"].as_u64().is_some(), "{asked}");
        // Sin dispositivo, sin `device`.
        assert!(hub.hub_session(None).await.is_ok());
        let asked = rec.next("session").await;
        assert!(asked.get("device").is_none(), "{asked}");
        assert!(hub.session_pending.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn the_hub_errors_become_session_errors() {
        let tmp = tempfile::tempdir().unwrap();
        // Lo que contesta, según el dispositivo que pide (no según el número del pedido).
        let (hub, _rec) = recorded(tmp.path(), |v| {
            let req = v["session"]["req"].as_u64()?;
            let reply = match v["session"]["device"].as_str() {
                Some("freno") => serde_json::json!({ "ok": false, "error": "límite de sesiones" }),
                Some("sin") => {
                    serde_json::json!({ "ok": false, "error": "el hub no ofrece sesiones" })
                }
                Some("sin-vale") => serde_json::json!({ "ok": true, "expires": 60000 }),
                Some("vale-raro") => serde_json::json!({ "ok": true, "grant": 7 }),
                _ => serde_json::json!({ "ok": false, "error": "error del hub" }),
            };
            let mut reply = reply;
            reply["req"] = req.into();
            Some(Reply::Send(serde_json::json!({ "sessioned": reply })))
        })
        .await;
        assert_eq!(
            hub.hub_session(Some("freno")).await,
            Err(SessionError::Limit)
        );
        assert_eq!(
            hub.hub_session(Some("sin")).await,
            Err(SessionError::NotOffered)
        );
        assert_eq!(
            hub.hub_session(None).await,
            Err(SessionError::Hub("error del hub".into()))
        );
        // Un `ok` sin vale (o con uno que no es texto) no es un vale.
        let no_grant = Err(SessionError::Hub("el hub no dio el vale".into()));
        assert_eq!(hub.hub_session(Some("sin-vale")).await, no_grant);
        assert_eq!(hub.hub_session(Some("vale-raro")).await, no_grant);
    }

    /// La lista de dispositivos va una vez por conexión, y otra vez al reconectar.
    #[tokio::test]
    async fn the_devices_go_once_per_connection() {
        let tmp = tempfile::tempdir().unwrap();
        let (hub, mut rec) = recorded(tmp.path(), |_| None).await;
        hub.send_devices(vec!["b2".into(), "a1".into()]);
        assert_eq!(
            rec.next("devices").await,
            serde_json::json!({ "ids": ["a1", "b2"] })
        );
        hub.send_devices(vec!["a1".into()]);
        rec.quiet("devices", Duration::from_millis(300)).await;
        rec.tell(fake::Tell::Hangup);
        let mut status = hub.status.subscribe();
        tokio::time::timeout(Duration::from_secs(10), status.wait_for(|s| !s.connected))
            .await
            .unwrap()
            .unwrap();
        rec.next("pub").await;
        tokio::time::timeout(Duration::from_secs(10), status.wait_for(|s| s.connected))
            .await
            .unwrap()
            .unwrap();
        hub.send_devices(vec![]);
        assert_eq!(rec.next("devices").await, serde_json::json!({ "ids": [] }));
    }

    #[tokio::test]
    async fn a_hub_that_does_not_answer_does_not_offer_sessions() {
        let tmp = tempfile::tempdir().unwrap();
        // Un hub antiguo: no sabe qué es `session` y no contesta.
        let (hub, _rec) = recorded(tmp.path(), |_| None).await;
        assert_eq!(
            hub.hub_session(Some("a1b2c3")).await,
            Err(SessionError::NotOffered)
        );
        assert!(hub.session_pending.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_hub_that_hangs_up_has_no_session() {
        let tmp = tempfile::tempdir().unwrap();
        let (hub, _rec) = recorded(tmp.path(), |v| v.get("session").map(|_| Reply::Close)).await;
        let started = std::time::Instant::now();
        assert_eq!(hub.hub_session(None).await, Err(SessionError::NoHub));
        assert!(started.elapsed() < SESSION_WAIT, "{:?}", started.elapsed());
        assert!(hub.session_pending.lock().unwrap().is_empty());
        let none = Hub::new(tmp.path().join("otro.json"));
        assert_eq!(none.hub_session(None).await, Err(SessionError::NoHub));
    }

    /// Un dispositivo dado de baja: se avisa al hub, si hay conexión (si no, nada).
    #[tokio::test]
    async fn unsession_tells_the_hub() {
        let tmp = tempfile::tempdir().unwrap();
        let (hub, mut rec) = recorded(tmp.path(), |_| None).await;
        hub.unsession("a1b2c3");
        assert_eq!(
            rec.next("unsession").await,
            serde_json::json!({ "device": "a1b2c3" })
        );
        let none = Hub::new(tmp.path().join("otro.json"));
        none.unsession("a1b2c3");
    }

    /// Lo que le llega al hub de mentira antes de la marca: un `unsession` de `marca`, que sale
    /// detrás por la misma cola (en orden).
    async fn sent_before_the_mark(hub: &Hub, rec: &mut fake::Recorder) -> Vec<serde_json::Value> {
        hub.unsession("marca");
        let mut sent = vec![];
        loop {
            let v = rec.next_any().await;
            if v["unsession"]["device"] == "marca" {
                return sent;
            }
            sent.push(v);
        }
    }

    /// Un pedido cuyo plazo vence antes de salir por la conexión (un hub lento, una reconexión)
    /// ya no sale: lo que diera el hub no le serviría a nadie y gastaría cupo de su freno.
    #[tokio::test]
    async fn an_expired_ask_is_not_sent() {
        let tmp = tempfile::tempdir().unwrap();
        let (hub, mut rec) = recorded(tmp.path(), |_| None).await;
        let r = hub
            .ask(Asking::Session, Default::default(), Duration::ZERO)
            .await;
        assert!(matches!(r, Err(Unanswered::Silent)));
        let sent = sent_before_the_mark(&hub, &mut rec).await;
        assert!(sent.iter().all(|v| v.get("session").is_none()), "{sent:?}");
    }

    /// Basta con el plazo: aunque quien lo pidió siga esperando, si venció, no sale.
    #[tokio::test]
    async fn an_expired_ask_is_not_sent_even_if_awaited() {
        let tmp = tempfile::tempdir().unwrap();
        let (hub, mut rec) = recorded(tmp.path(), |_| None).await;
        let (tx, _rx) = oneshot::channel();
        hub.session_pending.lock().unwrap().insert(77, tx);
        let control = hub.control.lock().unwrap().clone().unwrap();
        let ask = Asked {
            of: Asking::Session,
            req: 77,
            deadline: tokio::time::Instant::now(),
        };
        let text = serde_json::json!({ "session": { "req": 77 } }).to_string();
        assert!(
            control
                .send(Control {
                    text,
                    ask: Some(ask)
                })
                .is_ok()
        );
        let sent = sent_before_the_mark(&hub, &mut rec).await;
        assert!(sent.iter().all(|v| v.get("session").is_none()), "{sent:?}");
    }

    /// Ni uno que ya no espera nadie (quien lo pidió se fue antes de que saliera): tampoco se
    /// queda entre los que esperan respuesta.
    #[tokio::test]
    async fn an_ask_nobody_waits_for_is_not_sent() {
        use futures_util::FutureExt;
        let tmp = tempfile::tempdir().unwrap();
        let (hub, mut rec) = recorded(tmp.path(), |_| None).await;
        // Se pide y se deja de esperar enseguida, antes de que el pedido salga.
        assert!(hub.hub_session(Some("a1b2c3")).now_or_never().is_none());
        assert!(hub.voice_token().now_or_never().is_none());
        let sent = sent_before_the_mark(&hub, &mut rec).await;
        assert!(
            sent.iter()
                .all(|v| v.get("session").is_none() && v.get("voice").is_none()),
            "{sent:?}"
        );
        assert!(hub.session_pending.lock().unwrap().is_empty());
        assert!(hub.voice_pending.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn without_a_hub_there_is_no_voice() {
        let tmp = tempfile::tempdir().unwrap();
        let hub = Hub::new(tmp.path().join("hub.json"));
        assert_eq!(hub.voice_token().await, Err(VoiceError::NoHub));
    }

    // ---------- El tablero ----------

    const OBJ: &str = "0123456789abcdef0123456789abcdef";
    const KID: &str = "1ffd9bb07aebb8f4";

    fn shared(b: fake::Board) -> Arc<Mutex<fake::Board>> {
        Arc::new(Mutex::new(b))
    }

    /// Leer, escribir y borrar, como contesta el hub: la versión sube, un 409 trae el objeto
    /// como está y la lápida va sin `kid` ni blob.
    #[tokio::test]
    async fn the_board_reads_and_writes() {
        let tmp = tempfile::tempdir().unwrap();
        let board = shared(fake::Board::default());
        let mut rec = fake::board(board.clone()).await;
        let hub = connected(tmp.path(), rec.addr).await;
        let page = hub.board_get(0, None).await.unwrap();
        assert_eq!(
            page,
            BoardPage {
                items: vec![],
                next: 0,
                more: false,
                after: None,
                reset: false
            }
        );
        let asked = rec.next("board").await;
        assert_eq!(asked["op"], "get");
        assert_eq!(asked["since"], 0);
        assert!(asked["after"].is_null(), "{asked}");
        assert!(asked["req"].as_u64().is_some(), "{asked}");

        let st = hub.board_put(OBJ, 0, KID, "QUJD").await.unwrap();
        assert_eq!(st.v, 1);
        let asked = rec.next("board").await;
        assert_eq!(
            (
                &asked["op"],
                &asked["id"],
                &asked["v"],
                &asked["kid"],
                &asked["blob"]
            ),
            (
                &"put".into(),
                &OBJ.into(),
                &0.into(),
                &KID.into(),
                &"QUJD".into()
            )
        );
        let page = hub.board_get(0, None).await.unwrap();
        assert_eq!(
            page.items,
            vec![BoardItem {
                id: OBJ.into(),
                v: 1,
                kid: KID.into(),
                blob: Some("QUJD".into()),
                gone: false,
                at: st.at
            }]
        );
        assert_eq!(page.next, st.at);
        // Con la versión que no es: el objeto como está.
        let Err(BoardError::Conflict(cur)) = hub.board_put(OBJ, 0, KID, "REVG").await else {
            panic!("tenía que chocar")
        };
        assert_eq!((cur.v, cur.blob.as_deref()), (1, Some("QUJD")));
        let del = hub.board_del(OBJ, 1).await.unwrap();
        assert_eq!(del.v, 2);
        assert!(del.at > st.at);
        // Un borrado repetido: 409 con la lápida.
        let Err(BoardError::Conflict(cur)) = hub.board_del(OBJ, 1).await else {
            panic!("tenía que chocar")
        };
        assert!(
            cur.gone && cur.blob.is_none() && cur.kid.is_empty(),
            "{cur:?}"
        );
        assert_eq!(cur.v, 2);
        assert!(hub.board_pending.lock().unwrap().is_empty());
    }

    /// Las páginas siguen el cursor del hub, tal cual.
    #[tokio::test]
    async fn the_board_pages_carry_the_cursor() {
        let tmp = tempfile::tempdir().unwrap();
        let mut b = fake::Board {
            page: 2,
            ..Default::default()
        };
        for i in 0..3 {
            b.write(&format!("{i:032x}"), Some((KID, "QUJD")));
        }
        let board = shared(b);
        let rec = fake::board(board.clone()).await;
        let hub = connected(tmp.path(), rec.addr).await;
        let first = hub.board_get(0, None).await.unwrap();
        assert_eq!(first.items.len(), 2);
        assert!(first.more);
        let after = first.after.clone().unwrap();
        assert_eq!(
            after,
            format!("{}:{}", first.items[1].at, first.items[1].id)
        );
        let second = hub.board_get(0, Some(&after)).await.unwrap();
        assert_eq!(second.items.len(), 1);
        assert!(!second.more && second.after.is_none());
        assert_eq!(second.next, board.lock().unwrap().clock);
        assert_eq!(
            board.lock().unwrap().gets,
            vec![(0, None), (0, Some(after))]
        );
    }

    /// Cada estado del hub, su error: el 413 de la cuenta llena y el del blob largo se
    /// distinguen por el texto, los dos 429 son `Busy` y lo demás es `Hub`.
    #[tokio::test]
    async fn the_board_errors_from_the_hub() {
        let tmp = tempfile::tempdir().unwrap();
        // Lo que contesta, según el `kid` de la escritura.
        let (hub, _rec) = recorded(tmp.path(), |v| {
            let b = v.get("board")?;
            let req = b["req"].as_u64()?;
            let (status, error) = match b["kid"].as_str().unwrap_or_default() {
                "full" => (413, "la cuenta ya tiene 2000 objetos"),
                "big" => (413, "el blob pasa de 11000 bytes"),
                "rate" => (
                    429,
                    "demasiadas escrituras en el tablero: prueba en un rato",
                ),
                "many" => (429, "demasiadas peticiones del tablero a la vez"),
                "none" => (503, "el nido no tiene cuenta"),
                "bad" => (400, "petición del tablero no válida"),
                "odd" => (409, "la versión no es la que hay"),
                "silent" => return Some(Reply::Silent),
                _ => (503, "error del hub"),
            };
            Some(Reply::Send(serde_json::json!({ "boarded": {
                "req": req, "status": status, "error": error,
            }})))
        })
        .await;
        let put = |kid: &'static str| {
            let hub = hub.clone();
            async move { hub.board_put(OBJ, 0, kid, "QUJD").await }
        };
        assert_eq!(put("full").await, Err(BoardError::Full));
        assert_eq!(put("big").await, Err(BoardError::TooBig));
        assert_eq!(put("rate").await, Err(BoardError::Busy));
        assert_eq!(put("many").await, Err(BoardError::Busy));
        assert_eq!(
            put("none").await,
            Err(BoardError::Hub("el nido no tiene cuenta".into()))
        );
        assert_eq!(
            put("mongo").await,
            Err(BoardError::Hub("error del hub".into()))
        );
        assert!(matches!(put("bad").await, Err(BoardError::Hub(_))));
        // Un 409 sin el objeto como está no es un conflicto que se pueda resolver.
        assert!(matches!(put("odd").await, Err(BoardError::Hub(_))));
        // Uno que no contesta (un hub antiguo): al vencer el plazo, `Hub`.
        let started = std::time::Instant::now();
        assert!(matches!(put("silent").await, Err(BoardError::Hub(_))));
        assert!(started.elapsed() >= BOARD_WAIT);
        assert!(hub.board_pending.lock().unwrap().is_empty());
        // Sin conexión, también.
        let none = Hub::new(tmp.path().join("otro.json"));
        assert!(matches!(
            none.board_get(0, None).await,
            Err(BoardError::Hub(_))
        ));
    }

    /// Un corte con la petición en marcha: `Hub`, sin esperar al plazo.
    #[tokio::test]
    async fn a_board_request_when_the_hub_hangs_up() {
        let tmp = tempfile::tempdir().unwrap();
        let (hub, _rec) = recorded(tmp.path(), |v| v.get("board").map(|_| Reply::Close)).await;
        let started = std::time::Instant::now();
        assert!(matches!(
            hub.board_get(0, None).await,
            Err(BoardError::Hub(_))
        ));
        assert!(started.elapsed() < BOARD_WAIT, "{:?}", started.elapsed());
        assert!(hub.board_pending.lock().unwrap().is_empty());
    }

    /// Las respuestas pueden llegar en otro orden: cada una va a quien la pidió, por su `req`.
    #[tokio::test]
    async fn board_answers_out_of_order_find_their_request() {
        let tmp = tempfile::tempdir().unwrap();
        let (hub, mut rec) = recorded(tmp.path(), |_| None).await;
        let a = tokio::spawn({
            let hub = hub.clone();
            async move { hub.board_put(OBJ, 0, KID, "QUFB").await }
        });
        let first = rec.next("board").await;
        let b = tokio::spawn({
            let hub = hub.clone();
            async move { hub.board_del(OBJ, 7).await }
        });
        let second = rec.next("board").await;
        assert_eq!(
            (&first["op"], &second["op"]),
            (&"put".into(), &"del".into())
        );
        rec.tell(fake::Tell::Send(serde_json::json!({ "boarded": {
            "req": second["req"], "status": 200, "v": 8, "at": 20,
        }})));
        rec.tell(fake::Tell::Send(serde_json::json!({ "boarded": {
            "req": first["req"], "status": 200, "v": 1, "at": 10,
        }})));
        assert_eq!(b.await.unwrap(), Ok(BoardStamp { v: 8, at: 20 }));
        assert_eq!(a.await.unwrap(), Ok(BoardStamp { v: 1, at: 10 }));
    }

    /// Como mucho `BOARD_IN_FLIGHT` sin contestar: la quinta sale cuando se contesta una.
    #[tokio::test]
    async fn at_most_four_board_requests_in_flight() {
        let tmp = tempfile::tempdir().unwrap();
        let (hub, mut rec) = recorded(tmp.path(), |_| None).await;
        let mut calls = vec![];
        for i in 0..6 {
            let hub = hub.clone();
            calls.push(tokio::spawn(async move { hub.board_get(i, None).await }));
        }
        let mut asked = vec![];
        for _ in 0..BOARD_IN_FLIGHT {
            asked.push(rec.next("board").await);
        }
        rec.quiet("board", Duration::from_millis(300)).await;
        rec.tell(fake::Tell::Send(serde_json::json!({ "boarded": {
            "req": asked[0]["req"], "status": 200, "items": [], "next": 0, "more": false,
            "after": null, "reset": false,
        }})));
        asked.push(rec.next("board").await);
        rec.quiet("board", Duration::from_millis(300)).await;
        // Cada `since` una sola vez: ninguna se ha repetido.
        let mut since: Vec<i64> = asked.iter().map(|a| a["since"].as_i64().unwrap()).collect();
        since.sort();
        since.dedup();
        assert_eq!(since.len(), BOARD_IN_FLIGHT + 1);
        for c in calls {
            c.abort();
        }
    }

    /// El aviso del hub despierta a quien escucha, también con la misma marca otra vez, y no
    /// corta la conexión. Uno raro (sin objeto) no es un aviso.
    #[tokio::test]
    async fn the_board_notice_wakes_the_listener() {
        let tmp = tempfile::tempdir().unwrap();
        let (hub, rec) = recorded(tmp.path(), |_| None).await;
        let mut notices = hub.board_notices();
        notices.borrow_and_update();
        rec.tell(fake::Tell::Send(
            serde_json::json!({ "board": { "at": 1794868073946142i64 } }),
        ));
        tokio::time::timeout(Duration::from_secs(10), notices.changed())
            .await
            .expect("no llegó el aviso")
            .unwrap();
        assert_eq!(*notices.borrow_and_update(), 1794868073946142);
        rec.tell(fake::Tell::Send(
            serde_json::json!({ "board": { "at": 1794868073946142i64 } }),
        ));
        tokio::time::timeout(Duration::from_secs(10), notices.changed())
            .await
            .expect("no llegó el aviso repetido")
            .unwrap();
        notices.borrow_and_update();
        rec.tell(fake::Tell::Send(serde_json::json!({ "board": 5 })));
        rec.tell(fake::Tell::Send(
            serde_json::json!({ "board": { "at": 7 } }),
        ));
        tokio::time::timeout(Duration::from_secs(10), notices.changed())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(*notices.borrow_and_update(), 7);
        assert!(hub.status.borrow().connected);
    }
}

/// Un hub de mentira en `127.0.0.1`, para las pruebas de aquí y de la API.
#[cfg(test)]
pub(crate) mod fake {
    use super::*;

    /// Qué hace el hub con un pedido.
    pub enum Reply {
        /// Contesta esto.
        Send(serde_json::Value),
        /// Nada (como un hub antiguo).
        Silent,
        /// Cuelga.
        Close,
    }

    /// Un hub que pide la firma (no la comprueba), da un id y atiende cada
    /// `{"voice":{"req":N}}` según `answer(N)`. Solo acepta una conexión.
    pub async fn hub(answer: impl Fn(u64) -> Reply + Send + 'static) -> std::net::SocketAddr {
        serve(move |v| v["voice"]["req"].as_u64().map(&answer)).await
    }

    /// Como [`hub`], pero con los avisos (`{"push":…}`): a todos, `reply`.
    pub async fn hub_on_push(reply: Reply) -> std::net::SocketAddr {
        let reply = std::sync::Mutex::new(Some(reply));
        serve(move |v| v.get("push").and_then(|_| reply.lock().unwrap().take())).await
    }

    async fn serve(
        answer: impl Fn(&serde_json::Value) -> Option<Reply> + Send + 'static,
    ) -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
            let challenge = serde_json::json!({ "challenge": B64.encode([7u8; 32]) });
            ws.send(Message::Text(challenge.to_string().into()))
                .await
                .unwrap();
            while let Some(Ok(m)) = ws.next().await {
                if m.is_text() {
                    break;
                }
            }
            ws.send(Message::Text(r#"{"id":"este"}"#.into()))
                .await
                .unwrap();
            while let Some(Ok(m)) = ws.next().await {
                let Message::Text(t) = m else { continue };
                let v: serde_json::Value = serde_json::from_str(&t).unwrap();
                match answer(&v) {
                    Some(Reply::Send(reply)) => ws
                        .send(Message::Text(reply.to_string().into()))
                        .await
                        .unwrap(),
                    Some(Reply::Close) => return,
                    Some(Reply::Silent) | None => {}
                }
            }
        });
        addr
    }

    /// Lo que se le pide al hub de mentira que graba ([`recording`]).
    pub enum Tell {
        /// Que mande esto al nido.
        Send(serde_json::Value),
        /// Que cuelgue (y espere a la siguiente conexión).
        Hangup,
    }

    /// Un hub de mentira que graba lo que le manda el nido.
    pub struct Recorder {
        pub addr: std::net::SocketAddr,
        got: mpsc::UnboundedReceiver<serde_json::Value>,
        tell: mpsc::UnboundedSender<Tell>,
    }

    impl Recorder {
        /// Lo de dentro del siguiente texto del nido que sea `{kind: …}` (los demás se saltan).
        /// La firma (`pub`) es lo primero de cada conexión.
        pub async fn next(&mut self, kind: &str) -> serde_json::Value {
            let wait = async {
                loop {
                    let mut v = self.got.recv().await.expect("el hub de mentira se cerró");
                    if v.get(kind).is_some() {
                        return v[kind].take();
                    }
                }
            };
            tokio::time::timeout(Duration::from_secs(10), wait)
                .await
                .unwrap_or_else(|_| panic!("no llegó «{kind}»"))
        }

        /// El siguiente texto del nido, sea el que sea.
        pub async fn next_any(&mut self) -> serde_json::Value {
            tokio::time::timeout(Duration::from_secs(10), self.got.recv())
                .await
                .expect("no llegó nada")
                .expect("el hub de mentira se cerró")
        }

        /// Que no llegue ningún `{kind: …}` en `wait` (los demás se saltan).
        pub async fn quiet(&mut self, kind: &str, wait: Duration) {
            let deadline = tokio::time::Instant::now() + wait;
            while let Ok(Some(v)) = tokio::time::timeout_at(deadline, self.got.recv()).await {
                assert!(v.get(kind).is_none(), "no tenía que llegar: {v}");
            }
        }

        pub fn tell(&self, t: Tell) {
            self.tell.send(t).unwrap();
        }
    }

    /// Un hub que acepta conexiones una tras otra, pide la firma (no la comprueba), da el id
    /// `este`, graba todo lo que manda el nido (la firma también) y contesta según `answer`.
    pub async fn recording(
        answer: impl Fn(&serde_json::Value) -> Option<Reply> + Send + 'static,
    ) -> Recorder {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (got_tx, got) = mpsc::unbounded_channel();
        let (tell, mut tell_rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Ok((tcp, _)) = listener.accept().await {
                let Ok(mut ws) = tokio_tungstenite::accept_async(tcp).await else {
                    continue;
                };
                let challenge = serde_json::json!({ "challenge": B64.encode([7u8; 32]) });
                if ws
                    .send(Message::Text(challenge.to_string().into()))
                    .await
                    .is_err()
                {
                    continue;
                }
                loop {
                    tokio::select! {
                        m = ws.next() => {
                            let Some(Ok(m)) = m else { break };
                            let Message::Text(t) = m else { continue };
                            let v: serde_json::Value = serde_json::from_str(&t).unwrap();
                            let first = v.get("pub").is_some();
                            let reply = answer(&v);
                            let _ = got_tx.send(v);
                            if first {
                                let _ = ws.send(Message::Text(r#"{"id":"este"}"#.into())).await;
                                continue;
                            }
                            match reply {
                                Some(Reply::Send(r)) => {
                                    let _ = ws.send(Message::Text(r.to_string().into())).await;
                                }
                                Some(Reply::Close) => break,
                                Some(Reply::Silent) | None => {}
                            }
                        }
                        t = tell_rx.recv() => match t {
                            Some(Tell::Send(v)) => {
                                let _ = ws.send(Message::Text(v.to_string().into())).await;
                            }
                            Some(Tell::Hangup) => break,
                            None => return,
                        },
                    }
                }
                let _ = ws.close(None).await;
            }
        });
        Recorder { addr, got, tell }
    }

    /// Un equipo de prueba para el lado del nido.
    pub struct Host {
        id: e2e::Identity,
        pairing: e2e::Pairing,
        phones: Mutex<e2e::Phones>,
    }

    impl e2e::Host for Host {
        fn identity(&self) -> &e2e::Identity {
            &self.id
        }
        fn pairing(&self) -> &e2e::Pairing {
            &self.pairing
        }
        fn phones(&self) -> &Mutex<e2e::Phones> {
            &self.phones
        }
        fn local_addr(&self) -> Option<std::net::SocketAddr> {
            None
        }
        fn host_name(&self) -> String {
            "equipo".into()
        }
    }

    /// Un nido (con su configuración en `dir`) conectado al hub de mentira de `addr`.
    pub async fn nido(dir: &Path, addr: std::net::SocketAddr) -> Arc<Hub> {
        let hub = Hub::new(dir.join("hub.json"));
        let host = Arc::new(Host {
            id: e2e::Identity::load_or_create(&dir.join("nido.key")).unwrap(),
            pairing: e2e::Pairing::default(),
            phones: Mutex::new(e2e::Phones::load(dir.join("phones.json"))),
        });
        connect(&hub, host, addr).await;
        hub
    }

    // ---------- El tablero de mentira ----------

    /// La primera marca del tablero de mentira (µs, una fecha de verdad), y lo que sube con cada
    /// escritura.
    pub const BOARD_T0: i64 = 1_794_868_073_000_000;
    const BOARD_TICK: i64 = 1000;
    /// El solape de la primera página, como en el hub.
    const BOARD_OVERLAP: i64 = 2_000_000;

    /// Un tablero en memoria que contesta como el del hub (versiones, lápidas, el solape de 2 s,
    /// páginas por número de objetos y el cursor `after`), con lo que hace falta para las pruebas:
    /// un `reset`, escrituras de otro que se adelantan y el freno.
    pub struct Board {
        pub items: std::collections::BTreeMap<String, BoardItem>,
        /// La marca de la última escritura.
        pub clock: i64,
        /// Objetos por página.
        pub page: usize,
        /// Que la próxima primera página con `since > 0` diga `reset`.
        pub reset: bool,
        /// Lo que escribe otro justo antes de cada una de las próximas escrituras (su `kid` y su
        /// blob; `None`, una lápida): la nuestra choca (409).
        pub clash: std::collections::VecDeque<Option<(String, String)>>,
        /// Las próximas escrituras que dan 429.
        pub busy: usize,
        /// Lo que se ha pedido leer: `(since, after)`.
        pub gets: Vec<(i64, Option<String>)>,
        /// Las escrituras que han llegado: cuándo, `op`, id y `v`.
        pub writes: Vec<(std::time::Instant, String, String, i64)>,
    }

    impl Default for Board {
        fn default() -> Self {
            Self {
                items: Default::default(),
                clock: BOARD_T0,
                page: 100,
                reset: false,
                clash: Default::default(),
                busy: 0,
                gets: vec![],
                writes: vec![],
            }
        }
    }

    impl Board {
        fn tick(&mut self) -> i64 {
            self.clock += BOARD_TICK;
            self.clock
        }

        /// Otro escribe `id` (un blob, o una lápida con `None`): da el objeto como queda.
        pub fn write(&mut self, id: &str, blob: Option<(&str, &str)>) -> BoardItem {
            let v = self.items.get(id).map_or(0, |i| i.v) + 1;
            let at = self.tick();
            let item = BoardItem {
                id: id.into(),
                v,
                kid: blob.map(|b| b.0.to_owned()).unwrap_or_default(),
                blob: blob.map(|b| b.1.to_owned()),
                gone: blob.is_none(),
                at,
            };
            self.items.insert(id.into(), item.clone());
            item
        }

        /// Contesta un `{"board": …}` del nido como el hub; lo demás, nada.
        pub fn answer(&mut self, msg: &serde_json::Value) -> Option<Reply> {
            let b = msg.get("board")?;
            let req = b["req"].as_u64()?;
            let mut reply = match b["op"].as_str() {
                Some("get") => self.get(b["since"].as_i64().unwrap_or(0), b["after"].as_str()),
                Some(op @ ("put" | "del")) => self.change(op, b),
                _ => {
                    serde_json::json!({ "status": 400, "error": "petición del tablero no válida" })
                }
            };
            reply["req"] = req.into();
            Some(Reply::Send(serde_json::json!({ "boarded": reply })))
        }

        fn get(&mut self, since: i64, after: Option<&str>) -> serde_json::Value {
            self.gets.push((since, after.map(str::to_owned)));
            if after.is_none() && since > 0 && std::mem::take(&mut self.reset) {
                return serde_json::json!({ "status": 200, "items": [], "next": 0,
                    "more": false, "after": null, "reset": true });
            }
            let after = after.map(|a| {
                let (at, id) = a.split_once(':').unwrap();
                (at.parse::<i64>().unwrap(), id.to_owned())
            });
            let lo = if since > 0 {
                since - BOARD_OVERLAP
            } else {
                i64::MIN
            };
            let mut rows: Vec<&BoardItem> = self
                .items
                .values()
                .filter(|i| match &after {
                    Some((at, id)) => (i.at, &i.id) > (*at, id),
                    None => i.at > lo,
                })
                .collect();
            rows.sort_by(|a, b| (a.at, &a.id).cmp(&(b.at, &b.id)));
            let more = rows.len() > self.page;
            rows.truncate(self.page);
            let next = rows.iter().map(|i| i.at).max().unwrap_or(0).max(since);
            let after = rows
                .last()
                .filter(|_| more)
                .map(|i| format!("{}:{}", i.at, i.id));
            serde_json::json!({ "status": 200, "items": rows, "next": next, "more": more,
                "after": after, "reset": false })
        }

        fn change(&mut self, op: &str, b: &serde_json::Value) -> serde_json::Value {
            let id = b["id"].as_str().unwrap_or_default().to_owned();
            let v = b["v"].as_i64().unwrap_or(-1);
            self.writes
                .push((std::time::Instant::now(), op.into(), id.clone(), v));
            if self.busy > 0 {
                self.busy -= 1;
                return serde_json::json!({ "status": 429,
                    "error": "demasiadas escrituras en el tablero: prueba en un rato" });
            }
            if let Some(other) = self.clash.pop_front() {
                self.write(&id, other.as_ref().map(|(k, b)| (k.as_str(), b.as_str())));
            }
            let current = self.items.get(&id).cloned().unwrap_or(BoardItem {
                id: id.clone(),
                v: 0,
                kid: String::new(),
                blob: None,
                gone: true,
                at: 0,
            });
            if current.v != v {
                return serde_json::json!({ "status": 409,
                    "error": "la versión no es la que hay", "current": current });
            }
            let item = if op == "put" {
                let (kid, blob) = (b["kid"].as_str().unwrap(), b["blob"].as_str().unwrap());
                self.write(&id, Some((kid, blob)))
            } else {
                self.write(&id, None)
            };
            serde_json::json!({ "status": 200, "v": item.v, "at": item.at })
        }
    }

    /// Un hub de mentira (el que graba) con el tablero `board`.
    pub async fn board(board: Arc<Mutex<Board>>) -> Recorder {
        recording(move |v| board.lock().unwrap().answer(v)).await
    }

    /// Conecta `hub` al hub de mentira de `addr` y espera a que esté conectado.
    pub async fn connect<H: e2e::Host>(hub: &Arc<Hub>, host: Arc<H>, addr: std::net::SocketAddr) {
        hub.set_url(Some(format!("ws://{addr}")), None).unwrap();
        spawn(hub.clone(), host);
        let mut status = hub.status.subscribe();
        tokio::time::timeout(Duration::from_secs(10), status.wait_for(|s| s.connected))
            .await
            .expect("no se conectó al hub")
            .unwrap();
    }
}
