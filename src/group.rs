//! Un grupo de equipos con nido: se emparejan entre sí una vez y comparten quién está en el
//! grupo y qué dispositivos (móviles, navegadores) están autorizados. Un dispositivo emparejado
//! con cualquiera sirve para todos; quitarlo en uno lo quita en todos. Todos los equipos son
//! iguales.
//!
//! El estado es un conjunto de entradas con la hora del último cambio (y si es una baja): juntar
//! dos estados es quedarse, en cada entrada, con la más reciente. Así da igual el orden en que se
//! sincronicen los equipos. Se sincroniza por el hub, con Noise de nido a nido (ver `e2e.rs`).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::{STANDARD as B64, URL_SAFE_NO_PAD as B64URL};
use serde::{Deserialize, Serialize};
use tokio::sync::watch;

const INVITE_LIFE: Duration = Duration::from_secs(600);
/// Cada cuánto, como mucho, se avisa de claves del tablero que llegan mal.
const BAD_KEY_QUIET: Duration = Duration::from_secs(3600);

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Member {
    /// Su id en el hub.
    pub id: String,
    /// Su clave pública X25519 (la de `hub.key`), base64.
    pub key: String,
    pub name: String,
    /// Último cambio (ms).
    pub at: u64,
    #[serde(default)]
    pub removed: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Device {
    /// Clave pública X25519 del dispositivo, base64.
    pub key: String,
    pub name: String,
    pub at: u64,
    #[serde(default)]
    pub removed: bool,
}

/// Una clave del tablero de la cuenta (el llavero que comparten los equipos del grupo y que se
/// da a sus dispositivos). Es secreta: no sale en `Debug`.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct BoardKey {
    /// `board_kid` de la clave: la identifica en cada objeto cifrado con ella.
    pub kid: String,
    /// Los 32 bytes de la clave, en base64 estándar.
    pub key: String,
    /// Último cambio (ms).
    pub at: u64,
    /// Desde cuándo está retirada (ms): ya no se cifra con ella, pero se sigue pudiendo leer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retired: Option<u64>,
    #[serde(default)]
    pub removed: bool,
}

impl std::fmt::Debug for BoardKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BoardKey")
            .field("kid", &self.kid)
            .field("key", &"<oculta>")
            .field("at", &self.at)
            .field("retired", &self.retired)
            .field("removed", &self.removed)
            .finish()
    }
}

impl BoardKey {
    /// ¿Su `kid` es el de su clave? Una lápida sin clave, también vale.
    pub fn fits(&self) -> bool {
        if self.key.is_empty() {
            return self.removed;
        }
        B64.decode(&self.key)
            .is_ok_and(|k| k.len() == 32 && board_kid(&k) == self.kid)
    }

    /// Una clave nueva, al azar.
    pub fn generate(at: u64) -> Self {
        let mut key = [0u8; 32];
        getrandom::fill(&mut key).expect("aleatorio");
        Self {
            kid: board_kid(&key),
            key: B64.encode(key),
            at,
            retired: None,
            removed: false,
        }
    }
}

