//! Canal cifrado de extremo a extremo entre nido y sus teléfonos (Noise), igual por la LAN
//! (`/api/v1/e2e`) que a través del hub. Protocolo: `docs/plans/2026-10-23-hub-design.md`.
//!
//! - **Identidad:** el par de claves X25519 de `<config>/hub.key`. Es lo que los teléfonos
//!   emparejados reconocen: si cambia, hay que volver a emparejarlos. Nunca se regenera solo.
//! - **Emparejar:** un QR con la clave pública, las direcciones y un secreto de un solo uso
//!   (`Noise_IKpsk1`). El teléfono queda apuntado con su clave pública y un token propio.
//! - **Conectar:** `Noise_IK`; el teléfono tiene que estar apuntado. Dentro van flujos, cada uno
//!   una conexión TCP al servidor HTTP de nido: la API de siempre, con el token del teléfono.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::{STANDARD as B64, URL_SAFE_NO_PAD as B64URL};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

pub const PAIR: &str = "Noise_IKpsk1_25519_ChaChaPoly_SHA256";
pub const CONNECT: &str = "Noise_IK_25519_ChaChaPoly_SHA256";
pub const PROLOGUE: &[u8] = b"nido-e2e-v1";
/// Los mismos con AES-GCM, para el navegador (WebCrypto no tiene ChaCha20-Poly1305).
pub const PAIR_WEB: &str = "Noise_IKpsk1_25519_AESGCM_SHA256";
pub const CONNECT_WEB: &str = "Noise_IK_25519_AESGCM_SHA256";
pub const MODE_PAIR: u8 = 1;
pub const MODE_CONNECT: u8 = 2;
pub const MODE_PAIR_WEB: u8 = 3;
pub const MODE_CONNECT_WEB: u8 = 4;
/// De nido a nido: unirse al grupo con el código de invitación (IKpsk1) y sincronizar entre
/// miembros (IK).
pub const MODE_GROUP_JOIN: u8 = 5;
pub const MODE_GROUP_SYNC: u8 = 6;

/// Tramas del túnel: `[tipo][flujo u32 BE][datos]`.
pub const FRAME_OPEN: u8 = 1;
pub const FRAME_DATA: u8 = 2;
pub const FRAME_CLOSE: u8 = 3;
/// El mensaje Noise más largo.
pub const MAX_MSG: usize = 65535;
/// Lo que cabe en un mensaje Noise menos la etiqueta (16) y la cabecera de la trama (5).
pub const MAX_CHUNK: usize = MAX_MSG - 16 - 5;

/// Lo que vale un QR de emparejar.
const PAIR_LIFE: Duration = Duration::from_secs(600);
/// Si el teléfono no dice nada, se cierra.
const IDLE: Duration = Duration::from_secs(120);

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn random<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    getrandom::fill(&mut b).expect("aleatorio");
    b
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

// ---------- Identidad ----------

#[derive(Serialize, Deserialize)]
struct KeyFile {
    /// Clave privada X25519 (Noise), base64: la identidad ante los teléfonos.
    x25519: String,
    /// Semilla Ed25519, base64: la identidad ante el hub. Se añade si falta.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    ed25519: Option<String>,
}

pub struct Identity {
    private: [u8; 32],
    pub public: [u8; 32],
    /// Con la que se firma el reto del hub.
    pub signing: ed25519_dalek::SigningKey,
}

fn key32(b64: &str, what: &str, path: &Path) -> Result<[u8; 32], String> {
    B64.decode(b64.trim())
        .ok()
        .and_then(|v| v.try_into().ok())
        .ok_or_else(|| format!("{}: clave {what} no válida", path.display()))
}

impl Identity {
    /// La de `path`, o una nueva si no existe. Si existe pero no se entiende, error: nunca se
    /// sustituye en silencio (los teléfonos emparejados dejarían de funcionar). Si le falta la
    /// clave del hub, se le añade sin tocar la X25519.
    pub fn load_or_create(path: &Path) -> Result<Self, String> {
        let k = match std::fs::read_to_string(path) {
            Ok(raw) => serde_json::from_str::<KeyFile>(&raw)
                .map_err(|e| format!("{}: {e}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => KeyFile {
                x25519: B64.encode(random::<32>()),
                ed25519: None,
            },
            Err(e) => return Err(format!("{}: {e}", path.display())),
        };
        let private = key32(&k.x25519, "X25519", path)?;
        let seed = match &k.ed25519 {
            Some(e) => key32(e, "Ed25519", path)?,
            None => {
                let seed = random::<32>();
                let file = KeyFile {
                    x25519: k.x25519.clone(),
                    ed25519: Some(B64.encode(seed)),
                };
                write_private(path, &serde_json::to_vec_pretty(&file).expect("json"))
                    .map_err(|e| format!("{}: {e}", path.display()))?;
                seed
            }
        };
        Ok(Self::new(private, seed))
    }

    pub fn new(private: [u8; 32], seed: [u8; 32]) -> Self {
        let public = x25519_public(&private);
        Self {
            private,
            public,
            signing: ed25519_dalek::SigningKey::from_bytes(&seed),
        }
    }
}

/// La pública de una privada X25519.
fn x25519_public(private: &[u8; 32]) -> [u8; 32] {
    x25519_dalek::PublicKey::from(&x25519_dalek::StaticSecret::from(*private)).to_bytes()
}

/// Escribe un fichero que solo puede leer el usuario (0600), de una vez.
fn write_private(path: &Path, data: &[u8]) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)?;
    std::io::Write::write_all(&mut f, data)?;
    std::fs::rename(&tmp, path)
}

