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

const INVITE_LIFE: Duration = Duration::from_secs(600);

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

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct State {
    #[serde(default)]
    pub members: Vec<Member>,
    #[serde(default)]
    pub devices: Vec<Device>,
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
        }
    }

    fn save(&self, st: &State) {
        let tmp = self.path.with_extension("json.tmp");
        let write = || -> std::io::Result<()> {
            if let Some(dir) = self.path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            std::fs::write(&tmp, serde_json::to_vec_pretty(st)?)?;
            std::fs::rename(&tmp, &self.path)
        };
        if let Err(e) = write() {
            eprintln!("nido: no se pudo guardar el grupo: {e}");
        }
    }

    pub fn snapshot(&self) -> State {
        self.state.lock().expect("grupo").clone()
    }

    /// Junta el estado de otro equipo; si cambió, lo guarda. Si a este equipo lo han sacado del
    /// grupo, se queda solo (con sus dispositivos).
    pub fn merge(&self, other: &State) -> bool {
        let mut st = self.state.lock().expect("grupo");
        let changed = st.merge(other);
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
            }
            self.save(&st);
        }
        changed
    }

    fn change(&self, f: impl FnOnce(&mut State)) {
        let mut st = self.state.lock().expect("grupo");
        f(&mut st);
        self.save(&st);
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

    /// Los dispositivos dados de baja en el grupo (para quitarlos también de este equipo).
    pub fn removed_devices(&self) -> Vec<String> {
        self.snapshot()
            .devices
            .into_iter()
            .filter(|d| d.removed)
            .map(|d| d.key)
            .collect()
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
        };
        let b = State {
            members: vec![member("mac", 15, false)],
            devices: vec![device("iphone", 30, true), device("chrome", 20, true)],
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
        });
        assert_eq!(c, a);
        // Un emparejamiento posterior lo vuelve a autorizar.
        assert!(a.merge(&State {
            members: vec![],
            devices: vec![device("iphone", 40, false)]
        }));
        assert!(a.device("iphone").is_some());
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
}