/// El id de una clave del tablero: `hex(SHA-256("nido-board-kid" || clave))[0..16]`, sobre los 32
/// bytes en bruto. Es el mismo en nido, la web y la app.
pub fn board_kid(key: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(b"nido-board-kid");
    h.update(key);
    h.finalize()[..8]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Las claves del tablero que siguen en el llavero (las retiradas también; las borradas no),
/// por `kid`.
#[derive(Debug, Clone, PartialEq)]
pub struct Keyring {
    pub keys: Vec<BoardKey>,
}

impl Keyring {
    pub fn of(st: &State) -> Self {
        let mut keys: Vec<BoardKey> = st.keys.iter().filter(|k| !k.removed).cloned().collect();
        keys.sort_by(|a, b| a.kid.cmp(&b.kid));
        Self { keys }
    }

    /// Con la que se cifra: la no retirada más nueva por `at` y, a igualdad, la de `kid` menor.
    /// Así todos los equipos eligen la misma sin ponerse de acuerdo.
    pub fn current(&self) -> Option<&BoardKey> {
        self.keys
            .iter()
            .filter(|k| !k.removed && k.retired.is_none())
            .min_by(|a, b| b.at.cmp(&a.at).then_with(|| a.kid.cmp(&b.kid)))
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct State {
    #[serde(default)]
    pub members: Vec<Member>,
    #[serde(default)]
    pub devices: Vec<Device>,
    /// El llavero del tablero. Un nido de antes (0.9.1) no lo conoce: le llega vacío y, como
    /// juntar es una unión, no borra las nuestras.
    #[serde(default)]
    pub keys: Vec<BoardKey>,
}

/// La entrada más reciente; a igual hora, la baja gana (no resucita lo quitado).
fn newer(a_at: u64, a_removed: bool, b_at: u64, b_removed: bool) -> bool {
    b_at > a_at || (b_at == a_at && b_removed && !a_removed)
}

impl State {
    /// Junta `other` en este estado. Devuelve si cambió algo.
    pub fn merge(&mut self, other: &State) -> bool {
        let mut changed = false;
        let mut members: BTreeMap<String, Member> =
            self.members.drain(..).map(|m| (m.id.clone(), m)).collect();
        for m in &other.members {
            match members.get(&m.id) {
                Some(cur) if !newer(cur.at, cur.removed, m.at, m.removed) => {}
                _ => {
                    members.insert(m.id.clone(), m.clone());
                    changed = true;
                }
            }
        }
        self.members = members.into_values().collect();
        let mut devices: BTreeMap<String, Device> =
            self.devices.drain(..).map(|d| (d.key.clone(), d)).collect();
        for d in &other.devices {
            match devices.get(&d.key) {
                Some(cur) if !newer(cur.at, cur.removed, d.at, d.removed) => {}
                _ => {
                    devices.insert(d.key.clone(), d.clone());
                    changed = true;
                }
            }
        }
        self.devices = devices.into_values().collect();
        let mut keys: BTreeMap<String, BoardKey> =
            self.keys.drain(..).map(|k| (k.kid.clone(), k)).collect();
        for k in &other.keys {
            match keys.get(&k.kid) {
                Some(cur) if !newer(cur.at, cur.removed, k.at, k.removed) => {}
                _ => {
                    let mut k = k.clone();
                    // Una lápida no guarda el secreto, aunque quien la manda no lo vaciara.
                    if k.removed {
                        k.key.clear();
                    }
                    keys.insert(k.kid.clone(), k);
                    changed = true;
                }
            }
        }
        self.keys = keys.into_values().collect();
        changed
    }

    pub fn live_members(&self) -> impl Iterator<Item = &Member> {
        self.members.iter().filter(|m| !m.removed)
    }

    pub fn member_by_key(&self, key_b64: &str) -> Option<&Member> {
        self.live_members().find(|m| m.key == key_b64)
    }

    pub fn device(&self, key_b64: &str) -> Option<&Device> {
        self.devices.iter().find(|d| d.key == key_b64 && !d.removed)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct MemberView {
    pub id: String,
    pub name: String,
    /// Este equipo.
    #[serde(rename = "self")]
    pub me: bool,
    pub key: String,
}

struct Invite {
    secret: [u8; 32],
    expires: Instant,
}

pub struct Group {
    path: PathBuf,
    /// La clave pública de este nido, base64.
    pub own_key: String,
    state: Mutex<State>,
    invite: Mutex<Option<Invite>>,
    /// La revisión: sube cada vez que cambia el estado. La escucha quien quiera enterarse de
    /// cualquier cambio (venga de este equipo o de otro), aparte de la sincronización.
    revision: watch::Sender<u64>,
    /// Cuándo se avisó por última vez de una clave que llegó mal (para no llenar el registro).
    bad_key_said: Mutex<Option<Instant>>,
}

impl Group {
    pub fn load(path: PathBuf, own_key: [u8; 32]) -> Self {
        let state = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        Self {
            path,
            own_key: B64.encode(own_key),
            state: Mutex::new(state),
            invite: Mutex::new(None),
            revision: watch::Sender::new(0),
            bad_key_said: Mutex::new(None),
        }
    }

    /// Avisa de cada cambio del grupo: el valor es la revisión, que solo sube.
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.revision.subscribe()
    }

    /// Guarda el estado cambiado. Se llama con el estado bloqueado; la revisión la sube después
    /// quien llama (`bump`), ya sin él.
    /// Solo para el usuario (0600): lleva el llavero del tablero. Un temporal que quedara de
    /// antes se quita primero, porque los permisos solo se ponen al crear el fichero.
    fn save(&self, st: &State) {
        use std::io::Write;
        let tmp = self.path.with_extension("json.tmp");
        let write = || -> std::io::Result<()> {
            if let Some(dir) = self.path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let body = serde_json::to_vec_pretty(st)?;
            let _ = std::fs::remove_file(&tmp);
            crate::sys::private(std::fs::OpenOptions::new().write(true).create_new(true))
                .open(&tmp)?
                .write_all(&body)?;
            std::fs::rename(&tmp, &self.path)
        };
        if let Err(e) = write() {
            eprintln!("nido: no se pudo guardar el grupo: {e}");
        }
    }

    /// Sube la revisión tras un cambio, aunque no se haya podido guardar (en memoria ya ha
    /// cambiado). Se llama con el estado ya suelto: `send_modify` espera a que nadie tenga la
    /// revisión leída (un `Ref` de `borrow`), y quien la tiene puede estar pidiendo el estado;
    /// con él cogido, se bloquearían el uno al otro. El orden de las revisiones lo pone el cerrojo
    /// del propio `watch`, y quien reciba el aviso ya ve el estado cambiado. `send_modify` y no
    /// `send`: sube también sin nadie escuchando.
    fn bump(&self) {
        self.revision.send_modify(|r| *r += 1);
    }

    pub fn snapshot(&self) -> State {
        self.state.lock().expect("grupo").clone()
    }

    /// Junta el estado de otro equipo; si cambió, lo guarda. Si a este equipo lo han sacado del
    /// grupo, se queda solo (con sus dispositivos, pero sin el llavero del tablero).
    pub fn merge(&self, other: &State) -> bool {
        self.merge_if(other, |_| true)
    }

    /// ¿Es un miembro vivo del grupo (por su clave)?
    pub fn is_live(&self, key_b64: &str) -> bool {
        self.state
            .lock()
            .expect("grupo")
            .member_by_key(key_b64)
            .is_some()
    }

    /// Lo que se le cuenta a otro equipo al sincronizar: nada si no es del grupo; el estado sin
    /// el llavero si ya está fuera (para que se entere, pero no lea lo que se cifre después).
    pub fn state_for(&self, key_b64: &str) -> Option<State> {
        let mut st = self.snapshot();
        if !st.members.iter().any(|m| m.key == key_b64) {
            return None;
        }
        if st.member_by_key(key_b64).is_none() {
            st.keys.clear();
        }
        Some(st)
    }

    /// Junta lo que cuenta otro equipo, solo si es un miembro vivo (se mira con el mismo cerrojo
    /// con el que se junta). De uno quitado no entra nada: si no, se daría de alta él solo con
    /// su entrada más nueva y `removed: false`, colaría miembros, dispositivos o una clave suya
    /// como la actual, y en la siguiente se llevaría el llavero. Volver es con una invitación
    /// nueva (`merge` desde `group_join`). Vale igual para quien llama y para quien contesta.
    pub fn merge_from(&self, key_b64: &str, other: &State) -> bool {
        self.merge_if(other, |st| st.member_by_key(key_b64).is_some())
    }

    fn merge_if(&self, other: &State, heard: impl FnOnce(&State) -> bool) -> bool {
        let other = self.fitting_keys(other);
        let mut st = self.state.lock().expect("grupo");
        if !heard(&st) {
            return false;
        }
        let changed = st.merge(&other);
        if changed {
            if st
                .members
                .iter()
                .any(|m| m.key == self.own_key && m.removed)
            {
                eprintln!("nido: este equipo ha salido del grupo");
                st.members.retain(|m| m.key == self.own_key);
                if let Some(me) = st.members.first_mut() {
                    me.removed = false;
                    me.at = now_ms();
                }
                // El tablero sigue a la casa: el equipo que sale no se lleva sus claves.
                st.keys.clear();
            }
            self.save(&st);
        }
        drop(st);
        if changed {
            self.bump();
        }
        changed
    }

    /// `other` sin las claves cuyo `kid` no es el de su clave (de un fallo o de alguien que se hace
    /// pasar por otra). Se avisa con freno y sin el secreto.
    fn fitting_keys<'a>(&self, other: &'a State) -> std::borrow::Cow<'a, State> {
        if other.keys.iter().all(BoardKey::fits) {
            return std::borrow::Cow::Borrowed(other);
        }
        let mut st = other.clone();
        let bad: Vec<String> = st
            .keys
            .iter()
            .filter(|k| !k.fits())
            .map(|k| {
                k.kid
                    .chars()
                    .filter(char::is_ascii_hexdigit)
                    .take(16)
                    .collect()
            })
            .collect();
        st.keys.retain(BoardKey::fits);
        let mut said = self.bad_key_said.lock().expect("aviso");
        if said.is_none_or(|t| t.elapsed() >= BAD_KEY_QUIET) {
            *said = Some(Instant::now());
            eprintln!(
                "nido: claves del tablero que no casan con su kid, descartadas: {}",
                bad.join(", ")
            );
        }
        std::borrow::Cow::Owned(st)
    }

    /// Cambia el estado y, si de verdad cambió, lo guarda.
    fn change(&self, f: impl FnOnce(&mut State)) {
        let changed = {
            let mut st = self.state.lock().expect("grupo");
            let before = st.clone();
            f(&mut st);
            let changed = *st != before;
            if changed {
                self.save(&st);
            }
            changed
        };
        if changed {
            self.bump();
        }
    }

    /// Este equipo, con su id en el hub (se conoce al conectar con él) y su nombre.
    pub fn set_self(&self, id: &str, name: &str) {
        let own = self.own_key.clone();
        let current = self
            .snapshot()
            .members
            .into_iter()
            .find(|m| m.key == own && !m.removed);
        if current
            .as_ref()
            .is_some_and(|m| m.id == id && m.name == name)
        {
            return;
        }
        self.change(|st| {
            // Si cambió de id (otro hub), la entrada vieja se da de baja.
            for m in st.members.iter_mut().filter(|m| m.key == own && m.id != id) {
                m.removed = true;
                m.at = now_ms();
            }
            st.members.retain(|m| m.id != id);
            st.members.push(Member {
                id: id.into(),
                key: own.clone(),
                name: name.into(),
                at: now_ms(),
                removed: false,
            });
        });
    }

    pub fn self_id(&self) -> Option<String> {
        self.snapshot()
            .members
            .into_iter()
            .find(|m| m.key == self.own_key && !m.removed)
            .map(|m| m.id)
    }

    /// Los demás equipos del grupo (sin este).
    pub fn others(&self) -> Vec<Member> {
        let own = self.own_key.clone();
        self.snapshot()
            .live_members()
            .filter(|m| m.key != own)
            .cloned()
            .collect()
    }

    pub fn members(&self) -> Vec<MemberView> {
        self.snapshot()
            .live_members()
            .map(|m| MemberView {
                id: m.id.clone(),
                name: m.name.clone(),
                me: m.key == self.own_key,
                key: m.key.clone(),
            })
            .collect()
    }

    pub fn add_member(&self, id: &str, key_b64: &str, name: &str) {
        self.change(|st| {
            st.members.retain(|m| m.id != id);
            st.members.push(Member {
                id: id.into(),
                key: key_b64.into(),
                name: clean(name),
                at: now_ms(),
                removed: false,
            });
        });
    }

    pub fn remove_member(&self, id: &str) -> bool {
        let mut found = false;
        self.change(|st| {
            for m in st.members.iter_mut().filter(|m| m.id == id && !m.removed) {
                m.removed = true;
                m.at = now_ms();
                found = true;
            }
        });
        found
    }

    /// Un dispositivo emparejado aquí: autorizado en todo el grupo.
    pub fn add_device(&self, key: &[u8], name: &str) {
        let k = B64.encode(key);
        self.change(|st| {
            st.devices.retain(|d| d.key != k);
            st.devices.push(Device {
                key: k,
                name: clean(name),
                at: now_ms(),
                removed: false,
            });
        });
    }

    pub fn remove_device(&self, key_b64: &str) {
        self.change(|st| {
            for d in st
                .devices
                .iter_mut()
                .filter(|d| d.key == key_b64 && !d.removed)
            {
                d.removed = true;
                d.at = now_ms();
            }
        });
    }

    /// ¿Está autorizado en el grupo? Devuelve su nombre.
    pub fn device_name(&self, key: &[u8]) -> Option<String> {
        self.snapshot()
            .device(&B64.encode(key))
            .map(|d| d.name.clone())
    }

    /// ¿Está autorizado en el grupo (y no dado de baja)?
    pub fn has_device(&self, key: &[u8]) -> bool {
        self.snapshot().device(&B64.encode(key)).is_some()
    }

    /// Los dispositivos dados de baja en el grupo (para quitarlos también de este equipo).
    pub fn removed_devices(&self) -> Vec<String> {
        self.snapshot()
            .devices
            .into_iter()
            .filter(|d| d.removed)
            .map(|d| d.key)
            .collect()
    }

    // ---------- el llavero del tablero ----------

    pub fn board_keys(&self) -> Keyring {
        Keyring::of(&self.state.lock().expect("grupo"))
    }

    /// La clave actual; si no hay ninguna activa, la crea.
    pub fn ensure_board_key(&self) -> BoardKey {
        let mut current = None;
        self.change(|st| {
            if let Some(k) = Keyring::of(st).current() {
                current = Some(k.clone());
                return;
            }
            let k = BoardKey::generate(next_key_at(st));
            st.keys.push(k.clone());
            current = Some(k);
        });
        current.expect("clave actual")
    }

    /// Una clave nueva, que pasa a ser la actual; las demás quedan retiradas desde ahora (las que
    /// ya lo estaban conservan su fecha). Devuelve la nueva.
    pub fn rotate_board_key(&self) -> BoardKey {
        let mut new = None;
        self.change(|st| {
            for k in st
                .keys
                .iter_mut()
                .filter(|k| !k.removed && k.retired.is_none())
            {
                let t = now_ms().max(k.at.saturating_add(1));
                k.retired = Some(t);
                k.at = t;
            }
            let k = BoardKey::generate(next_key_at(st));
            st.keys.push(k.clone());
            new = Some(k);
        });
        new.expect("clave nueva")
    }

    /// Quita una clave del llavero (en todo el grupo). La lápida se queda sin el secreto: para
    /// ganar al juntar le bastan `kid`, `at` y `removed`. Devuelve si estaba.
    pub fn drop_board_key(&self, kid: &str) -> bool {
        let mut found = false;
        self.change(|st| {
            for k in st.keys.iter_mut().filter(|k| k.kid == kid && !k.removed) {
                k.removed = true;
                k.key.clear();
                k.at = now_ms().max(k.at.saturating_add(1));
                found = true;
            }
        });
        found
    }

    // ---------- invitar ----------

    /// Un código para que otro equipo se una (el anterior deja de valer). Hace falta estar
    /// conectado a un hub: por él se encuentran los equipos.
    pub fn invite(&self, hub: &str, self_id: &str) -> (String, u64) {
        let mut secret = [0u8; 32];
        getrandom::fill(&mut secret).expect("aleatorio");
        *self.invite.lock().expect("invitación") = Some(Invite {
            secret,
            expires: Instant::now() + INVITE_LIFE,
        });
        let own = B64.decode(&self.own_key).unwrap_or_default();
        let expires = now_ms() / 1000 + INVITE_LIFE.as_secs();
        let uri = format!(
            "nido://group?k={}&s={}&x={expires}&h={}&n={self_id}",
            B64URL.encode(own),
            B64URL.encode(secret),
            B64URL.encode(hub)
        );
        (uri, expires)
    }

    pub fn current_invite(&self) -> Option<[u8; 32]> {
        let i = self.invite.lock().expect("invitación");
        i.as_ref()
            .filter(|i| Instant::now() < i.expires)
            .map(|i| i.secret)
    }

    /// Gasta la invitación (solo tras un primer mensaje válido).
    pub fn consume_invite(&self, secret: &[u8; 32]) -> bool {
        let mut i = self.invite.lock().expect("invitación");
        if i.as_ref().is_some_and(|x| &x.secret == secret) {
            *i = None;
            true
        } else {
            false
        }
    }
}

/// Lo que trae un código de invitación al grupo.
#[derive(Debug, Clone, PartialEq)]
pub struct InviteCode {
    pub key: [u8; 32],
    pub secret: [u8; 32],
    pub expires: u64,
    pub hub: String,
    pub id: String,
}

pub fn parse_invite(code: &str) -> Result<InviteCode, String> {
    let rest = code
        .trim()
        .strip_prefix("nido://group?")
        .ok_or("no es un código de grupo de nido (nido://group?…)")?;
    let mut q = std::collections::HashMap::new();
    for kv in rest.split('&') {
        if let Some((k, v)) = kv.split_once('=') {
            q.insert(k, v);
        }
    }
    let b = |k: &str| -> Result<[u8; 32], String> {
        q.get(k)
            .and_then(|v| B64URL.decode(v).ok())
            .and_then(|v| v.try_into().ok())
            .ok_or_else(|| format!("al código le falta «{k}» o no es válido"))
    };
    let hub = q
        .get("h")
        .and_then(|v| B64URL.decode(v).ok())
        .and_then(|v| String::from_utf8(v).ok())
        .ok_or("al código le falta el hub")?;
    let id = q
        .get("n")
        .ok_or("al código le falta el equipo")?
        .to_string();
    let expires = q.get("x").and_then(|v| v.parse().ok()).unwrap_or(0);
    if expires != 0 && expires < now_ms() / 1000 {
        return Err("el código ha caducado: genera otro con «nido group invite»".into());
    }
    Ok(InviteCode {
        key: b("k")?,
        secret: b("s")?,
        expires,
        hub,
        id,
    })
}

/// La hora de una clave nueva: posterior a la de todas las que hay, para que gane al juntar
/// aunque algún reloj vaya adelantado.
fn next_key_at(st: &State) -> u64 {
    let last = st
        .keys
        .iter()
        .map(|k| k.at.saturating_add(1))
        .max()
        .unwrap_or(0);
    now_ms().max(last)
}

fn clean(n: &str) -> String {
    let n: String = n.chars().filter(|c| !c.is_control()).take(60).collect();
    let n = n.trim();
    if n.is_empty() {
        "equipo".into()
    } else {
        n.into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn member(id: &str, at: u64, removed: bool) -> Member {
        Member {
            id: id.into(),
            key: format!("k-{id}"),
            name: id.into(),
            at,
            removed,
        }
    }

    fn device(key: &str, at: u64, removed: bool) -> Device {
        Device {
            key: key.into(),
            name: key.into(),
            at,
            removed,
        }
    }

    #[test]
    fn merge_keeps_the_newest_and_removals_win_ties() {
        let mut a = State {
            members: vec![member("wsl", 10, false)],
            devices: vec![device("iphone", 10, false), device("chrome", 20, false)],
            keys: vec![],
        };
        let b = State {
            members: vec![member("mac", 15, false)],
            devices: vec![device("iphone", 30, true), device("chrome", 20, true)],
            keys: vec![],
        };
        assert!(a.merge(&b));
        assert_eq!(a.live_members().count(), 2);
        assert!(a.device("iphone").is_none(), "quitado después");
        assert!(a.device("chrome").is_none(), "a igual hora, gana la baja");
        // Juntar lo mismo otra vez no cambia nada (y da igual el orden).
        assert!(!a.merge(&b));
        let mut c = b.clone();
        c.merge(&State {
            members: vec![member("wsl", 10, false)],
            devices: vec![device("iphone", 10, false), device("chrome", 20, false)],
            keys: vec![],
        });
        assert_eq!(c, a);
        // Un emparejamiento posterior lo vuelve a autorizar.
        assert!(a.merge(&State {
            members: vec![],
            devices: vec![device("iphone", 40, false)],
            keys: vec![],
        }));
        assert!(a.device("iphone").is_some());
    }

    #[test]
    fn devices_come_and_go() {
        let dir = tempfile::tempdir().unwrap();
        let g = Group::load(dir.path().join("group.json"), [1u8; 32]);
        assert!(!g.has_device(&[7u8; 32]));
        g.add_device(&[7u8; 32], "Firefox");
        g.add_device(&[8u8; 32], "iPhone");
        assert!(g.has_device(&[7u8; 32]));
        g.remove_device(&B64.encode([7u8; 32]));
        assert!(!g.has_device(&[7u8; 32]), "dado de baja");
        assert!(g.has_device(&[8u8; 32]), "el otro sigue");
        assert_eq!(g.removed_devices(), [B64.encode([7u8; 32])]);
    }

    #[test]
    fn invite_codes() {
        let dir = tempfile::tempdir().unwrap();
        let g = Group::load(dir.path().join("group.json"), [7u8; 32]);
        let (uri, _) = g.invite("https://hub.ejemplo", "abcdefghijklmnopqrstuvwxyz");
        let c = parse_invite(&uri).unwrap();
        assert_eq!(c.key, [7u8; 32]);
        assert_eq!(c.hub, "https://hub.ejemplo");
        assert_eq!(c.id, "abcdefghijklmnopqrstuvwxyz");
        assert!(g.consume_invite(&c.secret));
        assert!(!g.consume_invite(&c.secret), "una sola vez");
        assert!(parse_invite("nido://pair?k=x").is_err());
    }

    #[test]
    fn removed_member_leaves_the_group() {
        let dir = tempfile::tempdir().unwrap();
        let g = Group::load(dir.path().join("group.json"), [1u8; 32]);
        g.set_self("yo", "wsl");
        g.add_member("otro", "k-otro", "mac");
        assert_eq!(g.others().len(), 1);
        // El otro equipo nos saca.
        let mut st = g.snapshot();
        for m in st.members.iter_mut().filter(|m| m.id == "yo") {
            m.removed = true;
            m.at = now_ms() + 1;
        }
        assert!(g.merge(&st));
        assert!(g.others().is_empty());
        assert_eq!(g.self_id().as_deref(), Some("yo"));
        // Y lo guardado se recupera.
        let again = Group::load(dir.path().join("group.json"), [1u8; 32]);
        assert_eq!(again.self_id().as_deref(), Some("yo"));
    }

    /// Que saquen a este equipo del grupo también es un cambio: avisa, y una vez.
    #[test]
    fn leaving_the_group_bumps_the_revision() {
        let dir = tempfile::tempdir().unwrap();
        let g = Group::load(dir.path().join("group.json"), [1u8; 32]);
        g.set_self("yo", "wsl");
        g.add_member("otro", "k-otro", "mac");
        let mut rx = g.subscribe();
        let before = *rx.borrow_and_update();
        let mut st = g.snapshot();
        for m in st.members.iter_mut().filter(|m| m.id == "yo") {
            m.removed = true;
            m.at = now_ms() + 1;
        }
        assert!(g.merge(&st));
        assert!(rx.has_changed().unwrap(), "salir del grupo avisa");
        assert_eq!(*rx.borrow_and_update(), before + 1);
        assert!(g.others().is_empty());
    }

    /// Si no se puede escribir el fichero, el estado en memoria ya ha cambiado: la revisión sube
    /// igual.
    #[test]
    fn the_revision_goes_up_even_if_the_disk_fails() {
        let dir = tempfile::tempdir().unwrap();
        // El padre de la ruta es un fichero: ni se crea la carpeta ni se escribe.
        let file = dir.path().join("fichero");
        std::fs::write(&file, b"no es una carpeta").unwrap();
        let g = Group::load(file.join("group.json"), [1u8; 32]);
        let mut rx = g.subscribe();
        g.add_device(&[7u8; 32], "Firefox");
        assert!(rx.has_changed().unwrap());
        assert_eq!(*rx.borrow_and_update(), 1);
        assert!(g.has_device(&[7u8; 32]), "en memoria sí está");
        assert!(!file.join("group.json").exists());
    }

    /// Quien tiene leída la revisión (un `Ref` de `borrow`) mientras otro hilo cambia el grupo no
    /// deja el estado bloqueado: la revisión sube después de soltarlo.
    #[test]
    fn holding_the_revision_does_not_lock_the_state() {
        let dir = tempfile::tempdir().unwrap();
        let g = std::sync::Arc::new(Group::load(dir.path().join("group.json"), [1u8; 32]));
        let rx = g.subscribe();
        let held = rx.borrow();
        let writer = {
            let g = g.clone();
            std::thread::spawn(move || g.add_device(&[7u8; 32], "Firefox"))
        };
        // Otro hilo lee el estado mientras el que escribe espera para subir la revisión.
        let (seen_tx, seen) = std::sync::mpsc::channel();
        {
            let g = g.clone();
            std::thread::spawn(move || {
                while !g.has_device(&[7u8; 32]) {
                    std::thread::sleep(Duration::from_millis(1));
                }
                let _ = seen_tx.send(());
            });
        }
        let r = seen.recv_timeout(Duration::from_secs(5));
        drop(held);
        assert!(r.is_ok(), "el estado se quedó bloqueado");
        writer.join().unwrap();
        assert_eq!(*rx.borrow(), 1);
    }

    #[test]
    fn every_change_bumps_the_revision() {
        let dir = tempfile::tempdir().unwrap();
        let g = Group::load(dir.path().join("group.json"), [1u8; 32]);
        let mut rx = g.subscribe();
        let start = *rx.borrow_and_update();
        // Cada mutación avisa una vez, y la revisión sube.
        let mut expect = start;
        let mut bumped = |rx: &mut tokio::sync::watch::Receiver<u64>, what: &str| {
            expect += 1;
            assert!(rx.has_changed().unwrap(), "{what} avisa");
            assert_eq!(*rx.borrow_and_update(), expect, "{what} sube la revisión");
        };
        g.set_self("yo", "wsl");
        bumped(&mut rx, "set_self");
        g.add_member("otro", "k-otro", "mac");
        bumped(&mut rx, "add_member");
        g.add_device(&[7u8; 32], "Firefox");
        bumped(&mut rx, "add_device");
        g.remove_device(&B64.encode([7u8; 32]));
        bumped(&mut rx, "remove_device");
        assert!(g.remove_member("otro"));
        bumped(&mut rx, "remove_member");
        let theirs = State {
            members: vec![member("mac", now_ms() + 1, false)],
            devices: vec![],
            keys: vec![],
        };
        assert!(g.merge(&theirs));
        bumped(&mut rx, "merge con cambios");
        // Lo que no cambia nada no avisa.
        assert!(!g.merge(&theirs));
        assert!(!rx.has_changed().unwrap(), "merge sin cambios");
        g.set_self("yo", "wsl");
        assert!(!rx.has_changed().unwrap(), "set_self igual");
        assert!(!g.remove_member("nadie"));
        g.remove_device("nadie");
        assert!(!rx.has_changed().unwrap(), "quitar lo que no está");
        // El llavero: crear la primera avisa; pedirla otra vez, no.
        let first = g.ensure_board_key();
        bumped(&mut rx, "ensure_board_key");
        assert_eq!(g.ensure_board_key(), first);
        assert!(!rx.has_changed().unwrap(), "ensure_board_key con clave");
        g.rotate_board_key();
        bumped(&mut rx, "rotate_board_key");
        assert!(g.drop_board_key(&first.kid));
        bumped(&mut rx, "drop_board_key");
        assert!(!g.drop_board_key(&first.kid));
        assert!(!g.drop_board_key("nadie"));
        assert!(!rx.has_changed().unwrap(), "quitar una clave que no está");
    }

    #[test]
    fn revision_counts_without_listeners() {
        let dir = tempfile::tempdir().unwrap();
        let g = Group::load(dir.path().join("group.json"), [1u8; 32]);
        // Sin nadie escuchando (como al arrancar) la revisión sigue subiendo, y quien se apunte
        // después la ve.
        g.add_device(&[7u8; 32], "Firefox");
        g.set_self("yo", "wsl");
        let rx = g.subscribe();
        assert_eq!(*rx.borrow(), 2);
        // Varios oyentes a la vez, cada uno con su aviso.
        let mut a = g.subscribe();
        let mut b = g.subscribe();
        g.add_member("otro", "k-otro", "mac");
        assert_eq!(*a.borrow_and_update(), 3);
        assert_eq!(*b.borrow_and_update(), 3);
    }

    // ---------- el llavero ----------

    fn board_key(kid: &str, at: u64, retired: Option<u64>, removed: bool) -> BoardKey {
        BoardKey {
            kid: kid.into(),
            // Una lápida, como las de `drop_board_key`, sin el secreto.
            key: if removed {
                String::new()
            } else {
                B64.encode([kid.len() as u8; 32])
            },
            at,
            retired,
            removed,
        }
    }

    fn keys_state(keys: Vec<BoardKey>) -> State {
        State {
            keys,
            ..State::default()
        }
    }

    /// El `kid` sale de la clave en bruto (los 32 bytes, no su base64).
    #[test]
    fn the_kid_comes_from_the_raw_key() {
        assert_eq!(board_kid(&[0u8; 32]), "cf1718d3fe1a8755");
        let seq: Vec<u8> = (0u8..32).collect();
        assert_eq!(board_kid(&seq), "1ffd9bb07aebb8f4");
        let k = BoardKey::generate(5);
        assert_eq!(k.kid, board_kid(&B64.decode(&k.key).unwrap()));
        assert_eq!(B64.decode(&k.key).unwrap().len(), 32);
        assert_eq!(k.at, 5);
        assert_eq!((k.retired, k.removed), (None, false));
        assert_ne!(BoardKey::generate(5).key, k.key, "cada una al azar");
    }

    /// Un nido 0.9.1 no conoce `keys`: lo que manda no las trae, y juntarlo no las borra.
    #[test]
    fn a_state_without_keys_keeps_ours() {
        let dir = tempfile::tempdir().unwrap();
        let g = Group::load(dir.path().join("group.json"), [1u8; 32]);
        g.set_self("yo", "wsl");
        let ours = g.ensure_board_key();
        let old: State = serde_json::from_str(&format!(
            r#"{{"members":[{{"id":"mac","key":"k-mac","name":"mac","at":{},"removed":false}}],"devices":[]}}"#,
            now_ms() + 1
        ))
        .unwrap();
        assert!(old.keys.is_empty());
        assert!(g.merge(&old));
        assert_eq!(g.others().len(), 1, "lo suyo entra");
        assert_eq!(g.board_keys().current(), Some(&ours), "lo nuestro sigue");
        // Y al revés: lo que él guarde y nos devuelva sin `keys` tampoco las quita.
        let back: serde_json::Value = serde_json::to_value(g.snapshot()).unwrap();
        let mut back = back.as_object().unwrap().clone();
        back.remove("keys");
        let back: State = serde_json::from_value(back.into()).unwrap();
        assert!(!g.merge(&back));
        assert_eq!(g.board_keys().current(), Some(&ours));
    }

    /// Dos equipos crean su primera clave a la vez: al juntarse, los dos eligen la misma.
    #[test]
    fn keys_made_at_once_pick_the_same_current() {
        // A igual `at`, la de `kid` menor; da igual el orden en que se junten.
        let a = keys_state(vec![board_key("bbbbbbbbbbbbbbbb", 100, None, false)]);
        let b = keys_state(vec![board_key("aaaaaaaaaaaaaaaa", 100, None, false)]);
        let mut ab = a.clone();
        assert!(ab.merge(&b));
        let mut ba = b.clone();
        assert!(ba.merge(&a));
        assert_eq!(ab, ba);
        assert_eq!(
            Keyring::of(&ab).current().map(|k| k.kid.as_str()),
            Some("aaaaaaaaaaaaaaaa")
        );
        // A distinto `at`, la más nueva.
        let c = keys_state(vec![board_key("cccccccccccccccc", 101, None, false)]);
        assert!(ab.merge(&c));
        assert_eq!(
            Keyring::of(&ab).current().map(|k| k.kid.as_str()),
            Some("cccccccccccccccc")
        );

        // Y con dos grupos de verdad.
        let d1 = tempfile::tempdir().unwrap();
        let d2 = tempfile::tempdir().unwrap();
        let g1 = Group::load(d1.path().join("group.json"), [1u8; 32]);
        let g2 = Group::load(d2.path().join("group.json"), [2u8; 32]);
        let k1 = g1.ensure_board_key();
        let k2 = g2.ensure_board_key();
        assert_ne!(k1.kid, k2.kid);
        assert!(g1.merge(&g2.snapshot()));
        assert!(g2.merge(&g1.snapshot()));
        let c1 = g1.board_keys().current().cloned().unwrap();
        let c2 = g2.board_keys().current().cloned().unwrap();
        assert_eq!(c1, c2);
        assert_eq!(
            g1.board_keys().keys.len(),
            2,
            "la otra sigue: hay objetos con ella"
        );
        // Pedir la clave ya no crea otra.
        assert_eq!(g1.ensure_board_key(), c1);
        assert_eq!(g2.ensure_board_key(), c1);
    }

    /// Rotar retira las demás, y la retirada llega a los otros equipos (aunque se rote en el
    /// mismo milisegundo en que se creó).
    #[test]
    fn a_retired_key_merges() {
        let d1 = tempfile::tempdir().unwrap();
        let d2 = tempfile::tempdir().unwrap();
        let g1 = Group::load(d1.path().join("group.json"), [1u8; 32]);
        let g2 = Group::load(d2.path().join("group.json"), [2u8; 32]);
        let old = g1.ensure_board_key();
        assert!(g2.merge(&g1.snapshot()));
        assert_eq!(g2.board_keys().current(), Some(&old));
        let new = g1.rotate_board_key();
        assert_ne!(new.kid, old.kid);
        assert_eq!(g1.board_keys().current(), Some(&new));
        assert!(g2.merge(&g1.snapshot()));
        let ring = g2.board_keys();
        assert_eq!(ring.current(), Some(&new));
        let retired = ring.keys.iter().find(|k| k.kid == old.kid).unwrap();
        assert!(retired.retired.is_some(), "retirada también en el otro");
        assert!(retired.at > old.at, "el cambio gana al juntar");
        assert_eq!(g1.board_keys(), g2.board_keys());
        // Rotar otra vez no vuelve a sellar la ya retirada (su plazo de 30 días sigue).
        let since = retired.retired;
        let newer_key = g1.rotate_board_key();
        let ring = g1.board_keys();
        assert_eq!(ring.current(), Some(&newer_key));
        let first = ring.keys.iter().find(|k| k.kid == old.kid).unwrap();
        assert_eq!(first.retired, since);
        assert!(
            ring.keys
                .iter()
                .find(|k| k.kid == new.kid)
                .unwrap()
                .retired
                .is_some()
        );
        // Lo retirado no es la actual aunque sea lo más nuevo por `at`.
        let st = keys_state(vec![
            board_key("aaaaaaaaaaaaaaaa", 100, None, false),
            board_key("bbbbbbbbbbbbbbbb", 200, Some(200), false),
        ]);
        assert_eq!(
            Keyring::of(&st).current().map(|k| k.kid.as_str()),
            Some("aaaaaaaaaaaaaaaa")
        );
    }

    /// A igual `at`, el borrado gana (y no resucita), en los dos órdenes.
    #[test]
    fn a_removed_key_wins_ties() {
        let live = keys_state(vec![board_key("aaaaaaaaaaaaaaaa", 100, None, false)]);
        let gone = keys_state(vec![board_key("aaaaaaaaaaaaaaaa", 100, None, true)]);
        let mut a = live.clone();
        assert!(a.merge(&gone));
        let mut b = gone.clone();
        assert!(!b.merge(&live));
        assert_eq!(a, b);
        assert!(a.keys[0].removed);
        assert!(Keyring::of(&a).current().is_none());
        assert!(
            Keyring::of(&a).keys.is_empty(),
            "el llavero no da las borradas"
        );
        // Una más nueva sí gana al borrado.
        let back = keys_state(vec![board_key("aaaaaaaaaaaaaaaa", 101, None, false)]);
        assert!(a.merge(&back));
        assert!(!a.keys[0].removed);

        // Quitar la actual deja sin actual, y pedirla crea otra.
        let dir = tempfile::tempdir().unwrap();
        let g = Group::load(dir.path().join("group.json"), [1u8; 32]);
        let k = g.ensure_board_key();
        assert!(g.drop_board_key(&k.kid));
        assert!(g.board_keys().current().is_none());
        let dropped = g
            .snapshot()
            .keys
            .into_iter()
            .find(|x| x.kid == k.kid)
            .unwrap();
        assert!(
            dropped.removed && dropped.at > k.at,
            "la baja gana al juntar"
        );
        let k2 = g.ensure_board_key();
        assert_ne!(k2.kid, k.kid);
    }

    /// El equipo al que sacan del grupo se queda sin claves (decisión 8): el tablero sigue a la
    /// casa. También en el disco.
    #[test]
    fn the_removed_machine_has_no_keys() {
        let dir = tempfile::tempdir().unwrap();
        let g = Group::load(dir.path().join("group.json"), [1u8; 32]);
        g.set_self("yo", "wsl");
        g.add_member("otro", "k-otro", "mac");
        g.ensure_board_key();
        g.rotate_board_key();
        let mut st = g.snapshot();
        for m in st.members.iter_mut().filter(|m| m.id == "yo") {
            m.removed = true;
            m.at = now_ms() + 1;
        }
        assert!(g.merge(&st));
        assert!(g.snapshot().keys.is_empty());
        assert!(g.board_keys().current().is_none());
        let again = Group::load(dir.path().join("group.json"), [1u8; 32]);
        assert!(again.snapshot().keys.is_empty(), "ni en el disco");
        assert_eq!(again.self_id().as_deref(), Some("yo"));
    }

    /// El `State` entero va en un mensaje Noise de 65535 bytes: con 8 claves (con lápidas) y un
    /// grupo grande, el JSON compacto que viaja queda muy por debajo.
    #[test]
    fn eight_keys_fit_in_one_message() {
        let mut st = State::default();
        for i in 0..12 {
            st.members.push(Member {
                id: format!("{i:026}"),
                key: B64.encode([i as u8; 32]),
                name: "un nombre de equipo bastante largo".into(),
                at: now_ms(),
                removed: i % 4 == 0,
            });
        }
        for i in 0..60 {
            st.devices.push(Device {
                key: B64.encode([100 + i as u8; 32]),
                name: "Firefox en el portátil de casa".into(),
                at: now_ms(),
                removed: i % 3 == 0,
            });
        }
        for i in 0..8 {
            let mut k = BoardKey::generate(now_ms());
            if i < 6 {
                k.retired = Some(now_ms());
            }
            k.removed = i == 0;
            st.keys.push(k);
        }
        let one = serde_json::to_vec(&st.keys[1]).unwrap().len();
        assert!(one <= 140, "una clave ocupa {one} bytes");
        let wire = serde_json::to_vec(&st).unwrap().len();
        assert!(wire < 16 * 1024, "el estado ocupa {wire} bytes");
        assert!(wire < crate::e2e::MAX_MSG);
    }

    /// `group.json` lleva el llavero: solo para el usuario, también si quedó un temporal de antes
    /// con otros permisos.
    #[cfg(unix)]
    #[test]
    fn the_group_file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("group.json");
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, b"de antes").unwrap();
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o644)).unwrap();
        let g = Group::load(path.clone(), [1u8; 32]);
        g.ensure_board_key();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        assert!(!tmp.exists());
        // Y un fichero de antes, con otros permisos, también queda solo para el usuario.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o664)).unwrap();
        g.rotate_board_key();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    /// Una clave quitada no deja el secreto en su lápida: ni en el disco ni en lo que viaja. Y la
    /// lápida gana igual al juntar.
    #[test]
    fn a_dropped_key_leaves_no_secret() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("group.json");
        let g = Group::load(path.clone(), [1u8; 32]);
        let k = g.ensure_board_key();
        let before = g.snapshot();
        assert!(g.drop_board_key(&k.kid));
        let wire = serde_json::to_string(&g.snapshot()).unwrap();
        assert!(!wire.contains(&k.key), "viaja la clave");
        assert!(wire.contains(&k.kid), "la lápida sí va");
        let disk = std::fs::read_to_string(&path).unwrap();
        assert!(!disk.contains(&k.key), "la clave sigue en el disco");
        let tomb = &g.snapshot().keys[0];
        assert!(tomb.removed && tomb.key.is_empty() && tomb.at > k.at);
        // Otro equipo con la clave viva recibe la lápida: se queda sin el secreto.
        let mut other = before.clone();
        assert!(other.merge(&g.snapshot()));
        assert!(other.keys[0].removed && other.keys[0].key.is_empty());
        assert!(!serde_json::to_string(&other).unwrap().contains(&k.key));
        // Y su copia viva, más vieja, no la resucita.
        assert!(!g.merge(&before));
        assert!(g.board_keys().current().is_none());
        // Una lápida que llegue con la clave (de quien no la vació) tampoco la guarda.
        let mut leaky = before.clone();
        leaky.keys[0].removed = true;
        leaky.keys[0].at = k.at + 1_000_000;
        let mut st = before.clone();
        assert!(st.merge(&leaky));
        assert!(st.keys[0].key.is_empty());
    }

    /// Un `at` al borde (de un reloj o un equipo raro) no desborda.
    #[test]
    fn a_huge_at_does_not_overflow() {
        let dir = tempfile::tempdir().unwrap();
        let g = Group::load(dir.path().join("group.json"), [1u8; 32]);
        let edge = BoardKey::generate(u64::MAX);
        assert!(g.merge(&keys_state(vec![edge.clone()])));
        let new = g.rotate_board_key();
        assert_eq!(new.at, u64::MAX);
        assert!(g.drop_board_key(&edge.kid));
    }

    /// Lo que cuenta otro equipo solo entra si es un miembro vivo, y a uno ya quitado se le da el
    /// estado sin el llavero (la misma regla para quien llama y para quien contesta).
    #[test]
    fn only_live_members_are_heard() {
        let dir = tempfile::tempdir().unwrap();
        let g = Group::load(dir.path().join("group.json"), [1u8; 32]);
        g.set_self("yo", "wsl");
        g.add_member("mac", "k-mac", "mac");
        g.add_member("viejo", "k-viejo", "viejo");
        assert!(g.remove_member("viejo"));
        let ours = g.ensure_board_key();
        let before = g.snapshot();
        let later = now_ms() + 60_000;
        let theirs = State {
            members: vec![
                Member {
                    id: "viejo".into(),
                    key: "k-viejo".into(),
                    name: "viejo".into(),
                    at: later,
                    removed: false,
                },
                member("intruso", later, false),
            ],
            devices: vec![device("colado", later, false)],
            keys: vec![BoardKey::generate(later)],
        };
        assert!(g.is_live("k-mac"));
        assert!(!g.is_live("k-viejo"));
        assert!(!g.is_live("k-nadie"));
        // Del quitado no entra nada, ni dándose de alta a sí mismo.
        assert!(!g.merge_from("k-viejo", &theirs));
        assert_eq!(g.snapshot(), before);
        // Y se entera de que está fuera, sin claves.
        let told = g.state_for("k-viejo").unwrap();
        assert!(told.keys.is_empty());
        assert!(told.members.iter().any(|m| m.id == "viejo" && m.removed));
        // A uno de fuera, nada; de él, tampoco.
        assert!(g.state_for("k-nadie").is_none());
        assert!(!g.merge_from("k-nadie", &theirs));
        assert_eq!(g.snapshot(), before);
        // Un miembro vivo recibe las claves, y lo suyo entra.
        assert!(g.state_for("k-mac").unwrap().keys.contains(&ours));
        assert!(g.merge_from("k-mac", &theirs));
        assert!(
            g.board_keys()
                .keys
                .iter()
                .any(|k| k.kid == theirs.keys[0].kid)
        );
    }

    /// Una clave que llega con un `kid` que no es el suyo no entra (lo demás, sí). Las lápidas
    /// sin clave pasan.
    #[test]
    fn keys_must_match_their_kid() {
        let dir = tempfile::tempdir().unwrap();
        let g = Group::load(dir.path().join("group.json"), [1u8; 32]);
        let good = BoardKey::generate(10);
        let mut forged = BoardKey::generate(10);
        forged.kid = good.kid.clone();
        forged.at = 20;
        let mut liar = BoardKey::generate(10);
        liar.kid = "0123456789abcdef".into();
        let tomb = BoardKey {
            kid: "fedcba9876543210".into(),
            key: String::new(),
            at: 10,
            retired: None,
            removed: true,
        };
        let empty = BoardKey {
            kid: "1111111111111111".into(),
            key: String::new(),
            at: 10,
            retired: None,
            removed: false,
        };
        let junk = BoardKey {
            kid: "2222222222222222".into(),
            key: "no es base64".into(),
            at: 10,
            retired: None,
            removed: false,
        };
        assert!(good.fits() && tomb.fits());
        assert!(!forged.fits() && !liar.fits() && !empty.fits() && !junk.fits());
        assert!(g.merge(&keys_state(vec![
            good.clone(),
            liar,
            tomb.clone(),
            empty,
            junk
        ])));
        let kids: Vec<String> = g.snapshot().keys.into_iter().map(|k| k.kid).collect();
        let mut expect = vec![good.kid.clone(), tomb.kid.clone()];
        expect.sort();
        assert_eq!(kids, expect);
        // Una que se hace pasar por la nuestra (mismo `kid`, otra clave, más nueva) tampoco.
        assert!(!g.merge(&keys_state(vec![forged])));
        assert_eq!(g.board_keys().current(), Some(&good));
    }

    /// La clave es secreta: ni en `Debug` del llavero ni en el del estado.
    #[test]
    fn debug_hides_the_key() {
        let k = BoardKey::generate(1);
        let st = keys_state(vec![k.clone()]);
        let ring = Keyring::of(&st);
        for out in [format!("{k:?}"), format!("{st:?}"), format!("{ring:?}")] {
            assert!(!out.contains(&k.key), "se ve la clave: {out}");
            assert!(out.contains(&k.kid), "el kid sí: {out}");
        }
    }
}