// ---------- Teléfonos emparejados ----------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Phone {
    pub id: String,
    pub name: String,
    /// Clave pública X25519 del teléfono, base64.
    pub key: String,
    /// Token propio del teléfono para la API (lo tiene solo él).
    pub token: String,
    /// Clave para cifrar sus avisos (ChaCha20-Poly1305), base64; se le da al emparejar.
    #[serde(default)]
    pub push_key: String,
    /// Su handle en el hub para los avisos (secreto entre el teléfono y nido), si lo tiene.
    #[serde(default)]
    pub handle: Option<String>,
    pub added: u64,
}

/// Lo que se puede enseñar de un teléfono (sin claves ni token).
#[derive(Debug, Clone, Serialize)]
pub struct PhoneView {
    pub id: String,
    pub name: String,
    pub added: u64,
    pub push: bool,
}

pub struct Phones {
    path: PathBuf,
    list: Vec<Phone>,
}

impl Phones {
    pub fn load(path: PathBuf) -> Self {
        let list = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        Self { path, list }
    }

    fn save(&self) -> std::io::Result<()> {
        write_private(&self.path, &serde_json::to_vec_pretty(&self.list)?)
    }

    pub fn views(&self) -> Vec<PhoneView> {
        self.list
            .iter()
            .map(|p| PhoneView {
                id: p.id.clone(),
                name: p.name.clone(),
                added: p.added,
                push: p.handle.is_some(),
            })
            .collect()
    }

    pub fn list(&self) -> &[Phone] {
        &self.list
    }

    pub fn by_key(&self, key: &[u8]) -> Option<&Phone> {
        let k = B64.encode(key);
        self.list.iter().find(|p| p.key == k)
    }

    /// ¿Es el token de algún teléfono? (comparación en tiempo constante, como la del principal)
    pub fn token_ok(&self, given: &str) -> bool {
        self.list
            .iter()
            .any(|p| crate::auth::ct_eq(given, &p.token))
    }

    /// Apunta un teléfono (o lo renueva, si ya estaba con esa clave). Devuelve su token y su
    /// clave de avisos.
    pub fn add(
        &mut self,
        key: &[u8],
        name: &str,
        handle: Option<String>,
    ) -> std::io::Result<Phone> {
        let k = B64.encode(key);
        self.list.retain(|p| p.key != k);
        let phone = Phone {
            id: hex(&random::<6>()),
            name: clean_name(name),
            key: k,
            token: hex(&random::<32>()),
            push_key: B64.encode(random::<32>()),
            handle,
            added: unix_now(),
        };
        self.list.push(phone.clone());
        self.save()?;
        Ok(phone)
    }

    /// El teléfono avisa de su handle en el hub al conectar (por si cambió).
    pub fn set_handle(&mut self, id: &str, handle: Option<String>) -> std::io::Result<()> {
        let Some(p) = self.list.iter_mut().find(|p| p.id == id) else {
            return Ok(());
        };
        if p.handle != handle {
            p.handle = handle;
            self.save()?;
        }
        Ok(())
    }

    /// El hub dice que ese teléfono ya no existe para APNs: sin avisos hasta que se vuelva a
    /// apuntar.
    pub fn forget_handle(&mut self, handle: &str) -> std::io::Result<()> {
        let mut changed = false;
        for p in self
            .list
            .iter_mut()
            .filter(|p| p.handle.as_deref() == Some(handle))
        {
            p.handle = None;
            changed = true;
        }
        if changed {
            self.save()?;
        }
        Ok(())
    }

    pub fn remove_by_key(&mut self, key_b64: &str) -> std::io::Result<bool> {
        let before = self.list.len();
        self.list.retain(|p| p.key != key_b64);
        let removed = self.list.len() != before;
        if removed {
            self.save()?;
        }
        Ok(removed)
    }

    pub fn remove(&mut self, id: &str) -> std::io::Result<bool> {
        let before = self.list.len();
        self.list.retain(|p| p.id != id);
        let removed = self.list.len() != before;
        if removed {
            self.save()?;
        }
        Ok(removed)
    }
}

fn clean_name(n: &str) -> String {
    let n: String = n.chars().filter(|c| !c.is_control()).take(60).collect();
    let n = n.trim();
    if n.is_empty() {
        "teléfono".into()
    } else {
        n.into()
    }
}

// ---------- Emparejar ----------

struct Pending {
    psk: [u8; 32],
    expires: Instant,
}

