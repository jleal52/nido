//! El tablero de la cuenta: proyectos, objetivos y enlaces de sesiones con objetivos. El hub
//! guarda cada objeto como un blob cifrado que no puede leer; lo cifran y lo descifran nido, la
//! web y la app con las claves del llavero del grupo (`group::BoardKey`).
//!
//! ```text
//! blob = base64( 0x01 || nonce(12) || AES-256-GCM(clave, nonce, texto, aad) )
//! aad  = "nido-board/1/" || id
//! kid  = hex( SHA-256("nido-board-kid" || clave) )[0..16]
//! ```
//!
//! La salida de AES-256-GCM lleva la etiqueta de 16 bytes al final, como la dan WebCrypto y
//! `aes-gcm`. El id va en `aad` para que un blob no se pueda pasar a otro objeto; la cuenta no,
//! porque al juntar dos cuentas los objetos pasan a la que queda y se tienen que seguir leyendo.
//! `tests/data/board_vectors.json` fija todo esto para la web y la app.
//!
//! Abajo, `BoardCache`: la copia de este nido del tablero del hub (`data/board.json`), que se
//! sincroniza por la conexión con el hub (`hub::Hub::board_get`…).

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::watch;

use crate::group::Keyring;
use crate::hub::{BoardError, BoardItem, Hub};

/// El id de una clave del tablero: `hex(SHA-256("nido-board-kid" || clave))[0..16]`.
pub fn kid(key: &[u8; 32]) -> String {
    crate::group::board_kid(key)
}

/// La versión del formato del blob: su primer byte.
pub const VERSION: u8 = 1;
/// Lo más que puede ocupar el texto de un objeto (su JSON en UTF-8). Cifrado y en base64 son
/// 10 964 caracteres, por debajo de los 11 000 que acepta el hub.
pub const MAX_PLAIN: usize = 8192;
/// Lo más largo que acepta el hub en un blob (caracteres): uno más largo ni se decodifica.
pub const MAX_BLOB: usize = 11_000;
const NONCE_LEN: usize = 12;
const TAG_LEN: usize = 16;
const AAD_PREFIX: &[u8] = b"nido-board/1/";

#[derive(Debug)]
pub enum BlobError {
    /// El texto pasa de `MAX_PLAIN` bytes (lleva los que tiene).
    TooBig(usize),
    /// El id no son 32 caracteres hex en minúsculas.
    BadId,
    /// No se pudo sacar el nonce al azar.
    Random(std::io::Error),
    /// No es base64 estándar o es demasiado corto para llevar nonce y etiqueta.
    BadBlob,
    /// Un formato que no conocemos (lleva el primer byte).
    Version(u8),
    /// No se descifra: otra clave, otro id o un blob tocado.
    Open,
    /// El objeto no se puede escribir (un `Unknown`, un `order` no finito) o lo descifrado no es
    /// un objeto.
    BadObject(String),
}

impl std::fmt::Display for BlobError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BlobError::TooBig(n) => {
                write!(
                    f,
                    "objeto demasiado grande: {n} bytes (como mucho {MAX_PLAIN})"
                )
            }
            BlobError::BadId => write!(f, "id de objeto no válido (32 hex en minúsculas)"),
            BlobError::Random(e) => write!(f, "sin números al azar: {e}"),
            BlobError::BadBlob => write!(f, "blob mal formado"),
            BlobError::Version(v) => write!(f, "blob de una versión que no se conoce ({v})"),
            BlobError::Open => write!(f, "el blob no se descifra con esta clave y este id"),
            BlobError::BadObject(e) => write!(f, "objeto del tablero no válido: {e}"),
        }
    }
}

impl std::error::Error for BlobError {}

/// ¿Es un id de objeto? 32 caracteres hex en minúsculas: van tal cual en `aad`, así que
/// `"AB…"` y `"ab…"` serían objetos distintos y uno no descifraría lo del otro.
pub fn is_id(id: &str) -> bool {
    id.len() == 32 && id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Un id nuevo para un objeto: 16 bytes al azar en hex. Lo elige el cliente, así sabe el `aad`
/// antes de escribir.
pub fn new_id() -> std::io::Result<String> {
    let mut b = [0u8; 16];
    getrandom::fill(&mut b).map_err(std::io::Error::other)?;
    Ok(hex(&b))
}

/// Cifra el texto de un objeto (su JSON) con un nonce nuevo al azar.
pub fn seal(key: &[u8; 32], id: &str, plaintext: &[u8]) -> Result<String, BlobError> {
    let mut nonce = [0u8; NONCE_LEN];
    getrandom::fill(&mut nonce).map_err(|e| BlobError::Random(std::io::Error::other(e)))?;
    seal_with_nonce(key, id, &nonce, plaintext)
}

/// `seal` con el nonce dado: solo para `seal` y para los vectores. Repetir un nonce con la misma
/// clave rompe AES-GCM.
fn seal_with_nonce(
    key: &[u8; 32],
    id: &str,
    nonce: &[u8; NONCE_LEN],
    plaintext: &[u8],
) -> Result<String, BlobError> {
    if plaintext.len() > MAX_PLAIN {
        return Err(BlobError::TooBig(plaintext.len()));
    }
    if !is_id(id) {
        return Err(BlobError::BadId);
    }
    let ct = Aes256Gcm::new(key.into())
        .encrypt(
            Nonce::from_slice(nonce),
            Payload {
                msg: plaintext,
                aad: &aad(id),
            },
        )
        .expect("AES-GCM cifra cualquier texto de hasta MAX_PLAIN bytes");
    let mut out = Vec::with_capacity(1 + NONCE_LEN + ct.len());
    out.push(VERSION);
    out.extend_from_slice(nonce);
    out.extend_from_slice(&ct);
    Ok(B64.encode(out))
}

/// Descifra el blob del objeto `id`.
pub fn open(key: &[u8; 32], id: &str, blob: &str) -> Result<Vec<u8>, BlobError> {
    if !is_id(id) {
        return Err(BlobError::BadId);
    }
    if blob.len() > MAX_BLOB {
        return Err(BlobError::BadBlob);
    }
    let raw = B64.decode(blob).map_err(|_| BlobError::BadBlob)?;
    if raw.len() < 1 + NONCE_LEN + TAG_LEN {
        return Err(BlobError::BadBlob);
    }
    if raw[0] != VERSION {
        return Err(BlobError::Version(raw[0]));
    }
    let (nonce, ct) = raw[1..].split_at(NONCE_LEN);
    Aes256Gcm::new(key.into())
        .decrypt(
            Nonce::from_slice(nonce),
            Payload {
                msg: ct,
                aad: &aad(id),
            },
        )
        .map_err(|_| BlobError::Open)
}

/// Cifra un objeto: su JSON, sin lo de `extra` que choca con lo conocido (`clean_extra`). Es la
/// forma de escribir un objeto; `seal` cifra bytes y no sabe de objetos.
pub fn seal_object(key: &[u8; 32], id: &str, obj: &Object) -> Result<String, BlobError> {
    let mut obj = obj.clone();
    obj.clean_extra();
    let text = serde_json::to_vec(&obj).map_err(|e| BlobError::BadObject(e.to_string()))?;
    seal(key, id, &text)
}

/// Descifra y lee el objeto `id`.
pub fn open_object(key: &[u8; 32], id: &str, blob: &str) -> Result<Object, BlobError> {
    serde_json::from_slice(&open(key, id, blob)?).map_err(|e| BlobError::BadObject(e.to_string()))
}

fn aad(id: &str) -> Vec<u8> {
    [AAD_PREFIX, id.as_bytes()].concat()
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

// ---------- Los objetos (el texto que va cifrado) ----------
//
// Los campos desconocidos (de versiones futuras) no rompen la lectura y se conservan: van a
// `extra` y se vuelven a escribir tal cual, así que un nido viejo que cambia el estado de un
// objetivo no borra lo que añadió una versión nueva. `t` no llega a `extra`: lo consume el
// `tag` de `Object` antes de leer la variante, y al escribir lo pone él delante. Si alguien
// mete en `extra` `t` o un campo conocido, saldría dos veces: `seal_object` los quita antes con
// `clean_extra`.
//
// Un `null` en un campo con valor por omisión se lee como si faltara (`null_default`): otro
// cliente puede escribir `"desc": null` y no por eso el objeto entero tiene que ser ilegible.

/// Un campo que falta o que viene a `null`: su valor por omisión.
fn null_default<'de, D, T>(d: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

/// `order` solo se escribe si es finito: JSON no tiene `NaN` ni infinitos, y `serde_json` los
/// escribiría como `null`, que al leer sería un 0. Así no llega a cifrarse.
fn finite<S: Serializer>(x: &f64, s: S) -> Result<S::Ok, S::Error> {
    if !x.is_finite() {
        return Err(serde::ser::Error::custom(format!(
            "order no es finito: {x}"
        )));
    }
    s.serialize_f64(*x)
}

/// Quita de `extra` las claves que chocan con los campos conocidos (y `t`, si `tag`).
fn clean(extra: &mut Map<String, Value>, known: &[&str], tag: bool) {
    extra.retain(|k, _| !((tag && k == "t") || known.contains(&k.as_str())));
}

/// Un objeto del tablero, por su `t`. Un `t` que no se conoce (de una versión futura) se lee
/// como `Unknown` en vez de fallar, y quien sincroniza se lo salta: así un tipo nuevo no rompe
/// la sincronización entera. `Unknown` no se escribe nunca (daría error): no sabemos qué era.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "lowercase")]
pub enum Object {
    Project(Project),
    Objective(Objective),
    Link(Link),
    #[serde(other, skip_serializing)]
    Unknown,
}

impl Object {
    /// Quita de `extra` (también de `session` y `plan`) lo que repetiría un campo conocido o
    /// `t`. Lo leído de un JSON ya viene así; esto es para lo que se rellena a mano.
    pub fn clean_extra(&mut self) {
        match self {
            Object::Project(p) => clean(&mut p.extra, Project::FIELDS, true),
            Object::Objective(o) => clean(&mut o.extra, Objective::FIELDS, true),
            Object::Link(l) => {
                clean(&mut l.extra, Link::FIELDS, true);
                clean(&mut l.session.extra, LinkSession::FIELDS, false);
                if let Some(p) = &mut l.plan {
                    clean(&mut p.extra, PlanSummary::FIELDS, false);
                }
            }
            Object::Unknown => {}
        }
    }
}

impl Project {
    /// Los campos que se conocen, como salen en el JSON.
    pub const FIELDS: &[&str] = &["name", "desc", "state", "created", "changed", "by"];
}

impl Objective {
    pub const FIELDS: &[&str] = &[
        "project", "name", "desc", "state", "note", "order", "created", "changed", "by",
    ];
}

impl Link {
    pub const FIELDS: &[&str] = &[
        "objective",
        "session",
        "plan",
        "copied",
        "closed",
        "created",
        "changed",
        "by",
    ];
}

impl LinkSession {
    pub const FIELDS: &[&str] = &["machine", "id", "name"];
}

impl PlanSummary {
    pub const FIELDS: &[&str] = &["title", "done", "total", "active", "blocked", "failed"];
}

/// Un proyecto: agrupa objetivos.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Project {
    pub name: String,
    #[serde(default, deserialize_with = "null_default")]
    pub desc: String,
    pub state: ProjectState,
    /// Cuándo se creó y cuándo cambió por última vez (ms Unix).
    pub created: u64,
    pub changed: u64,
    /// Quién lo cambió: `"<equipo>/<sesión>"`, `"web"`, `"app"` o `"Cuco"`.
    #[serde(default, deserialize_with = "null_default")]
    pub by: String,
    /// Los campos que no conocemos, tal cual, para no perderlos al reescribir.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Un objetivo de un proyecto.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Objective {
    /// El id del proyecto.
    pub project: String,
    pub name: String,
    #[serde(default, deserialize_with = "null_default")]
    pub desc: String,
    pub state: ObjectiveState,
    #[serde(default, deserialize_with = "null_default")]
    pub note: String,
    /// Dónde va en su proyecto. Es real: para meter uno entre dos se usa la media, sin renumerar.
    #[serde(default, deserialize_with = "null_default", serialize_with = "finite")]
    pub order: f64,
    pub created: u64,
    pub changed: u64,
    #[serde(default, deserialize_with = "null_default")]
    pub by: String,
    /// Los campos que no conocemos, tal cual, para no perderlos al reescribir.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Una sesión que trabaja en un objetivo. Es de una sesión con un objetivo: una sesión en dos
/// objetivos tiene dos enlaces.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Link {
    /// El id del objetivo.
    pub objective: String,
    pub session: LinkSession,
    /// El último resumen del plan de la sesión que se copió (`null` si no tiene).
    #[serde(default)]
    pub plan: Option<PlanSummary>,
    /// Cuándo se copió el resumen (ms).
    #[serde(default, deserialize_with = "null_default")]
    pub copied: u64,
    /// Cuándo se cerró la sesión (ms), o `null` si sigue.
    #[serde(default)]
    pub closed: Option<u64>,
    pub created: u64,
    pub changed: u64,
    #[serde(default, deserialize_with = "null_default")]
    pub by: String,
    /// Los campos que no conocemos, tal cual, para no perderlos al reescribir.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// La sesión de un enlace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkSession {
    /// El id del equipo en el hub.
    pub machine: String,
    /// El id de la sesión en su nido (8 hex).
    pub id: String,
    /// Cómo se ve: `"<equipo>/<sesión>"`.
    #[serde(default, deserialize_with = "null_default")]
    pub name: String,
    /// Los campos que no conocemos, tal cual, para no perderlos al reescribir.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Cuánto va hecho del plan de una sesión (como `plan::Summary`, sin lo que solo es de la
/// lista de sesiones). El título es opcional porque un plan puede no tenerlo: va `null`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanSummary {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default, deserialize_with = "null_default")]
    pub done: usize,
    #[serde(default, deserialize_with = "null_default")]
    pub total: usize,
    #[serde(default, deserialize_with = "null_default")]
    pub active: usize,
    #[serde(default, deserialize_with = "null_default")]
    pub blocked: usize,
    #[serde(default, deserialize_with = "null_default")]
    pub failed: usize,
    /// Los campos que no conocemos, tal cual, para no perderlos al reescribir.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// El estado de un proyecto. Uno que no se conoce (de una versión futura) se guarda tal cual en
/// `Other` y se vuelve a escribir igual: no rompe el objeto entero ni se pierde al reescribirlo.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProjectState {
    Active,
    Archived,
    #[serde(untagged)]
    Other(String),
}

/// El estado de un objetivo; los desconocidos, como en `ProjectState`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ObjectiveState {
    Pending,
    Doing,
    Blocked,
    Done,
    Dropped,
    #[serde(untagged)]
    Other(String),
}

