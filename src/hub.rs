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

type VoicePending = Mutex<HashMap<u64, oneshot::Sender<serde_json::Value>>>;

/// El cliente: se reconecta solo y atiende los móviles que llegan por el hub.
pub struct Hub {
    pub status: watch::Sender<HubStatus>,
    config_path: PathBuf,
    config: watch::Sender<HubConfig>,
    /// Lo que va hacia el hub (mientras haya conexión).
    out: Mutex<Option<mpsc::Sender<Message>>>,
    pending: Pending,
    /// Los pedidos de token de voz que esperan respuesta (lo que vino en `voiced`).
    voice_pending: VoicePending,
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

    /// Pide al hub un token de voz para que la web o la app hablen con el modelo directamente.
    /// Devuelve lo que da el hub (`token`, `expires`, `project`, `location`, `host`, `model`).
    pub async fn voice_token(&self) -> Result<serde_json::Value, VoiceError> {
        let out = self
            .out
            .lock()
            .expect("hub")
            .clone()
            .ok_or(VoiceError::NoHub)?;
        let req = self.next_req.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.voice_pending.lock().expect("voz").insert(req, tx);
        let msg = serde_json::json!({ "voice": { "req": req } });
        if out
            .send(Message::Text(msg.to_string().into()))
            .await
            .is_err()
        {
            self.voice_pending.lock().expect("voz").remove(&req);
            return Err(VoiceError::NoHub);
        }
        let mut v = match tokio::time::timeout(VOICE_WAIT, rx).await {
            Ok(Ok(v)) => v,
            // Se cerró la conexión con el pedido en marcha (ver `disconnected`).
            Ok(Err(_)) => return Err(VoiceError::NoHub),
            Err(_) => {
                self.voice_pending.lock().expect("voz").remove(&req);
                return Err(VoiceError::NotOffered);
            }
        };
        if v["ok"] != true {
            return Err(VoiceError::from_hub(
                v["error"].as_str().unwrap_or_default(),
            ));
        }
        if let Some(o) = v.as_object_mut() {
            o.remove("req");
            o.remove("ok");
        }
        Ok(v)
    }

    /// Si es una respuesta `{"voiced":{"req":N,…}}`, se la entrega a quien la espera.
    fn voiced(&self, t: &str) {
        let Ok(mut v) = serde_json::from_str::<serde_json::Value>(t) else {
            return;
        };
        if let Some(req) = v["voiced"]["req"].as_u64()
            && let Some(waiter) = self.voice_pending.lock().expect("voz").remove(&req)
        {
            let _ = waiter.send(v["voiced"].take());
        }
    }

    /// Se acabó la conexión: ya no se manda nada y los que esperan respuesta dejan de esperar
    /// (sus `oneshot` se cierran), en vez de esperar a que venza el plazo. Lo que se mande con
    /// un `out` de antes ya no llega a nadie: falla al mandarlo.
    fn disconnected(&self) {
        *self.out.lock().expect("hub") = None;
        self.pending.lock().expect("avisos").clear();
        self.voice_pending.lock().expect("voz").clear();
    }

    fn set_status(&self, f: impl FnOnce(&mut HubStatus)) {
        self.status.send_modify(f);
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
                *s = HubStatus {
                    url: url.clone(),
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
    *hub.out.lock().expect("hub") = Some(out_tx.clone());
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
                    } else {
                        hub.voiced(&t);
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
                "req": req, "ok": true, "token": "ya29.prueba", "expires": 1_700_000_000,
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
                "token": "ya29.prueba", "expires": 1_700_000_000, "project": "proyecto",
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