/// El QR de emparejar en vigor (solo uno a la vez).
#[derive(Default)]
pub struct Pairing {
    pending: Mutex<Option<Pending>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PairOffer {
    pub uri: String,
    /// Segundos Unix en que caduca.
    pub expires: u64,
}

impl Pairing {
    /// Un QR nuevo (el anterior deja de valer). `direct`: direcciones de nido para el móvil;
    /// `hub`: la del hub y el id de este nido en él, si hay.
    pub fn offer(&self, id: &Identity, direct: &[String], hub: Option<(&str, &str)>) -> PairOffer {
        let psk = random::<32>();
        *self.pending.lock().expect("emparejar") = Some(Pending {
            psk,
            expires: Instant::now() + PAIR_LIFE,
        });
        let expires = unix_now() + PAIR_LIFE.as_secs();
        let mut uri = format!(
            "nido://pair?k={}&s={}&x={expires}",
            B64URL.encode(id.public),
            B64URL.encode(psk)
        );
        if !direct.is_empty() {
            uri.push_str(&format!("&u={}", B64URL.encode(direct.join(","))));
        }
        if let Some((h, n)) = hub {
            uri.push_str(&format!("&h={}&n={n}", B64URL.encode(h)));
        }
        PairOffer { uri, expires }
    }

    fn current(&self) -> Option<[u8; 32]> {
        let p = self.pending.lock().expect("emparejar");
        p.as_ref()
            .filter(|p| Instant::now() < p.expires)
            .map(|p| p.psk)
    }

    /// Gasta el secreto, si sigue siendo el mismo (solo tras un primer mensaje válido).
    fn consume(&self, psk: &[u8; 32]) -> bool {
        let mut p = self.pending.lock().expect("emparejar");
        if p.as_ref().is_some_and(|x| &x.psk == psk) {
            *p = None;
            true
        } else {
            false
        }
    }
}

// ---------- El canal ----------

/// Lo que el canal necesita del gestor.
pub trait Host: Send + Sync + 'static {
    fn identity(&self) -> &Identity;
    fn pairing(&self) -> &Pairing;
    fn phones(&self) -> &Mutex<Phones>;
    /// Dónde abrir los flujos (el servidor HTTP de nido en loopback).
    fn local_addr(&self) -> Option<SocketAddr>;
    /// Nombre del equipo, para que el teléfono lo enseñe.
    fn host_name(&self) -> String;
    /// El hub y el id de este nido en él, si está conectado (el teléfono lo apunta al conectar,
    /// por si se emparejó antes de haber hub).
    fn hub_info(&self) -> Option<(String, String)> {
        None
    }
    /// El grupo de equipos, si lo hay.
    fn group(&self) -> Option<&crate::group::Group> {
        None
    }
    /// El grupo ha cambiado aquí: hay que contárselo a los demás equipos.
    fn group_changed(&self) {}
    /// Lo mismo, pero esperando a que se lo hayan contado (al emparejar: el dispositivo recibe la
    /// lista de equipos y puede ir a cualquiera en el momento).
    fn sync_group_now(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
        Box::pin(async {})
    }
}

#[derive(Deserialize)]
struct PairHello {
    #[serde(default)]
    name: String,
    #[serde(default)]
    push: Option<String>,
}

#[derive(Deserialize, Default)]
struct ConnectHello {
    #[serde(default)]
    push: Option<String>,
}

fn builder(pattern: &'static str) -> snow::Builder<'static> {
    snow::Builder::new(pattern.parse().expect("patrón"))
}

/// Atiende una conexión de un teléfono: `rx` trae sus mensajes (WebSocket del propio nido o
/// trama Datos del hub) y `tx` le manda los nuestros. Termina al cerrarse cualquiera de los dos.
pub async fn serve<H: Host>(
    host: std::sync::Arc<H>,
    mut rx: mpsc::Receiver<Vec<u8>>,
    tx: mpsc::Sender<Vec<u8>>,
) {
    let Ok(Some(first)) = tokio::time::timeout(IDLE, rx.recv()).await else {
        return;
    };
    let Some((&mode, msg1)) = first.split_first() else {
        return;
    };
    match mode {
        MODE_PAIR | MODE_PAIR_WEB => {
            let pattern = if mode == MODE_PAIR { PAIR } else { PAIR_WEB };
            pair(&*host, pattern, msg1, &tx).await
        }
        MODE_GROUP_JOIN => group_join(&*host, msg1, &tx).await,
        MODE_GROUP_SYNC => group_sync(&*host, msg1, &tx).await,
        MODE_CONNECT | MODE_CONNECT_WEB => {
            let pattern = if mode == MODE_CONNECT {
                CONNECT
            } else {
                CONNECT_WEB
            };
            if let Some((transport, phone)) = connect(&*host, pattern, msg1, &tx).await {
                tunnel(host, transport, phone, rx, tx).await;
            }
        }
        _ => {}
    }
}

async fn pair<H: Host>(host: &H, pattern: &'static str, msg1: &[u8], tx: &mpsc::Sender<Vec<u8>>) {
    let Some(psk) = host.pairing().current() else {
        return;
    };
    let Ok(mut hs) = builder(pattern)
        .local_private_key(&host.identity().private)
        .and_then(|b| b.psk(1, &psk))
        .and_then(|b| b.prologue(PROLOGUE))
        .and_then(|b| b.build_responder())
    else {
        return;
    };
    let mut payload = vec![0u8; MAX_MSG];
    // Con psk1 el primer mensaje ya depende del secreto: si no descifra, no era del QR.
    let Ok(n) = hs.read_message(msg1, &mut payload) else {
        return;
    };
    if !host.pairing().consume(&psk) {
        return;
    }
    let hello: PairHello = serde_json::from_slice(&payload[..n]).unwrap_or(PairHello {
        name: String::new(),
        push: None,
    });
    let Some(key) = hs.get_remote_static().map(<[u8]>::to_vec) else {
        return;
    };
    let phone = match host.phones().lock().expect("teléfonos").add(
        &key,
        &hello.name,
        hello.push.filter(|h| valid_handle(h)),
    ) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("nido: no se pudo apuntar el teléfono: {e}");
            return;
        }
    };
    eprintln!("nido: teléfono emparejado: {}", phone.name);
    if let Some(g) = host.group() {
        g.add_device(&key, &phone.name);
        let _ = tokio::time::timeout(Duration::from_secs(10), host.sync_group_now()).await;
    }
    let mut reply = serde_json::json!({
        "token": phone.token,
        "push_key": phone.push_key,
        "name": host.host_name(),
    });
    add_group_info(host, &mut reply);
    let mut msg2 = vec![0u8; MAX_MSG];
    if let Ok(n) = hs.write_message(reply.to_string().as_bytes(), &mut msg2) {
        msg2.truncate(n);
        let _ = tx.send(msg2).await;
    }
}