// ---------- La copia del tablero ----------

/// Cada cuánto se sincroniza aunque no llegue ningún aviso, y se mira si el hub se ha quedado
/// sin tablero (`probe`).
pub const SYNC_EVERY: Duration = Duration::from_secs(5 * 60);
/// Lo que se espera entre dos escrituras de la resubida: 20 por minuto. El hub frena a 120 por
/// minuto y conexión y a 3000 por hora y cuenta, y la resubida tiene que dejar sitio a lo que se
/// edita a mano (1200 por hora).
pub const REUPLOAD_GAP: Duration = Duration::from_secs(3);
/// Con el freno del hub (429), la resubida espera esto en huecos de `REUPLOAD_GAP` (un minuto).
const BUSY_GAPS: u32 = 20;
/// Cuántas veces se intenta una escritura que choca (409) antes de dejarlo.
pub const MAX_TRIES: usize = 3;
/// Cada cuántos objetos resubidos se guarda la copia.
const SAVE_EVERY: usize = 20;

/// Un objeto en la copia, como lo da el hub (sin su id, que es la clave).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stored {
    pub v: i64,
    #[serde(default)]
    pub kid: String,
    #[serde(default)]
    pub blob: Option<String>,
    #[serde(default)]
    pub gone: bool,
    #[serde(default)]
    pub at: i64,
}

impl From<&BoardItem> for Stored {
    fn from(i: &BoardItem) -> Self {
        Self {
            v: i.v,
            kid: i.kid.clone(),
            blob: i.blob.clone(),
            gone: i.gone,
            at: i.at,
        }
    }
}

/// Lo que se guarda en `data/board.json`.
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
struct Copy {
    /// El `since` de la próxima sincronización.
    #[serde(default)]
    next: i64,
    #[serde(default)]
    items: BTreeMap<String, Stored>,
    /// Los que faltan por volver a subir a un hub que se quedó sin tablero (decisión 8).
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    reupload: BTreeSet<String>,
}

/// Por qué un objeto de la copia no se puede leer. No lleva nada de lo descifrado.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unreadable {
    /// Su `kid` no está en el llavero.
    NoKey,
    /// El blob no se descifra con la clave de su `kid` (o no es un blob).
    Broken,
    /// Se descifra, pero no es un objeto.
    NotAnObject,
    /// Es un objeto de un tipo que no conocemos (de una versión futura).
    Unknown,
}

impl std::fmt::Display for Unreadable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Unreadable::NoKey => "sin su clave",
            Unreadable::Broken => "no se descifra",
            Unreadable::NotAnObject => "no es un objeto",
            Unreadable::Unknown => "de un tipo que no se conoce",
        })
    }
}

/// Un objeto legible del tablero.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub id: String,
    pub v: i64,
    pub object: Object,
}

/// La copia descifrada: lo legible y, aparte, lo que no se puede leer (que se conserva tal cual).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Decrypted {
    pub objects: Vec<Entry>,
    pub unreadable: Vec<(String, Unreadable)>,
}

/// Por qué no salió un cambio del tablero.
#[derive(Debug)]
pub enum EditError {
    /// No hay clave actual en el llavero con la que cifrar.
    NoKey,
    /// El objeto no está (o está borrado).
    Missing,
    /// El objeto no se puede leer: no se reescribe.
    Unreadable(Unreadable),
    /// No se pudo cifrar (demasiado grande, un `order` que no es finito…).
    Blob(BlobError),
    /// Lo que dijo el hub; con `Conflict`, tras `MAX_TRIES` choques.
    Board(BoardError),
}

impl std::fmt::Display for EditError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EditError::NoKey => write!(f, "no hay clave del tablero"),
            EditError::Missing => write!(f, "el objeto no está en el tablero"),
            EditError::Unreadable(u) => write!(f, "el objeto no se puede leer ({u})"),
            // `BadObject` puede llevar trozos del texto: solo su nombre.
            EditError::Blob(e @ BlobError::TooBig(_)) => write!(f, "{e}"),
            EditError::Blob(e) => write!(f, "no se pudo cifrar el objeto ({})", blob_error_kind(e)),
            EditError::Board(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for EditError {}

/// El nombre de un error del blob, para los registros: nunca su texto, que en `BadObject` puede
/// llevar trozos de lo descifrado.
pub fn blob_error_kind(e: &BlobError) -> &'static str {
    match e {
        BlobError::TooBig(_) => "demasiado grande",
        BlobError::BadId => "id no válido",
        BlobError::Random(_) => "sin números al azar",
        BlobError::BadBlob => "blob mal formado",
        BlobError::Version(_) => "versión desconocida",
        BlobError::Open => "no se descifra",
        BlobError::BadObject(_) => "objeto no válido",
    }
}

/// La copia de este nido del tablero de la cuenta: los objetos tal y como los da el hub (cifrados)
/// y hasta dónde se ha leído, en `data/board.json`.
pub struct BoardCache {
    path: PathBuf,
    hub: Arc<Hub>,
    copy: Mutex<Copy>,
    /// Que los guardados (y el vaciado) vayan de uno en uno y en orden.
    disk: Mutex<()>,
    /// Una sincronización a la vez.
    syncing: tokio::sync::Mutex<()>,
    /// Sube al vaciar la copia: lo que estaba en marcha (sincronizar, resubir, escribir) ya no
    /// la toca.
    epoch: AtomicU64,
    /// Si hay una resubida en marcha.
    reuploading: AtomicBool,
    /// Lo que se espera entre dos escrituras de la resubida.
    gap: Duration,
    /// Sube con cada cambio de la copia.
    revision: watch::Sender<u64>,
    /// El último error al sincronizar que se apuntó, para no repetirlo cada vez.
    said: Mutex<Option<String>>,
}

impl Copy {
    /// Junta un objeto que da el hub. Gana el de `v` mayor o marca más nueva: dentro de una
    /// cuenta, `v` solo sube; al pasar a otra cuenta (resubida, cuentas juntadas) la `v` puede
    /// volver a empezar, pero la marca es nueva. Lo que ya se tiene (mismo `v` y marca, como lo
    /// del solape) no cambia nada. Uno que faltaba por resubir ya está en el hub: se toma tal
    /// cual y deja de faltar. Devuelve si cambió algo.
    fn apply(&mut self, item: BoardItem) -> bool {
        let pending = self.reupload.remove(&item.id);
        let newer = match self.items.get(&item.id) {
            None => true,
            Some(cur) => pending || item.v > cur.v || item.at > cur.at,
        };
        if newer {
            let stored = Stored::from(&item);
            self.items.insert(item.id, stored);
        }
        newer || pending
    }

    /// Si hay objetos propios (no lápidas) que se podrían volver a subir.
    fn has_objects(&self) -> bool {
        self.items.values().any(|s| !s.gone && s.blob.is_some())
    }
}

/// La clave del llavero con este `kid` (nunca otra).
fn key_of(keys: &Keyring, kid: &str) -> Option<[u8; 32]> {
    let k = keys.keys.iter().find(|k| k.kid == kid && !k.removed)?;
    B64.decode(&k.key).ok()?.try_into().ok()
}

/// La clave con que se cifra ahora: su `kid` y sus bytes.
fn current_key(keys: &Keyring) -> Option<(String, [u8; 32])> {
    let k = keys.current()?;
    Some((k.kid.clone(), key_of(keys, &k.kid)?))
}

/// Lee el objeto `id` de la copia con la clave de su `kid`.
fn read(keys: &Keyring, id: &str, s: &Stored) -> Result<Object, Unreadable> {
    let key = key_of(keys, &s.kid).ok_or(Unreadable::NoKey)?;
    let blob = s.blob.as_deref().ok_or(Unreadable::Broken)?;
    match open_object(&key, id, blob) {
        Ok(Object::Unknown) => Err(Unreadable::Unknown),
        Ok(o) => Ok(o),
        Err(BlobError::BadObject(_)) => Err(Unreadable::NotAnObject),
        Err(_) => Err(Unreadable::Broken),
    }
}

/// Lo que se vuelve a subir de `s`: su `kid` y su blob. Los bytes descifrados van tal cual, sin
/// pasar por `Object` (así un tipo que no conocemos no se pierde), cifrados con la clave actual
/// si es otra; con la misma, el blob de siempre. Si no se descifra con la clave de su `kid`, nada.
fn reupload_blob(keys: &Keyring, id: &str, s: &Stored) -> Option<(String, String)> {
    let blob = s.blob.as_deref()?;
    let bytes = open(&key_of(keys, &s.kid)?, id, blob).ok()?;
    match current_key(keys) {
        Some((kid, key)) if kid != s.kid => Some((kid, seal(&key, id, &bytes).ok()?)),
        _ => Some((s.kid.clone(), blob.to_owned())),
    }
}

impl BoardCache {
    pub fn load(path: PathBuf, hub: Arc<Hub>) -> Arc<Self> {
        Self::load_with(path, hub, REUPLOAD_GAP)
    }

