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
//! sincroniza por la conexión con el hub (`hub::Hub::board_get`…). Y la tarea del llavero
//! (`spawn_upkeep`): recifra con la clave actual lo cifrado con claves retiradas y quita del
//! llavero las que ya no usa nadie.

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

use crate::group::{Group, Keyring};
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
            // Sin su texto, que puede llevar trozos de lo descifrado.
            BlobError::BadObject(_) => write!(f, "objeto del tablero no válido"),
        }
    }
}

/// A mano: `BadObject` puede llevar trozos de lo descifrado (mensajes de `serde_json`), y no
/// tiene que salir en ningún registro ni en ningún `{:?}`.
impl std::fmt::Debug for BlobError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BlobError::TooBig(n) => f.debug_tuple("TooBig").field(n).finish(),
            BlobError::BadId => f.write_str("BadId"),
            BlobError::Random(e) => f.debug_tuple("Random").field(e).finish(),
            BlobError::BadBlob => f.write_str("BadBlob"),
            BlobError::Version(v) => f.debug_tuple("Version").field(v).finish(),
            BlobError::Open => f.write_str("Open"),
            BlobError::BadObject(_) => f.write_str("BadObject(..)"),
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
///
/// Para recifrar con otra clave no se usa: se recifran los bytes que da `open`, tal cual, con
/// `seal` (`BoardCache::recrypt`). Pasar por `Object` perdería un tipo que no conocemos
/// (`Unknown` no se escribe) y reescribiría distinto los que sí.
pub fn seal_object(key: &[u8; 32], id: &str, obj: &Object) -> Result<String, BlobError> {
    let mut obj = obj.clone();
    obj.clean_extra();
    let text = serde_json::to_vec(&obj).map_err(|e| BlobError::BadObject(e.to_string()))?;
    seal(key, id, &text)
}

/// Descifra y lee el objeto `id`. Para recifrar, `open` y `seal` con los bytes tal cual (ver
/// `seal_object`).
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
#[cfg(not(test))]
pub const SYNC_EVERY: Duration = Duration::from_secs(5 * 60);
#[cfg(test)]
pub const SYNC_EVERY: Duration = Duration::from_secs(1);
/// Lo que se espera entre dos escrituras de la resubida: 20 por minuto. El hub frena a 120 por
/// minuto y conexión y a 3000 por hora y cuenta, y la resubida tiene que dejar sitio a lo que se
/// edita a mano (1200 por hora).
pub const REUPLOAD_GAP: Duration = Duration::from_secs(3);
/// Con el freno del hub (429), la resubida espera esto en huecos de `REUPLOAD_GAP` (un minuto).
const BUSY_GAPS: u32 = 20;
/// Cuántas veces se intenta una escritura que choca (409) antes de dejarlo.
pub const MAX_TRIES: usize = 3;
/// Lo que espera la copia para ir al disco desde el primer cambio sin guardar: lo que cambie
/// mientras tanto va en la misma escritura (es entera, y con fsync).
#[cfg(not(test))]
const SAVE_GAP: Duration = Duration::from_secs(20);
#[cfg(test)]
const SAVE_GAP: Duration = Duration::from_millis(200);
/// Lo que se espera para confirmar que el hub está de verdad vacío antes de resubir: una cuenta
/// recién hecha o un hub recién arrancado pueden estar llenándose en ese momento.
#[cfg(not(test))]
const EMPTY_CONFIRM: Duration = Duration::from_secs(60);
#[cfg(test)]
const EMPTY_CONFIRM: Duration = Duration::from_millis(50);
/// Lo más que se lee en una sincronización: objetos y páginas. El hub no pasa de 2000 objetos
/// por cuenta (algo más al juntar cuentas); un hub que no para de dar páginas no deja a nido en
/// un bucle sin fin.
#[cfg(not(test))]
const MAX_READ: usize = 5000;
#[cfg(test)]
const MAX_READ: usize = 60;
/// Lo más que guarda la copia.
pub const MAX_COPY: usize = 5000;
/// Lo que se dice de una paginación del hub que no tiene sentido.
const BAD_PAGING: &str = "paginación no válida";
/// Cuántas veces puede cambiar la `ref` de las páginas en una lectura. Irse a otra cuenta es un
/// cambio (dos, si se vuelve a mover mientras se relee); más es un hub que contesta de cuentas
/// distintas sin parar (servidores que no se ponen de acuerdo). Entonces se deja esa
/// sincronización sin tocar la copia, y la próxima (aviso, tic o conexión) lo vuelve a intentar.
const MAX_REF_CHANGES: usize = 3;
/// Lo que se dice entonces.
const REF_FLAPPING: &str = "la cuenta del tablero cambia sin parar";

/// El llavero del grupo, que se pide cada vez que hace falta (cambia con las rotaciones).
pub type Keys = Arc<dyn Fn() -> Keyring + Send + Sync>;

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
    /// Los que se sabe que no están en el hub y no se van a subir (no se pudieron leer, o el hub
    /// no los quiso). Se enseñan como ilegibles; el próximo sondeo decide (`probe`).
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    missing: BTreeSet<String>,
    /// El hub del que es la copia (su URL): con otro, se vuelve a leer desde 0.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    hub: Option<String>,
    /// La cuenta de la que es `next` (su `ref`, como la dan las páginas): con otra, también.
    #[serde(default, rename = "ref", skip_serializing_if = "Option::is_none")]
    account_ref: Option<String>,
}

impl Copy {
    /// Junta un objeto que da el hub. Gana el de `v` mayor o marca más nueva: dentro de una
    /// cuenta, `v` solo sube; al pasar a otra cuenta (resubida, cuentas juntadas) la `v` puede
    /// volver a empezar, pero la marca es nueva. Lo que ya se tiene (mismo `v` y marca, como lo
    /// del solape) no cambia nada. Uno que faltaba por resubir, o que no estaba en el hub, ya
    /// está: se toma tal cual. Un id que no es un id se ignora, y uno nuevo no entra si la copia
    /// ya tiene `MAX_COPY`. Devuelve si cambió algo.
    fn apply(&mut self, item: BoardItem) -> bool {
        if !is_id(&item.id) {
            return false;
        }
        let pending = self.reupload.remove(&item.id);
        let missing = self.missing.remove(&item.id);
        let newer = match self.items.get(&item.id) {
            None => self.items.len() < MAX_COPY,
            Some(cur) => pending || missing || item.v > cur.v || item.at > cur.at,
        };
        if newer {
            let stored = Stored::from(&item);
            self.items.insert(item.id, stored);
        }
        newer || pending || missing
    }

    /// Deja `item` (una lápida) tal cual, aunque pierda frente a lo que hay: es lo que dice el
    /// hub ahora. Devuelve si cambió algo.
    fn bury(&mut self, item: &BoardItem) -> bool {
        let pending = self.reupload.remove(&item.id);
        let missing = self.missing.remove(&item.id);
        let stored = Stored::from(item);
        let changed = self.items.get(&item.id) != Some(&stored);
        if changed {
            self.items.insert(item.id.clone(), stored);
        }
        changed || pending || missing
    }

    /// Quita `id` del todo. Devuelve si estaba.
    fn forget(&mut self, id: &str) -> bool {
        self.reupload.remove(id);
        self.missing.remove(id);
        self.items.remove(id).is_some()
    }

    /// Si hay objetos propios (no lápidas) que se podrían volver a subir.
    fn has_objects(&self) -> bool {
        self.items.values().any(|s| !s.gone && s.blob.is_some())
    }
}

/// Por qué un objeto de la copia no se puede leer. No lleva nada de lo descifrado.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Unreadable {
    /// Su `kid` no está en el llavero.
    NoKey,
    /// El blob no se descifra con la clave de su `kid` (o no es un blob).
    Broken,
    /// Se descifra, pero no es un objeto.
    NotAnObject,
    /// Es un objeto de un tipo que no conocemos (de una versión futura).
    Unknown,
    /// No está en el hub y no se va a subir (ver `Copy::missing`).
    NotInHub,
}

impl std::fmt::Display for Unreadable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Unreadable::NoKey => "sin su clave",
            Unreadable::Broken => "no se descifra",
            Unreadable::NotAnObject => "no es un objeto",
            Unreadable::Unknown => "de un tipo que no se conoce",
            Unreadable::NotInHub => "no está en el hub",
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
            EditError::Blob(e) => {
                write!(f, "no se pudo cifrar el objeto ({})", blob_error_kind(e))
            }
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
///
/// Los cerrojos:
/// - `copy` (de `std`) solo se coge para leer o cambiar la copia en memoria, o clonarla para
///   guardarla: nunca a través de un `await`, ni para escribir en el disco o serializar.
/// - `disk` (de `std`) va alrededor de cada guardado (clonar, serializar y escribir), para que
///   vayan de uno en uno y uno tardío no pise con algo más viejo lo que guardó otro. Se coge
///   antes que `copy`, nunca al revés.
/// - `syncing` (de `tokio`): una lectura del hub a la vez (`sync`, y la completa de `prune`).
/// - `clear` se llama con el grupo cogido (`Group::on_leave`): coge `disk` y `copy`, y no usa el
///   grupo.
pub struct BoardCache {
    path: PathBuf,
    hub: Arc<Hub>,
    copy: Mutex<Copy>,
    disk: Mutex<()>,
    /// Si la copia cambió desde que se guardó: la guarda `saver`, como mucho una vez cada
    /// `SAVE_GAP`.
    dirty: AtomicBool,
    /// Despierta a la tarea que guarda.
    saver: Arc<tokio::sync::Notify>,
    /// Si ya está en marcha la tarea que guarda.
    saving: AtomicBool,
    syncing: tokio::sync::Mutex<()>,
    /// Sube al vaciar la copia: lo que estaba en marcha (sincronizar, resubir, escribir) ya no
    /// la toca.
    epoch: AtomicU64,
    /// Si hay una resubida en marcha.
    reuploading: AtomicBool,
    /// Lo que se espera entre dos escrituras de la resubida.
    gap: Duration,
    /// Que `spawn` mire cuanto antes si el hub se ha quedado sin tablero (una escritura vio que
    /// el hub no tiene un objeto de la copia).
    probe_wanted: tokio::sync::Notify,
    /// Cuándo fue la última escritura del recifrado: entre dos, `gap` (como la resubida).
    recrypt_last: Mutex<Option<tokio::time::Instant>>,
    /// Sube con cada cambio de la copia.
    revision: watch::Sender<u64>,
    /// El último error al sincronizar que se apuntó, para no repetirlo cada vez.
    said: Mutex<Option<String>>,
    /// Para que las pruebas paren `refresh` entre la sincronización y el sondeo.
    #[cfg(test)]
    hold: tokio::sync::Mutex<()>,
    /// Cuándo se ha escrito la copia en el disco, cada vez.
    #[cfg(test)]
    disk_writes: Mutex<Vec<std::time::Instant>>,
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

/// ¿Está retirada en el llavero la clave de este `kid` (y la tenemos)? Lo cifrado con ella se
/// recifra con la actual.
fn retired(keys: &Keyring, kid: &str) -> bool {
    keys.keys
        .iter()
        .any(|k| k.kid == kid && !k.removed && k.retired.is_some())
}

/// Lo que dio recifrar un objeto.
enum Recrypted {
    /// Escrito con la clave actual.
    Wrote,
    /// Nada que hacer ya: lo de ahora va con una clave que no está retirada, otro lo borró, el
    /// hub no lo tiene, o no tiene su clave.
    Done,
    /// No se puede (no se descifra, el hub no lo quiere, choca una y otra vez): en esta pasada,
    /// no se vuelve a intentar.
    Skip,
    /// Cambió la clave actual: se empieza otra vez con la nueva.
    Again,
    /// Se para la pasada: se vació la copia, falta algo por resubir o no hay hub.
    Stop,
}

/// Lo que el recifrado y la poda del llavero necesitan del grupo (`spawn_upkeep`).
pub trait KeyHolder: Send + Sync + 'static {
    fn keychain(&self) -> &Group;
    /// Se retiró o se quitó una clave aquí: que se entere el resto del grupo.
    fn keys_changed(&self) {}
}

impl KeyHolder for Group {
    fn keychain(&self) -> &Group {
        self
    }
}

/// Un número al azar menor que `n` (> 0).
fn random_below(n: usize) -> usize {
    let mut b = [0u8; 8];
    let _ = getrandom::fill(&mut b);
    (u64::from_le_bytes(b) % n as u64) as usize
}

/// El cursor `"<at>:<id>"` del hub.
fn parse_after(a: &str) -> Option<(i64, String)> {
    let (at, id) = a.split_once(':')?;
    let at = at.parse().ok()?;
    is_id(id).then(|| (at, id.to_owned()))
}

/// Lo leído del hub en una sincronización entera.
struct Read {
    items: Vec<BoardItem>,
    next: i64,
    reset: bool,
    /// La cuenta de lo leído: la `ref` esperada al acabar.
    account_ref: Option<String>,
}

