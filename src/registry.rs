//! Registro de sesiones: qué hay, dónde y cómo volver a abrirlas.

use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// El Claude que corre en la sesión (como orden de arranque o lanzado a mano en una shell).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaudeLink {
    /// La conversación: `claude --resume <session_id>` la retoma.
    pub session_id: String,
    /// `CLAUDE_CONFIG_DIR` con el que corría (perfil de cswap); `None` es `~/.claude`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_dir: Option<PathBuf>,
    /// Cuenta de cswap con la que se creó la sesión (`cswap run <cuenta> -- claude …`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    /// Lanzado a mano dentro de una shell: al restaurar se abre la shell y se escribe en ella.
    #[serde(default)]
    pub inside_shell: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub name: String,
    pub cwd: PathBuf,
    /// Orden de arranque y argumentos (sin shell de por medio).
    pub argv: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env: Vec<(String, String)>,
    pub autostart: bool,
    /// Segundos desde 1970.
    pub created: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude: Option<ClaudeLink>,
    /// Carpeta lógica para agruparla en la web (nada en disco).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder: Option<String>,
    /// Cuándo terminó de trabajar por última vez (ms desde 1970; 0: nunca).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub finished: u64,
    /// Cuándo la vio el usuario por última vez (ms). Terminó después: «sin leer».
    #[serde(default, skip_serializing_if = "is_zero")]
    pub seen: u64,
}

fn is_zero(v: &u64) -> bool {
    *v == 0
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct File {
    sessions: Vec<Session>,
    /// Orden de las carpetas en la web (las que no estén, detrás).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    folders: Vec<String>,
}

pub struct Registry {
    path: PathBuf,
    sessions: Vec<Session>,
    folders: Vec<String>,
}

#[derive(Debug)]
pub enum RegistryError {
    Io(io::Error),
    BadName(String),
    BadFolder(String),
    NameTaken(String),
    NotFound(String),
}

impl std::fmt::Display for RegistryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RegistryError::Io(e) => write!(f, "{e}"),
            RegistryError::BadName(n) => {
                write!(
                    f,
                    "nombre no válido: {n:?} (letras, números, «-», «_» o «.»; hasta 64)"
                )
            }
            RegistryError::BadFolder(n) => {
                write!(f, "carpeta no válida: {n:?} (hasta 64 caracteres, sin «/»)")
            }
            RegistryError::NameTaken(n) => write!(f, "ya hay una sesión llamada {n:?}"),
            RegistryError::NotFound(n) => write!(f, "no hay ninguna sesión {n:?}"),
        }
    }
}

impl std::error::Error for RegistryError {}

impl From<io::Error> for RegistryError {
    fn from(e: io::Error) -> Self {
        RegistryError::Io(e)
    }
}

pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.chars().count() <= 64
        && name
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.'))
        && !name.starts_with(['-', '.'])
}

/// Una carpeta: texto libre de una línea, sin «/» (de momento no hay subcarpetas).
pub fn valid_folder(folder: &str) -> bool {
    let f = folder.trim();
    !f.is_empty()
        && f.chars().count() <= 64
        && f == folder
        && !f.contains('/')
        && !f.chars().any(char::is_control)
}