    /// `load`, con otro ritmo para la resubida (para las pruebas).
    pub fn load_with(path: PathBuf, hub: Arc<Hub>, gap: Duration) -> Arc<Self> {
        // Es una copia: si no se entiende, se empieza de cero y se vuelve a bajar.
        let copy = match std::fs::read(&path) {
            Ok(b) => serde_json::from_slice(&b).unwrap_or_else(|e| {
                eprintln!("nido: la copia del tablero no se entiende ({e}); se empieza de cero");
                Copy::default()
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Copy::default(),
            Err(e) => {
                eprintln!("nido: no se pudo leer la copia del tablero ({e}); se empieza de cero");
                Copy::default()
            }
        };
        Arc::new(Self {
            path,
            hub,
            copy: Mutex::new(copy),
            disk: Mutex::new(()),
            syncing: tokio::sync::Mutex::new(()),
            epoch: AtomicU64::new(0),
            reuploading: AtomicBool::new(false),
            gap,
            revision: watch::Sender::new(0),
            said: Mutex::new(None),
        })
    }

    /// Avisa de cada cambio de la copia. Quien la escuche no debe tener el valor leído (`borrow`)
    /// mientras usa el grupo: `clear` avisa con el grupo cogido.
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.revision.subscribe()
    }

    /// El `since` de la próxima sincronización.
    pub fn next(&self) -> i64 {
        self.lock().next
    }

    /// El objeto `id` de la copia, como lo dio el hub.
    pub fn item(&self, id: &str) -> Option<Stored> {
        self.lock().items.get(id).cloned()
    }

    /// Los ids de la copia (lápidas incluidas), en orden.
    pub fn ids(&self) -> Vec<String> {
        self.lock().items.keys().cloned().collect()
    }

    /// Cuántos faltan por resubir.
    pub fn pending_reupload(&self) -> usize {
        self.lock().reupload.len()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Copy> {
        self.copy.lock().expect("tablero")
    }

    fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::SeqCst)
    }

    fn bump(&self) {
        self.revision.send_modify(|r| *r += 1);
    }

    /// Escribe la copia en el disco. Con `disk` cogido, y la copia se lee después de cogerlo:
    /// así uno que guarda tarde no pisa lo que guardó otro (o un vaciado) con algo más viejo.
    fn write_now(&self) {
        let _disk = self.disk.lock().expect("tablero");
        let body = serde_json::to_vec(&*self.lock()).expect("la copia se escribe");
        if let Err(e) = crate::sys::write_private_atomic(&self.path, &body) {
            eprintln!("nido: no se pudo guardar la copia del tablero: {e}");
        }
    }

    /// Guarda la copia (fuera del hilo de las tareas: puede ser grande) y avisa del cambio.
    async fn save(self: &Arc<Self>) {
        let me = self.clone();
        let _ = tokio::task::spawn_blocking(move || me.write_now()).await;
        self.bump();
    }

    /// Junta lo que dio el hub en una escritura, si la copia no se vació mientras tanto; si
    /// cambió algo, la guarda.
    async fn applied(self: &Arc<Self>, epoch: u64, item: BoardItem) {
        let changed = {
            let mut c = self.lock();
            self.epoch() == epoch && c.apply(item)
        };
        if changed {
            self.save().await;
        }
    }

    /// Deja `id` como lápida en la copia (si no lo es ya).
    async fn bury(self: &Arc<Self>, epoch: u64, id: &str) {
        let changed = {
            let mut c = self.lock();
            match c.items.get_mut(id) {
                Some(s) if self.epoch() == epoch && !s.gone => {
                    s.gone = true;
                    s.blob = None;
                    s.kid.clear();
                    true
                }
                _ => false,
            }
        };
        if changed {
            self.save().await;
        }
    }

    /// Vacía la copia, en memoria y en el disco, y para lo que estuviera en marcha (cuando este
    /// equipo sale del grupo). No usa el grupo: se llama con él cogido.
    pub fn clear(&self) {
        self.epoch.fetch_add(1, Ordering::SeqCst);
        {
            let _disk = self.disk.lock().expect("tablero");
            let body = {
                let mut c = self.lock();
                *c = Copy::default();
                serde_json::to_vec(&*c).expect("la copia se escribe")
            };
            if let Err(e) = crate::sys::write_private_atomic(&self.path, &body) {
                eprintln!("nido: no se pudo vaciar la copia del tablero: {e}");
            }
        }
        self.bump();
    }

    /// Se pone al día con el hub desde el `since` guardado, página a página: mientras haya
    /// `more`, con el mismo `since` y el `after` de la anterior. Solo al acabar se guarda `next`
    /// (que nunca baja); si se corta a medias, la próxima empieza otra vez desde el guardado. Con
    /// `reset`, se vacía la copia y se empieza desde 0. Devuelve si cambió algo.
    pub async fn sync(self: &Arc<Self>) -> Result<bool, BoardError> {
        let _one = self.syncing.lock().await;
        let epoch = self.epoch();
        let mut since = self.next();
        let mut after: Option<String> = None;
        let mut changed = false;
        let mut was_reset = false;
        loop {
            let page = self.hub.board_get(since, after.as_deref()).await?;
            let mut c = self.lock();
            if self.epoch() != epoch {
                // Se vació mientras tanto: lo de antes ya no vale.
                return Ok(false);
            }
            if page.reset {
                if after.is_some() || was_reset {
                    return Err(BoardError::Hub("reset fuera de sitio".into()));
                }
                *c = Copy::default();
                drop(c);
                (since, was_reset, changed) = (0, true, true);
                continue;
            }
            for item in page.items {
                changed |= c.apply(item);
            }
            if !page.more {
                if page.next > c.next {
                    c.next = page.next;
                    changed = true;
                }
                break;
            }
            after = page.after;
        }
        if changed {
            self.save().await;
        }
        Ok(changed)
    }

    /// Mira si el hub se ha quedado sin tablero teniendo esta copia: una lectura completa
    /// (`since = 0`) que no da nada, ni lápidas. Entonces la vuelve a subir (decisión 8): el
    /// hub perdió lo que tenía (uno sin MongoDB que se reinició) o la cuenta es nueva (al partir
    /// un grupo de dos, la cuenta se la puede quedar el equipo quitado). Si da algo, lo junta.
    pub async fn probe(self: &Arc<Self>, keys: &Keyring) -> Result<(), BoardError> {
        if !self.lock().has_objects() {
            return Ok(());
        }
        let epoch = self.epoch();
        let page = self.hub.board_get(0, None).await?;
        let changed = {
            let mut c = self.lock();
            if self.epoch() != epoch {
                return Ok(());
            }
            if !page.items.is_empty() || page.more || page.reset {
                let mut changed = false;
                for item in page.items {
                    changed |= c.apply(item);
                }
                changed
            } else if current_key(keys).is_none() {
                // Sin clave con que subirla, se deja la copia como está: el próximo sondeo.
                return Ok(());
            } else {
                // Se sube lo que se puede leer con su clave. Lo demás se queda en la copia (es lo
                // único que queda de ello), pero no se sube; las lápidas ya no hacen falta.
                c.items.retain(|_, s| !s.gone);
                c.reupload = c
                    .items
                    .iter()
                    .filter(|(id, s)| reupload_blob(keys, id, s).is_some())
                    .map(|(id, _)| id.clone())
                    .collect();
                // El hub está vacío: la próxima, desde el principio.
                c.next = 0;
                eprintln!(
                    "nido: el hub no tiene tablero; se vuelven a subir {} objetos de la copia",
                    c.reupload.len()
                );
                true
            }
        };
        if changed {
            self.save().await;
        }
        self.resume_reupload(keys.clone());
        Ok(())
    }

    /// Sigue con la resubida, si falta algo y no está ya en marcha.
    fn resume_reupload(self: &Arc<Self>, keys: Keyring) {
        if self.lock().reupload.is_empty() || self.reuploading.swap(true, Ordering::SeqCst) {
            return;
        }
        let me = self.clone();
        tokio::spawn(async move {
            me.reupload(keys).await;
            me.reuploading.store(false, Ordering::SeqCst);
        });
    }

    /// La resubida, de uno en uno y a ritmo (`gap`), con `v = 0`. Un 409 es que otro nido de la
    /// casa se adelantó: cuenta como hecho y se queda lo suyo. Con el freno del hub, espera y
    /// sigue; sin hub, lo deja para la próxima conexión (lo que falta está en `board.json`).
    async fn reupload(self: &Arc<Self>, keys: Keyring) {
        let epoch = self.epoch();
        let (mut done, mut first) = (0usize, true);
        loop {
            let (id, blob) = {
                let mut c = self.lock();
                if self.epoch() != epoch {
                    return;
                }
                let Some(id) = c.reupload.first().cloned() else {
                    break;
                };
                let blob = c.items.get(&id).and_then(|s| reupload_blob(&keys, &id, s));
                if blob.is_none() {
                    // Ya no se puede leer (otro llavero): no se sube, pero se queda en la copia.
                    c.reupload.remove(&id);
                    continue;
                }
                (id, blob)
            };
            let Some((kid, blob)) = blob else { continue };
            if !std::mem::take(&mut first) {
                tokio::time::sleep(self.gap).await;
            }
            let r = self.hub.board_put(&id, 0, &kid, &blob).await;
            {
                let mut c = self.lock();
                if self.epoch() != epoch {
                    return;
                }
                match r {
                    Ok(st) => {
                        c.apply(BoardItem {
                            id: id.clone(),
                            v: st.v,
                            kid,
                            blob: Some(blob),
                            gone: false,
                            at: st.at,
                        });
                    }
                    Err(BoardError::Conflict(current)) => {
                        c.apply(current);
                    }
                    Err(BoardError::Busy) => {
                        drop(c);
                        tokio::time::sleep(self.gap * BUSY_GAPS).await;
                        continue;
                    }
                    Err(BoardError::TooBig) => {
                        eprintln!("nido: tablero: {id} no cabe en el hub; no se vuelve a subir");
                        c.reupload.remove(&id);
                    }
                    Err(BoardError::Full) => {
                        eprintln!("nido: tablero: la cuenta está llena; se deja la resubida");
                        c.reupload.clear();
                    }
                    Err(BoardError::Hub(e)) => {
                        eprintln!("nido: tablero: resubida parada ({e}); sigue al volver el hub");
                        drop(c);
                        self.save().await;
                        return;
                    }
                }
            }
            done += 1;
            if done.is_multiple_of(SAVE_EVERY) {
                self.save().await;
            }
        }
        self.save().await;
        if done > 0 {
            eprintln!("nido: tablero: resubida acabada ({done} escrituras)");
        }
    }

    /// Los objetos de la copia, descifrados cada uno con la clave de su `kid`. Lo que no se puede
    /// leer va aparte y se conserva tal cual; las lápidas no salen.
    pub fn decrypted(&self, keys: &Keyring) -> Decrypted {
        let c = self.lock();
        let mut d = Decrypted::default();
        for (id, s) in c.items.iter().filter(|(_, s)| !s.gone) {
            match read(keys, id, s) {
                Ok(object) => d.objects.push(Entry {
                    id: id.clone(),
                    v: s.v,
                    object,
                }),
                Err(why) => d.unreadable.push((id.clone(), why)),
            }
        }
        d
    }

    /// Crea un objeto nuevo con la clave actual. Devuelve su id.
    pub async fn create(
        self: &Arc<Self>,
        keys: &Keyring,
        obj: &Object,
    ) -> Result<String, EditError> {
        let (kid, key) = current_key(keys).ok_or(EditError::NoKey)?;
        let epoch = self.epoch();
        let id = new_id().map_err(|e| EditError::Blob(BlobError::Random(e)))?;
        let blob = seal_object(&key, &id, obj).map_err(EditError::Blob)?;
        let st = self
            .hub
            .board_put(&id, 0, &kid, &blob)
            .await
            .map_err(EditError::Board)?;
        self.applied(
            epoch,
            BoardItem {
                id: id.clone(),
                v: st.v,
                kid,
                blob: Some(blob),
                gone: false,
                at: st.at,
            },
        )
        .await;
        Ok(id)
    }