/// El hub, el id de este nido y los equipos del grupo, para el dispositivo.
fn add_group_info<H: Host>(host: &H, reply: &mut serde_json::Value) {
    if let Some((hub, id)) = host.hub_info() {
        reply["hub"] = hub.into();
        reply["nido"] = id.into();
    }
    if let Some(g) = host.group() {
        reply["group"] = serde_json::to_value(g.members()).unwrap_or_default();
    }
}

/// Otro equipo se une al grupo con el código de `nido group invite`.
async fn group_join<H: Host>(host: &H, msg1: &[u8], tx: &mpsc::Sender<Vec<u8>>) {
    let Some(g) = host.group() else { return };
    let Some(secret) = g.current_invite() else {
        return;
    };
    let Ok(mut hs) = builder(PAIR)
        .local_private_key(&host.identity().private)
        .and_then(|b| b.psk(1, &secret))
        .and_then(|b| b.prologue(PROLOGUE))
        .and_then(|b| b.build_responder())
    else {
        return;
    };
    let mut payload = vec![0u8; MAX_MSG];
    let Ok(n) = hs.read_message(msg1, &mut payload) else {
        return;
    };
    if !g.consume_invite(&secret) {
        return;
    }
    #[derive(Deserialize)]
    struct Join {
        id: String,
        name: String,
        state: crate::group::State,
    }
    let Ok(join) = serde_json::from_slice::<Join>(&payload[..n]) else {
        return;
    };
    let Some(key) = hs.get_remote_static().map(|k| B64.encode(k)) else {
        return;
    };
    g.merge(&join.state);
    g.add_member(&join.id, &key, &join.name);
    eprintln!("nido: {} se ha unido al grupo", join.name);
    let reply = serde_json::to_vec(&g.snapshot()).unwrap_or_default();
    let mut msg2 = vec![0u8; MAX_MSG];
    if let Ok(n) = hs.write_message(&reply, &mut msg2) {
        msg2.truncate(n);
        let _ = tx.send(msg2).await;
    }
    host.group_changed();
}

/// Otro equipo del grupo nos cuenta su estado; le devolvemos el junto.
async fn group_sync<H: Host>(host: &H, msg1: &[u8], tx: &mpsc::Sender<Vec<u8>>) {
    let Some(g) = host.group() else { return };
    let Ok(mut hs) = builder(CONNECT)
        .local_private_key(&host.identity().private)
        .and_then(|b| b.prologue(PROLOGUE))
        .and_then(|b| b.build_responder())
    else {
        return;
    };
    let mut payload = vec![0u8; MAX_MSG];
    let Ok(n) = hs.read_message(msg1, &mut payload) else {
        return;
    };
    // Solo un miembro del grupo (o uno que acabamos de sacar, para que se entere).
    let Some(key) = hs.get_remote_static().map(|k| B64.encode(k)) else {
        return;
    };
    if !g.snapshot().members.iter().any(|m| m.key == key) {
        return;
    }
    let Ok(theirs) = serde_json::from_slice::<crate::group::State>(&payload[..n]) else {
        return;
    };
    let changed = g.merge(&theirs);
    let reply = serde_json::to_vec(&g.snapshot()).unwrap_or_default();
    let mut msg2 = vec![0u8; MAX_MSG];
    if let Ok(n) = hs.write_message(&reply, &mut msg2) {
        msg2.truncate(n);
        let _ = tx.send(msg2).await;
    }
    if changed {
        host.group_changed();
    }
}

