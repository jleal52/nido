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

/// Los pedidos que esperan respuesta del hub, por su número.
type Waiters = Mutex<HashMap<u64, oneshot::Sender<serde_json::Value>>>;

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
    /// llenarlo) y salgan en el orden en que se mandaron.
    control: Mutex<Option<mpsc::UnboundedSender<Message>>>,
    pending: Pending,
    /// Los pedidos de token de voz que esperan respuesta (lo que vino en `voiced`).
    voice_pending: Waiters,
    /// Los pedidos de sesión de dispositivo que esperan respuesta (lo que vino en `sessioned`).
    session_pending: Waiters,
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

    /// Manda `{kind: {"req": N, …}}` (lo de `body`, con su número) y espera lo que conteste el
    /// hub con ese número, que le entrega el despachador de los textos (`answered`).
    async fn ask(
        &self,
        waiters: &Waiters,
        kind: &str,
        mut body: serde_json::Map<String, serde_json::Value>,
        wait: Duration,
    ) -> Result<serde_json::Value, Unanswered> {
        let control = self
            .control
            .lock()
            .expect("hub")
            .clone()
            .ok_or(Unanswered::NoHub)?;
        let req = self.next_req.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        waiters.lock().expect("pedidos").insert(req, tx);
        body.insert("req".into(), req.into());
        let mut msg = serde_json::Map::new();
        msg.insert(kind.into(), body.into());
        let msg = serde_json::Value::Object(msg);
        if control.send(Message::Text(msg.to_string().into())).is_err() {
            waiters.lock().expect("pedidos").remove(&req);
            return Err(Unanswered::NoHub);
        }
        match tokio::time::timeout(wait, rx).await {
            Ok(Ok(v)) => Ok(v),
            // Se cerró la conexión con el pedido en marcha (ver `disconnected`).
            Ok(Err(_)) => Err(Unanswered::NoHub),
            Err(_) => {
                waiters.lock().expect("pedidos").remove(&req);
                Err(Unanswered::Silent)
            }
        }
    }

    /// Pide al hub un token de voz para que la web o la app hablen con el modelo directamente.
    /// Devuelve lo que da el hub (`token`, `expires`, `project`, `location`, `host`, `model`).
    pub async fn voice_token(&self) -> Result<serde_json::Value, VoiceError> {
        let mut v = match self
            .ask(&self.voice_pending, "voice", Default::default(), VOICE_WAIT)
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
        let mut v = match self
            .ask(&self.session_pending, "session", body, SESSION_WAIT)
            .await
        {
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
        if control.send(Message::Text(msg.to_string().into())).is_ok() {
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
        if control.send(Message::Text(msg.to_string().into())).is_ok() {
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
        let _ = control.send(Message::Text(msg.to_string().into()));
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
    let (control_tx, mut control_rx) = mpsc::unbounded_channel::<Message>();
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
            Some(m) = control_rx.recv() => {
                tx.send(m).await.map_err(|e| e.to_string())?;
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
                        let _ = hub.voiced(&mut v) || hub.sessioned(&mut v) || hub.account(&v);
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

    struct TestHost {
        id: e2e::Identity,
        pairing: e2e::Pairing,
        phones: Mutex<e2e::Phones>,
    }

    impl e2e::Host for TestHost {
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

    /// Un nido conectado al hub de mentira de `addr`.
    async fn connected(dir: &Path, addr: std::net::SocketAddr) -> Arc<Hub> {
        let hub = Hub::new(dir.join("hub.json"));
        let host = Arc::new(TestHost {
            id: e2e::Identity::load_or_create(&dir.join("nido.key")).unwrap(),
            pairing: e2e::Pairing::default(),
            phones: Mutex::new(e2e::Phones::load(dir.join("phones.json"))),
        });
        fake::connect(&hub, host, addr).await;
        hub
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

    #[tokio::test]
    async fn without_a_hub_there_is_no_voice() {
        let tmp = tempfile::tempdir().unwrap();
        let hub = Hub::new(tmp.path().join("hub.json"));
        assert_eq!(hub.voice_token().await, Err(VoiceError::NoHub));
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