    /// Cambia el objeto `id`: `f` recibe el objeto leído (con la clave de su `kid`) y lo cambia,
    /// así se conserva lo que no conocemos, en `extra`. Se escribe con la clave actual. Si choca
    /// (409), se vuelve a aplicar `f` sobre el objeto como está, hasta `MAX_TRIES` escrituras en
    /// total. Lo que no se puede leer no se reescribe. Devuelve el objeto como quedó.
    pub async fn update(
        self: &Arc<Self>,
        keys: &Keyring,
        id: &str,
        f: impl Fn(&mut Object),
    ) -> Result<Object, EditError> {
        let (kid, key) = current_key(keys).ok_or(EditError::NoKey)?;
        let epoch = self.epoch();
        let (mut stored, mut v) = {
            let c = self.lock();
            let s = c.items.get(id).filter(|s| !s.gone).cloned();
            let s = s.ok_or(EditError::Missing)?;
            // Uno que falta por resubir no está en el hub.
            let v = if c.reupload.contains(id) { 0 } else { s.v };
            (s, v)
        };
        let mut tries = 0;
        loop {
            tries += 1;
            let mut obj = read(keys, id, &stored).map_err(EditError::Unreadable)?;
            f(&mut obj);
            let blob = seal_object(&key, id, &obj).map_err(EditError::Blob)?;
            match self.hub.board_put(id, v, &kid, &blob).await {
                Ok(st) => {
                    let item = BoardItem {
                        id: id.into(),
                        v: st.v,
                        kid,
                        blob: Some(blob),
                        gone: false,
                        at: st.at,
                    };
                    self.applied(epoch, item).await;
                    return Ok(obj);
                }
                Err(BoardError::Conflict(current)) => {
                    // El mismo blob: era nuestra escritura repetida, ya hecha.
                    let ours = !current.gone && current.blob.as_deref() == Some(blob.as_str());
                    self.applied(epoch, current.clone()).await;
                    if ours {
                        return Ok(obj);
                    }
                    if current.gone {
                        return Err(EditError::Missing);
                    }
                    if tries >= MAX_TRIES {
                        return Err(EditError::Board(BoardError::Conflict(current)));
                    }
                    v = current.v;
                    stored = Stored::from(&current);
                }
                Err(e) => return Err(EditError::Board(e)),
            }
        }
    }

    /// Borra el objeto `id` (el hub deja una lápida). Un borrado que ya estaba hecho (un 409 con
    /// la lápida: lo borró otro, o es el nuestro repetido tras un corte) cuenta como hecho; uno
    /// que choca con un cambio se vuelve a intentar sobre la versión de ahora, hasta `MAX_TRIES`.
    pub async fn remove(self: &Arc<Self>, id: &str) -> Result<(), EditError> {
        let epoch = self.epoch();
        // `None`: uno que faltaba por resubir, que no está en el hub: basta con no subirlo.
        let v = {
            let mut c = self.lock();
            let Some(s) = c.items.get(id) else {
                return Err(EditError::Missing);
            };
            if s.gone {
                return Ok(());
            }
            let v = s.v;
            if c.reupload.remove(id) {
                c.items.remove(id);
                None
            } else {
                Some(v)
            }
        };
        let Some(mut v) = v else {
            self.save().await;
            return Ok(());
        };
        let mut tries = 0;
        loop {
            tries += 1;
            match self.hub.board_del(id, v).await {
                Ok(st) => {
                    let item = BoardItem {
                        id: id.into(),
                        v: st.v,
                        kid: String::new(),
                        blob: None,
                        gone: true,
                        at: st.at,
                    };
                    self.applied(epoch, item).await;
                    return Ok(());
                }
                Err(BoardError::Conflict(current)) => {
                    let gone = current.gone;
                    let cv = current.v;
                    self.applied(epoch, current.clone()).await;
                    if gone {
                        // El hub no lo tiene (o ya es una lápida): aquí también, borrado, aunque
                        // su lápida (`v: 0` si no existe) pierda frente a lo que tenía la copia.
                        self.bury(epoch, id).await;
                        return Ok(());
                    }
                    if tries >= MAX_TRIES {
                        return Err(EditError::Board(BoardError::Conflict(current)));
                    }
                    v = cv;
                }
                Err(e) => return Err(EditError::Board(e)),
            }
        }
    }

    /// Sincroniza y, con `probe`, mira si el hub se quedó sin tablero; luego sigue con la
    /// resubida que faltara. Los errores se apuntan (sin repetir el mismo seguido).
    pub async fn refresh(self: &Arc<Self>, keys: &Keyring, probe: bool) {
        let mut r = self.sync().await.map(|_| ());
        if r.is_ok() && probe {
            r = self.probe(keys).await;
        }
        let mut said = self.said.lock().expect("tablero");
        match r {
            Ok(()) => {
                *said = None;
                drop(said);
                self.resume_reupload(keys.clone());
            }
            Err(e) => {
                let e = e.to_string();
                if said.as_deref() != Some(e.as_str()) {
                    eprintln!("nido: tablero: no se pudo sincronizar ({e})");
                    *said = Some(e);
                }
            }
        }
    }
}