pub fn new_id() -> String {
    let mut b = [0u8; 4];
    getrandom::fill(&mut b).expect("sin aleatoriedad del sistema");
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Milisegundos desde 1970 (para «terminó» y «visto», que no deben empatar en el mismo segundo).
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl Registry {
    /// Sin fichero, registro vacío.
    pub fn load(path: &Path) -> Result<Self, RegistryError> {
        let file = match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice::<File>(&bytes)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => File::default(),
            Err(e) => return Err(e.into()),
        };
        Ok(Self {
            path: path.to_owned(),
            sessions: file.sessions,
            folders: file.folders,
        })
    }

    /// Escribe en un temporal y lo renombra: nunca queda un registro a medias.
    pub fn save(&self) -> Result<(), RegistryError> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = self.path.with_extension("json.tmp");
        let body = serde_json::to_vec_pretty(&File {
            sessions: self.sessions.clone(),
            folders: self.folders.clone(),
        })
        .map_err(io::Error::other)?;
        {
            let mut opts = std::fs::OpenOptions::new();
            opts.write(true).create(true).truncate(true);
            #[cfg(unix)]
            std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
            let mut f = opts.open(&tmp)?;
            f.write_all(&body)?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }

    pub fn sessions(&self) -> &[Session] {
        &self.sessions
    }

    pub fn get(&self, id: &str) -> Option<&Session> {
        self.sessions.iter().find(|s| s.id == id)
    }

    pub fn by_name(&self, name: &str) -> Option<&Session> {
        self.sessions.iter().find(|s| s.name == name)
    }

    /// Por id o por nombre.
    pub fn find(&self, key: &str) -> Option<&Session> {
        self.get(key).or_else(|| self.by_name(key))
    }

    pub fn add(&mut self, session: Session) -> Result<Session, RegistryError> {
        if !valid_name(&session.name) {
            return Err(RegistryError::BadName(session.name));
        }
        if self.by_name(&session.name).is_some() {
            return Err(RegistryError::NameTaken(session.name));
        }
        if let Some(f) = session.folder.as_deref().filter(|f| !valid_folder(f)) {
            return Err(RegistryError::BadFolder(f.to_owned()));
        }
        self.sessions.push(session.clone());
        self.save()?;
        Ok(session)
    }

    pub fn update(
        &mut self,
        id: &str,
        f: impl FnOnce(&mut Session),
    ) -> Result<Session, RegistryError> {
        let idx = self
            .sessions
            .iter()
            .position(|s| s.id == id)
            .ok_or_else(|| RegistryError::NotFound(id.to_owned()))?;
        let mut updated = self.sessions[idx].clone();
        f(&mut updated);
        if let Some(f) = updated.folder.as_deref().filter(|f| !valid_folder(f)) {
            return Err(RegistryError::BadFolder(f.to_owned()));
        }
        if updated.name != self.sessions[idx].name {
            if !valid_name(&updated.name) {
                return Err(RegistryError::BadName(updated.name));
            }
            if self.by_name(&updated.name).is_some() {
                return Err(RegistryError::NameTaken(updated.name));
            }
        }
        self.sessions[idx] = updated.clone();
        self.save()?;
        Ok(updated)
    }

    /// Orden de las carpetas tal como lo dejó el usuario.
    pub fn folders(&self) -> &[String] {
        &self.folders
    }

    pub fn set_folder_order(&mut self, order: Vec<String>) -> Result<(), RegistryError> {
        let mut seen = std::collections::HashSet::new();
        self.folders = order
            .into_iter()
            .filter(|f| valid_folder(f) && seen.insert(f.clone()))
            .collect();
        self.save()
    }

    /// Pone delante, en este orden, las sesiones de `ids`; las demás siguen detrás como estaban.
    pub fn reorder(&mut self, ids: &[String]) -> Result<(), RegistryError> {
        let mut rest = std::mem::take(&mut self.sessions);
        let mut ordered = Vec::with_capacity(rest.len());
        for id in ids {
            if let Some(i) = rest.iter().position(|s| &s.id == id) {
                ordered.push(rest.remove(i));
            }
        }
        ordered.extend(rest);
        self.sessions = ordered;
        self.save()
    }

    /// Renombra una carpeta (vacío: la deshace) en todas sus sesiones y en el orden, de una vez.
    pub fn rename_folder(&mut self, from: &str, to: &str) -> Result<usize, RegistryError> {
        let to = to.trim();
        if !to.is_empty() && !valid_folder(to) {
            return Err(RegistryError::BadFolder(to.to_owned()));
        }
        let new = (!to.is_empty()).then(|| to.to_owned());
        let mut n = 0;
        for s in self
            .sessions
            .iter_mut()
            .filter(|s| s.folder.as_deref() == Some(from))
        {
            s.folder.clone_from(&new);
            n += 1;
        }
        match (self.folders.iter().position(|f| f == from), &new) {
            (Some(i), Some(t)) if !self.folders.contains(t) => self.folders[i] = t.clone(),
            (Some(i), _) => {
                self.folders.remove(i);
            }
            (None, _) => {}
        }
        self.save()?;
        Ok(n)
    }

    pub fn remove(&mut self, id: &str) -> Result<Session, RegistryError> {
        let idx = self
            .sessions
            .iter()
            .position(|s| s.id == id)
            .ok_or_else(|| RegistryError::NotFound(id.to_owned()))?;
        let removed = self.sessions.remove(idx);
        self.save()?;
        Ok(removed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(name: &str) -> Session {
        Session {
            id: new_id(),
            name: name.into(),
            cwd: "/tmp".into(),
            argv: vec!["bash".into()],
            env: vec![],
            autostart: true,
            created: now(),
            claude: None,
            folder: None,
            finished: 0,
            seen: 0,
        }
    }

    #[test]
    fn saves_and_loads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nido/sessions.json");
        let mut r = Registry::load(&path).unwrap();
        assert!(r.sessions().is_empty());
        let mut s = session("proyecto-api");
        s.claude = Some(ClaudeLink {
            session_id: "fc88cfd5-bea9-4ea4-b5da-5dcb4efbfdd6".into(),
            config_dir: Some("/perfil".into()),
            account: None,
            inside_shell: true,
        });
        r.add(s.clone()).unwrap();
        let r = Registry::load(&path).unwrap();
        assert_eq!(r.sessions(), [s.clone()]);
        assert_eq!(r.find("proyecto-api").unwrap().id, s.id);
        assert_eq!(r.find(&s.id).unwrap().name, "proyecto-api");
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn names_are_unique_and_valid() {
        let dir = tempfile::tempdir().unwrap();
        let mut r = Registry::load(&dir.path().join("s.json")).unwrap();
        let a = r.add(session("uno")).unwrap();
        r.add(session("dos")).unwrap();
        assert!(matches!(
            r.add(session("uno")),
            Err(RegistryError::NameTaken(_))
        ));
        for bad in ["", "-x", ".x", "con espacio", "a/b", &"x".repeat(65)] {
            assert!(
                matches!(r.add(session(bad)), Err(RegistryError::BadName(_))),
                "{bad:?}"
            );
        }
        assert!(valid_name("niño_2.0"));
        assert!(matches!(
            r.update(&a.id, |s| s.name = "dos".into()),
            Err(RegistryError::NameTaken(_))
        ));
        r.update(&a.id, |s| s.autostart = false).unwrap();
        r.update(&a.id, |s| s.folder = Some("Trabajo".into()))
            .unwrap();
        assert_eq!(r.get(&a.id).unwrap().folder.as_deref(), Some("Trabajo"));
        for bad in ["", " x", "a/b", "x\n"] {
            assert!(matches!(
                r.update(&a.id, |s| s.folder = Some(bad.into())),
                Err(RegistryError::BadFolder(_))
            ));
        }
        assert!(!r.get(&a.id).unwrap().autostart);
        r.remove(&a.id).unwrap();
        assert!(r.get(&a.id).is_none());
    }

    #[test]
    fn reorders_sessions_and_folders() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.json");
        let mut r = Registry::load(&path).unwrap();
        let ids: Vec<String> = ["a", "b", "c", "d"]
            .iter()
            .map(|n| {
                let mut s = session(n);
                s.folder = Some(if *n < "c" { "uno".into() } else { "dos".into() });
                r.add(s).unwrap().id
            })
            .collect();
        r.reorder(&[ids[2].clone(), ids[0].clone(), "no-existe".into()])
            .unwrap();
        r.set_folder_order(vec!["dos".into(), "uno".into(), "dos".into(), "a/b".into()])
            .unwrap();
        let back = Registry::load(&path).unwrap();
        let names: Vec<_> = back.sessions().iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["c", "a", "b", "d"]);
        assert_eq!(back.folders(), ["dos", "uno"]);

        let mut back = back;
        assert_eq!(back.rename_folder("uno", "primera").unwrap(), 2);
        assert_eq!(back.folders(), ["dos", "primera"]);
        assert_eq!(back.rename_folder("dos", "").unwrap(), 2);
        assert_eq!(back.folders(), ["primera"]);
        assert!(
            back.sessions()
                .iter()
                .filter(|s| s.folder.is_none())
                .count()
                == 2
        );
    }

    #[test]
    fn a_failed_save_keeps_the_old_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.json");
        let mut r = Registry::load(&path).unwrap();
        r.add(session("uno")).unwrap();
        // El temporal no se puede crear: el guardado falla y el fichero bueno sigue intacto.
        std::fs::create_dir(path.with_extension("json.tmp")).unwrap();
        assert!(r.add(session("dos")).is_err());
        let back = Registry::load(&path).unwrap();
        assert_eq!(back.sessions().len(), 1);
    }
}