/// La lápida que deja un borrado propio.
fn tombstone(id: &str, st: crate::hub::BoardStamp) -> BoardItem {
    BoardItem {
        id: id.into(),
        v: st.v,
        kid: String::new(),
        blob: None,
        gone: true,
        at: st.at,
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
            dirty: AtomicBool::new(false),
            saver: Default::default(),
            saving: AtomicBool::new(false),
            syncing: tokio::sync::Mutex::new(()),
            epoch: AtomicU64::new(0),
            reuploading: AtomicBool::new(false),
            gap,
            probe_wanted: Default::default(),
            recrypt_last: Mutex::new(None),
            revision: watch::Sender::new(0),
            said: Mutex::new(None),
            #[cfg(test)]
            hold: tokio::sync::Mutex::new(()),
            #[cfg(test)]
            disk_writes: Default::default(),
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

    /// La copia cambió. Con `seen`, cambió algo que se ve (los objetos): se avisa, y se guardará
    /// al final del hueco (`saver`). Si solo avanzó `next`, ni lo uno ni lo otro: queda en `dirty`
    /// y va al disco con el próximo cambio de verdad (o al parar, `flush`); perderlo solo hace
    /// releer un poco más la próxima vez.
    fn mark(self: &Arc<Self>, seen: bool) {
        self.dirty.store(true, Ordering::SeqCst);
        if seen {
            self.save_soon();
            self.bump();
        }
    }

    /// Que se guarde al final del hueco (`saver`), sin avisar de nada.
    fn save_soon(self: &Arc<Self>) {
        self.start_saver();
        self.saver.notify_one();
    }

    /// La tarea que guarda la copia: el primer cambio abre un hueco de `SAVE_GAP`, y al acabar
    /// se guarda todo lo que haya cambiado (una edición y la sincronización del aviso del hub que
    /// la sigue, en una sola escritura). Acaba al soltar la copia.
    fn start_saver(self: &Arc<Self>) {
        if self.saving.swap(true, Ordering::SeqCst) {
            return;
        }
        let Ok(rt) = tokio::runtime::Handle::try_current() else {
            self.saving.store(false, Ordering::SeqCst);
            return;
        };
        let me = Arc::downgrade(self);
        let wake = self.saver.clone();
        rt.spawn(async move {
            loop {
                wake.notified().await;
                tokio::time::sleep(SAVE_GAP).await;
                let Some(cache) = me.upgrade() else { return };
                cache.flush().await;
            }
        });
    }

    /// Guarda ya la copia si cambió (fuera del hilo de las tareas: puede ser grande).
    pub async fn flush(self: &Arc<Self>) {
        let me = self.clone();
        let _ = tokio::task::spawn_blocking(move || me.write_if_dirty()).await;
    }

    /// Escribe la copia en el disco si cambió. Con `disk` cogido, y la copia se clona después de
    /// cogerlo (y se serializa sin `copy`): así uno que guarda tarde no pisa con algo más viejo
    /// lo que guardó otro (o un vaciado).
    fn write_if_dirty(&self) {
        let _disk = self.disk.lock().expect("tablero");
        if !self.dirty.swap(false, Ordering::SeqCst) {
            return;
        }
        let copy = self.lock().clone();
        let body = serde_json::to_vec(&copy).expect("la copia se escribe");
        self.write(&body);
    }

    /// Escribe `body` en `board.json`. Con `disk` cogido.
    fn write(&self, body: &[u8]) {
        #[cfg(test)]
        let started = std::time::Instant::now();
        if let Err(e) = crate::sys::write_private_atomic(&self.path, body) {
            eprintln!("nido: no se pudo guardar la copia del tablero: {e}");
            self.dirty.store(true, Ordering::SeqCst);
        }
        // Se apunta ya escrita (y renombrada): quien la cuenta puede leerla del disco.
        #[cfg(test)]
        self.disk_writes.lock().expect("tablero").push(started);
    }

    /// Cambia la copia con `f` si no se vació mientras tanto (`epoch`); si `f` dice que cambió
    /// algo, se guardará. Devuelve lo que diga `f`, o `false` si se vació.
    fn change(self: &Arc<Self>, epoch: u64, f: impl FnOnce(&mut Copy) -> bool) -> bool {
        let changed = {
            let mut c = self.lock();
            self.epoch() == epoch && f(&mut c)
        };
        if changed {
            self.mark(true);
        }
        changed
    }

    /// Vacía la copia, en memoria y en el disco (ya, sin esperar), y para lo que estuviera en
    /// marcha (cuando este equipo sale del grupo). No usa el grupo: se llama con él cogido.
    pub fn clear(&self) {
        self.epoch.fetch_add(1, Ordering::SeqCst);
        {
            let _disk = self.disk.lock().expect("tablero");
            *self.lock() = Copy::default();
            self.dirty.store(false, Ordering::SeqCst);
            let body = serde_json::to_vec(&Copy::default()).expect("la copia se escribe");
            self.write(&body);
        }
        self.bump();
    }

    /// Lee todo lo cambiado desde `since`, página a página: mientras haya `more`, con el mismo
    /// `since` y el `after` de la anterior. Comprueba que el hub pagina como dice: el cursor
    /// avanza, lo de cada página va detrás del cursor pedido, `reset` solo en la primera y con
    /// `since > 0`, y como mucho `MAX_READ` objetos y páginas. Los ids que no son ids se saltan.
    ///
    /// `account_ref` es la cuenta que se espera (la guardada con `since`). Una página con otra
    /// `ref` es otra cuenta: su `since` no vale, y la nueva pasa a ser la esperada y se lee otra
    /// vez desde 0, sin tocar la copia (no hace falta si era la primera página desde 0). Una
    /// página sin `ref` (un hub de antes) no cuenta como cambio. Si cambia más de
    /// `MAX_REF_CHANGES` veces, se deja.
    async fn read_all(&self, since: i64, account_ref: Option<String>) -> Result<Read, BoardError> {
        let bad = || BoardError::Hub(BAD_PAGING.into());
        let (mut since, mut expected, mut changes) = (since, account_ref, 0);
        let mut items = Vec::new();
        let mut cursor: Option<(i64, String)> = None;
        let mut pages = 0;
        while pages < MAX_READ {
            pages += 1;
            let after = cursor.as_ref().map(|(at, id)| format!("{at}:{id}"));
            let page = self.hub.board_get(since, after.as_deref()).await?;
            if let Some(r) = &page.account_ref
                && expected.as_ref() != Some(r)
            {
                // La primera página de toda la lectura no cuenta: es la primera `ref` que se ve.
                if pages > 1 || expected.is_some() {
                    changes += 1;
                    if changes > MAX_REF_CHANGES {
                        return Err(BoardError::Hub(REF_FLAPPING.into()));
                    }
                }
                expected = Some(r.clone());
                if since > 0 || cursor.is_some() {
                    since = 0;
                    cursor = None;
                    items.clear();
                    continue;
                }
            }
            if page.reset {
                if cursor.is_some() || since == 0 || !page.items.is_empty() {
                    return Err(bad());
                }
                return Ok(Read {
                    items: vec![],
                    next: 0,
                    reset: true,
                    account_ref: expected,
                });
            }
            if let Some((at, id)) = &cursor
                && page.items.iter().any(|i| (i.at, &i.id) <= (*at, id))
            {
                return Err(bad());
            }
            items.extend(page.items.into_iter().filter(|i| is_id(&i.id)));
            if items.len() > MAX_READ {
                return Err(bad());
            }
            if !page.more {
                return Ok(Read {
                    items,
                    next: page.next,
                    reset: false,
                    account_ref: expected,
                });
            }
            let next = page
                .after
                .as_deref()
                .and_then(parse_after)
                .ok_or_else(bad)?;
            if cursor.as_ref().is_some_and(|c| next <= *c) {
                return Err(bad());
            }
            cursor = Some(next);
        }
        Err(bad())
    }

    /// Se pone al día con el hub desde el `since` guardado (desde 0 si la copia es de otro hub o
    /// de otra cuenta, sin tirar lo que tiene; ver `read_all`). Todo se lee antes de tocar la
    /// copia: si se corta a medias, la próxima empieza otra vez desde el guardado. Con la misma
    /// cuenta, `next` nunca baja; con otra, es el de esa cuenta, y se guarda con su `ref`. Con
    /// `reset`, la copia se rehace aparte desde 0 y se cambia solo al acabar. Devuelve si cambió
    /// algún objeto.
    pub async fn sync(self: &Arc<Self>) -> Result<bool, BoardError> {
        let _one = self.syncing.lock().await;
        let epoch = self.epoch();
        let url = self.hub.url();
        let (since, same_hub, saved_ref) = {
            let c = self.lock();
            let same = c.hub == url;
            (if same { c.next } else { 0 }, same, c.account_ref.clone())
        };
        let read = self.read_all(since, saved_ref.clone()).await?;
        if read.reset {
            let fresh = self.read_all(0, read.account_ref).await?;
            let mut copy = Copy {
                hub: url,
                next: fresh.next,
                account_ref: fresh.account_ref,
                ..Default::default()
            };
            for item in fresh.items {
                copy.apply(item);
            }
            return Ok(self.change(epoch, |c| {
                *c = copy;
                true
            }));
        }
        let (changed, moved, other, lost) = {
            let mut c = self.lock();
            if self.epoch() != epoch {
                return Ok(false);
            }
            // Con otro hub o con otra cuenta (otra `ref`), la lectura es completa (desde 0): lo
            // vivo que no trae no está en esa cuenta. Se marca, y el sondeo decide si se resube
            // (hub vacío) o sale de la copia. `next` es entonces el de esa cuenta, aunque baje.
            let other = !same_hub || read.account_ref != saved_ref;
            let lost: Vec<String> = if !other {
                vec![]
            } else {
                let read_ids: BTreeSet<&str> = read.items.iter().map(|i| i.id.as_str()).collect();
                c.items
                    .iter()
                    .filter(|(id, s)| {
                        !s.gone && !read_ids.contains(id.as_str()) && !c.reupload.contains(*id)
                    })
                    .map(|(id, _)| id.clone())
                    .collect()
            };
            let mut changed = false;
            for id in &lost {
                changed |= c.missing.insert(id.clone());
            }
            for item in read.items {
                changed |= c.apply(item);
            }
            let moved = if other {
                c.hub = url;
                c.next = read.next;
                c.account_ref = read.account_ref;
                true
            } else if read.next > c.next {
                c.next = read.next;
                true
            } else {
                false
            };
            (changed, moved, other, !lost.is_empty())
        };
        if changed || moved {
            self.mark(changed);
        }
        // Otra cuenta sin nada nuevo que enseñar: su `ref` y su `next` van al disco igual, que
        // si no un corte brusco obliga a otra lectura completa.
        if other && !changed {
            self.save_soon();
        }
        if lost {
            self.probe_wanted.notify_one();
        }
        Ok(changed)
    }

    /// Una lectura completa (desde 0), y fuera de la copia lo que el hub no tiene (salvo lo que
    /// falta por resubir): para lo que se marcó como que no está en el hub, con un hub que sí
    /// tiene tablero. Solo sale lo que se puede leer: lo que no tiene su clave en el llavero
    /// (puede llegar en una rotación) se queda, marcado como que no está en el hub.
    async fn prune(self: &Arc<Self>, epoch: u64, keys: &Keyring) -> Result<(), BoardError> {
        let _one = self.syncing.lock().await;
        let read = self.read_all(0, None).await?;
        let on_hub: BTreeSet<String> = read.items.iter().map(|i| i.id.clone()).collect();
        let (mut gone, mut other) = (0, false);
        self.change(epoch, |c| {
            // Lo leído es de otra cuenta que la de la copia: no dice qué falta en la suya. No se
            // toca nada, y el sondeo se pone antes al día con esa cuenta (`sync`).
            if let (Some(read), Some(saved)) = (&read.account_ref, &c.account_ref)
                && read != saved
            {
                other = true;
                return false;
            }
            let before = c.clone();
            let keep = c.reupload.clone();
            let no_key: BTreeSet<String> = c
                .items
                .iter()
                .filter(|(id, s)| {
                    !s.gone
                        && !on_hub.contains(*id)
                        && !keep.contains(*id)
                        && key_of(keys, &s.kid).is_none()
                })
                .map(|(id, _)| id.clone())
                .collect();
            c.items
                .retain(|id, _| on_hub.contains(id) || keep.contains(id) || no_key.contains(id));
            gone = before.items.len() - c.items.len();
            c.missing = no_key;
            for item in read.items {
                c.apply(item);
            }
            c.next = c.next.max(read.next);
            *c != before
        });
        if other {
            self.probe_wanted.notify_one();
        }
        if gone > 0 {
            eprintln!("nido: tablero: {gone} objetos que el hub ya no tiene, fuera de la copia");
        }
        Ok(())
    }

    /// Junta lo que dio una lectura del sondeo y, si hay objetos marcados como que no están en
    /// el hub, los repasa (`prune`).
    async fn probed(
        self: &Arc<Self>,
        epoch: u64,
        items: Vec<BoardItem>,
        keys: &Keys,
    ) -> Result<(), BoardError> {
        let (changed, missing) = {
            let mut c = self.lock();
            if self.epoch() != epoch {
                return Ok(());
            }
            let mut changed = false;
            for item in items {
                changed |= c.apply(item);
            }
            (changed, !c.missing.is_empty())
        };
        if changed {
            self.mark(true);
        }
        if missing {
            self.prune(epoch, &keys()).await?;
        }
        Ok(())
    }

    /// Mira si el hub se ha quedado sin tablero teniendo esta copia: una lectura completa
    /// (`since = 0`) que no da nada, ni lápidas, dos veces con `EMPTY_CONFIRM` entre medias.
    /// Entonces la vuelve a subir (decisión 8): el hub perdió lo que tenía (uno sin MongoDB que
    /// se reinició) o la cuenta es nueva (al partir un grupo de dos, la cuenta se la puede quedar
    /// el equipo quitado). Lo que no se puede leer con su clave no se sube y queda marcado como
    /// que no está en el hub. Si el hub tiene algo, lo junta (y repasa lo marcado). Solo se
    /// apunta y se guarda si cambia algo.
    ///
    /// Sus `get` desde 0 no miran la `ref`, y no hace falta. Va justo después de un `sync`, que
    /// ya se puso al día con la cuenta de ahora. Si aun así fuera otra: juntar lo que trae
    /// (gana lo más nuevo) no quita nada y es lo que traerá la próxima sincronización; el repaso
    /// sí la mira (`prune`); y un hub vacío es vacío en cualquier cuenta, y lo que se resube va a
    /// la cuenta en la que está ahora este nido, que es la que tiene que tenerlo.
    pub async fn probe(self: &Arc<Self>, keys: &Keys) -> Result<(), BoardError> {
        if !self.lock().has_objects() {
            return Ok(());
        }
        let epoch = self.epoch();
        let empty = |p: &crate::hub::BoardPage| p.items.is_empty() && !p.more && !p.reset;
        let page = self.hub.board_get(0, None).await?;
        if !empty(&page) {
            self.probed(epoch, page.items, keys).await?;
            self.resume_reupload(keys);
            return Ok(());
        }
        tokio::time::sleep(EMPTY_CONFIRM).await;
        if self.epoch() != epoch {
            return Ok(());
        }
        let page = self.hub.board_get(0, None).await?;
        if !empty(&page) {
            self.probed(epoch, page.items, keys).await?;
            self.resume_reupload(keys);
            return Ok(());
        }
        let keyring = keys();
        if current_key(&keyring).is_none() {
            // Sin clave con que subirla, se deja la copia como está: el próximo sondeo.
            return Ok(());
        }
        // Qué se puede subir, mirado sin el cerrojo (descifra cada objeto).
        let items = self.lock().items.clone();
        let (up, rest): (BTreeSet<String>, BTreeSet<String>) = items
            .iter()
            .filter(|(_, s)| !s.gone)
            .map(|(id, _)| id.clone())
            .partition(|id| reupload_blob(&keyring, id, &items[id]).is_some());
        let mut count = 0;
        let changed = self.change(epoch, |c| {
            let before = c.clone();
            // Las lápidas ya no hacen falta: el hub no tiene nada.
            c.items.retain(|_, s| !s.gone);
            c.reupload = up
                .into_iter()
                .filter(|id| c.items.contains_key(id))
                .collect();
            c.missing = rest
                .into_iter()
                .filter(|id| c.items.contains_key(id))
                .collect();
            // La próxima, desde el principio.
            c.next = 0;
            count = c.reupload.len();
            *c != before
        });
        if changed {
            eprintln!(
                "nido: el hub no tiene tablero; se vuelven a subir {count} objetos de la copia"
            );
        }
        self.resume_reupload(keys);
        Ok(())
    }

    /// Sigue con la resubida, si falta algo y no está ya en marcha.
    fn resume_reupload(self: &Arc<Self>, keys: &Keys) {
        if self.lock().reupload.is_empty() || self.reuploading.swap(true, Ordering::SeqCst) {
            return;
        }
        let me = self.clone();
        let keys = keys.clone();
        tokio::spawn(async move {
            me.reupload(keys).await;
            me.reuploading.store(false, Ordering::SeqCst);
        });
    }

    /// La resubida, de uno en uno, a ritmo (`gap`) y en orden al azar (si varios nidos de la casa
    /// resuben a la vez, chocan menos), con `v = 0` y el llavero de ese momento. Un 409 es que
    /// otro nido se adelantó: cuenta como hecho y se queda lo suyo. Lo que el hub no quiere
    /// (demasiado grande, no válido, la cuenta llena) o ya no se puede leer pasa a `missing`. Con
    /// el freno del hub, espera y sigue; sin hub, lo deja para la próxima conexión (lo que falta
    /// está en `board.json`).
    async fn reupload(self: &Arc<Self>, keys: Keys) {
        let epoch = self.epoch();
        let (mut done, mut first) = (0usize, true);
        loop {
            let keyring = keys();
            let pick = {
                let mut c = self.lock();
                if self.epoch() != epoch {
                    return;
                }
                if c.reupload.is_empty() {
                    break;
                }
                let i = random_below(c.reupload.len());
                let id = c.reupload.iter().nth(i).cloned().expect("hay tantos");
                match c
                    .items
                    .get(&id)
                    .and_then(|s| reupload_blob(&keyring, &id, s))
                {
                    Some(blob) => Some((id, blob)),
                    None => {
                        // Ya no se puede leer (otro llavero): no se sube.
                        c.reupload.remove(&id);
                        if c.items.contains_key(&id) {
                            c.missing.insert(id);
                        }
                        None
                    }
                }
            };
            let Some((id, (kid, blob))) = pick else {
                self.mark(true);
                continue;
            };
            if !std::mem::take(&mut first) {
                tokio::time::sleep(self.gap).await;
                // Mientras tanto se pudo vaciar la copia, o subir (o borrar) este por otro lado.
                if self.epoch() != epoch {
                    return;
                }
                if !self.lock().reupload.contains(&id) {
                    continue;
                }
            }
            let r = self.hub.board_put(&id, 0, &kid, &blob).await;
            let stop = {
                let mut c = self.lock();
                if self.epoch() != epoch {
                    return;
                }
                // Ya no está ni en la copia ni por subir: no se toca.
                if !c.items.contains_key(&id) && !c.reupload.contains(&id) {
                    continue;
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
                        None
                    }
                    Err(BoardError::Conflict(current)) => {
                        c.apply(current);
                        None
                    }
                    Err(BoardError::Busy) => Some(false),
                    Err(e @ (BoardError::TooBig | BoardError::Bad(_))) => {
                        eprintln!("nido: tablero: {id} no se vuelve a subir ({e})");
                        c.reupload.remove(&id);
                        c.missing.insert(id.clone());
                        None
                    }
                    Err(BoardError::Full) => {
                        eprintln!("nido: tablero: la cuenta está llena; se deja la resubida");
                        let rest = std::mem::take(&mut c.reupload);
                        c.missing.extend(rest);
                        None
                    }
                    Err(BoardError::Hub(e)) => {
                        eprintln!("nido: tablero: resubida parada ({e}); sigue al volver el hub");
                        Some(true)
                    }
                }
            };
            match stop {
                Some(false) => {
                    tokio::time::sleep(self.gap * BUSY_GAPS).await;
                    continue;
                }
                Some(true) => {
                    self.flush().await;
                    return;
                }
                None => {}
            }
            self.mark(true);
            done += 1;
        }
        self.flush().await;
        if done > 0 {
            eprintln!("nido: tablero: resubida acabada ({done} escrituras)");
        }
    }

    /// Los objetos de la copia, descifrados cada uno con la clave de su `kid`. Lo que no se puede
    /// leer (o no está en el hub) va aparte y se conserva tal cual; las lápidas no salen.
    pub fn decrypted(&self, keys: &Keyring) -> Decrypted {
        // Se descifra sin el cerrojo, sobre una copia.
        let (items, missing) = {
            let c = self.lock();
            (c.items.clone(), c.missing.clone())
        };
        let mut d = Decrypted::default();
        for (id, s) in items.iter().filter(|(_, s)| !s.gone) {
            let r = if missing.contains(id) {
                Err(Unreadable::NotInHub)
            } else {
                read(keys, id, s)
            };
            match r {
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

    /// Lo que dijo el hub de `id`: que no lo tiene (`v: 0`). En la copia se marca así (no se
    /// entierra: puede ser que el hub perdiera el tablero) y se pide un sondeo.
    fn not_in_hub(self: &Arc<Self>, epoch: u64, id: &str) {
        self.change(epoch, |c| {
            c.items.contains_key(id) && !c.reupload.contains(id) && c.missing.insert(id.into())
        });
        self.probe_wanted.notify_one();
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
        let item = BoardItem {
            id: id.clone(),
            v: st.v,
            kid,
            blob: Some(blob),
            gone: false,
            at: st.at,
        };
        self.change(epoch, |c| c.apply(item));
        Ok(id)
    }

    /// Cambia el objeto `id`: `f` recibe el objeto leído (con la clave de su `kid`) y lo cambia,
    /// así se conserva lo que no conocemos, en `extra`. Se escribe con la clave actual. Si choca
    /// (409), se vuelve a aplicar `f` sobre el objeto como está, hasta `MAX_TRIES` escrituras en
    /// total. Lo que no se puede leer no se reescribe. Uno borrado en el hub queda borrado aquí
    /// (`Missing`); uno que el hub no tiene se marca así (`Missing`, y se pide un sondeo).
    /// Devuelve el objeto como quedó.
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
            if c.missing.contains(id) {
                return Err(EditError::Unreadable(Unreadable::NotInHub));
            }
            // Uno que falta por resubir no está en el hub: se crea.
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
                    self.change(epoch, |c| c.apply(item));
                    return Ok(obj);
                }
                Err(BoardError::Conflict(current)) if current.gone => {
                    if current.v > 0 {
                        self.change(epoch, |c| c.bury(&current));
                    } else {
                        self.not_in_hub(epoch, id);
                    }
                    return Err(EditError::Missing);
                }
                Err(BoardError::Conflict(current)) => {
                    self.change(epoch, |c| c.apply(current.clone()));
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
    ///
    /// Uno que falta por resubir se sube y se borra, para que en el hub quede la lápida (otro
    /// nido de la casa que aún lo tenga lo resubiría). Uno que el hub no tiene (`v: 0`) se quita
    /// de la copia, sin más (marcarlo como que no está en el hub lo volvería a subir si el hub
    /// se quedó sin tablero), y se pide un sondeo.
    pub async fn remove(self: &Arc<Self>, keys: &Keyring, id: &str) -> Result<(), EditError> {
        let epoch = self.epoch();
        let (mut v, pending) = {
            let mut c = self.lock();
            let Some(s) = c.items.get(id) else {
                return Err(EditError::Missing);
            };
            if s.gone {
                return Ok(());
            }
            let v = s.v;
            let pending = c.reupload.remove(id);
            let lost = c.missing.contains(id);
            (v, if lost { None } else { Some(pending) })
        };
        let Some(pending) = pending else {
            // No está en el hub ni se va a subir: basta con quitarlo de aquí.
            self.change(epoch, |c| c.forget(id));
            return Ok(());
        };
        if pending {
            let blob = self
                .lock()
                .items
                .get(id)
                .and_then(|s| reupload_blob(keys, id, s));
            let Some((kid, blob)) = blob else {
                self.change(epoch, |c| c.forget(id));
                return Ok(());
            };
            match self.hub.board_put(id, 0, &kid, &blob).await {
                Ok(st) => v = st.v,
                Err(BoardError::Conflict(current)) if current.gone => {
                    self.change(epoch, |c| c.bury(&current));
                    return Ok(());
                }
                Err(BoardError::Conflict(current)) => v = current.v,
                Err(e) => {
                    // Sigue faltando por subir.
                    self.change(epoch, |c| {
                        c.items.contains_key(id) && c.reupload.insert(id.into())
                    });
                    return Err(EditError::Board(e));
                }
            }
        }
        let mut tries = 0;
        loop {
            tries += 1;
            match self.hub.board_del(id, v).await {
                Ok(st) => {
                    self.change(epoch, |c| c.bury(&tombstone(id, st)));
                    return Ok(());
                }
                Err(BoardError::Conflict(current)) if current.gone => {
                    if current.v > 0 {
                        self.change(epoch, |c| c.bury(&current));
                    } else {
                        self.change(epoch, |c| c.forget(id));
                        self.probe_wanted.notify_one();
                    }
                    return Ok(());
                }
                Err(BoardError::Conflict(current)) => {
                    self.change(epoch, |c| c.apply(current.clone()));
                    if tries >= MAX_TRIES {
                        return Err(EditError::Board(BoardError::Conflict(current)));
                    }
                    v = current.v;
                }
                Err(e) => return Err(EditError::Board(e)),
            }
        }
    }

    /// Los `kid` que usa algún objeto de la copia: todo lo que no es lápida, también lo que no
    /// está en el hub, lo que falta por resubir y lo que no se puede leer.
    pub fn kids_in_use(&self) -> BTreeSet<String> {
        self.lock()
            .items
            .values()
            .filter(|s| !s.gone)
            .map(|s| s.kid.clone())
            .collect()
    }

    /// Espera lo que falte para que pase `gap` desde la última escritura del recifrado.
    async fn recrypt_pace(&self) {
        let last = *self.recrypt_last.lock().expect("tablero");
        if let Some(t) = last {
            tokio::time::sleep_until(t + self.gap).await;
        }
        *self.recrypt_last.lock().expect("tablero") = Some(tokio::time::Instant::now());
    }

    /// Recifra con la clave actual lo que la copia tiene cifrado con claves retiradas (decisión
    /// 9: cualquier nido que lo vea). Uno a uno, a ritmo (`gap`, como la resubida) y en orden al
    /// azar: todos los nidos del grupo lo hacen a la vez y así chocan menos. Se recifran los
    /// bytes que da `open`, tal cual (`recrypt_one`). No toca lo que no está en el hub ni lo que
    /// no tiene su clave en el llavero, y no empieza ni sigue mientras falte algo por resubir
    /// (la resubida ya cifra con la actual). Se para si se vacía la copia (`epoch`), y si cambia
    /// la clave actual sigue con la nueva.
    ///
    /// Devuelve si acabó: no queda nada con una clave retirada que se pueda recifrar.
    pub async fn recrypt(self: &Arc<Self>, keys: &Keys) -> bool {
        let epoch = self.epoch();
        let mut skipped = BTreeSet::new();
        let mut done = 0usize;
        let finished = loop {
            let ring = keys();
            let Some((kid, key)) = current_key(&ring) else {
                // Sin clave con que cifrar, no hay nada que hacer.
                break true;
            };
            let pick = {
                let c = self.lock();
                if self.epoch() != epoch || !c.reupload.is_empty() {
                    break false;
                }
                let ids: Vec<&String> = c
                    .items
                    .iter()
                    .filter(|(id, s)| {
                        !s.gone
                            && s.blob.is_some()
                            && !c.missing.contains(*id)
                            && !skipped.contains(*id)
                            && retired(&ring, &s.kid)
                    })
                    .map(|(id, _)| id)
                    .collect();
                if ids.is_empty() {
                    break true;
                }
                ids[random_below(ids.len())].clone()
            };
            match self.recrypt_one(epoch, keys, &pick, &kid, &key).await {
                Recrypted::Wrote => done += 1,
                Recrypted::Done | Recrypted::Again => {}
                Recrypted::Skip => {
                    skipped.insert(pick);
                }
                Recrypted::Stop => break false,
            }
        };
        if done > 0 {
            self.flush().await;
            eprintln!("nido: tablero: {done} objetos recifrados con la clave actual");
        }
        finished
    }

    /// Recifra el objeto `id` con la clave actual (`kid`, `key`): es `update` con los bytes tal
    /// cual, sin pasar por `Object`. Antes de cada escritura espera su turno (`recrypt_pace`) y
    /// vuelve a mirar el objeto en la copia: el aviso de otro nido que se adelantó ya lo habrá
    /// traído recifrado. Si choca (409), sigue sobre lo de ahora mientras su clave siga retirada,
    /// hasta `MAX_TRIES` escrituras.
    async fn recrypt_one(
        self: &Arc<Self>,
        epoch: u64,
        keys: &Keys,
        id: &str,
        kid: &str,
        key: &[u8; 32],
    ) -> Recrypted {
        let mut tries = 0;
        loop {
            self.recrypt_pace().await;
            if self.epoch() != epoch {
                return Recrypted::Stop;
            }
            let ring = keys();
            if current_key(&ring).is_none_or(|(k, _)| k != kid) {
                return Recrypted::Again;
            }
            let s = {
                let c = self.lock();
                if !c.reupload.is_empty() {
                    return Recrypted::Stop;
                }
                match c.items.get(id) {
                    Some(s) if !s.gone && !c.missing.contains(id) && retired(&ring, &s.kid) => {
                        s.clone()
                    }
                    _ => return Recrypted::Done,
                }
            };
            // Con la clave de su `kid`, nunca otra.
            let Some(old) = key_of(&ring, &s.kid) else {
                return Recrypted::Done;
            };
            let sealed = open(&old, id, s.blob.as_deref().unwrap_or_default())
                .and_then(|b| seal(key, id, &b));
            let blob = match sealed {
                Ok(b) => b,
                Err(e) => {
                    eprintln!(
                        "nido: tablero: {id} no se recifra ({})",
                        blob_error_kind(&e)
                    );
                    return Recrypted::Skip;
                }
            };
            tries += 1;
            match self.hub.board_put(id, s.v, kid, &blob).await {
                Ok(st) => {
                    let item = BoardItem {
                        id: id.into(),
                        v: st.v,
                        kid: kid.into(),
                        blob: Some(blob),
                        gone: false,
                        at: st.at,
                    };
                    self.change(epoch, |c| c.apply(item));
                    return Recrypted::Wrote;
                }
                Err(BoardError::Conflict(current)) if current.gone => {
                    if current.v > 0 {
                        self.change(epoch, |c| c.bury(&current));
                    } else {
                        self.not_in_hub(epoch, id);
                    }
                    return Recrypted::Done;
                }
                Err(BoardError::Conflict(current)) => {
                    // Lo de ahora, a la copia: la vuelta siguiente lo mira.
                    self.change(epoch, |c| c.apply(current));
                    if tries >= MAX_TRIES {
                        return Recrypted::Skip;
                    }
                }
                Err(BoardError::Busy) => tokio::time::sleep(self.gap * BUSY_GAPS).await,
                Err(e @ (BoardError::TooBig | BoardError::Bad(_) | BoardError::Full)) => {
                    eprintln!("nido: tablero: {id} no se recifra ({e})");
                    return Recrypted::Skip;
                }
                Err(BoardError::Hub(e)) => {
                    eprintln!("nido: tablero: recifrado parado ({e}); sigue al volver el hub");
                    return Recrypted::Stop;
                }
            }
        }
    }

    /// Lo que hace la tarea del llavero cada vez (`spawn_upkeep`): ordena el llavero (retira
    /// las activas que no son la actual, poda lápidas), y con hub recifra lo cifrado con claves
    /// retiradas; si acaba, quita las retiradas que ya no usa nadie (`drop_unused_keys`).
    pub async fn upkeep<H: KeyHolder>(self: &Arc<Self>, holder: &Arc<H>) {
        if holder.keychain().tidy_board_keys() {
            holder.keys_changed();
        }
        if !self.hub.status.borrow().connected {
            return;
        }
        let keys: Keys = {
            let h = holder.clone();
            Arc::new(move || h.keychain().board_keys())
        };
        if self.recrypt(&keys).await {
            self.drop_unused_keys(holder.as_ref()).await;
        }
    }

    /// Quita del llavero las retiradas que no usa ningún objeto (decisión 9; ver
    /// `Group::drop_unused_board_keys`). Si hay alguna candidata, antes se pone al día con el
    /// hub: lo que haya escrito con ella un dispositivo con el llavero viejo tiene que estar en
    /// la copia. Sin poder sincronizar, o si se vacía la copia mientras tanto, no se quita nada.
    ///
    /// Los `kid` en uso se sacan de la copia y se suelta antes de ir al grupo (que se coge
    /// antes que la copia, ver `clear`): una escritura con la clave vieja que llegue justo en
    /// ese momento es lo que cubren los 30 días.
    async fn drop_unused_keys<H: KeyHolder>(self: &Arc<Self>, holder: &H) {
        let group = holder.keychain();
        if group.droppable_board_keys(&self.kids_in_use()).is_empty() {
            return;
        }
        let epoch = self.epoch();
        // El error ya lo apunta la sincronización de `spawn` si se repite.
        if self.sync().await.is_err() || self.epoch() != epoch {
            return;
        }
        let in_use = self.kids_in_use();
        if self.epoch() != epoch {
            return;
        }
        let dropped = group.drop_unused_board_keys(&in_use);
        if !dropped.is_empty() {
            eprintln!(
                "nido: tablero: fuera del llavero, retiradas y sin objetos: {}",
                dropped.join(", ")
            );
            holder.keys_changed();
        }
    }

    /// Sincroniza y, con `probe`, mira si el hub se quedó sin tablero; luego sigue con la
    /// resubida que faltara. Los errores se apuntan (sin repetir el mismo seguido).
    pub async fn refresh(self: &Arc<Self>, keys: &Keys, probe: bool) {
        let mut r = self.sync().await.map(|_| ());
        #[cfg(test)]
        drop(self.hold.lock().await);
        if r.is_ok() && probe {
            r = self.probe(keys).await;
        }
        let e = match r {
            Ok(()) => {
                *self.said.lock().expect("tablero") = None;
                self.resume_reupload(keys);
                return;
            }
            Err(e) => e.to_string(),
        };
        let mut said = self.said.lock().expect("tablero");
        if said.as_deref() != Some(e.as_str()) {
            eprintln!("nido: tablero: no se pudo sincronizar ({e})");
            *said = Some(e);
        }
    }
}

/// Mantiene la copia al día mientras haya conexión con el hub: con cada conexión nueva (lo
/// primero, un `get`, que es lo que hace que el hub avise en esa conexión), con cada aviso del
/// hub y cada `SYNC_EVERY`. Mira además si el hub se quedó sin tablero (`probe`), de lo que el
/// hub no avisa: al conectar, si cambia la cuenta (nombre, idioma o `ref`, también frente a la
/// `ref` de la copia), cada `SYNC_EVERY` y
/// cuando una escritura ve que el hub no tiene un objeto de la copia. `keys` da el llavero del
/// grupo cada vez.
///
/// Se atiende una cosa cada vez: mientras dura un `refresh` no se miran los avisos ni el tic. Y
/// el sondeo va dentro, con lo que tarda: confirmar que el hub está vacío espera `EMPTY_CONFIRM`
/// (un minuto), y el repaso de lo marcado (`prune`) es una lectura completa. Lo que llegue
/// mientras tanto no se pierde: los avisos y las conexiones se juntan (`watch`) y se atienden al
/// acabar.
pub fn spawn(cache: Arc<BoardCache>, keys: impl Fn() -> Keyring + Send + Sync + 'static) {
    spawn_with(cache, keys, SYNC_EVERY);
}

/// `spawn`, sincronizando cada `every` (para las pruebas).
pub fn spawn_with(
    cache: Arc<BoardCache>,
    keys: impl Fn() -> Keyring + Send + Sync + 'static,
    every: Duration,
) {
    let keys: Keys = Arc::new(keys);
    tokio::spawn(async move {
        let mut status = cache.hub.status.subscribe();
        status.mark_changed();
        let mut notices = cache.hub.board_notices();
        let mut tick = tokio::time::interval_at(tokio::time::Instant::now() + every, every);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // La conexión que ya se atendió (por su número), y la última cuenta que se supo.
        let (mut connected, mut seen) = (false, 0u64);
        let mut account = None;
        loop {
            let probe = tokio::select! {
                r = status.changed() => {
                    if r.is_err() {
                        return;
                    }
                    let (now, connections, acct) = {
                        let s = status.borrow_and_update();
                        (s.connected, s.connections, (s.account.clone(), s.account_ref.clone()))
                    };
                    connected = now;
                    // Por el número y no por `connected`: si mientras se atendía otra cosa se
                    // cortó y se volvió a conectar, `connected` ya estaba a `true`.
                    let fresh = now && connections != seen;
                    if now {
                        seen = connections;
                    }
                    // Otra cuenta (no la primera que se sabe, o una `ref` que no es la de la
                    // copia): puede venir con el tablero vacío.
                    let known = acct.0.is_some() || acct.1.is_some();
                    let saved = cache.lock().account_ref.clone();
                    let other_ref = acct.1.is_some() && saved.is_some() && acct.1 != saved;
                    let moved = known
                        && (other_ref || account.as_ref().is_some_and(|a| *a != acct));
                    if known {
                        account = Some(acct);
                    }
                    if !(fresh || moved && now) {
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
                _ = cache.probe_wanted.notified() => true,
                _ = tick.tick() => true,
            };
            if connected {
                cache.refresh(&keys, probe).await;
            }
        }
    });
}

/// La tarea del llavero: cada vez que cambia el grupo o la copia, al conectar con el hub y cada
/// `SYNC_EVERY`, `BoardCache::upkeep`. Va aparte de `spawn`, que atiende una cosa cada vez:
/// recifrar 2000 objetos a ritmo lleva más de una hora, y mientras tanto la copia tiene que
/// seguir al día (es la que dice si otro nido ya recifró algo).
pub fn spawn_upkeep<H: KeyHolder>(cache: Arc<BoardCache>, holder: Arc<H>) {
    spawn_upkeep_with(cache, holder, SYNC_EVERY);
}

/// `spawn_upkeep`, cada `every` (para las pruebas).
pub fn spawn_upkeep_with<H: KeyHolder>(cache: Arc<BoardCache>, holder: Arc<H>, every: Duration) {
    tokio::spawn(async move {
        let mut group = holder.keychain().subscribe();
        let mut copy = cache.subscribe();
        let mut status = cache.hub.status.subscribe();
        // El primer tic, ya.
        let mut tick = tokio::time::interval(every);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                r = group.changed() => if r.is_err() { return },
                r = copy.changed() => if r.is_err() { return },
                r = status.changed() => {
                    if r.is_err() {
                        return;
                    }
                    if !status.borrow_and_update().connected {
                        continue;
                    }
                }
                _ = tick.tick() => {}
            }
            // Vistos, y soltados antes de usar el grupo (`clear` avisa con él cogido).
            let _ = *group.borrow_and_update();
            let _ = *copy.borrow_and_update();
            cache.upkeep(&holder).await;
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
    /// Cada cuánto sincroniza `spawn` en las pruebas que no van de eso.
    const LONG: Duration = Duration::from_secs(3600);

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

    fn keys() -> Keys {
        Arc::new(keyring)
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
        rig_gap(b, GAP).await
    }

    /// `rig`, con otro ritmo para la resubida.
    async fn rig_gap(b: fake::Board, gap: Duration) -> Rig {
        let dir = tempfile::tempdir().unwrap();
        let board = Arc::new(Mutex::new(b));
        let rec = fake::board(board.clone()).await;
        let hub = fake::nido(dir.path(), rec.addr).await;
        let cache = BoardCache::load_with(dir.path().join("data/board.json"), hub.clone(), gap);
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
            writes(&self.board)
        }

        /// La copia como quedó en el disco.
        async fn on_disk(&self) -> Arc<BoardCache> {
            self.cache.flush().await;
            BoardCache::load_with(self.cache.path.clone(), self.hub.clone(), GAP)
        }
    }

    fn writes(board: &Mutex<fake::Board>) -> Vec<(String, String, i64)> {
        board
            .lock()
            .unwrap()
            .writes
            .iter()
            .map(|w| (w.1.clone(), w.2.clone(), w.3))
            .collect()
    }

    /// Espera (como mucho 10 s) a que se cumpla `ok`.
    async fn until(what: &str, ok: impl Fn() -> bool) {
        let wait = async {
            while !ok() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        };
        tokio::time::timeout(Duration::from_secs(10), wait)
            .await
            .unwrap_or_else(|_| panic!("no llegó: {what}"));
    }

    /// Cuántas veces se ha escrito la copia en el disco.
    fn saved(c: &BoardCache) -> usize {
        c.disk_writes.lock().unwrap().len()
    }

    /// La copia como está en el disco, sin guardar antes.
    fn disk(r: &Rig) -> Arc<BoardCache> {
        BoardCache::load_with(r.cache.path.clone(), r.hub.clone(), GAP)
    }

    fn unreadable(c: &BoardCache) -> Vec<(String, Unreadable)> {
        c.decrypted(&keyring()).unreadable
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
        let again = r.on_disk().await;
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

    /// Con `reset` la copia se rehace desde 0: lo que ya no está en el hub (una lápida caducada)
    /// desaparece. Se monta aparte: si se corta a medias, la copia sigue como estaba.
    #[tokio::test]
    async fn a_reset_rebuilds_the_copy_from_zero() {
        let r = rig(fake::Board {
            page: 1,
            ..Default::default()
        })
        .await;
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
        // Se corta en la segunda página de la lectura desde 0.
        let hub = r.hub.clone();
        let started = r.board.lock().unwrap().gets.len();
        r.board.lock().unwrap().on_get = Some(Box::new(move |n| {
            if n == started + 3 {
                hub.set_url(Some("ws://127.0.0.1:9".into()), None).unwrap();
            }
        }));
        assert!(r.cache.sync().await.is_err());
        assert_eq!(r.cache.ids(), vec![bid(1), bid(2)]);
        assert_eq!(r.cache.next(), old_next);

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
        let three_ = r.write(&K_CUR, &bid(3), &project("tres"));
        assert!(r.cache.sync().await.unwrap());
        assert_eq!(r.cache.ids(), vec![bid(1), bid(3)]);
        assert_eq!(r.cache.next(), three_.at);
        let gets = r.board.lock().unwrap().gets.clone();
        assert_eq!(&gets[gets.len() - 2..], &[(old_next, None), (0, None)]);
        let _ = three;
    }

    /// Un hub que pagina mal no deja a nido en un bucle ni con la copia a medias.
    #[tokio::test]
    async fn bad_paging_is_an_error() {
        fn id(n: u8) -> String {
            bid(n)
        }
        fn item(n: u8, at: i64) -> serde_json::Value {
            serde_json::json!({
                "id": id(n), "v": 1, "kid": "", "blob": null, "gone": true, "at": at,
            })
        }
        type Script = Box<dyn Fn(i64, Option<&str>) -> serde_json::Value + Send>;
        let cases: Vec<(&str, Script)> = vec![
            (
                "el cursor no avanza",
                Box::new(move |_, _| {
                    serde_json::json!({ "items": [item(1, 5)], "next": 5, "more": true,
                        "after": format!("5:{}", id(1)), "reset": false })
                }),
            ),
            (
                "un objeto antes del cursor",
                Box::new(move |_, after| match after {
                    None => serde_json::json!({ "items": [item(1, 10)], "next": 10, "more": true,
                        "after": format!("10:{}", id(1)), "reset": false }),
                    Some(_) => serde_json::json!({ "items": [item(2, 5)], "next": 10,
                        "more": false, "after": null, "reset": false }),
                }),
            ),
            (
                "páginas sin fin",
                Box::new(move |_, after| {
                    let n: i64 = after.map_or(0, |a| a.split(':').next().unwrap().parse().unwrap());
                    serde_json::json!({ "items": [], "next": 0, "more": true,
                        "after": format!("{}:{}", n + 1, id(1)), "reset": false })
                }),
            ),
            (
                "un cursor que no es un cursor",
                Box::new(|_, _| {
                    serde_json::json!({ "items": [], "next": 0, "more": true,
                        "after": "mañana", "reset": false })
                }),
            ),
            (
                "reset desde 0",
                Box::new(|_, _| {
                    serde_json::json!({ "items": [], "next": 0, "more": false,
                        "after": null, "reset": true })
                }),
            ),
        ];
        for (name, script) in cases {
            let dir = tempfile::tempdir().unwrap();
            let rec = fake::recording(move |v| {
                let b = v.get("board")?;
                let mut page = script(b["since"].as_i64().unwrap_or(0), b["after"].as_str());
                page["req"] = b["req"].clone();
                page["status"] = 200.into();
                Some(fake::Reply::Send(serde_json::json!({ "boarded": page })))
            })
            .await;
            let hub = fake::nido(dir.path(), rec.addr).await;
            let cache = BoardCache::load_with(dir.path().join("board.json"), hub, GAP);
            let r = tokio::time::timeout(Duration::from_secs(10), cache.sync())
                .await
                .unwrap_or_else(|_| panic!("{name}: no acabó"));
            assert_eq!(
                r,
                Err(BoardError::Hub("paginación no válida".into())),
                "{name}"
            );
            assert!(cache.ids().is_empty(), "{name}");
            assert_eq!(cache.next(), 0, "{name}");
        }
    }

    /// Si se vacía la copia a mitad de una sincronización, lo leído ya no entra.
    #[tokio::test]
    async fn a_clear_in_the_middle_of_a_sync_wins() {
        let r = rig(fake::Board {
            page: 1,
            ..Default::default()
        })
        .await;
        for n in 1..=3 {
            r.write(&K_CUR, &bid(n), &project("x"));
        }
        let cache = r.cache.clone();
        r.board.lock().unwrap().on_get = Some(Box::new(move |n| {
            if n == 2 {
                cache.clear();
            }
        }));
        assert!(!r.cache.sync().await.unwrap());
        assert!(r.cache.ids().is_empty());
        assert_eq!(r.cache.next(), 0);
        assert!(r.on_disk().await.ids().is_empty());
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

    /// Cambiar algo que otro borró: queda borrado aquí. Cambiar algo que el hub no tiene: no se
    /// entierra, se marca como que no está en el hub y se pide un sondeo.
    #[tokio::test]
    async fn updating_what_the_hub_does_not_have() {
        let r = rig(fake::Board::default()).await;
        let (deleted, lost) = (bid(1), bid(2));
        r.write(&K_CUR, &deleted, &project("uno"));
        r.write(&K_CUR, &lost, &project("dos"));
        r.cache.sync().await.unwrap();
        r.board.lock().unwrap().write(&deleted, None);
        r.board.lock().unwrap().items.remove(&lost);
        let e = r
            .cache
            .update(&keyring(), &deleted, |_| {})
            .await
            .unwrap_err();
        assert!(matches!(e, EditError::Missing), "{e:?}");
        let s = r.cache.item(&deleted).unwrap();
        assert!(s.gone && s.v == 2, "{s:?}");
        let e = r.cache.update(&keyring(), &lost, |_| {}).await.unwrap_err();
        assert!(matches!(e, EditError::Missing), "{e:?}");
        assert!(!r.cache.item(&lost).unwrap().gone);
        assert_eq!(
            unreadable(&r.cache),
            vec![(lost.clone(), Unreadable::NotInHub)]
        );
        tokio::time::timeout(Duration::from_secs(1), r.cache.probe_wanted.notified())
            .await
            .expect("no se pidió el sondeo");
        // Y no se vuelve a intentar escribir.
        let e = r.cache.update(&keyring(), &lost, |_| {}).await.unwrap_err();
        assert!(
            matches!(e, EditError::Unreadable(Unreadable::NotInHub)),
            "{e:?}"
        );
        assert_eq!(r.writes().len(), 2);
    }

    /// Un borrado que otro ya hizo (o uno repetido tras un corte): 409 con la lápida, y cuenta
    /// como hecho. Uno ya borrado en la copia no pide nada. Uno que el hub ni tiene, fuera de la
    /// copia (y se pide un sondeo).
    #[tokio::test]
    async fn a_repeated_delete_counts_as_done() {
        let r = rig(fake::Board::default()).await;
        let id = bid(9);
        r.write(&K_CUR, &id, &project("x"));
        r.cache.sync().await.unwrap();
        r.board.lock().unwrap().write(&id, None);
        r.cache.remove(&keyring(), &id).await.unwrap();
        let s = r.cache.item(&id).unwrap();
        assert!(s.gone && s.blob.is_none(), "{s:?}");
        assert_eq!(s.v, 2);
        r.cache.remove(&keyring(), &id).await.unwrap();
        assert_eq!(r.writes(), vec![("del".into(), id.clone(), 1)]);
        let lost = bid(11);
        r.write(&K_CUR, &lost, &project("perdido"));
        r.cache.sync().await.unwrap();
        r.board.lock().unwrap().items.remove(&lost);
        r.cache.remove(&keyring(), &lost).await.unwrap();
        assert!(r.cache.item(&lost).is_none());
        tokio::time::timeout(Duration::from_secs(1), r.cache.probe_wanted.notified())
            .await
            .expect("no se pidió el sondeo");
        // Uno vivo: se borra.
        let other = bid(10);
        r.write(&K_CUR, &other, &project("y"));
        r.cache.sync().await.unwrap();
        r.cache.remove(&keyring(), &other).await.unwrap();
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

    /// `BadObject` lleva trozos de lo descifrado: no salen ni con `{:?}` ni con `{}`.
    #[test]
    fn bad_object_does_not_show_its_text() {
        let e = BlobError::BadObject("secreto del objeto".into());
        assert_eq!(format!("{e:?}"), "BadObject(..)");
        assert!(!e.to_string().contains("secreto"));
        let e = EditError::Blob(BlobError::BadObject("secreto del objeto".into()));
        assert!(!format!("{e:?}").contains("secreto"));
        assert!(!e.to_string().contains("secreto"));
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

    /// Lo que se escribe en el disco: al final del hueco (`SAVE_GAP`) que abre el primer cambio,
    /// con lo que llegue mientras tanto; entre dos escrituras pasa al menos `SAVE_GAP` (la
    /// máquina cargada solo lo alarga). Vaciar se escribe ya.
    #[tokio::test]
    async fn the_copy_is_saved_at_the_end_of_the_gap() {
        let r = rig(fake::Board::default()).await;
        let id = bid(1);
        r.write(&K_CUR, &id, &goal(""));
        let t0 = std::time::Instant::now();
        r.cache.sync().await.unwrap();
        until("el primer guardado", || saved(&r.cache) == 1).await;
        let first = r.cache.disk_writes.lock().unwrap()[0];
        assert!(first - t0 >= SAVE_GAP, "se guardó al empezar el hueco");
        for n in 0..5 {
            r.cache
                .update(&keyring(), &id, |o| {
                    if let Object::Objective(g) = o {
                        g.note = format!("nota {n}");
                    }
                })
                .await
                .unwrap();
        }
        let last = r.cache.item(&id);
        until("lo último, en el disco", || disk(&r).item(&id) == last).await;
        // Con `disk`, ninguna escritura a medias: ya está apuntada.
        drop(r.cache.disk.lock().unwrap());
        let at = r.cache.disk_writes.lock().unwrap().clone();
        assert!(at.len() >= 2, "{at:?}");
        for w in at.windows(2) {
            assert!(w[1] - w[0] >= SAVE_GAP, "dos guardados seguidos: {at:?}");
        }
        r.cache.clear();
        assert_eq!(saved(&r.cache), at.len() + 1, "vaciar se escribe ya");
    }

    /// Una edición y el aviso que le sigue (cuya sincronización solo mueve `next`): un guardado.
    #[tokio::test]
    async fn an_edit_and_its_notice_are_saved_once() {
        let mut r = rig(fake::Board::default()).await;
        let id = bid(1);
        r.write(&K_CUR, &id, &goal(""));
        spawn_with(r.cache.clone(), keyring, LONG);
        // Al conectar, la sincronización y el sondeo.
        assert_eq!(r.rec.next("board").await["op"], "get");
        assert_eq!(r.rec.next("board").await["op"], "get");
        until("el primer guardado", || saved(&r.cache) == 1).await;
        // Con el guardador ya en reposo.
        tokio::time::sleep(SAVE_GAP * 2).await;
        let gets = r.board.lock().unwrap().gets.len();
        r.cache
            .update(&keyring(), &id, |o| {
                if let Object::Objective(g) = o {
                    g.note = "nueva".into();
                }
            })
            .await
            .unwrap();
        let at = r.cache.item(&id).unwrap().at;
        until("la sincronización del aviso", || r.cache.next() == at).await;
        assert_eq!(r.board.lock().unwrap().gets.len(), gets + 1);
        until("el guardado", || saved(&r.cache) == 2).await;
        tokio::time::sleep(SAVE_GAP * 3).await;
        assert_eq!(saved(&r.cache), 2, "la edición y su aviso, un guardado");
        assert_eq!(disk(&r).item(&id), r.cache.item(&id));
    }

    /// Un tic que solo mueve `next` no guarda: va con el próximo cambio de verdad (o al parar).
    #[tokio::test]
    async fn a_tick_that_only_moves_next_does_not_save() {
        let r = rig(fake::Board::default()).await;
        r.write(&K_CUR, &bid(1), &project("uno"));
        spawn_with(r.cache.clone(), keyring, Duration::from_millis(50));
        until("el primer guardado", || saved(&r.cache) == 1).await;
        let floor = r.cache.next() + 1000;
        r.board.lock().unwrap().floor = floor;
        until("el tic", || r.cache.next() == floor).await;
        tokio::time::sleep(SAVE_GAP * 3).await;
        assert_eq!(saved(&r.cache), 1, "solo se movió next");
        assert!(disk(&r).next() < floor);
        r.cache.create(&keyring(), &project("dos")).await.unwrap();
        until("next, en el disco", || disk(&r).next() >= floor).await;
        drop(r.cache.disk.lock().unwrap());
        assert_eq!(saved(&r.cache), 2);
    }

    /// La resubida (decisión 8): una lectura completa sin nada (ni lápidas), confirmada al rato,
    /// teniendo copia. Se sube con `v = 0` lo que se puede leer, con la clave actual y los bytes
    /// tal cual (también un tipo que no conocemos), a ritmo; un 409 es que otro se adelantó y
    /// cuenta como hecho. Lo que no se puede leer se queda, marcado como que no está en el hub,
    /// hasta que el hub vuelve a tener tablero.
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
        r.cache.probe(&keys()).await.unwrap();
        assert!(r.writes().is_empty());

        // El hub se queda sin nada; otro nido de la casa se adelanta con `cur`.
        let theirs = sealed(&K_CUR, &cur, &project("de otro"));
        {
            let mut b = r.board.lock().unwrap();
            b.items.clear();
            b.clash(&cur, Some(theirs));
        }
        r.cache.probe(&keys()).await.unwrap();
        // Dos lecturas desde 0, con un rato entre medias.
        let gets = r.board.lock().unwrap().gets.clone();
        assert_eq!(&gets[gets.len() - 2..], &[(0, None), (0, None)]);
        until("la resubida", || r.cache.pending_reupload() == 0).await;
        let w = r.board.lock().unwrap().writes.clone();
        let mut sent: Vec<(&str, &str, i64)> = w
            .iter()
            .map(|w| (w.1.as_str(), w.2.as_str(), w.3))
            .collect();
        sent.sort();
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
        // Lo que no se podía subir (sin clave) se queda en la copia, como que no está en el
        // hub; la lápida, no.
        assert_eq!(
            r.cache.ids(),
            vec![old.clone(), cur.clone(), unknown.clone(), no_key.clone()]
        );
        assert!(unreadable(&r.cache).contains(&(no_key.clone(), Unreadable::NotInHub)));

        // Ya hay objetos: nada que subir. Lo que el hub no tiene saldría de la copia, pero el que
        // no tiene su clave se queda (puede llegar), marcado.
        r.cache.sync().await.unwrap();
        r.cache.probe(&keys()).await.unwrap();
        tokio::time::sleep(GAP * 3).await;
        assert_eq!(r.board.lock().unwrap().writes.len(), 3);
        assert_eq!(
            r.cache.ids(),
            vec![old.clone(), cur.clone(), unknown.clone(), no_key.clone()]
        );
        let not_in_hub: Vec<_> = unreadable(&r.cache)
            .into_iter()
            .filter(|u| u.1 == Unreadable::NotInHub)
            .collect();
        assert_eq!(not_in_hub, vec![(no_key.clone(), Unreadable::NotInHub)]);
    }

    /// Si en el rato de confirmar alguien escribe, no hay resubida.
    #[tokio::test]
    async fn an_empty_hub_is_confirmed_before_reuploading() {
        let r = rig(fake::Board::default()).await;
        r.write(&K_CUR, &bid(1), &project("uno"));
        r.cache.sync().await.unwrap();
        r.board.lock().unwrap().items.clear();
        let board = r.board.clone();
        let (k, b) = sealed(&K_CUR, &bid(2), &project("nuevo"));
        let at = r.board.lock().unwrap().gets.len();
        r.board.lock().unwrap().on_get = Some(Box::new(move |n| {
            if n == at + 1 {
                // Justo tras la primera lectura: la segunda ya lo ve.
                let mut board = board.clone();
                let (k, b) = (k.clone(), b.clone());
                std::thread::spawn(move || {
                    board_write(&mut board, &bid(2), &k, &b);
                });
            }
        }));
        r.cache.probe(&keys()).await.unwrap();
        tokio::time::sleep(GAP * 3).await;
        assert!(r.writes().is_empty());
        assert_eq!(r.cache.pending_reupload(), 0);
        assert!(r.cache.item(&bid(2)).is_some());
    }

    fn board_write(board: &mut Arc<Mutex<fake::Board>>, id: &str, k: &str, b: &str) {
        board.lock().unwrap().write(id, Some((k, b)));
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
        r.cache.probe(&keys()).await.unwrap();
        tokio::time::sleep(GAP * 3).await;
        assert!(r.writes().is_empty());
        assert_eq!(r.cache.pending_reupload(), 0);

        // Sin copia: ni se mira.
        let empty = rig(fake::Board::default()).await;
        empty.cache.probe(&keys()).await.unwrap();
        tokio::time::sleep(GAP * 3).await;
        assert!(empty.writes().is_empty());
        assert!(empty.board.lock().unwrap().gets.is_empty());
    }

    /// Sin clave actual no se resube ni se toca la copia: se vuelve a mirar en el próximo sondeo.
    /// Y un sondeo que no cambia nada no guarda nada.
    #[tokio::test]
    async fn no_reupload_without_a_key() {
        let r = rig(fake::Board::default()).await;
        r.write(&K_CUR, &bid(1), &project("uno"));
        r.cache.sync().await.unwrap();
        r.cache.flush().await;
        let next = r.cache.next();
        r.board.lock().unwrap().items.clear();
        let before = saved(&r.cache);
        let rev = *r.cache.subscribe().borrow();
        let none: Keys = Arc::new(|| Keyring { keys: vec![] });
        r.cache.probe(&none).await.unwrap();
        tokio::time::sleep(GAP * 3).await;
        assert!(r.writes().is_empty());
        assert_eq!(r.cache.ids(), vec![bid(1)]);
        assert_eq!(r.cache.pending_reupload(), 0);
        assert_eq!(r.cache.next(), next);
        assert_eq!(saved(&r.cache), before);
        assert_eq!(*r.cache.subscribe().borrow(), rev);
        // Con la clave, sí.
        r.cache.probe(&keys()).await.unwrap();
        until("la resubida", || r.cache.pending_reupload() == 0).await;
        assert_eq!(on_hub(&r.board, &K_CUR, &bid(1)), project("uno"));
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
        r.cache.probe(&keys()).await.unwrap();
        until("la resubida", || r.cache.pending_reupload() == 0).await;
        let w = r.board.lock().unwrap().writes.clone();
        assert_eq!(w.len(), 2);
        assert!(w[1].0 - w[0].0 >= GAP * BUSY_GAPS - Duration::from_millis(5));
        assert_eq!(on_hub(&r.board, &K_CUR, &bid(1)), project("uno"));
    }

    /// Lo que el hub no quiere en la resubida (demasiado grande, 400, la cuenta llena) se queda,
    /// marcado como que no está en el hub.
    #[tokio::test]
    async fn what_the_hub_refuses_in_the_reupload_stays_as_not_in_hub() {
        let r = rig(fake::Board::default()).await;
        let (big, bad, fine) = (bid(1), bid(2), bid(3));
        for id in [&big, &bad, &fine] {
            r.write(&K_CUR, id, &project("x"));
        }
        r.cache.sync().await.unwrap();
        {
            let mut b = r.board.lock().unwrap();
            b.items.clear();
            b.too_big.insert(big.clone());
            b.bad.insert(bad.clone());
        }
        r.cache.probe(&keys()).await.unwrap();
        until("la resubida", || r.cache.pending_reupload() == 0).await;
        assert_eq!(
            r.board
                .lock()
                .unwrap()
                .items
                .keys()
                .cloned()
                .collect::<Vec<_>>(),
            vec![fine.clone()]
        );
        let mut u = unreadable(&r.cache);
        u.sort();
        assert_eq!(
            u,
            vec![
                (big.clone(), Unreadable::NotInHub),
                (bad.clone(), Unreadable::NotInHub)
            ]
        );

        // La cuenta llena: lo que falta, todo marcado.
        let r = rig(fake::Board {
            limit: 1,
            ..Default::default()
        })
        .await;
        for n in 1..=3 {
            r.write(&K_CUR, &bid(n), &project("x"));
        }
        r.board.lock().unwrap().limit = 10;
        r.cache.sync().await.unwrap();
        {
            let mut b = r.board.lock().unwrap();
            b.items.clear();
            b.limit = 1;
        }
        r.cache.probe(&keys()).await.unwrap();
        until("la resubida", || r.cache.pending_reupload() == 0).await;
        tokio::time::sleep(GAP * 3).await;
        assert_eq!(r.board.lock().unwrap().items.len(), 1);
        assert_eq!(r.writes().len(), 2, "tras la cuenta llena, no sigue");
        assert_eq!(unreadable(&r.cache).len(), 2);
    }

    /// Cambiar o borrar algo que falta por resubir: el cambio lo crea en el hub; el borrado lo
    /// crea y lo borra, para que quede la lápida.
    #[tokio::test]
    async fn editing_what_is_waiting_to_go_back_up() {
        let r = rig(fake::Board::default()).await;
        let (a, b) = (bid(1), bid(2));
        r.write(&K_CUR, &a, &goal(""));
        r.write(&K_CUR, &b, &goal(""));
        r.cache.sync().await.unwrap();
        {
            let mut board = r.board.lock().unwrap();
            board.items.clear();
            // La primera de la resubida da 429: se queda esperando un buen rato.
            board.busy = 1;
        }
        r.cache.probe(&keys()).await.unwrap();
        until("el 429", || r.writes().len() == 1).await;
        assert_eq!(r.cache.pending_reupload(), 2);
        r.cache
            .update(&keyring(), &a, |o| {
                if let Object::Objective(g) = o {
                    g.note = "cambiada".into();
                }
            })
            .await
            .unwrap();
        r.cache.remove(&keyring(), &b).await.unwrap();
        assert_eq!(r.cache.pending_reupload(), 0);
        assert_eq!(
            r.writes()[1..].to_vec(),
            vec![
                ("put".into(), a.clone(), 0),
                ("put".into(), b.clone(), 0),
                ("del".into(), b.clone(), 1)
            ]
        );
        let Object::Objective(g) = on_hub(&r.board, &K_CUR, &a) else {
            panic!()
        };
        assert_eq!(g.note, "cambiada");
        assert!(r.board.lock().unwrap().items[&b].gone);
        assert!(r.cache.item(&b).unwrap().gone);
    }

    /// Si se vacía la copia a mitad de la resubida, no sigue.
    #[tokio::test]
    async fn a_clear_stops_the_reupload() {
        let r = rig(fake::Board::default()).await;
        for n in 1..=3 {
            r.write(&K_CUR, &bid(n), &project("x"));
        }
        r.cache.sync().await.unwrap();
        r.board.lock().unwrap().items.clear();
        let cache = r.cache.clone();
        r.board.lock().unwrap().on_write = Some(Box::new(move |n| {
            if n == 1 {
                cache.clear();
            }
        }));
        r.cache.probe(&keys()).await.unwrap();
        tokio::time::sleep(GAP * 5).await;
        assert_eq!(r.writes().len(), 1);
        assert!(r.cache.ids().is_empty());
        assert_eq!(r.cache.pending_reupload(), 0);
    }

    /// Una resubida cortada (se cae la conexión) sigue al volver el hub, desde donde se quedó:
    /// lo hecho está en el disco, y nada se sube dos veces.
    #[tokio::test]
    async fn a_cut_reupload_goes_on_after_reconnecting() {
        // Lento, para que la conexión se caiga entre la primera escritura y la segunda.
        let r = rig_gap(fake::Board::default(), Duration::from_millis(800)).await;
        for n in 1..=3 {
            r.write(&K_CUR, &bid(n), &project(&format!("p{n}")));
        }
        r.cache.sync().await.unwrap();
        r.board.lock().unwrap().items.clear();
        let mut status = r.hub.status.subscribe();
        spawn_with(r.cache.clone(), keyring, LONG);
        until("la primera escritura", || r.cache.pending_reupload() == 2).await;
        r.rec.tell(fake::Tell::Hangup);
        tokio::time::timeout(Duration::from_secs(10), status.wait_for(|s| !s.connected))
            .await
            .unwrap()
            .unwrap();
        // Lo hecho, en el disco (lo guarda la tarea que guarda, a su ritmo).
        let path = r.cache.path.clone();
        let hub = r.hub.clone();
        until("lo hecho en el disco", || {
            BoardCache::load_with(path.clone(), hub.clone(), GAP).pending_reupload() == 2
        })
        .await;
        until("la resubida", || r.cache.pending_reupload() == 0).await;
        for n in 1..=3 {
            assert_eq!(on_hub(&r.board, &K_CUR, &bid(n)), project(&format!("p{n}")));
        }
        let mut ids: Vec<String> = r.writes().into_iter().map(|w| w.1).collect();
        ids.sort();
        assert_eq!(ids, vec![bid(1), bid(2), bid(3)], "una vez cada uno");
    }

    /// Con la conexión: lo primero, un `get`; con un aviso del hub, otra sincronización. Y al
    /// reconectar, otra vez un `get`.
    #[tokio::test]
    async fn the_notice_and_each_connection_bring_a_sync() {
        let mut r = rig(fake::Board::default()).await;
        r.write(&K_CUR, &bid(1), &project("uno"));
        spawn_with(r.cache.clone(), keyring, LONG);
        until("la primera", || r.cache.item(&bid(1)).is_some()).await;
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
        until("con el aviso", || r.cache.item(&bid(2)).is_some()).await;
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
        until("al reconectar", || r.cache.item(&bid(3)).is_some()).await;
    }

    /// Cada `SYNC_EVERY`, aunque no haya aviso.
    #[tokio::test]
    async fn the_tick_brings_a_sync() {
        let r = rig(fake::Board::default()).await;
        spawn(r.cache.clone(), keyring);
        until("la primera", || !r.board.lock().unwrap().gets.is_empty()).await;
        r.write(&K_CUR, &bid(1), &project("uno"));
        until("con el tic", || r.cache.item(&bid(1)).is_some()).await;
    }

    /// Dos cortes seguidos mientras se atiende otra cosa: al acabar, la conexión nueva tiene su
    /// `get` (sin él, el hub no le avisa de nada).
    #[tokio::test]
    async fn reconnections_during_a_slow_refresh_are_not_lost() {
        let mut r = rig(fake::Board::default()).await;
        let hold = r.cache.hold.lock().await;
        let mut status = r.hub.status.subscribe();
        spawn_with(r.cache.clone(), keyring, LONG);
        assert_eq!(r.rec.next("board").await["op"], "get");
        for _ in 0..2 {
            r.rec.tell(fake::Tell::Hangup);
            tokio::time::timeout(Duration::from_secs(10), status.wait_for(|s| !s.connected))
                .await
                .unwrap()
                .unwrap();
            tokio::time::timeout(Duration::from_secs(10), status.wait_for(|s| s.connected))
                .await
                .unwrap()
                .unwrap();
        }
        assert_eq!(r.hub.status.borrow().connections, 3);
        let before = r.board.lock().unwrap().gets.len();
        drop(hold);
        until("el get de la conexión nueva", || {
            r.board.lock().unwrap().gets.len() > before
        })
        .await;
    }

    /// Otra cuenta (otro `ref`) puede venir con el tablero vacío: se mira enseguida. La primera
    /// que se sabe, no.
    #[tokio::test]
    async fn another_account_brings_a_probe() {
        let r = rig(fake::Board::default()).await;
        r.write(&K_CUR, &bid(1), &project("uno"));
        spawn_with(r.cache.clone(), keyring, LONG);
        until("la primera", || r.cache.item(&bid(1)).is_some()).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        r.board.lock().unwrap().items.clear();
        let account = |r_: &str| {
            fake::Tell::Send(serde_json::json!({
                "account": { "name": "mirlo-sereno", "lang": "es", "ref": r_ }
            }))
        };
        r.rec.tell(account("0123456789abcdef"));
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(r.writes().is_empty(), "con la primera cuenta, nada");
        r.rec.tell(account("fedcba9876543210"));
        until("la resubida", || r.writes().len() == 1).await;
        assert_eq!(on_hub(&r.board, &K_CUR, &bid(1)), project("uno"));
    }

    /// Con otro hub, se vuelve a leer desde 0, sin tirar lo que se tiene.
    #[tokio::test]
    async fn another_hub_reads_from_zero_keeping_the_copy() {
        let r = rig(fake::Board::default()).await;
        r.write(&K_CUR, &bid(1), &project("uno"));
        r.cache.sync().await.unwrap();
        assert!(r.cache.next() > 0);
        // Otro hub (aquí, el mismo con otro nombre en la copia).
        r.cache.lock().hub = Some("https://otro.example".into());
        r.write(&K_CUR, &bid(2), &project("dos"));
        r.cache.sync().await.unwrap();
        let last = r.board.lock().unwrap().gets.last().cloned().unwrap();
        assert_eq!(last, (0, None));
        assert_eq!(r.cache.ids(), vec![bid(1), bid(2)]);
        assert_eq!(r.cache.lock().hub, r.hub.url());
    }

    /// Con otro hub, lo que la copia tiene y ese hub no queda marcado como que no está en el hub,
    /// y se pide un sondeo, que decide: si el hub tiene tablero, fuera de la copia, salvo lo que
    /// no se puede leer (sin su clave), que se queda marcado; si no tiene nada, se resube.
    #[tokio::test]
    async fn another_hub_leaves_what_it_lacks_to_the_probe() {
        let r = rig(fake::Board::default()).await;
        let (cur, alien, other) = (bid(1), bid(2), bid(3));
        r.write(&K_CUR, &cur, &project("uno"));
        r.write(&[0x33; 32], &alien, &project("ajena"));
        r.cache.sync().await.unwrap();
        r.cache.lock().hub = Some("https://otro.example".into());
        r.board.lock().unwrap().items.clear();
        r.write(&K_CUR, &other, &project("tres"));
        r.cache.sync().await.unwrap();
        tokio::time::timeout(Duration::from_secs(1), r.cache.probe_wanted.notified())
            .await
            .expect("no se pidió el sondeo");
        assert_eq!(
            unreadable(&r.cache),
            vec![
                (cur.clone(), Unreadable::NotInHub),
                (alien.clone(), Unreadable::NotInHub)
            ]
        );
        r.cache.probe(&keys()).await.unwrap();
        assert_eq!(r.cache.ids(), vec![alien.clone(), other.clone()]);
        assert_eq!(
            unreadable(&r.cache),
            vec![(alien.clone(), Unreadable::NotInHub)]
        );
        assert!(r.writes().is_empty());

        // Otro hub más, sin nada: se vuelve a subir lo que se puede leer.
        r.cache.lock().hub = Some("https://vacio.example".into());
        r.board.lock().unwrap().items.clear();
        r.cache.sync().await.unwrap();
        tokio::time::timeout(Duration::from_secs(1), r.cache.probe_wanted.notified())
            .await
            .expect("no se pidió el sondeo");
        r.cache.probe(&keys()).await.unwrap();
        until("la resubida", || r.cache.pending_reupload() == 0).await;
        assert_eq!(r.writes(), vec![("put".into(), other.clone(), 0)]);
        assert_eq!(on_hub(&r.board, &K_CUR, &other), project("tres"));
        assert_eq!(
            unreadable(&r.cache),
            vec![(alien.clone(), Unreadable::NotInHub)]
        );
    }

    const REF_A: &str = "0123456789abcdef";
    const REF_B: &str = "fedcba9876543210";

    fn with_ref(r: &str) -> fake::Board {
        fake::Board {
            account_ref: Some(r.into()),
            ..Default::default()
        }
    }

    /// Otra `ref` en las páginas (el nido se fue solo a otra cuenta, o se partió el grupo): el
    /// `since` guardado no vale en esa cuenta y se vuelve a leer desde 0, sin vaciar la copia, y
    /// `next` pasa a ser el de esa cuenta. Lo que no vino queda marcado como que no está en el
    /// hub, y el sondeo lo saca (salvo lo que no tiene su clave).
    #[tokio::test]
    async fn another_account_ref_reads_from_zero() {
        let r = rig(with_ref(REF_A)).await;
        let (cur, alien, theirs) = (bid(1), bid(2), bid(3));
        r.write(&K_CUR, &cur, &project("uno"));
        r.write(&[0x33; 32], &alien, &project("ajena"));
        r.cache.sync().await.unwrap();
        assert_eq!(r.cache.lock().account_ref.as_deref(), Some(REF_A));
        let since = r.cache.next();
        // La otra cuenta tiene su tablero, con marcas que no tienen que ver (aquí, más viejas).
        {
            let mut b = r.board.lock().unwrap();
            b.account_ref = Some(REF_B.into());
            b.items.clear();
            b.clock = fake::BOARD_T0 - 10_000_000;
        }
        let t = r.write(&K_CUR, &theirs, &project("suyo"));
        r.cache.sync().await.unwrap();
        let gets = r.board.lock().unwrap().gets.clone();
        assert_eq!(&gets[gets.len() - 2..], &[(since, None), (0, None)]);
        assert_eq!(r.cache.next(), t.at);
        assert_eq!(r.cache.lock().account_ref.as_deref(), Some(REF_B));
        tokio::time::timeout(Duration::from_secs(1), r.cache.probe_wanted.notified())
            .await
            .expect("no se pidió el sondeo");
        assert_eq!(
            r.cache.ids(),
            vec![cur.clone(), alien.clone(), theirs.clone()]
        );
        assert_eq!(
            unreadable(&r.cache),
            vec![
                (cur.clone(), Unreadable::NotInHub),
                (alien.clone(), Unreadable::NotInHub)
            ]
        );
        r.cache.probe(&keys()).await.unwrap();
        assert_eq!(r.cache.ids(), vec![alien.clone(), theirs.clone()]);
        assert_eq!(
            unreadable(&r.cache),
            vec![(alien.clone(), Unreadable::NotInHub)]
        );
        assert!(r.writes().is_empty());
        assert_eq!(r.on_disk().await.lock().account_ref.as_deref(), Some(REF_B));
        // La siguiente, desde su `next`.
        let next = r.cache.next();
        r.cache.sync().await.unwrap();
        let last = r.board.lock().unwrap().gets.last().cloned().unwrap();
        assert_eq!(last, (next, None));
    }

    /// Otra cuenta con los mismos objetos: no cambia nada que se vea, pero la `ref` y el `next`
    /// nuevos van al disco al final del hueco, sin esperar a otro cambio (si no, tras un corte
    /// brusco habría otra lectura completa).
    #[tokio::test]
    async fn another_account_with_nothing_new_is_saved() {
        let r = rig(with_ref(REF_A)).await;
        r.write(&K_CUR, &bid(1), &project("uno"));
        r.cache.sync().await.unwrap();
        // El guardado de esa, hecho: el guardador, en reposo.
        until("el primer guardado", || saved(&r.cache) == 1).await;
        let rev = *r.cache.subscribe().borrow();
        r.board.lock().unwrap().account_ref = Some(REF_B.into());
        assert!(!r.cache.sync().await.unwrap());
        assert_eq!(
            *r.cache.subscribe().borrow(),
            rev,
            "no cambió nada que se vea"
        );
        until("la ref nueva, en el disco", || {
            disk(&r).lock().account_ref.as_deref() == Some(REF_B)
        })
        .await;
        assert_eq!(disk(&r).next(), r.cache.next());
    }

    /// El repaso (`prune`) con una lectura de otra cuenta que la de la copia no toca nada: pide
    /// un sondeo, cuya sincronización se pone antes al día con esa cuenta.
    #[tokio::test]
    async fn prune_leaves_another_account_to_the_probe() {
        let r = rig(with_ref(REF_A)).await;
        let (cur, other) = (bid(1), bid(2));
        r.write(&K_CUR, &cur, &project("uno"));
        r.write(&K_CUR, &other, &project("dos"));
        r.cache.sync().await.unwrap();
        {
            let mut b = r.board.lock().unwrap();
            b.items.remove(&cur);
            b.account_ref = Some(REF_B.into());
        }
        r.cache.lock().missing.insert(cur.clone());
        r.cache.probe(&keys()).await.unwrap();
        assert_eq!(r.cache.ids(), vec![cur.clone(), other.clone()]);
        assert_eq!(
            unreadable(&r.cache),
            vec![(cur.clone(), Unreadable::NotInHub)]
        );
        assert_eq!(r.cache.lock().account_ref.as_deref(), Some(REF_A));
        tokio::time::timeout(Duration::from_secs(1), r.cache.probe_wanted.notified())
            .await
            .expect("no se pidió el sondeo");
    }

    /// La primera `ref`, con la copia vacía: una lectura, sin marcar nada ni pedir sondeo.
    #[tokio::test]
    async fn the_first_ref_with_an_empty_copy() {
        let r = rig(with_ref(REF_A)).await;
        r.write(&K_CUR, &bid(1), &project("uno"));
        r.cache.sync().await.unwrap();
        assert_eq!(r.board.lock().unwrap().gets, vec![(0, None)]);
        assert_eq!(r.cache.ids(), vec![bid(1)]);
        assert!(unreadable(&r.cache).is_empty());
        assert_eq!(r.cache.lock().account_ref.as_deref(), Some(REF_A));
        let probe =
            tokio::time::timeout(Duration::from_millis(200), r.cache.probe_wanted.notified());
        assert!(probe.await.is_err(), "un sondeo de más");
    }

    /// Un hub que no manda `ref` sigue como siempre: desde el `next` guardado, sin marcar nada.
    /// Y la `ref` que tuviera la copia se queda: que no venga no es otra cuenta.
    #[tokio::test]
    async fn a_hub_without_ref_works_as_before() {
        let r = rig(fake::Board::default()).await;
        r.write(&K_CUR, &bid(1), &project("uno"));
        r.cache.sync().await.unwrap();
        assert_eq!(r.cache.lock().account_ref, None);
        for n in [2, 3] {
            let next = r.cache.next();
            r.write(&K_CUR, &bid(n), &project("otro"));
            r.cache.sync().await.unwrap();
            let last = r.board.lock().unwrap().gets.last().cloned().unwrap();
            assert_eq!(last, (next, None));
            r.cache.lock().account_ref = Some(REF_A.into());
        }
        assert_eq!(r.cache.ids(), vec![bid(1), bid(2), bid(3)]);
        assert!(unreadable(&r.cache).is_empty());
        assert_eq!(r.cache.lock().account_ref.as_deref(), Some(REF_A));
        let probe =
            tokio::time::timeout(Duration::from_millis(200), r.cache.probe_wanted.notified());
        assert!(probe.await.is_err(), "un sondeo de más");
    }

    /// Un hub de guion: la llamada `n` (desde 0) a `get` da `page(n)`.
    async fn scripted(
        dir: &Path,
        page: impl Fn(usize) -> serde_json::Value + Send + 'static,
    ) -> (
        fake::Recorder,
        Arc<BoardCache>,
        Arc<std::sync::atomic::AtomicUsize>,
    ) {
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let n = calls.clone();
        let rec = fake::recording(move |v| {
            let b = v.get("board")?;
            let mut p = page(n.fetch_add(1, Ordering::SeqCst));
            p["req"] = b["req"].clone();
            p["status"] = 200.into();
            Some(fake::Reply::Send(serde_json::json!({ "boarded": p })))
        })
        .await;
        let hub = fake::nido(dir, rec.addr).await;
        let cache = BoardCache::load_with(dir.join("board.json"), hub, GAP);
        (rec, cache, calls)
    }

    /// Una página del guion: el objeto `n` con marca `at`, la `ref` (si hay) y si sigue.
    fn scripted_page(n: u8, at: i64, r: Option<&str>, more: bool) -> serde_json::Value {
        let mut p = serde_json::json!({
            "items": [{ "id": bid(n), "v": 1, "kid": "", "blob": null, "gone": true, "at": at }],
            "next": at, "more": more, "reset": false,
            "after": if more { serde_json::json!(format!("{at}:{}", bid(n))) } else { serde_json::Value::Null },
        });
        if let Some(r) = r {
            p["ref"] = r.into();
        }
        p
    }

    /// La `ref` cambia a mitad de una lectura: la nueva pasa a ser la esperada y se empieza
    /// otra vez desde 0; con ella se guarda `next`, y solo queda lo de esa lectura.
    #[tokio::test]
    async fn a_ref_change_while_reading_starts_again() {
        let dir = tempfile::tempdir().unwrap();
        let (_rec, cache, calls) = scripted(dir.path(), |n| match n {
            0 => scripted_page(1, 10, Some(REF_A), true),
            1 => scripted_page(2, 20, Some(REF_B), true),
            2 => scripted_page(3, 5, Some(REF_B), true),
            _ => scripted_page(4, 7, Some(REF_B), false),
        })
        .await;
        cache.sync().await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 4);
        assert_eq!(cache.ids(), vec![bid(3), bid(4)]);
        assert_eq!(cache.next(), 7);
        assert_eq!(cache.lock().account_ref.as_deref(), Some(REF_B));
    }

    /// Una página sin `ref` (un hub de antes, mientras se despliega el nuevo) no es otra cuenta:
    /// la esperada se queda como estaba, y la lectura sigue.
    #[tokio::test]
    async fn pages_without_ref_do_not_count() {
        let dir = tempfile::tempdir().unwrap();
        let (_rec, cache, calls) = scripted(dir.path(), |n| match n {
            0 => scripted_page(1, 10, Some(REF_A), true),
            1 => scripted_page(2, 20, None, true),
            2 => scripted_page(3, 30, Some(REF_A), true),
            _ => scripted_page(4, 40, None, false),
        })
        .await;
        cache.sync().await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 4);
        assert_eq!(cache.ids(), vec![bid(1), bid(2), bid(3), bid(4)]);
        assert_eq!(cache.lock().account_ref.as_deref(), Some(REF_A));
    }

    /// Una `ref` que cambia una y otra vez: como mucho `MAX_REF_CHANGES` cambios, y luego un
    /// error, sin tocar la copia.
    #[tokio::test]
    async fn a_ref_that_keeps_changing_gives_up() {
        let dir = tempfile::tempdir().unwrap();
        let (_rec, cache, calls) = scripted(dir.path(), |n| {
            let r = if n % 2 == 0 { REF_A } else { REF_B };
            scripted_page(1, 10, Some(r), true)
        })
        .await;
        let e = tokio::time::timeout(Duration::from_secs(10), cache.sync())
            .await
            .expect("no acabó")
            .unwrap_err();
        assert_eq!(e, BoardError::Hub(REF_FLAPPING.into()));
        assert_eq!(calls.load(Ordering::SeqCst), MAX_REF_CHANGES + 2);
        assert!(cache.ids().is_empty());
        assert_eq!(cache.next(), 0);
        assert_eq!(cache.lock().account_ref, None);
    }

    /// Una `ref` por `{"account"}` que no es la guardada trae una sincronización con sondeo,
    /// aunque sea la primera que se sabe en esta conexión.
    #[tokio::test]
    async fn an_account_ref_other_than_the_saved_one_brings_a_probe() {
        let r = rig(with_ref(REF_A)).await;
        r.write(&K_CUR, &bid(1), &project("uno"));
        spawn_with(r.cache.clone(), keyring, LONG);
        until("la primera", || r.cache.item(&bid(1)).is_some()).await;
        until("el sondeo al conectar", || {
            r.board.lock().unwrap().gets.len() == 2
        })
        .await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        {
            let mut b = r.board.lock().unwrap();
            b.account_ref = Some(REF_B.into());
            b.items.clear();
        }
        r.rec.tell(fake::Tell::Send(serde_json::json!({
            "account": { "name": "mirlo-sereno", "lang": "es", "ref": REF_B }
        })));
        until("la resubida", || r.writes().len() == 1).await;
        assert_eq!(on_hub(&r.board, &K_CUR, &bid(1)), project("uno"));
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
        r.cache.flush().await;
        let mode = std::fs::metadata(&r.cache.path)
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    // ---------- El recifrado y la poda del llavero (Tarea 9) ----------

    const K_NEW: [u8; 32] = [0x33; 32];
    const DAY_MS: u64 = 24 * 3600 * 1000;

    /// Bytes que no son ningún objeto que conozcamos (de una versión futura): se recifran tal
    /// cual.
    const FUTURE: &[u8] = br#"{"t":"futuro","x":[1,2,3],"by":"web"}"#;

    /// Lo que hay en el hub para `id`: su `kid`, su `v` y su texto descifrado con `key`.
    fn hub_bytes(board: &Mutex<fake::Board>, key: &[u8; 32], id: &str) -> (String, i64, Vec<u8>) {
        let b = board.lock().unwrap();
        let item = &b.items[id];
        let bytes = open(key, id, item.blob.as_deref().unwrap()).unwrap();
        (item.kid.clone(), item.v, bytes)
    }

    /// Sin hub: se quita y se espera a que se corte.
    async fn offline(r: &Rig) {
        let mut status = r.hub.status.subscribe();
        r.hub.set_url(None, None).unwrap();
        tokio::time::timeout(Duration::from_secs(10), status.wait_for(|s| !s.connected))
            .await
            .unwrap()
            .unwrap();
    }

    /// Un llavero que se puede cambiar a mitad.
    fn shared(ring: Keyring) -> (Arc<Mutex<Keyring>>, Keys) {
        let ring = Arc::new(Mutex::new(ring));
        let r = ring.clone();
        (ring, Arc::new(move || r.lock().unwrap().clone()))
    }

    /// Lo cifrado con una clave retirada se recifra con la actual, uno a uno y a ritmo: los
    /// bytes tal cual (un tipo que no conocemos también), con compare-and-set. Lo que no tiene su
    /// clave en el llavero, o no se descifra con ella, no se toca. Lo que ya va con la actual,
    /// tampoco.
    #[tokio::test]
    async fn retired_objects_are_recrypted_with_the_current_key() {
        let r = rig(fake::Board::default()).await;
        let (p, g, fut, nokey, broken, fine) = (bid(1), bid(2), bid(3), bid(4), bid(5), bid(6));
        r.write(&K_OLD, &p, &project("uno"));
        r.write(&K_OLD, &g, &goal("nota"));
        let blob = seal(&K_OLD, &fut, FUTURE).unwrap();
        r.board
            .lock()
            .unwrap()
            .write(&fut, Some((&kid(&K_OLD), &blob)));
        r.write(&K_NEW, &nokey, &project("sin clave"));
        // Dice ir con la vieja, pero no se descifra con ella.
        let bad = seal_object(&K_CUR, &broken, &project("roto")).unwrap();
        r.board
            .lock()
            .unwrap()
            .write(&broken, Some((&kid(&K_OLD), &bad)));
        r.write(&K_CUR, &fine, &project("ya está"));
        r.cache.sync().await.unwrap();
        let before = r.writes().len();
        assert!(r.cache.recrypt(&keys()).await, "acabado");
        let w = r.board.lock().unwrap().writes.clone();
        let mut ids: Vec<String> = w[before..].iter().map(|w| w.2.clone()).collect();
        ids.sort();
        assert_eq!(
            ids,
            vec![p.clone(), g.clone(), fut.clone()],
            "una vez cada uno"
        );
        for pair in w[before..].windows(2) {
            assert!(pair[1].0 - pair[0].0 >= GAP - Duration::from_millis(5));
        }
        let cur = kid(&K_CUR);
        assert_eq!(on_hub(&r.board, &K_CUR, &p), project("uno"));
        assert_eq!(on_hub(&r.board, &K_CUR, &g), goal("nota"));
        assert_eq!(
            hub_bytes(&r.board, &K_CUR, &fut),
            (cur.clone(), 2, FUTURE.to_vec())
        );
        // En la copia, como en el hub.
        for id in [&p, &g, &fut] {
            let s = r.cache.item(id).unwrap();
            assert_eq!((s.kid.as_str(), s.v), (cur.as_str(), 2));
        }
        assert_eq!(r.cache.item(&nokey).unwrap().v, 1);
        assert_eq!(r.cache.item(&broken).unwrap().kid, kid(&K_OLD));
        assert_eq!(r.cache.item(&fine).unwrap().v, 1);
        // Otra vez: nada que hacer.
        assert!(r.cache.recrypt(&keys()).await);
        assert_eq!(r.writes().len(), before + 3);
    }

    /// Un 409 a mitad: si lo de ahora sigue con una clave retirada, se recifra eso (lo del
    /// otro); si ya va con la actual, está hecho. Uno que otro borró queda borrado.
    #[tokio::test]
    async fn a_conflict_while_recrypting_is_retried_on_the_current_object() {
        let r = rig(fake::Board::default()).await;
        let (still_old, now_new, deleted) = (bid(1), bid(2), bid(3));
        for id in [&still_old, &now_new, &deleted] {
            r.write(&K_OLD, id, &goal(""));
        }
        r.cache.sync().await.unwrap();
        {
            let mut b = r.board.lock().unwrap();
            b.clash(
                &still_old,
                Some(sealed(&K_OLD, &still_old, &goal("del otro"))),
            );
            b.clash(&now_new, Some(sealed(&K_CUR, &now_new, &goal("del otro"))));
            b.clash(&deleted, None);
        }
        assert!(r.cache.recrypt(&keys()).await);
        let mut w = r.writes();
        w.sort();
        assert_eq!(
            w,
            vec![
                ("put".into(), still_old.clone(), 1),
                ("put".into(), still_old.clone(), 2),
                ("put".into(), now_new.clone(), 1),
                ("put".into(), deleted.clone(), 1),
            ]
        );
        assert_eq!(on_hub(&r.board, &K_CUR, &still_old), goal("del otro"));
        assert_eq!(on_hub(&r.board, &K_CUR, &now_new), goal("del otro"));
        assert_eq!(r.cache.item(&still_old).unwrap().kid, kid(&K_CUR));
        assert_eq!(r.cache.item(&now_new).unwrap().v, 2);
        assert!(r.cache.item(&deleted).unwrap().gone);
    }

    /// Con el freno del hub (429), espera y sigue con el mismo.
    #[tokio::test]
    async fn the_recrypt_waits_out_the_brake() {
        let r = rig(fake::Board::default()).await;
        r.write(&K_OLD, &bid(1), &project("uno"));
        r.cache.sync().await.unwrap();
        r.board.lock().unwrap().busy = 1;
        assert!(r.cache.recrypt(&keys()).await);
        let w = r.board.lock().unwrap().writes.clone();
        assert_eq!(w.len(), 2);
        assert!(w[1].0 - w[0].0 >= GAP * BUSY_GAPS - Duration::from_millis(5));
        assert_eq!(on_hub(&r.board, &K_CUR, &bid(1)), project("uno"));
    }

    /// Si se vacía la copia a mitad (el equipo sale del grupo), no sigue; sin hub, tampoco.
    #[tokio::test]
    async fn a_clear_or_a_lost_hub_stops_the_recrypt() {
        let r = rig(fake::Board::default()).await;
        for n in 1..=3 {
            r.write(&K_OLD, &bid(n), &project("x"));
        }
        r.cache.sync().await.unwrap();
        let cache = r.cache.clone();
        r.board.lock().unwrap().on_write = Some(Box::new(move |n| {
            if n == 1 {
                cache.clear();
            }
        }));
        assert!(!r.cache.recrypt(&keys()).await, "parado");
        assert_eq!(r.writes().len(), 1);
        assert!(r.cache.ids().is_empty());

        let r = rig(fake::Board::default()).await;
        r.write(&K_OLD, &bid(1), &project("x"));
        r.cache.sync().await.unwrap();
        offline(&r).await;
        assert!(!r.cache.recrypt(&keys()).await, "sin hub");
        assert_eq!(r.cache.item(&bid(1)).unwrap().kid, kid(&K_OLD));
    }

    /// Si cambia la clave actual a mitad, se sigue con la nueva, y lo ya recifrado con la de
    /// antes (que ahora está retirada) también pasa a la nueva.
    #[tokio::test]
    async fn a_new_key_midway_starts_again_with_it() {
        let r = rig(fake::Board::default()).await;
        for n in 1..=3 {
            r.write(&K_OLD, &bid(n), &project(&format!("p{n}")));
        }
        r.cache.sync().await.unwrap();
        let (ring, keys) = shared(keyring());
        let ring2 = ring.clone();
        r.board.lock().unwrap().on_write = Some(Box::new(move |n| {
            if n == 1 {
                let mut r = ring2.lock().unwrap();
                for k in r.keys.iter_mut() {
                    k.retired.get_or_insert(4);
                }
                r.keys.push(BoardKey::from_raw(&K_NEW, 5));
            }
        }));
        assert!(r.cache.recrypt(&keys).await);
        assert_eq!(ring.lock().unwrap().current().unwrap().kid, kid(&K_NEW));
        for n in 1..=3 {
            assert_eq!(on_hub(&r.board, &K_NEW, &bid(n)), project(&format!("p{n}")));
            assert_eq!(r.cache.item(&bid(n)).unwrap().kid, kid(&K_NEW));
        }
    }

    /// Mientras falte algo por resubir, no se recifra (la resubida ya cifra con la actual, y
    /// las dos a la vez pasarían del ritmo).
    #[tokio::test]
    async fn no_recrypt_while_reuploading() {
        let r = rig(fake::Board::default()).await;
        r.write(&K_OLD, &bid(1), &project("uno"));
        r.cache.sync().await.unwrap();
        r.cache.lock().reupload.insert(bid(1));
        assert!(!r.cache.recrypt(&keys()).await);
        assert!(r.writes().is_empty());
    }

    /// Los `kid` en uso: todo lo que no es lápida, también lo que no está en el hub, lo que falta
    /// por resubir y lo ilegible.
    #[tokio::test]
    async fn kids_in_use_count_everything_but_tombstones() {
        let r = rig(fake::Board::default()).await;
        r.write(&K_OLD, &bid(1), &project("uno"));
        r.write(&K_NEW, &bid(2), &project("sin clave"));
        r.write(&K_CUR, &bid(3), &project("tres"));
        r.board.lock().unwrap().write(&bid(3), None);
        r.cache.sync().await.unwrap();
        {
            let mut c = r.cache.lock();
            let stored = |k: &[u8; 32]| Stored {
                v: 1,
                kid: kid(k),
                blob: Some("AAAA".into()),
                gone: false,
                at: 1,
            };
            c.items.insert(bid(4), stored(&[0x44; 32]));
            c.missing.insert(bid(4));
            c.items.insert(bid(5), stored(&[0x55; 32]));
            c.reupload.insert(bid(5));
        }
        let want: BTreeSet<String> = [&K_OLD, &K_NEW, &[0x44; 32], &[0x55; 32]]
            .into_iter()
            .map(kid)
            .collect();
        assert_eq!(r.cache.kids_in_use(), want);
    }

    /// Un grupo con el llavero en disco y las claves dadas.
    fn group_with(dir: &std::path::Path, keys: Vec<BoardKey>) -> Arc<crate::group::Group> {
        let g = crate::group::Group::load(dir.join("data/group.json"), [1u8; 32]);
        g.merge(&crate::group::State {
            keys,
            ..Default::default()
        });
        Arc::new(g)
    }

    fn raw(k: &BoardKey) -> [u8; 32] {
        B64.decode(&k.key).unwrap().try_into().unwrap()
    }

    /// Tras quitar un dispositivo (rotar), la clave nueva es la actual y la tarea del llavero
    /// acaba recifrándolo todo con ella.
    #[tokio::test]
    async fn after_a_rotation_everything_ends_up_recrypted() {
        let r = rig(fake::Board::default()).await;
        let now = crate::group::now_ms();
        let group = group_with(r.dir.path(), vec![BoardKey::from_raw(&K_OLD, now - 1000)]);
        for n in 1..=3 {
            r.write(&K_OLD, &bid(n), &project(&format!("p{n}")));
        }
        let blob = seal(&K_OLD, &bid(4), FUTURE).unwrap();
        r.board
            .lock()
            .unwrap()
            .write(&bid(4), Some((&kid(&K_OLD), &blob)));
        r.cache.sync().await.unwrap();
        let new = group.rotate_board_key().unwrap();
        assert_eq!(group.board_keys().current(), Some(&new));
        spawn_upkeep_with(r.cache.clone(), group.clone(), LONG);
        let board = r.board.clone();
        let new_kid = new.kid.clone();
        until("todo recifrado", || {
            let b = board.lock().unwrap();
            b.items.values().all(|i| i.kid == new_kid)
        })
        .await;
        for n in 1..=3 {
            assert_eq!(
                on_hub(&r.board, &raw(&new), &bid(n)),
                project(&format!("p{n}"))
            );
        }
        assert_eq!(hub_bytes(&r.board, &raw(&new), &bid(4)).2, FUTURE.to_vec());
        // La vieja sigue (retirada hace nada).
        assert!(group.board_keys().keys.iter().any(|k| k.kid == kid(&K_OLD)));
    }

    /// La poda: tras ponerse al día con el hub, se quita la retirada de más de 30 días sin
    /// objetos. Ni la de 29 días, ni una de 31 con objetos que no se pueden recifrar (no se
    /// descifran), ni una con objetos que no están en el hub.
    #[tokio::test]
    async fn retired_keys_without_objects_go_after_30_days() {
        let r = rig(fake::Board::default()).await;
        let now = crate::group::now_ms();
        let retired = |k: &[u8; 32], days: u64| {
            let mut b = BoardKey::from_raw(k, now - 60 * DAY_MS);
            b.retired = Some(now - days * DAY_MS);
            b.at = now - days * DAY_MS;
            b
        };
        let (k31, k29, kbroken, kmissing) = ([0x41; 32], [0x42; 32], [0x43; 32], [0x44; 32]);
        let group = group_with(
            r.dir.path(),
            vec![
                BoardKey::from_raw(&K_CUR, now - 1000),
                retired(&k31, 31),
                retired(&k29, 29),
                retired(&kbroken, 31),
                retired(&kmissing, 31),
            ],
        );
        // Dice ir con `kbroken`, pero no se descifra con ella: no se puede recifrar.
        let bad = seal_object(&K_CUR, &bid(1), &project("roto")).unwrap();
        r.board
            .lock()
            .unwrap()
            .write(&bid(1), Some((&kid(&kbroken), &bad)));
        r.cache.sync().await.unwrap();
        {
            let mut c = r.cache.lock();
            let (k, b) = sealed(&kmissing, &bid(2), &project("perdido"));
            c.items.insert(
                bid(2),
                Stored {
                    v: 1,
                    kid: k,
                    blob: Some(b),
                    gone: false,
                    at: 1,
                },
            );
            c.missing.insert(bid(2));
        }
        // Lo que hay en el hub y la copia aún no tiene también cuenta: por eso se sincroniza
        // antes de decidir.
        let k31_obj = sealed(&k31, &bid(3), &project("recién llegado"));
        let gets = r.board.lock().unwrap().gets.len();
        r.board
            .lock()
            .unwrap()
            .write(&bid(3), Some((&k31_obj.0, &k31_obj.1)));
        // La primera pasada no ve el objeto nuevo (la copia no lo tiene): no lo recifra, pero la
        // poda sí lo ve al sincronizar.
        r.cache.upkeep(&group).await;
        assert!(
            r.board.lock().unwrap().gets.len() > gets,
            "se sincroniza antes"
        );
        let have: BTreeSet<String> = group
            .board_keys()
            .keys
            .iter()
            .map(|k| k.kid.clone())
            .collect();
        for k in [&K_CUR, &k29, &kbroken, &kmissing, &k31] {
            assert!(have.contains(&kid(k)), "{} se queda", kid(k));
        }
        // Recifrado lo que trajo la sincronización, la de 31 días ya no tiene objetos.
        r.cache.upkeep(&group).await;
        assert_eq!(r.cache.item(&bid(3)).unwrap().kid, kid(&K_CUR));
        let have: BTreeSet<String> = group
            .board_keys()
            .keys
            .iter()
            .map(|k| k.kid.clone())
            .collect();
        assert!(
            !have.contains(&kid(&k31)),
            "la de 31 días sin objetos se va"
        );
        for k in [&K_CUR, &k29, &kbroken, &kmissing] {
            assert!(have.contains(&kid(k)), "{} se queda", kid(k));
        }
    }

    /// Sin poder ponerse al día con el hub, no se quita nada.
    #[tokio::test]
    async fn no_pruning_without_the_hub() {
        let r = rig(fake::Board::default()).await;
        let now = crate::group::now_ms();
        let mut old = BoardKey::from_raw(&K_OLD, now - 60 * DAY_MS);
        old.retired = Some(now - 40 * DAY_MS);
        old.at = now - 40 * DAY_MS;
        let group = group_with(r.dir.path(), vec![BoardKey::from_raw(&K_CUR, now), old]);
        offline(&r).await;
        r.cache.upkeep(&group).await;
        assert_eq!(group.board_keys().keys.len(), 2);
    }
}