/// Mantiene la copia al día mientras haya conexión con el hub: al conectar (lo primero, un `get`,
/// que es lo que hace que el hub avise en esa conexión), con cada aviso del hub y cada
/// `SYNC_EVERY`. Al conectar, si cambia la cuenta y cada `SYNC_EVERY`, mira además si el hub se
/// quedó sin tablero (`probe`): el hub no avisa de eso, y cuanto antes se vea, menos fácil es que
/// alguien empiece a llenar un tablero vacío (con un objeto en el hub ya no se resube). `keys` da el llavero del grupo cada vez.
pub fn spawn(cache: Arc<BoardCache>, keys: impl Fn() -> Keyring + Send + Sync + 'static) {
    tokio::spawn(async move {
        let mut status = cache.hub.status.subscribe();
        status.mark_changed();
        let mut notices = cache.hub.board_notices();
        let start = tokio::time::Instant::now() + SYNC_EVERY;
        let mut tick = tokio::time::interval_at(start, SYNC_EVERY);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let (mut connected, mut account) = (false, None);
        loop {
            let probe = tokio::select! {
                r = status.changed() => {
                    if r.is_err() {
                        return;
                    }
                    let (now, acct) = {
                        let s = status.borrow_and_update();
                        (s.connected, s.account.clone())
                    };
                    let fresh = now && !connected;
                    // Otra cuenta (no la primera que se sabe): puede venir con el tablero vacío.
                    let moved = now && account.is_some() && acct.is_some() && acct != account;
                    connected = now;
                    if acct.is_some() {
                        account = acct;
                    }
                    if !fresh && !moved {
                        continue;
                    }
                    true
                }
                r = notices.changed() => {
                    if r.is_err() {
                        return;
                    }
                    false
                }
                _ = tick.tick() => true,
            };
            if connected {
                cache.refresh(&keys(), probe).await;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    const ID: &str = "0123456789abcdef0123456789abcdef";

    fn seq_key() -> [u8; 32] {
        std::array::from_fn(|i| i as u8)
    }

    fn unhex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn round_trip() {
        let key = seq_key();
        let text = "{\"t\":\"project\",\"name\":\"Baño\"}".as_bytes();
        let a = seal(&key, ID, text).unwrap();
        assert_eq!(open(&key, ID, &a).unwrap(), text);
        // Nonce nuevo en cada cifrado: el mismo texto da otro blob.
        let b = seal(&key, ID, text).unwrap();
        assert_ne!(a, b);
        assert_eq!(open(&key, ID, &b).unwrap(), text);
        // Forma: versión, nonce, cifrado y etiqueta.
        let raw = B64.decode(&a).unwrap();
        assert_eq!(raw[0], VERSION);
        assert_eq!(raw.len(), 1 + 12 + text.len() + 16);
        // Vacío también vale.
        assert!(
            open(&key, ID, &seal(&key, ID, b"").unwrap())
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn another_id_or_key_does_not_open() {
        let key = seq_key();
        let blob = seal(&key, ID, b"hola").unwrap();
        let other = "0123456789abcdef0123456789abcdee";
        assert!(matches!(open(&key, other, &blob), Err(BlobError::Open)));
        assert!(matches!(open(&[0u8; 32], ID, &blob), Err(BlobError::Open)));
        // Un byte cambiado, en el cifrado o en la etiqueta.
        let mut raw = B64.decode(&blob).unwrap();
        let last = raw.len() - 1;
        raw[last] ^= 1;
        assert!(matches!(
            open(&key, ID, &B64.encode(&raw)),
            Err(BlobError::Open)
        ));
        raw[last] ^= 1;
        raw[13] ^= 1;
        assert!(matches!(
            open(&key, ID, &B64.encode(&raw)),
            Err(BlobError::Open)
        ));
    }

    #[test]
    fn bad_blobs_and_ids() {
        let key = seq_key();
        let blob = seal(&key, ID, b"hola").unwrap();
        let mut raw = B64.decode(&blob).unwrap();
        raw[0] = 2;
        assert!(matches!(
            open(&key, ID, &B64.encode(&raw)),
            Err(BlobError::Version(2))
        ));
        // Más corto que versión + nonce + etiqueta.
        assert!(matches!(
            open(&key, ID, &B64.encode([1u8; 28])),
            Err(BlobError::BadBlob)
        ));
        assert!(matches!(
            open(&key, ID, "no es base64!"),
            Err(BlobError::BadBlob)
        ));
        // Base64 URL o sin relleno no es el estándar. Con bytes fijos: `0xfb` da `+` y `/` en
        // estándar (`-` y `_` en URL), y 34 bytes llevan `==` de relleno. Si se aceptaran, el
        // error sería otro (versión 0xfb), no `BadBlob`.
        use base64::engine::general_purpose::{STANDARD_NO_PAD, URL_SAFE};
        let url = URL_SAFE.encode([0xfb; 33]);
        assert!(url.contains('-') && url.contains('_'), "{url}");
        assert!(matches!(open(&key, ID, &url), Err(BlobError::BadBlob)));
        assert!(matches!(
            open(&key, ID, &B64.encode([0xfb; 33])),
            Err(BlobError::Version(0xfb))
        ));
        let no_pad = STANDARD_NO_PAD.encode([0xfb; 34]);
        assert!(B64.encode([0xfb; 34]).ends_with("=="));
        assert!(matches!(open(&key, ID, &no_pad), Err(BlobError::BadBlob)));
        // Más largo de lo que acepta el hub, aunque sea base64 bueno: ni se decodifica.
        let long = B64.encode(vec![1u8; 8253]);
        assert_eq!(long.len(), 11004);
        assert!(matches!(open(&key, ID, &long), Err(BlobError::BadBlob)));
        assert!(matches!(
            open(&key, ID, &B64.encode(vec![1u8; 8250])),
            Err(BlobError::Open)
        ));
        // El id va tal cual en `aad`: en mayúsculas o con otro largo no vale.
        let upper = ID.to_uppercase();
        assert!(matches!(seal(&key, &upper, b"x"), Err(BlobError::BadId)));
        assert!(matches!(seal(&key, "abc", b"x"), Err(BlobError::BadId)));
        assert!(matches!(open(&key, &upper, &blob), Err(BlobError::BadId)));
    }

    /// El `kid` es el de `group::board_kid`, con sus mismos valores.
    #[test]
    fn kid_is_stable() {
        assert_eq!(kid(&[0u8; 32]), "cf1718d3fe1a8755");
        assert_eq!(kid(&seq_key()), "1ffd9bb07aebb8f4");
        assert_eq!(kid(&seq_key()), kid(&seq_key()));
    }

    #[test]
    fn too_big_is_rejected() {
        let key = seq_key();
        let big = vec![b'a'; MAX_PLAIN + 1];
        assert!(matches!(seal(&key, ID, &big), Err(BlobError::TooBig(8193))));
        // El más grande que cabe da exactamente 10 964 caracteres, por debajo de los 11 000 del
        // hub.
        let max = vec![b'a'; MAX_PLAIN];
        let blob = seal(&key, ID, &max).unwrap();
        assert_eq!(blob.len(), 10964);
        assert_eq!(open(&key, ID, &blob).unwrap(), max);
    }

    #[test]
    fn new_ids_are_32_hex() {
        let a = new_id().unwrap();
        assert!(is_id(&a), "{a}");
        assert_ne!(a, new_id().unwrap());
    }

    #[test]
    fn objects_read_with_extra_fields() {
        let o: Object = serde_json::from_str(
            r#"{"t":"objective","project":"p1","name":"Cocina","desc":"","state":"doing",
                "note":"falta el horno","order":1.5,"created":1,"changed":2,"by":"web",
                "color":"verde","tags":["a",{"b":1}]}"#,
        )
        .unwrap();
        let Object::Objective(ob) = o else {
            panic!("{o:?}")
        };
        assert_eq!(ob.state, ObjectiveState::Doing);
        assert_eq!(ob.order, 1.5);
        assert_eq!(ob.note, "falta el horno");
        // Lo desconocido queda en `extra`, sin `t` ni los campos que sí se conocen.
        assert_eq!(
            Value::Object(ob.extra),
            serde_json::json!({"color": "verde", "tags": ["a", {"b": 1}]})
        );
        // Lo que falta y tiene valor por omisión.
        let o: Object = serde_json::from_str(
            r#"{"t":"project","name":"Casa","state":"active","created":1,"changed":1}"#,
        )
        .unwrap();
        assert_eq!(
            o,
            Object::Project(Project {
                name: "Casa".into(),
                desc: String::new(),
                state: ProjectState::Active,
                created: 1,
                changed: 1,
                by: String::new(),
                extra: Map::new(),
            })
        );
    }

    /// Un nido viejo que reescribe un objeto no borra lo que añadió una versión nueva: los
    /// campos desconocidos (también dentro de `session` y `plan`) vuelven tal cual, y `t` sale una
    /// sola vez.
    #[test]
    fn unknown_fields_survive_a_rewrite() {
        let text = r#"{"t":"link","objective":"o1","session":{"machine":"m1","id":"ab12cd34","name":"n","host":{"os":"linux"}},"plan":{"title":"Obra","done":1,"total":2,"active":0,"blocked":0,"failed":0,"skipped":3},"copied":5,"closed":null,"created":1,"changed":2,"by":"web","color":"verde","tags":["a",{"b":1}],"zz":1.25}"#;
        let mut o: Object = serde_json::from_str(text).unwrap();
        let Object::Link(link) = &mut o else {
            panic!("{o:?}")
        };
        assert!(!link.extra.contains_key("t"));
        assert_eq!(link.extra.len(), 3);
        assert_eq!(
            link.session.extra["host"],
            serde_json::json!({"os": "linux"})
        );
        assert_eq!(link.plan.as_ref().unwrap().extra["skipped"], 3);
        // Sin cambios, vuelve el mismo texto (los desconocidos van al final, por orden
        // alfabético).
        assert_eq!(serde_json::to_string(&o).unwrap(), text);
        // Con un cambio, cambia solo eso.
        let Object::Link(link) = &mut o else {
            unreachable!()
        };
        link.closed = Some(9);
        link.changed = 3;
        let out = serde_json::to_string(&o).unwrap();
        assert_eq!(out.matches(r#""t":"#).count(), 1, "{out}");
        let mut want: Value = serde_json::from_str(text).unwrap();
        want["closed"] = 9.into();
        want["changed"] = 3.into();
        assert_eq!(serde_json::from_str::<Value>(&out).unwrap(), want);

        // Un objetivo al que se le cambia el estado.
        let text = r#"{"t":"objective","project":"p","name":"n","desc":"","state":"doing","note":"","order":2.0,"created":1,"changed":1,"by":"app","due":1763400000000,"labels":["x"]}"#;
        let mut o: Object = serde_json::from_str(text).unwrap();
        assert_eq!(serde_json::to_string(&o).unwrap(), text);
        let Object::Objective(ob) = &mut o else {
            panic!("{o:?}")
        };
        ob.state = ObjectiveState::Done;
        let out: Value = serde_json::to_value(&o).unwrap();
        assert_eq!(out["state"], "done");
        assert_eq!(out["due"], 1763400000000u64);
        assert_eq!(out["labels"], serde_json::json!(["x"]));
        assert_eq!(out["t"], "objective");

        // Y un proyecto, con un estado desconocido además.
        let text = r#"{"t":"project","name":"Casa","desc":"","state":"paused","created":1,"changed":2,"by":"app","color":"verde","zona":{"a":[1,2]}}"#;
        let o: Object = serde_json::from_str(text).unwrap();
        assert_eq!(serde_json::to_string(&o).unwrap(), text);
    }

    /// Un `t` repetido no es un objeto, y un campo conocido con el tipo equivocado tampoco: no
    /// acaba en `extra`.
    #[test]
    fn extra_does_not_swallow_errors() {
        assert!(
            serde_json::from_str::<Object>(
                r#"{"t":"project","t":"link","name":"x","state":"active","created":1,"changed":1}"#
            )
            .is_err()
        );
        assert!(
            serde_json::from_str::<Object>(
                r#"{"t":"project","name":5,"state":"active","created":1,"changed":1}"#
            )
            .is_err()
        );
        assert!(
            serde_json::from_str::<Object>(
                r#"{"t":"project","name":"x","state":"active","created":"ayer","changed":1}"#
            )
            .is_err()
        );
    }

    /// Un `null` explícito en un campo con valor por omisión no hace ilegible el objeto: se lee
    /// como si faltara.
    #[test]
    fn nulls_read_as_missing() {
        let o: Object = serde_json::from_str(
            r#"{"t":"objective","project":"p","name":"n","desc":null,"state":"pending",
                "note":null,"order":null,"created":1,"changed":1,"by":null}"#,
        )
        .unwrap();
        let Object::Objective(ob) = o else {
            panic!("{o:?}")
        };
        assert_eq!(
            (ob.desc.as_str(), ob.note.as_str(), ob.by.as_str()),
            ("", "", "")
        );
        assert_eq!(ob.order, 0.0);
        let o: Object = serde_json::from_str(
            r#"{"t":"link","objective":"o","session":{"machine":"m","id":"ab12cd34","name":null},
                "plan":{"title":null,"done":null,"total":null,"active":null,"blocked":null,"failed":null},
                "copied":null,"closed":null,"created":1,"changed":1,"by":"web"}"#,
        )
        .unwrap();
        let Object::Link(link) = o else {
            panic!("{o:?}")
        };
        assert_eq!(link.session.name, "");
        assert_eq!(link.copied, 0);
        let plan = link.plan.unwrap();
        assert_eq!(
            (
                plan.done,
                plan.total,
                plan.active,
                plan.blocked,
                plan.failed
            ),
            (0, 0, 0, 0, 0)
        );
        let o: Object = serde_json::from_str(
            r#"{"t":"project","name":"x","desc":null,"state":"active","created":1,"changed":1}"#,
        )
        .unwrap();
        assert!(matches!(o, Object::Project(p) if p.desc.is_empty()));
        // Los que no tienen valor por omisión siguen siendo obligatorios.
        assert!(
            serde_json::from_str::<Object>(
                r#"{"t":"project","name":null,"state":"active","created":1,"changed":1}"#
            )
            .is_err()
        );
    }

    fn objective(order: f64) -> Object {
        Object::Objective(Objective {
            project: "p".into(),
            name: "n".into(),
            desc: String::new(),
            state: ObjectiveState::Pending,
            note: String::new(),
            order,
            created: 1,
            changed: 1,
            by: "web".into(),
            extra: Map::new(),
        })
    }

    /// Un `order` que no es un número finito no se escribe: JSON no tiene `NaN` ni infinitos y
    /// `serde_json` los escribiría como `null`.
    #[test]
    fn order_must_be_finite() {
        let key = seq_key();
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert!(serde_json::to_string(&objective(bad)).is_err(), "{bad}");
            assert!(
                matches!(
                    seal_object(&key, ID, &objective(bad)),
                    Err(BlobError::BadObject(_))
                ),
                "{bad}"
            );
        }
        let blob = seal_object(&key, ID, &objective(-2.5)).unwrap();
        assert_eq!(open_object(&key, ID, &blob).unwrap(), objective(-2.5));
    }

    /// Los campos conocidos de cada tipo, tal y como salen al escribirlo: `clean_extra` los
    /// necesita al día.
    #[test]
    fn known_fields_are_up_to_date() {
        fn keys(v: &Value) -> Vec<String> {
            let mut k: Vec<_> = v.as_object().unwrap().keys().cloned().collect();
            k.sort();
            k
        }
        fn want(fields: &[&str], tagged: bool) -> Vec<String> {
            let mut k: Vec<String> = fields.iter().map(|f| f.to_string()).collect();
            if tagged {
                k.push("t".into());
            }
            k.sort();
            k
        }
        let project = Object::Project(Project {
            name: "x".into(),
            desc: String::new(),
            state: ProjectState::Active,
            created: 1,
            changed: 1,
            by: String::new(),
            extra: Map::new(),
        });
        let v = serde_json::to_value(&project).unwrap();
        assert_eq!(keys(&v), want(Project::FIELDS, true));
        let v = serde_json::to_value(objective(1.0)).unwrap();
        assert_eq!(keys(&v), want(Objective::FIELDS, true));
        let link: Object = serde_json::from_str(
            r#"{"t":"link","objective":"o","session":{"machine":"m","id":"i","name":"n"},
                "plan":{"title":"x","done":1,"total":1,"active":0,"blocked":0,"failed":0},
                "copied":1,"closed":null,"created":1,"changed":1,"by":"web"}"#,
        )
        .unwrap();
        let v = serde_json::to_value(&link).unwrap();
        assert_eq!(keys(&v), want(Link::FIELDS, true));
        assert_eq!(keys(&v["session"]), want(LinkSession::FIELDS, false));
        assert_eq!(keys(&v["plan"]), want(PlanSummary::FIELDS, false));
    }

    /// Aunque alguien meta en `extra` lo que mande un cliente, al cifrar no sale un JSON con
    /// claves repetidas: `seal_object` quita de `extra` `t` y los campos conocidos.
    #[test]
    fn sealing_cleans_extra() {
        let key = seq_key();
        let mut extra = Map::new();
        extra.insert("t".into(), "link".into());
        extra.insert("name".into(), "otro".into());
        extra.insert("color".into(), "verde".into());
        let p = Object::Project(Project {
            name: "Casa".into(),
            desc: String::new(),
            state: ProjectState::Active,
            created: 1,
            changed: 1,
            by: "app".into(),
            extra,
        });
        let text = open(&key, ID, &seal_object(&key, ID, &p).unwrap()).unwrap();
        let text = String::from_utf8(text).unwrap();
        assert_eq!(text.matches(r#""t":"#).count(), 1, "{text}");
        assert_eq!(text.matches(r#""name":"#).count(), 1, "{text}");
        let v: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(
            (&v["t"], &v["name"], &v["color"]),
            (&"project".into(), &"Casa".into(), &"verde".into())
        );

        // También dentro de `session` y `plan` (ahí `t` no es de nadie y se queda).
        let mut link: Object = serde_json::from_str(
            r#"{"t":"link","objective":"o","session":{"machine":"m","id":"i","name":"n"},
                "plan":{"title":"x","done":1,"total":1,"active":0,"blocked":0,"failed":0},
                "copied":1,"closed":null,"created":1,"changed":1,"by":"web"}"#,
        )
        .unwrap();
        let Object::Link(l) = &mut link else {
            unreachable!()
        };
        l.extra.insert("objective".into(), "otro".into());
        l.session.extra.insert("machine".into(), "otra".into());
        l.session.extra.insert("t".into(), "x".into());
        l.plan
            .as_mut()
            .unwrap()
            .extra
            .insert("done".into(), 9.into());
        let mut clean = link.clone();
        clean.clean_extra();
        let Object::Link(c) = &clean else {
            unreachable!()
        };
        assert!(c.extra.is_empty());
        assert_eq!(
            Value::Object(c.session.extra.clone()),
            serde_json::json!({"t": "x"})
        );
        assert!(c.plan.as_ref().unwrap().extra.is_empty());
        let back = open_object(&key, ID, &seal_object(&key, ID, &link).unwrap()).unwrap();
        assert_eq!(back, clean);
        // `Unknown` no se cifra.
        assert!(matches!(
            seal_object(&key, ID, &Object::Unknown),
            Err(BlobError::BadObject(_))
        ));
    }

    /// Un tipo que no se conoce se lee como `Unknown` (y no se puede volver a escribir).
    #[test]
    fn unknown_types_are_skipped() {
        let o: Object = serde_json::from_str(
            r#"{"t":"milestone","name":"x","when":{"at":5,"list":[1,2]},"created":1}"#,
        )
        .unwrap();
        assert_eq!(o, Object::Unknown);
        assert!(serde_json::to_string(&Object::Unknown).is_err());
        // Sin `t` sí es un error: eso no es un objeto.
        assert!(serde_json::from_str::<Object>(r#"{"name":"x"}"#).is_err());
    }

    /// Un estado que no se conoce no rompe el objeto y se vuelve a escribir igual.
    #[test]
    fn unknown_states_are_kept() {
        let text = r#"{"t":"project","name":"Casa","desc":"","state":"paused","created":1,"changed":2,"by":"app"}"#;
        let o: Object = serde_json::from_str(text).unwrap();
        let Object::Project(p) = &o else {
            panic!("{o:?}")
        };
        assert_eq!(p.state, ProjectState::Other("paused".into()));
        assert_eq!(serde_json::to_string(&o).unwrap(), text);
        let o: Object = serde_json::from_str(
            r#"{"t":"objective","project":"p","name":"n","state":"waiting","created":1,"changed":1}"#,
        )
        .unwrap();
        let Object::Objective(ob) = o else {
            panic!("{o:?}")
        };
        assert_eq!(ob.state, ObjectiveState::Other("waiting".into()));
    }

    /// Se escriben como dice el contrato: `t` delante, los estados en minúsculas y `plan` y
    /// `closed` como `null` si no hay.
    #[test]
    fn objects_write_as_the_contract() {
        let link = Object::Link(Link {
            objective: "o1".into(),
            session: LinkSession {
                machine: "m1".into(),
                id: "ab12cd34".into(),
                name: "portatil/obras".into(),
                extra: Map::new(),
            },
            plan: None,
            copied: 0,
            closed: None,
            created: 1,
            changed: 2,
            by: "Cuco".into(),
            extra: Map::new(),
        });
        let v = serde_json::to_value(&link).unwrap();
        assert_eq!(
            v,
            serde_json::json!({"t":"link","objective":"o1",
                "session":{"machine":"m1","id":"ab12cd34","name":"portatil/obras"},
                "plan":null,"copied":0,"closed":null,"created":1,"changed":2,"by":"Cuco"})
        );
        assert!(
            serde_json::to_string(&link)
                .unwrap()
                .starts_with(r#"{"t":"link""#)
        );
        assert_eq!(serde_json::from_value::<Object>(v).unwrap(), link);
        let with_plan = r#"{"t":"link","objective":"o1","session":{"machine":"m1","id":"ab12cd34","name":"n"},"plan":{"title":null,"done":3,"total":7,"active":1,"blocked":0,"failed":0},"copied":5,"closed":9,"created":1,"changed":2,"by":"web"}"#;
        let o: Object = serde_json::from_str(with_plan).unwrap();
        assert_eq!(serde_json::to_string(&o).unwrap(), with_plan);
        let states = [
            (ObjectiveState::Pending, "pending"),
            (ObjectiveState::Doing, "doing"),
            (ObjectiveState::Blocked, "blocked"),
            (ObjectiveState::Done, "done"),
            (ObjectiveState::Dropped, "dropped"),
        ];
        for (s, name) in states {
            assert_eq!(serde_json::to_value(&s).unwrap(), name);
        }
        assert_eq!(
            serde_json::to_value(ProjectState::Archived).unwrap(),
            "archived"
        );
    }

    /// Vectores para la web y la app: blobs con clave, nonce, id y texto fijos, y blobs que no se
    /// deben descifrar. Se guardan en `tests/data/board_vectors.json` (la web y la app llevan una
    /// copia). Con `NIDO_WRITE_VECTORS=1` se regenera; si no, tiene que estar, el código tiene
    /// que seguir dando lo mismo y cada caso del archivo se comprueba como lo harán ellas.
    #[test]
    fn vectors() {
        let project = r#"{"t":"project","name":"Reforma del baño","desc":"Azulejos, ducha y la luz del espejo","state":"active","created":1763300000000,"changed":1763300000000,"by":"app"}"#;
        let objective = r#"{"t":"objective","project":"0123456789abcdef0123456789abcdef","name":"Elegir azulejos","desc":"Muestras de la tienda, antes del viernes","state":"doing","note":"Quedan dos por decidir: ¿mate o brillo?","order":1.5,"created":1763300000000,"changed":1763386400000,"by":"web"}"#;
        let nonce1: [u8; 12] = std::array::from_fn(|i| i as u8);
        let nonce2: [u8; 12] = std::array::from_fn(|i| 0xa0 + i as u8);
        let cases = [
            ("proyecto", seq_key(), ID, nonce1, project),
            (
                "objetivo",
                [0x42; 32],
                "fedcba9876543210fedcba9876543210",
                nonce2,
                objective,
            ),
        ];
        let mut ok = Vec::new();
        for (name, key, id, nonce, plain) in &cases {
            // El texto es un objeto de verdad.
            let o: Object = serde_json::from_str(plain).unwrap();
            assert!(!matches!(o, Object::Unknown), "{name}");
            let blob = seal_with_nonce(key, id, nonce, plain.as_bytes()).unwrap();
            assert_eq!(open(key, id, &blob).unwrap(), plain.as_bytes());
            ok.push(serde_json::json!({
                "name": name,
                "key": B64.encode(key),
                "kid": kid(key),
                "id": id,
                "aad": format!("nido-board/1/{id}"),
                "nonce": hex(nonce),
                "plaintext": plain,
                "blob": blob,
            }));
        }
        // Los que no se deben descifrar, todos a partir del primero.
        let (key, id) = (seq_key(), ID);
        let blob = ok[0]["blob"].as_str().unwrap().to_string();
        let raw = B64.decode(&blob).unwrap();
        let mut version = raw.clone();
        version[0] = 2;
        let mut tag = raw.clone();
        *tag.last_mut().unwrap() ^= 0x01;
        let mut body = raw.clone();
        body[13] ^= 0x80;
        // El mismo blob en base64 URL (tiene `+` y `/`), y uno de 40 bytes (con `==`) sin relleno.
        use base64::engine::general_purpose::{STANDARD_NO_PAD, URL_SAFE};
        let url = URL_SAFE.encode(&raw);
        assert!(blob.contains('+') && blob.contains('/') && url != blob);
        let nonce3: [u8; 12] = std::array::from_fn(|i| 0x30 + i as u8);
        let padded = seal_with_nonce(&key, id, &nonce3, b"sin relleno").unwrap();
        assert!(padded.ends_with("=="), "{padded}");
        assert_eq!(open(&key, id, &padded).unwrap(), b"sin relleno");
        let no_pad = STANDARD_NO_PAD.encode(B64.decode(&padded).unwrap());
        let bad = [
            (
                "otro id",
                key,
                "0123456789abcdef0123456789abcdee",
                blob.clone(),
            ),
            ("otra clave", [0u8; 32], id, blob.clone()),
            ("versión 2", key, id, B64.encode(&version)),
            ("etiqueta tocada", key, id, B64.encode(&tag)),
            ("cifrado tocado", key, id, B64.encode(&body)),
            ("corto", key, id, B64.encode(&raw[..28])),
            ("base64url", key, id, url),
            ("sin relleno", key, id, no_pad),
        ];
        let bad: Vec<_> = bad
            .iter()
            .map(|(name, key, id, blob)| {
                assert!(open(key, id, blob).is_err(), "{name}");
                serde_json::json!({"name": name, "key": B64.encode(key), "id": id, "blob": blob})
            })
            .collect();
        let v = serde_json::json!({
            "about": "nido: blobs del tablero. blob = base64(0x01 || nonce(12) || AES-256-GCM(key, nonce, plaintext, aad)), con la etiqueta de 16 bytes al final; aad = \"nido-board/1/\" || id en ASCII; kid = hex(SHA-256(\"nido-board-kid\" || key))[0..16]. key y blob en base64 estándar con relleno (ni base64url ni sin relleno: se rechazan); nonce en hex; plaintext en UTF-8. Antes de descifrar se rechaza un blob de más de 11000 caracteres, de menos de 29 bytes o cuyo primer byte no sea 0x01 (la etiqueta no lo cubre). Los de ok se descifran a su plaintext; los de bad no se deben descifrar.",
            "ok": ok,
            "bad": bad,
        });
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/board_vectors.json");
        if std::env::var_os("NIDO_WRITE_VECTORS").is_some() {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, serde_json::to_string_pretty(&v).unwrap() + "\n").unwrap();
            return;
        }
        let text = std::fs::read_to_string(&path).unwrap_or_else(|e| {
            panic!(
                "falta {}: genéralo con NIDO_WRITE_VECTORS=1 ({e})",
                path.display()
            )
        });
        let file: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(file, v, "vectores de {}", path.display());
        // Y lo que hay en el archivo, leído como lo leerán la web y la app.
        let key_of = |c: &Value| -> [u8; 32] {
            B64.decode(c["key"].as_str().unwrap())
                .unwrap()
                .try_into()
                .unwrap()
        };
        let s = |c: &Value, k: &str| c[k].as_str().unwrap().to_string();
        for c in file["ok"].as_array().unwrap() {
            let key = key_of(c);
            let nonce: [u8; 12] = unhex(&s(c, "nonce")).try_into().unwrap();
            let (id, plain) = (s(c, "id"), s(c, "plaintext"));
            assert_eq!(
                seal_with_nonce(&key, &id, &nonce, plain.as_bytes()).unwrap(),
                s(c, "blob"),
                "{}",
                c["name"]
            );
            assert_eq!(open(&key, &id, &s(c, "blob")).unwrap(), plain.as_bytes());
            assert_eq!(kid(&key), s(c, "kid"));
        }
        for c in file["bad"].as_array().unwrap() {
            assert!(
                open(&key_of(c), &s(c, "id"), &s(c, "blob")).is_err(),
                "{}",
                c["name"]
            );
        }
    }

    // ---------- La copia del tablero ----------

    use crate::group::BoardKey;
    use crate::hub::fake;

    /// Lo que se espera entre dos escrituras de la resubida en las pruebas.
    const GAP: Duration = Duration::from_millis(60);

    const K_OLD: [u8; 32] = [0x11; 32];
    const K_CUR: [u8; 32] = [0x22; 32];

    fn bid(n: u8) -> String {
        format!("{n:032x}")
    }

    /// El llavero: `K_OLD` retirada y `K_CUR` la actual.
    fn keyring() -> Keyring {
        let mut old = BoardKey::from_raw(&K_OLD, 1);
        old.retired = Some(2);
        Keyring {
            keys: vec![old, BoardKey::from_raw(&K_CUR, 3)],
        }
    }

    fn project(name: &str) -> Object {
        Object::Project(Project {
            name: name.into(),
            desc: String::new(),
            state: ProjectState::Active,
            created: 1,
            changed: 1,
            by: "web".into(),
            extra: Map::new(),
        })
    }

    fn goal(note: &str) -> Object {
        let mut extra = Map::new();
        extra.insert("color".into(), "verde".into());
        Object::Objective(Objective {
            project: bid(1),
            name: "Azulejos".into(),
            desc: String::new(),
            state: ObjectiveState::Pending,
            note: note.into(),
            order: 1.0,
            created: 1,
            changed: 1,
            by: "web".into(),
            extra,
        })
    }

    /// `obj` cifrado para `id` con `key`: su `kid` y su blob.
    fn sealed(key: &[u8; 32], id: &str, obj: &Object) -> (String, String) {
        (kid(key), seal_object(key, id, obj).unwrap())
    }

    /// Lo que hay en el hub de mentira para `id`, descifrado con `key`.
    fn on_hub(board: &Mutex<fake::Board>, key: &[u8; 32], id: &str) -> Object {
        let b = board.lock().unwrap();
        let item = &b.items[id];
        open_object(key, id, item.blob.as_deref().unwrap()).unwrap()
    }

    struct Rig {
        dir: tempfile::TempDir,
        board: Arc<Mutex<fake::Board>>,
        rec: fake::Recorder,
        hub: Arc<Hub>,
        cache: Arc<BoardCache>,
    }

    /// Un nido conectado a un hub de mentira con el tablero `b`, y su copia (vacía).
    async fn rig(b: fake::Board) -> Rig {
        let dir = tempfile::tempdir().unwrap();
        let board = Arc::new(Mutex::new(b));
        let rec = fake::board(board.clone()).await;
        let hub = fake::nido(dir.path(), rec.addr).await;
        let cache = BoardCache::load_with(dir.path().join("data/board.json"), hub.clone(), GAP);
        Rig {
            dir,
            board,
            rec,
            hub,
            cache,
        }
    }

    impl Rig {
        fn write(&self, key: &[u8; 32], id: &str, obj: &Object) -> BoardItem {
            let (k, b) = sealed(key, id, obj);
            self.board.lock().unwrap().write(id, Some((&k, &b)))
        }

        fn writes(&self) -> Vec<(String, String, i64)> {
            self.board
                .lock()
                .unwrap()
                .writes
                .iter()
                .map(|w| (w.1.clone(), w.2.clone(), w.3))
                .collect()
        }
    }

    /// Espera (como mucho 10 s) a que la copia cumpla `ok`.
    async fn until(cache: &BoardCache, ok: impl Fn(&BoardCache) -> bool) {
        let mut rev = cache.subscribe();
        let wait = async {
            while !ok(cache) {
                let _ = tokio::time::timeout(Duration::from_millis(50), rev.changed()).await;
            }
        };
        tokio::time::timeout(Duration::from_secs(10), wait)
            .await
            .expect("la copia no llegó a lo esperado");
    }

    /// Página a página con el cursor del hub, y al acabar se guarda `next`, también en el disco.
    /// La siguiente empieza desde ahí, y lo del solape que ya se tiene no cuenta como cambio.
    #[tokio::test]
    async fn sync_follows_the_pages_and_keeps_next() {
        let r = rig(fake::Board {
            page: 2,
            ..Default::default()
        })
        .await;
        for n in 1..=5 {
            r.write(&K_CUR, &bid(n), &project(&format!("p{n}")));
        }
        assert!(r.cache.sync().await.unwrap());
        assert_eq!(r.cache.ids(), (1..=5).map(bid).collect::<Vec<_>>());
        let clock = r.board.lock().unwrap().clock;
        assert_eq!(r.cache.next(), clock);
        let gets = r.board.lock().unwrap().gets.clone();
        assert_eq!(gets.len(), 3, "{gets:?}");
        assert!(gets.iter().all(|g| g.0 == 0), "el mismo since: {gets:?}");
        assert_eq!(gets[0].1, None);
        let items: Vec<BoardItem> = r.board.lock().unwrap().items.values().cloned().collect();
        assert_eq!(
            gets[1].1.as_deref(),
            Some(format!("{}:{}", items[1].at, items[1].id).as_str())
        );
        assert_eq!(
            gets[2].1.as_deref(),
            Some(format!("{}:{}", items[3].at, items[3].id).as_str())
        );

        // En el disco, lo mismo.
        let again = BoardCache::load_with(r.cache.path.clone(), r.hub.clone(), GAP);
        assert_eq!(again.ids(), r.cache.ids());
        assert_eq!(again.next(), clock);
        assert_eq!(again.item(&bid(3)), r.cache.item(&bid(3)));

        // La siguiente, desde `next`: lo del solape (aquí, todo) ya se tiene.
        let before = r.board.lock().unwrap().gets.len();
        assert!(!r.cache.sync().await.unwrap());
        let first = r.board.lock().unwrap().gets[before].clone();
        assert_eq!(first, (clock, None));
        assert_eq!(r.cache.next(), clock);
        // Algo nuevo.
        let six = r.write(&K_CUR, &bid(6), &project("p6"));
        assert!(r.cache.sync().await.unwrap());
        assert_eq!(r.cache.item(&bid(6)).unwrap().v, six.v);
        assert_eq!(r.cache.next(), six.at);
    }

    /// Con `reset` se vacía la copia y se empieza desde 0: lo que ya no está en el hub (una
    /// lápida caducada) desaparece.
    #[tokio::test]
    async fn a_reset_empties_the_copy_and_starts_over() {
        let r = rig(fake::Board::default()).await;
        r.write(&K_CUR, &bid(1), &project("uno"));
        r.write(&K_CUR, &bid(2), &project("dos"));
        r.cache.sync().await.unwrap();
        let old_next = r.cache.next();
        {
            let mut b = r.board.lock().unwrap();
            b.items.remove(&bid(2));
            b.reset = true;
        }
        let three = r.write(&K_CUR, &bid(3), &project("tres"));
        assert!(r.cache.sync().await.unwrap());
        assert_eq!(r.cache.ids(), vec![bid(1), bid(3)]);
        assert_eq!(r.cache.next(), three.at);
        let gets = r.board.lock().unwrap().gets.clone();
        assert_eq!(&gets[gets.len() - 2..], &[(old_next, None), (0, None)]);
    }

    /// Un 409 trae el objeto como está: se vuelve a aplicar el cambio sobre él (con lo que
    /// cambió el otro y lo que no conocemos, en `extra`) y se escribe con la clave actual.
    #[tokio::test]
    async fn a_conflict_is_reapplied_on_the_current_object() {
        let r = rig(fake::Board::default()).await;
        let id = bid(7);
        r.write(&K_OLD, &id, &goal(""));
        r.cache.sync().await.unwrap();
        let theirs = sealed(&K_CUR, &id, &goal("del otro"));
        r.board.lock().unwrap().clash(&id, Some(theirs));
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let out = r
            .cache
            .update(&keyring(), &id, |o| {
                calls.fetch_add(1, Ordering::Relaxed);
                if let Object::Objective(g) = o {
                    g.state = ObjectiveState::Done;
                }
            })
            .await
            .unwrap();
        assert_eq!(calls.load(Ordering::Relaxed), 2);
        let now = on_hub(&r.board, &K_CUR, &id);
        assert_eq!(out, now);
        let Object::Objective(g) = now else {
            panic!("{now:?}")
        };
        assert_eq!(g.state, ObjectiveState::Done);
        assert_eq!(g.note, "del otro");
        assert_eq!(g.extra["color"], "verde");
        assert_eq!(
            r.writes(),
            vec![("put".into(), id.clone(), 1), ("put".into(), id.clone(), 2)]
        );
        let stored = r.cache.item(&id).unwrap();
        assert_eq!((stored.v, stored.kid.as_str()), (3, kid(&K_CUR).as_str()));
    }

    /// Tras `MAX_TRIES` choques seguidos, se deja con el error; la copia se queda con lo último
    /// que dijo el hub.
    #[tokio::test]
    async fn three_conflicts_give_up() {
        let r = rig(fake::Board::default()).await;
        let id = bid(7);
        r.write(&K_CUR, &id, &goal(""));
        r.cache.sync().await.unwrap();
        for n in 0..MAX_TRIES {
            let theirs = sealed(&K_CUR, &id, &goal(&format!("otro {n}")));
            r.board.lock().unwrap().clash(&id, Some(theirs));
        }
        let e = r
            .cache
            .update(&keyring(), &id, |o| {
                if let Object::Objective(g) = o {
                    g.state = ObjectiveState::Done;
                }
            })
            .await
            .unwrap_err();
        let EditError::Board(BoardError::Conflict(cur)) = e else {
            panic!("{e:?}")
        };
        assert_eq!(cur.v, 1 + MAX_TRIES as i64);
        assert_eq!(r.writes().len(), MAX_TRIES);
        assert_eq!(r.cache.item(&id).unwrap().v, cur.v);
    }

    /// Un borrado que otro ya hizo (o uno repetido tras un corte): 409 con la lápida, y cuenta
    /// como hecho. Uno ya borrado en la copia no pide nada.
    #[tokio::test]
    async fn a_repeated_delete_counts_as_done() {
        let r = rig(fake::Board::default()).await;
        let id = bid(9);
        r.write(&K_CUR, &id, &project("x"));
        r.cache.sync().await.unwrap();
        r.board.lock().unwrap().write(&id, None);
        r.cache.remove(&id).await.unwrap();
        let s = r.cache.item(&id).unwrap();
        assert!(s.gone && s.blob.is_none(), "{s:?}");
        assert_eq!(s.v, 2);
        r.cache.remove(&id).await.unwrap();
        assert_eq!(r.writes(), vec![("del".into(), id.clone(), 1)]);
        // Uno que el hub ni tiene (409 con `v: 0`): borrado también en la copia.
        let lost = bid(11);
        r.write(&K_CUR, &lost, &project("perdido"));
        r.cache.sync().await.unwrap();
        r.board.lock().unwrap().items.remove(&lost);
        r.cache.remove(&lost).await.unwrap();
        assert!(r.cache.item(&lost).unwrap().gone);
        assert!(
            r.cache
                .decrypted(&keyring())
                .objects
                .iter()
                .all(|e| e.id != lost)
        );
        // Uno vivo: se borra.
        let other = bid(10);
        r.write(&K_CUR, &other, &project("y"));
        r.cache.sync().await.unwrap();
        r.cache.remove(&other).await.unwrap();
        assert!(r.board.lock().unwrap().items[&other].gone);
        assert!(r.cache.item(&other).unwrap().gone);
    }

    /// Cada objeto se descifra con la clave de su `kid` (nunca probando las demás). Lo que no se
    /// lee se lista aparte, y no se reescribe.
    #[tokio::test]
    async fn unreadable_objects_are_listed_and_never_rewritten() {
        let r = rig(fake::Board::default()).await;
        let (ok_old, ok_cur, unknown, no_key, wrong_kid, broken, gone) =
            (bid(1), bid(2), bid(3), bid(4), bid(5), bid(6), bid(7));
        r.write(&K_OLD, &ok_old, &project("vieja"));
        r.write(&K_CUR, &ok_cur, &project("actual"));
        let future = seal(&K_CUR, &unknown, br#"{"t":"milestone","name":"x"}"#).unwrap();
        r.board
            .lock()
            .unwrap()
            .write(&unknown, Some((&kid(&K_CUR), &future)));
        r.write(&[0x33; 32], &no_key, &project("ajena"));
        // Cifrado con la actual, pero con el `kid` de la vieja: no se prueba con otra.
        let blob = seal_object(&K_CUR, &wrong_kid, &project("mal")).unwrap();
        r.board
            .lock()
            .unwrap()
            .write(&wrong_kid, Some((&kid(&K_OLD), &blob)));
        r.board
            .lock()
            .unwrap()
            .write(&broken, Some((&kid(&K_CUR), "QUJD")));
        r.write(&K_CUR, &gone, &project("borrado"));
        r.board.lock().unwrap().write(&gone, None);
        r.cache.sync().await.unwrap();

        let d = r.cache.decrypted(&keyring());
        let ids: Vec<&str> = d.objects.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, vec![ok_old.as_str(), ok_cur.as_str()]);
        assert_eq!(d.objects[0].object, project("vieja"));
        assert_eq!(d.objects[1].v, 1);
        assert_eq!(
            d.unreadable,
            vec![
                (unknown.clone(), Unreadable::Unknown),
                (no_key.clone(), Unreadable::NoKey),
                (wrong_kid.clone(), Unreadable::Broken),
                (broken.clone(), Unreadable::Broken),
            ]
        );
        for id in [&unknown, &no_key, &wrong_kid, &broken] {
            let e = r.cache.update(&keyring(), id, |_| {}).await.unwrap_err();
            assert!(matches!(e, EditError::Unreadable(_)), "{id}: {e:?}");
        }
        let e = r.cache.update(&keyring(), &gone, |_| {}).await.unwrap_err();
        assert!(matches!(e, EditError::Missing), "{e:?}");
        assert!(r.writes().is_empty(), "{:?}", r.writes());
        // Sin clave actual no se escribe nada.
        let none = Keyring { keys: vec![] };
        let e = r.cache.update(&none, &ok_cur, |_| {}).await.unwrap_err();
        assert!(matches!(e, EditError::NoKey), "{e:?}");
    }

    /// Crear: id nuevo, `v = 0`, con la clave actual.
    #[tokio::test]
    async fn create_writes_with_the_current_key() {
        let r = rig(fake::Board::default()).await;
        let id = r.cache.create(&keyring(), &project("nuevo")).await.unwrap();
        assert!(is_id(&id), "{id}");
        assert_eq!(r.writes(), vec![("put".into(), id.clone(), 0)]);
        assert_eq!(on_hub(&r.board, &K_CUR, &id), project("nuevo"));
        let s = r.cache.item(&id).unwrap();
        assert_eq!((s.v, s.kid.as_str()), (1, kid(&K_CUR).as_str()));
    }

    /// La resubida (decisión 8): una lectura completa sin nada (ni lápidas) teniendo copia. Se
    /// sube con `v = 0` lo que se puede leer, con la clave actual y los bytes tal cual (también
    /// un tipo que no conocemos), a ritmo; un 409 es que otro se adelantó y cuenta como hecho.
    #[tokio::test]
    async fn the_copy_goes_back_up_to_an_empty_hub() {
        let r = rig(fake::Board::default()).await;
        let (old, cur, unknown, no_key, gone) = (bid(1), bid(2), bid(3), bid(4), bid(5));
        r.write(&K_OLD, &old, &project("vieja"));
        r.write(&K_CUR, &cur, &project("actual"));
        let future = br#"{"t":"milestone","name":"x"}"#;
        let fb = seal(&K_CUR, &unknown, future).unwrap();
        r.board
            .lock()
            .unwrap()
            .write(&unknown, Some((&kid(&K_CUR), &fb)));
        r.write(&[0x33; 32], &no_key, &project("ajena"));
        r.write(&K_CUR, &gone, &project("borrado"));
        r.board.lock().unwrap().write(&gone, None);
        r.cache.sync().await.unwrap();

        // Con objetos en el hub, nada.
        r.cache.probe(&keyring()).await.unwrap();
        assert!(r.writes().is_empty());

        // El hub se queda sin nada; otro nido de la casa se adelanta con `cur`.
        let theirs = sealed(&K_CUR, &cur, &project("de otro"));
        {
            let mut b = r.board.lock().unwrap();
            b.items.clear();
            b.clash(&cur, Some(theirs));
        }
        r.cache.probe(&keyring()).await.unwrap();
        until(&r.cache, |c| c.pending_reupload() == 0).await;
        let w = r.board.lock().unwrap().writes.clone();
        let sent: Vec<(&str, &str, i64)> = w
            .iter()
            .map(|w| (w.1.as_str(), w.2.as_str(), w.3))
            .collect();
        assert_eq!(
            sent,
            vec![
                ("put", old.as_str(), 0),
                ("put", cur.as_str(), 0),
                ("put", unknown.as_str(), 0)
            ]
        );
        for pair in w.windows(2) {
            let gap = pair[1].0 - pair[0].0;
            assert!(gap >= GAP - Duration::from_millis(5), "{gap:?}");
        }
        // La de la clave retirada, con la actual y lo mismo dentro.
        assert_eq!(on_hub(&r.board, &K_CUR, &old), project("vieja"));
        assert_eq!(r.board.lock().unwrap().items[&old].kid, kid(&K_CUR));
        // El tipo desconocido, byte a byte.
        let b = r.board.lock().unwrap().items[&unknown].clone();
        assert_eq!(
            open(&K_CUR, &unknown, b.blob.as_deref().unwrap()).unwrap(),
            future
        );
        // La del 409 se queda como la dejó el otro, sin volver a intentarlo.
        assert_eq!(on_hub(&r.board, &K_CUR, &cur), project("de otro"));
        assert_eq!(r.cache.item(&cur).unwrap().v, 1);
        // Lo que no se podía subir (sin clave) se queda en la copia; la lápida, no.
        assert_eq!(
            r.cache.ids(),
            vec![old.clone(), cur.clone(), unknown.clone(), no_key.clone()]
        );
        assert_eq!(r.cache.item(&no_key).unwrap().v, 1);

        // Ya hay objetos: otra vez, nada.
        r.cache.sync().await.unwrap();
        r.cache.probe(&keyring()).await.unwrap();
        tokio::time::sleep(GAP * 3).await;
        assert_eq!(r.board.lock().unwrap().writes.len(), 3);
    }

    /// Sin resubida si el hub tiene algo (aunque sea una lápida), ni sin copia propia.
    #[tokio::test]
    async fn no_reupload_without_an_empty_hub_and_an_own_copy() {
        let r = rig(fake::Board::default()).await;
        r.write(&K_CUR, &bid(1), &project("uno"));
        r.cache.sync().await.unwrap();
        {
            let mut b = r.board.lock().unwrap();
            b.items.clear();
            b.write(&bid(2), None);
        }
        r.cache.probe(&keyring()).await.unwrap();
        tokio::time::sleep(GAP * 3).await;
        assert!(r.writes().is_empty());
        assert_eq!(r.cache.pending_reupload(), 0);

        // Sin copia: ni se mira.
        let empty = rig(fake::Board::default()).await;
        empty.cache.probe(&keyring()).await.unwrap();
        tokio::time::sleep(GAP * 3).await;
        assert!(empty.writes().is_empty());
        assert!(empty.board.lock().unwrap().gets.is_empty());
    }

    /// Sin clave actual no se resube ni se toca la copia: se vuelve a mirar en el próximo sondeo.
    #[tokio::test]
    async fn no_reupload_without_a_key() {
        let r = rig(fake::Board::default()).await;
        r.write(&K_CUR, &bid(1), &project("uno"));
        r.cache.sync().await.unwrap();
        let next = r.cache.next();
        r.board.lock().unwrap().items.clear();
        r.cache.probe(&Keyring { keys: vec![] }).await.unwrap();
        tokio::time::sleep(GAP * 3).await;
        assert!(r.writes().is_empty());
        assert_eq!(r.cache.ids(), vec![bid(1)]);
        assert_eq!(r.cache.pending_reupload(), 0);
        assert_eq!(r.cache.next(), next);
        // Con la clave, sí.
        r.cache.probe(&keyring()).await.unwrap();
        until(&r.cache, |c| c.pending_reupload() == 0).await;
        assert_eq!(on_hub(&r.board, &K_CUR, &bid(1)), project("uno"));
    }

    /// Una resubida cortada (se cae la conexión) sigue al volver el hub, desde donde se quedó.
    #[tokio::test]
    async fn a_cut_reupload_goes_on_after_reconnecting() {
        let dir = tempfile::tempdir().unwrap();
        let board = Arc::new(Mutex::new(fake::Board::default()));
        let rec = fake::board(board.clone()).await;
        let hub = fake::nido(dir.path(), rec.addr).await;
        // Lento, para que la conexión se caiga entre la primera escritura y la segunda.
        let cache = BoardCache::load_with(
            dir.path().join("data/board.json"),
            hub.clone(),
            Duration::from_millis(800),
        );
        for n in 1..=3 {
            let (k, b) = sealed(&K_CUR, &bid(n), &project(&format!("p{n}")));
            board.lock().unwrap().write(&bid(n), Some((&k, &b)));
        }
        cache.sync().await.unwrap();
        board.lock().unwrap().items.clear();
        let mut status = hub.status.subscribe();
        spawn(cache.clone(), keyring);
        let first = async {
            while board.lock().unwrap().writes.is_empty() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        };
        tokio::time::timeout(Duration::from_secs(10), first)
            .await
            .expect("no empezó la resubida");
        rec.tell(fake::Tell::Hangup);
        tokio::time::timeout(Duration::from_secs(10), status.wait_for(|s| !s.connected))
            .await
            .unwrap()
            .unwrap();
        // Lo que falta, en el disco.
        let saved = BoardCache::load_with(cache.path.clone(), hub.clone(), GAP);
        let left = saved.pending_reupload();
        assert!(left >= 1, "{left}");
        until(&cache, |c| c.pending_reupload() == 0).await;
        for n in 1..=3 {
            assert_eq!(on_hub(&board, &K_CUR, &bid(n)), project(&format!("p{n}")));
        }
        let w = board.lock().unwrap().writes.clone();
        assert!(
            w.windows(2)
                .any(|p| p[1].0 - p[0].0 > Duration::from_secs(1)),
            "sin corte"
        );
    }

    /// Con el freno del hub (429), la resubida espera y sigue con el mismo.
    #[tokio::test]
    async fn the_reupload_waits_out_the_brake() {
        let r = rig(fake::Board::default()).await;
        r.write(&K_CUR, &bid(1), &project("uno"));
        r.cache.sync().await.unwrap();
        {
            let mut b = r.board.lock().unwrap();
            b.items.clear();
            b.busy = 1;
        }
        r.cache.probe(&keyring()).await.unwrap();
        until(&r.cache, |c| c.pending_reupload() == 0).await;
        let w = r.board.lock().unwrap().writes.clone();
        assert_eq!(w.len(), 2);
        assert!(w[1].0 - w[0].0 >= GAP * BUSY_GAPS - Duration::from_millis(5));
        assert_eq!(on_hub(&r.board, &K_CUR, &bid(1)), project("uno"));
    }

    /// Con la conexión: lo primero, un `get`; con un aviso del hub, otra sincronización. Y al
    /// reconectar, otra vez un `get`.
    #[tokio::test]
    async fn the_notice_and_each_connection_bring_a_sync() {
        let mut r = rig(fake::Board::default()).await;
        r.write(&K_CUR, &bid(1), &project("uno"));
        spawn(r.cache.clone(), keyring);
        until(&r.cache, |c| c.item(&bid(1)).is_some()).await;
        // Al conectar, la sincronización y el sondeo del hub vacío (los dos, `get`).
        assert_eq!(r.rec.next("board").await["since"], 0);
        assert_eq!(r.rec.next("board").await["since"], 0);
        tokio::time::sleep(Duration::from_millis(100)).await;
        let gets = r.board.lock().unwrap().gets.len();
        assert_eq!(gets, 2);
        let two = r.write(&K_CUR, &bid(2), &project("dos"));
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            r.cache.item(&bid(2)).is_none(),
            "sin aviso no se sincroniza"
        );
        r.rec.tell(fake::Tell::Send(
            serde_json::json!({ "board": { "at": two.at } }),
        ));
        until(&r.cache, |c| c.item(&bid(2)).is_some()).await;
        assert_eq!(r.board.lock().unwrap().gets.len(), gets + 1);

        r.rec.tell(fake::Tell::Hangup);
        let mut status = r.hub.status.subscribe();
        tokio::time::timeout(Duration::from_secs(10), status.wait_for(|s| !s.connected))
            .await
            .unwrap()
            .unwrap();
        r.write(&K_CUR, &bid(3), &project("tres"));
        tokio::time::timeout(Duration::from_secs(10), status.wait_for(|s| s.connected))
            .await
            .unwrap()
            .unwrap();
        until(&r.cache, |c| c.item(&bid(3)).is_some()).await;
    }

    /// Un equipo que sale del grupo vacía su copia, en memoria y en el disco.
    #[tokio::test]
    async fn a_removed_nido_empties_its_copy() {
        let r = rig(fake::Board::default()).await;
        r.write(&K_CUR, &bid(1), &project("uno"));
        r.cache.sync().await.unwrap();
        assert!(r.cache.next() > 0);
        let group = crate::group::Group::load(r.dir.path().join("data/group.json"), [1u8; 32]);
        group.set_self("yo", "yo");
        group.add_member("otro", &B64.encode([2u8; 32]), "otro");
        group.ensure_board_key().unwrap();
        let cache = r.cache.clone();
        group.on_leave(move || cache.clear());
        // El otro cuenta que a este lo quitaron.
        let mut st = group.snapshot();
        for m in st.members.iter_mut().filter(|m| m.key == group.own_key) {
            m.removed = true;
            m.at += 1000;
        }
        assert!(group.merge(&st));
        assert!(group.board_keys().keys.is_empty());
        assert!(r.cache.ids().is_empty());
        assert_eq!(r.cache.next(), 0);
        let again = BoardCache::load_with(r.cache.path.clone(), r.hub.clone(), GAP);
        assert!(again.ids().is_empty());
        assert_eq!(again.next(), 0);
    }

    /// `board.json` solo para el usuario, como `group.json`.
    #[cfg(unix)]
    #[tokio::test]
    async fn the_board_file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let r = rig(fake::Board::default()).await;
        r.write(&K_CUR, &bid(1), &project("uno"));
        r.cache.sync().await.unwrap();
        let mode = std::fs::metadata(&r.cache.path)
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