/// Lado del que inicia (de nido a otro nido): manda el primer mensaje y lee la respuesta. El
/// transporte lo pone quien llama (el hub).
pub struct Dial {
    hs: snow::HandshakeState,
}

impl Dial {
    /// `psk`: el secreto del código, al unirse; sin él, sincronizar entre miembros.
    pub fn start(
        id: &Identity,
        remote: &[u8; 32],
        psk: Option<&[u8; 32]>,
        payload: &[u8],
    ) -> Result<(Self, Vec<u8>), String> {
        let mut b = builder(if psk.is_some() { PAIR } else { CONNECT })
            .local_private_key(&id.private)
            .and_then(|b| b.remote_public_key(remote))
            .and_then(|b| b.prologue(PROLOGUE))
            .map_err(|e| e.to_string())?;
        if let Some(p) = psk {
            b = b.psk(1, p).map_err(|e| e.to_string())?;
        }
        let mut hs = b.build_initiator().map_err(|e| e.to_string())?;
        let mut msg = vec![0u8; MAX_MSG];
        let n = hs
            .write_message(payload, &mut msg)
            .map_err(|e| e.to_string())?;
        let mode = if psk.is_some() {
            MODE_GROUP_JOIN
        } else {
            MODE_GROUP_SYNC
        };
        let mut first = vec![mode];
        first.extend_from_slice(&msg[..n]);
        Ok((Self { hs }, first))
    }

    pub fn finish(mut self, msg2: &[u8]) -> Result<Vec<u8>, String> {
        let mut payload = vec![0u8; MAX_MSG];
        let n = self
            .hs
            .read_message(msg2, &mut payload)
            .map_err(|_| "el otro equipo no aceptó (¿código usado o caducado?)".to_owned())?;
        payload.truncate(n);
        Ok(payload)
    }
}

