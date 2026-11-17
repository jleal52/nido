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

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// El id de una clave del tablero (`hex(SHA-256("nido-board-kid" || clave))[0..16]`).
pub use crate::group::board_kid as kid;

/// La versión del formato del blob: su primer byte.
pub const VERSION: u8 = 1;
/// Lo más que puede ocupar el texto de un objeto (su JSON en UTF-8). Cifrado y en base64 son
/// 10 964 caracteres, por debajo de los 11 000 que acepta el hub.
pub const MAX_PLAIN: usize = 8192;
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
        .map_err(|_| BlobError::Open)?;
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
// `tag` de `Object` antes de leer la variante, y al escribir lo pone él delante (quien rellene
// `extra` a mano no debe meter `t` ni un campo conocido: saldría dos veces).

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

/// Un proyecto: agrupa objetivos.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Project {
    pub name: String,
    #[serde(default)]
    pub desc: String,
    pub state: ProjectState,
    /// Cuándo se creó y cuándo cambió por última vez (ms Unix).
    pub created: u64,
    pub changed: u64,
    /// Quién lo cambió: `"<equipo>/<sesión>"`, `"web"`, `"app"` o `"Cuco"`.
    #[serde(default)]
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
    #[serde(default)]
    pub desc: String,
    pub state: ObjectiveState,
    #[serde(default)]
    pub note: String,
    /// Dónde va en su proyecto. Es real: para meter uno entre dos se usa la media, sin renumerar.
    #[serde(default)]
    pub order: f64,
    pub created: u64,
    pub changed: u64,
    #[serde(default)]
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
    #[serde(default)]
    pub copied: u64,
    /// Cuándo se cerró la sesión (ms), o `null` si sigue.
    #[serde(default)]
    pub closed: Option<u64>,
    pub created: u64,
    pub changed: u64,
    #[serde(default)]
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
    #[serde(default)]
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
    #[serde(default)]
    pub done: usize,
    #[serde(default)]
    pub total: usize,
    #[serde(default)]
    pub active: usize,
    #[serde(default)]
    pub blocked: usize,
    #[serde(default)]
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    const ID: &str = "0123456789abcdef0123456789abcdef";

    fn seq_key() -> [u8; 32] {
        std::array::from_fn(|i| i as u8)
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
        // Base64 URL o sin relleno no es el estándar.
        let url = blob.replace('+', "-").replace('/', "_");
        if url != blob {
            assert!(matches!(open(&key, ID, &url), Err(BlobError::BadBlob)));
        }
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
        // Sin cambios, vuelve el mismo texto (los desconocidos van al final, por orden).
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
    /// copia); si falta o con `NIDO_WRITE_VECTORS=1` se regenera, y si no, se comprueba que el
    /// código sigue dando lo mismo.
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
        assert!(matches!(
            serde_json::from_str::<Object>(objective).unwrap(),
            Object::Objective(_)
        ));
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
        ];
        let bad: Vec<_> = bad
            .iter()
            .map(|(name, key, id, blob)| {
                assert!(open(key, id, blob).is_err(), "{name}");
                serde_json::json!({"name": name, "key": B64.encode(key), "id": id, "blob": blob})
            })
            .collect();
        let v = serde_json::json!({
            "about": "nido: blobs del tablero. blob = base64(0x01 || nonce(12) || AES-256-GCM(key, nonce, plaintext, aad)), con la etiqueta de 16 bytes al final; aad = \"nido-board/1/\" || id en ASCII; kid = hex(SHA-256(\"nido-board-kid\" || key))[0..16]. key y blob en base64 estándar con relleno; nonce en hex; plaintext en UTF-8. Los de ok se descifran a su plaintext; los de bad no se deben descifrar.",
            "ok": ok,
            "bad": bad,
        });
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/board_vectors.json");
        let text = serde_json::to_string_pretty(&v).unwrap() + "\n";
        if !path.exists() || std::env::var_os("NIDO_WRITE_VECTORS").is_some() {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, &text).unwrap();
        }
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            text,
            "vectores de {}",
            path.display()
        );
    }
}