fn valid_handle(h: &str) -> bool {
    (16..=64).contains(&h.len())
        && h.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

async fn connect<H: Host>(
    host: &H,
    pattern: &'static str,
    msg1: &[u8],
    tx: &mpsc::Sender<Vec<u8>>,
) -> Option<(snow::TransportState, String)> {
    let mut hs = builder(pattern)
        .local_private_key(&host.identity().private)
        .and_then(|b| b.prologue(PROLOGUE))
        .and_then(|b| b.build_responder())
        .ok()?;
    let mut payload = vec![0u8; MAX_MSG];
    let n = hs.read_message(msg1, &mut payload).ok()?;
    let key = hs.get_remote_static()?.to_vec();
    let hello: ConnectHello = serde_json::from_slice(&payload[..n]).unwrap_or_default();
    let known = host
        .phones()
        .lock()
        .expect("teléfonos")
        .by_key(&key)
        .map(|p| p.id.clone());
    // Un dispositivo emparejado con otro equipo del grupo: se apunta aquí con su propio token.
    let phone_id = match known {
        Some(id) => id,
        None => {
            let name = host.group().and_then(|g| g.device_name(&key))?;
            let p = host
                .phones()
                .lock()
                .expect("teléfonos")
                .add(&key, &name, None)
                .ok()?;
            eprintln!("nido: {} entra por el grupo", p.name);
            p.id
        }
    };
    if let Err(e) = host
        .phones()
        .lock()
        .expect("teléfonos")
        .set_handle(&phone_id, hello.push.filter(|h| valid_handle(h)))
    {
        eprintln!("nido: no se pudo guardar el handle del teléfono: {e}");
    }
    // Su token y su clave de avisos de este equipo, y cómo llegar al resto del grupo.
    let (token, push_key) = host
        .phones()
        .lock()
        .expect("teléfonos")
        .list()
        .iter()
        .find(|p| p.id == phone_id)
        .map(|p| (p.token.clone(), p.push_key.clone()))?;
    let mut reply =
        serde_json::json!({ "name": host.host_name(), "token": token, "push_key": push_key });
    add_group_info(host, &mut reply);
    let mut msg2 = vec![0u8; MAX_MSG];
    let n = hs
        .write_message(reply.to_string().as_bytes(), &mut msg2)
        .ok()?;
    msg2.truncate(n);
    tx.send(msg2).await.ok()?;
    Some((hs.into_transport_mode().ok()?, phone_id))
}

/// `[tipo][flujo][datos]`
pub fn frame(kind: u8, flow: u32, data: &[u8]) -> Vec<u8> {
    let mut f = Vec::with_capacity(5 + data.len());
    f.push(kind);
    f.extend_from_slice(&flow.to_be_bytes());
    f.extend_from_slice(data);
    f
}

pub fn parse_frame(f: &[u8]) -> Option<(u8, u32, &[u8])> {
    if f.len() < 5 {
        return None;
    }
    let flow = u32::from_be_bytes(f[1..5].try_into().ok()?);
    Some((f[0], flow, &f[5..]))
}

/// El túnel: cada flujo es una conexión TCP al servidor HTTP de nido. Una sola tarea tiene el
/// estado de Noise (cifrar y descifrar van en orden); los flujos le pasan lo que leen.
async fn tunnel<H: Host>(
    host: std::sync::Arc<H>,
    mut noise: snow::TransportState,
    phone_id: String,
    mut rx: mpsc::Receiver<Vec<u8>>,
    tx: mpsc::Sender<Vec<u8>>,
) {
    let (out_tx, mut out_rx) = mpsc::channel::<Vec<u8>>(64);
    let mut flows: HashMap<u32, mpsc::Sender<Vec<u8>>> = HashMap::new();
    let mut plain = vec![0u8; MAX_MSG];
    let mut cipher = vec![0u8; MAX_MSG];
    loop {
        tokio::select! {
            msg = tokio::time::timeout(IDLE, rx.recv()) => {
                let Ok(Some(msg)) = msg else { break };
                // Quitado mientras estaba conectado: fuera.
                if !host.phones().lock().expect("teléfonos").list().iter().any(|p| p.id == phone_id) {
                    break;
                }
                let Ok(n) = noise.read_message(&msg, &mut plain) else { break };
                let Some((kind, flow, data)) = parse_frame(&plain[..n]) else { break };
                match kind {
                    FRAME_OPEN => {
                        let Some(addr) = host.local_addr() else {
                            let _ = out_tx.send(frame(FRAME_CLOSE, flow, &[])).await;
                            continue;
                        };
                        let (w_tx, w_rx) = mpsc::channel::<Vec<u8>>(32);
                        flows.insert(flow, w_tx);
                        tokio::spawn(run_flow(addr, flow, w_rx, out_tx.clone()));
                    }
                    FRAME_DATA => {
                        if let Some(w) = flows.get(&flow)
                            && w.send(data.to_vec()).await.is_err()
                        {
                            flows.remove(&flow);
                        }
                    }
                    FRAME_CLOSE => {
                        flows.remove(&flow);
                    }
                    _ => break,
                }
            }
            Some(f) = out_rx.recv() => {
                if let Some((FRAME_CLOSE, flow, _)) = parse_frame(&f) {
                    flows.remove(&flow);
                }
                let Ok(n) = noise.write_message(&f, &mut cipher) else { break };
                if tx.send(cipher[..n].to_vec()).await.is_err() {
                    break;
                }
            }
        }
    }
}

/// Un flujo: conecta con nido y pasa los datos en los dos sentidos.
async fn run_flow(
    addr: SocketAddr,
    flow: u32,
    mut from_phone: mpsc::Receiver<Vec<u8>>,
    out: mpsc::Sender<Vec<u8>>,
) {
    let Ok(stream) = tokio::net::TcpStream::connect(addr).await else {
        let _ = out.send(frame(FRAME_CLOSE, flow, &[])).await;
        return;
    };
    let _ = stream.set_nodelay(true);
    let (mut rd, mut wr) = stream.into_split();
    let out2 = out.clone();
    let reader = tokio::spawn(async move {
        let mut buf = vec![0u8; MAX_CHUNK];
        loop {
            match rd.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if out2.send(frame(FRAME_DATA, flow, &buf[..n])).await.is_err() {
                        return;
                    }
                }
            }
        }
        let _ = out2.send(frame(FRAME_CLOSE, flow, &[])).await;
    });
    while let Some(data) = from_phone.recv().await {
        if wr.write_all(&data).await.is_err() {
            break;
        }
    }
    // El teléfono cerró su lado: se cierra el nuestro y se espera a que nido termine.
    let _ = wr.shutdown().await;
    let _ = reader.await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    struct TestHost {
        id: Identity,
        pairing: Pairing,
        phones: Mutex<Phones>,
        addr: Option<SocketAddr>,
    }

    impl Host for TestHost {
        fn identity(&self) -> &Identity {
            &self.id
        }
        fn pairing(&self) -> &Pairing {
            &self.pairing
        }
        fn phones(&self) -> &Mutex<Phones> {
            &self.phones
        }
        fn local_addr(&self) -> Option<SocketAddr> {
            self.addr
        }
        fn host_name(&self) -> String {
            "equipo".into()
        }
    }

    fn test_host(dir: &Path, addr: Option<SocketAddr>) -> Arc<TestHost> {
        Arc::new(TestHost {
            id: Identity::load_or_create(&dir.join("hub.key")).unwrap(),
            pairing: Pairing::default(),
            phones: Mutex::new(Phones::load(dir.join("phones.json"))),
            addr,
        })
    }

    /// Un teléfono de pruebas: su clave y un canal con nido.
    struct Phone {
        kp: snow::Keypair,
    }

    impl Phone {
        fn new() -> Self {
            Self {
                kp: builder(CONNECT).generate_keypair().unwrap(),
            }
        }

        fn start<H: Host>(host: &Arc<H>) -> (mpsc::Sender<Vec<u8>>, mpsc::Receiver<Vec<u8>>) {
            let (to_nido, nido_rx) = mpsc::channel(64);
            let (nido_tx, from_nido) = mpsc::channel(64);
            tokio::spawn(serve(host.clone(), nido_rx, nido_tx));
            (to_nido, from_nido)
        }

        async fn pair<H: Host>(
            &self,
            host: &Arc<H>,
            server_key: &[u8],
            psk: &[u8; 32],
        ) -> Option<serde_json::Value> {
            let (tx, mut rx) = Self::start(host);
            let mut hs = builder(PAIR)
                .local_private_key(&self.kp.private)
                .unwrap()
                .remote_public_key(server_key)
                .unwrap()
                .psk(1, psk)
                .unwrap()
                .prologue(PROLOGUE)
                .unwrap()
                .build_initiator()
                .unwrap();
            let mut m = vec![0u8; 65535];
            let n = hs
                .write_message(
                    br#"{"name":"iPhone de prueba","push":"hhhhhhhhhhhhhhhhhhhhhh"}"#,
                    &mut m,
                )
                .unwrap();
            let mut first = vec![MODE_PAIR];
            first.extend_from_slice(&m[..n]);
            tx.send(first).await.unwrap();
            let reply = tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .ok()??;
            let mut p = vec![0u8; 65535];
            let n = hs.read_message(&reply, &mut p).ok()?;
            serde_json::from_slice(&p[..n]).ok()
        }

        async fn connect<H: Host>(
            &self,
            host: &Arc<H>,
            server_key: &[u8],
        ) -> Option<(
            snow::TransportState,
            mpsc::Sender<Vec<u8>>,
            mpsc::Receiver<Vec<u8>>,
        )> {
            let (tx, mut rx) = Self::start(host);
            let mut hs = builder(CONNECT)
                .local_private_key(&self.kp.private)
                .unwrap()
                .remote_public_key(server_key)
                .unwrap()
                .prologue(PROLOGUE)
                .unwrap()
                .build_initiator()
                .unwrap();
            let mut m = vec![0u8; 65535];
            let n = hs.write_message(b"{}", &mut m).unwrap();
            let mut first = vec![MODE_CONNECT];
            first.extend_from_slice(&m[..n]);
            tx.send(first).await.unwrap();
            let reply = tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .ok()??;
            hs.read_message(&reply, &mut vec![0u8; 65535]).ok()?;
            Some((hs.into_transport_mode().unwrap(), tx, rx))
        }
    }

    fn psk_of(uri: &str) -> [u8; 32] {
        let s = uri
            .split(['?', '&'])
            .find_map(|kv| kv.strip_prefix("s="))
            .unwrap();
        B64URL.decode(s).unwrap().try_into().unwrap()
    }

    #[test]
    fn identity_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let a = Identity::load_or_create(&dir.path().join("hub.key")).unwrap();
        let b = Identity::load_or_create(&dir.path().join("hub.key")).unwrap();
        assert_eq!(a.public, b.public);
        std::fs::write(dir.path().join("hub.key"), "basura").unwrap();
        assert!(
            Identity::load_or_create(&dir.path().join("hub.key")).is_err(),
            "nunca se sustituye"
        );
    }

    #[tokio::test]
    async fn pair_once_then_connect_and_tunnel() {
        let dir = tempfile::tempdir().unwrap();
        // Un «servidor HTTP» de eco para los flujos.
        let echo = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = echo.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = echo.accept().await {
                tokio::spawn(async move {
                    let (mut r, mut w) = s.split();
                    let _ = tokio::io::copy(&mut r, &mut w).await;
                });
            }
        });
        let host = test_host(dir.path(), Some(addr));
        let server_key = host.id.public;
        let offer = host
            .pairing
            .offer(&host.id, &["http://10.0.0.1:7690".into()], None);
        assert!(offer.uri.starts_with("nido://pair?k="));
        let psk = psk_of(&offer.uri);

        // Con un secreto equivocado no empareja ni gasta el bueno.
        let phone = Phone::new();
        assert!(phone.pair(&host, &server_key, &[7u8; 32]).await.is_none());
        let reply = phone
            .pair(&host, &server_key, &psk)
            .await
            .expect("empareja");
        let token = reply["token"].as_str().unwrap().to_owned();
        assert_eq!(reply["name"], "equipo");
        assert_eq!(
            B64.decode(reply["push_key"].as_str().unwrap())
                .unwrap()
                .len(),
            32
        );
        assert!(host.phones.lock().unwrap().token_ok(&token));
        // De un solo uso.
        assert!(Phone::new().pair(&host, &server_key, &psk).await.is_none());

        // Un teléfono no emparejado no pasa.
        assert!(Phone::new().connect(&host, &server_key).await.is_none());

        // El emparejado abre un flujo y lo que manda vuelve (eco).
        let (mut noise, tx, mut rx) = phone.connect(&host, &server_key).await.expect("conecta");
        let mut buf = vec![0u8; 65535];
        for f in [
            frame(FRAME_OPEN, 7, &[]),
            frame(FRAME_DATA, 7, b"hola nido"),
        ] {
            let n = noise.write_message(&f, &mut buf).unwrap();
            tx.send(buf[..n].to_vec()).await.unwrap();
        }
        let msg = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .unwrap()
            .unwrap();
        let mut plain = vec![0u8; 65535];
        let n = noise.read_message(&msg, &mut plain).unwrap();
        assert_eq!(
            parse_frame(&plain[..n]),
            Some((FRAME_DATA, 7, &b"hola nido"[..]))
        );

        // Quitado el teléfono, el canal se corta en el siguiente mensaje.
        let id = host.phones.lock().unwrap().list()[0].id.clone();
        assert!(host.phones.lock().unwrap().remove(&id).unwrap());
        let n = noise
            .write_message(&frame(FRAME_DATA, 7, b"x"), &mut buf)
            .unwrap();
        tx.send(buf[..n].to_vec()).await.unwrap();
        let end = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .unwrap();
        assert!(end.is_none(), "cerrado");
    }

    /// Vectores para comprobar el Noise de la app (Dart) contra `snow`: handshakes completos con
    /// claves fijas. Se guardan en `tests/noise_vectors.json` (la app lleva una copia); con
    /// `NIDO_WRITE_VECTORS=1` se regeneran.
    #[test]
    fn noise_vectors() {
        fn run(pattern: &'static str, psk: Option<&[u8; 32]>) -> serde_json::Value {
            let key = |b: u8| [b; 32];
            let (is, rs, ie, re) = (key(1), key(2), key(3), key(4));
            let rs_pub = x25519_public(&rs);
            let mut ib = builder(pattern)
                .local_private_key(&is)
                .unwrap()
                .remote_public_key(&rs_pub)
                .unwrap()
                .fixed_ephemeral_key_for_testing_only(&ie)
                .prologue(PROLOGUE)
                .unwrap();
            let mut rb = builder(pattern)
                .local_private_key(&rs)
                .unwrap()
                .fixed_ephemeral_key_for_testing_only(&re)
                .prologue(PROLOGUE)
                .unwrap();
            if let Some(p) = psk {
                ib = ib.psk(1, p).unwrap();
                rb = rb.psk(1, p).unwrap();
            }
            let (mut i, mut r) = (ib.build_initiator().unwrap(), rb.build_responder().unwrap());
            let mut m = vec![0u8; 65535];
            let mut p = vec![0u8; 65535];
            let (p1, p2) = (br#"{"name":"iPhone"}"#, br#"{"token":"t"}"#);
            let n = i.write_message(p1, &mut m).unwrap();
            let msg1 = m[..n].to_vec();
            r.read_message(&msg1, &mut p).unwrap();
            let n = r.write_message(p2, &mut m).unwrap();
            let msg2 = m[..n].to_vec();
            i.read_message(&msg2, &mut p).unwrap();
            let (mut i, mut r) = (
                i.into_transport_mode().unwrap(),
                r.into_transport_mode().unwrap(),
            );
            let n = i.write_message(b"hola nido", &mut m).unwrap();
            let t1 = m[..n].to_vec();
            r.read_message(&t1, &mut p).unwrap();
            let n = i.write_message(b"otra vez", &mut m).unwrap();
            let t2 = m[..n].to_vec();
            let n = r.write_message("adiós".as_bytes(), &mut m).unwrap();
            let t3 = m[..n].to_vec();
            serde_json::json!({
                "pattern": pattern,
                "prologue": hex(PROLOGUE),
                "init_static": hex(&is), "resp_static": hex(&rs), "resp_static_pub": hex(&rs_pub),
                "init_static_pub": hex(&x25519_public(&is)),
                "init_ephemeral": hex(&ie), "resp_ephemeral": hex(&re),
                "psk": psk.map(|p| hex(p)),
                "payload1": hex(p1), "msg1": hex(&msg1),
                "payload2": hex(p2), "msg2": hex(&msg2),
                "transport": [
                    {"from": "init", "plain": hex(b"hola nido"), "cipher": hex(&t1)},
                    {"from": "init", "plain": hex(b"otra vez"), "cipher": hex(&t2)},
                    {"from": "resp", "plain": hex("adiós".as_bytes()), "cipher": hex(&t3)},
                ],
            })
        }
        let v = serde_json::json!([
            run(CONNECT, None),
            run(PAIR, Some(&[9u8; 32])),
            run(CONNECT_WEB, None),
            run(PAIR_WEB, Some(&[9u8; 32])),
        ]);
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/noise_vectors.json");
        let text = serde_json::to_string_pretty(&v).unwrap() + "\n";
        if std::env::var_os("NIDO_WRITE_VECTORS").is_some() {
            std::fs::write(&path, &text).unwrap();
        }
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            text,
            "vectores de {}",
            path.display()
        );
    }

    #[test]
    fn frames_and_names() {
        assert_eq!(
            parse_frame(&frame(2, 0x01020304, b"ab")),
            Some((2, 0x01020304, &b"ab"[..]))
        );
        assert_eq!(parse_frame(b"\x01\x00"), None);
        assert_eq!(clean_name("  \u{7}  "), "teléfono");
        assert!(valid_handle("abcdefghijklmnopqrstuv"));
        assert!(!valid_handle("../etc"));
    }
}
