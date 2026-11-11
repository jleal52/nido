//! Los secretos que piden las sesiones de Claude con el MCP de nido (`secret`): el usuario los
//! escribe en la web o en la app, llegan cifrados por el canal de siempre y se guardan en un
//! fichero solo suyo (0600) bajo `<state>/secrets/` (0700; en Windows, en el perfil del
//! usuario). La sesión recibe la ruta, nunca el valor.
//!
//! - Recordados: `equipo/<nombre>`, para todas las sesiones de este equipo. Su caducidad y quién
//!   lo pidió por última vez van aparte, en `equipo.json` (sin el valor).
//! - Sin recordar: `sesiones/<id>/<nombre>`, solo para esa sesión; se borra al cerrarla o a las
//!   24 h.
//!
//! Las preguntas en curso ([`Asks`]) viven en memoria: una por nombre (dos sesiones que piden el
//! mismo comparten diálogo) y una por sesión.

use std::collections::HashMap;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Cuánto espera una pregunta a que el usuario conteste.
pub const ASK_WAIT: Duration = Duration::from_secs(15 * 60);
/// Lo que vive, como mucho, un secreto sin recordar (ms).
pub const SESSION_KEEP_MS: u64 = 24 * 3600 * 1000;
/// Lo más que ocupa un secreto.
pub const MAX_VALUE: usize = 16 * 1024;

const HOUR_MS: u64 = 3600 * 1000;

/// Un nombre de secreto: `[A-Za-z0-9._-]`, de 1 a 64, sin `.` ni `..` solos. Va tal cual en una
/// ruta: nada más.
pub fn valid_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name != "."
        && name != ".."
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// Cuánto recordar lo que contesta el usuario.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Remember {
    #[serde(rename = "no")]
    No,
    #[serde(rename = "2h")]
    TwoHours,
    #[serde(rename = "7d")]
    SevenDays,
    #[serde(rename = "always")]
    Always,
}

impl Remember {
    /// Hasta cuándo (ms) vale si se recuerda; `None`, siempre.
    pub fn expires(self, now: u64) -> Option<u64> {
        match self {
            Remember::No => Some(now),
            Remember::TwoHours => Some(now + 2 * HOUR_MS),
            Remember::SevenDays => Some(now + 7 * 24 * HOUR_MS),
            Remember::Always => None,
        }
    }
}

/// Un secreto recordado, sin su valor (lo que guarda `equipo.json` y enseñan la web, la app y
/// `nido secrets`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Remembered {
    pub name: String,
    /// Hasta cuándo (ms); `None`: siempre.
    pub expires: Option<u64>,
    /// La sesión que lo pidió por última vez (su nombre, o los de las que lo pidieron a la vez).
    pub by: String,
    /// Cuándo (ms).
    pub at: u64,
}

/// Los secretos de este equipo, en `<state>/secrets`.
pub struct Store {
    dir: PathBuf,
}

fn bad(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, what.to_owned())
}

fn check_name(name: &str) -> io::Result<()> {
    if valid_name(name) {
        Ok(())
    } else {
        Err(bad("nombre de secreto no válido"))
    }
}

/// Un id de sesión va en una ruta: solo si parece uno de nido.
fn check_session(id: &str) -> io::Result<()> {
    match crate::mcp::session_from(Some(id.to_owned())) {
        Some(_) => Ok(()),
        None => Err(bad("sesión no válida")),
    }
}

/// Escribe `data` en `path` solo para el usuario (0600), entero o nada: a un temporal al lado y
/// luego se renombra.
pub(crate) fn write_private(path: &Path, data: &[u8]) -> io::Result<()> {
    let name = path
        .file_name()
        .ok_or_else(|| bad("ruta sin nombre"))?
        .to_string_lossy();
    let tmp = path.with_file_name(format!(".{name}.{}.tmp", crate::registry::new_id()));
    let res = write_new(&tmp, data).and_then(|()| std::fs::rename(&tmp, path));
    if res.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    res
}

/// Crea `path` (no puede existir) solo para el usuario y escribe `data`, ya en el disco.
fn write_new(path: &Path, data: &[u8]) -> io::Result<()> {
    let mut f =
        crate::sys::private(std::fs::OpenOptions::new().write(true).create_new(true)).open(path)?;
    f.write_all(data)?;
    f.sync_all()
}

/// Los ms desde 1970 de la última modificación de `p`.
fn mtime_ms(p: &Path) -> Option<u64> {
    let t = std::fs::metadata(p).ok()?.modified().ok()?;
    Some(t.duration_since(std::time::UNIX_EPOCH).ok()?.as_millis() as u64)
}

impl Store {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn team_dir(&self) -> PathBuf {
        self.dir.join("equipo")
    }

    fn index(&self) -> PathBuf {
        self.dir.join("equipo.json")
    }

    fn sessions_dir(&self) -> PathBuf {
        self.dir.join("sesiones")
    }

    /// `dir` y las carpetas entre él y `secrets/`, creadas y solo para el usuario (0700).
    fn private_dirs(&self, dir: &Path) -> io::Result<()> {
        std::fs::create_dir_all(dir)?;
        let mut d = dir;
        loop {
            crate::sys::private_dir(d)?;
            if d == self.dir {
                return Ok(());
            }
            match d.parent() {
                Some(p) if p.starts_with(&self.dir) => d = p,
                _ => return Ok(()),
            }
        }
    }

    fn load(&self) -> Vec<Remembered> {
        std::fs::read(self.index())
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    fn store(&self, list: &[Remembered]) -> io::Result<()> {
        self.private_dirs(&self.dir)?;
        let json = serde_json::to_vec_pretty(list).map_err(io::Error::other)?;
        write_private(&self.index(), &json)
    }

    /// La ruta del recordado `name`, si lo hay y no ha caducado.
    pub fn remembered(&self, name: &str, now: u64) -> Option<PathBuf> {
        let path = self.team_dir().join(name);
        let alive = valid_name(name)
            && self
                .load()
                .iter()
                .any(|r| r.name == name && r.expires.is_none_or(|e| e > now));
        (alive && path.is_file()).then_some(path)
    }

    /// Lo pidió otra vez `by` (se entrega sin diálogo, pero se apunta).
    pub fn touch(&self, name: &str, by: &str, now: u64) -> io::Result<()> {
        let mut list = self.load();
        if let Some(r) = list.iter_mut().find(|r| r.name == name) {
            r.by = by.to_owned();
            r.at = now;
        }
        self.store(&list)
    }

    /// Guarda un secreto para todo el equipo (lo sustituye si ya estaba).
    pub fn save_team(
        &self,
        name: &str,
        value: &[u8],
        remember: Remember,
        by: &str,
        now: u64,
    ) -> io::Result<PathBuf> {
        check_name(name)?;
        let dir = self.team_dir();
        self.private_dirs(&dir)?;
        let path = dir.join(name);
        write_private(&path, value)?;
        let mut list = self.load();
        list.retain(|r| r.name != name);
        list.push(Remembered {
            name: name.to_owned(),
            expires: remember.expires(now),
            by: by.to_owned(),
            at: now,
        });
        list.sort_by(|a, b| a.name.cmp(&b.name));
        self.store(&list)?;
        Ok(path)
    }

    /// Guarda un secreto solo para la sesión `session`.
    pub fn save_session(&self, session: &str, name: &str, value: &[u8]) -> io::Result<PathBuf> {
        check_name(name)?;
        check_session(session)?;
        let dir = self.sessions_dir().join(session);
        self.private_dirs(&dir)?;
        let path = dir.join(name);
        write_private(&path, value)?;
        Ok(path)
    }

    /// Lo que contestó el usuario a una pregunta de `sessions`: sin recordar, uno para cada una;
    /// recordado, uno para el equipo. Con `force` y sin recordar, el recordado de antes ya no
    /// valía: se olvida. Devuelve la ruta de cada sesión.
    #[allow(clippy::too_many_arguments)]
    pub fn save_answer(
        &self,
        name: &str,
        sessions: &[String],
        value: &[u8],
        remember: Remember,
        forced: bool,
        by: &str,
        now: u64,
    ) -> io::Result<HashMap<String, PathBuf>> {
        if remember == Remember::No {
            if forced {
                self.forget(name)?;
            }
            return sessions
                .iter()
                .map(|s| Ok((s.clone(), self.save_session(s, name, value)?)))
                .collect();
        }
        let path = self.save_team(name, value, remember, by, now)?;
        Ok(sessions.iter().map(|s| (s.clone(), path.clone())).collect())
    }

    /// Los recordados que valen, por nombre.
    pub fn list(&self, now: u64) -> Vec<Remembered> {
        let dir = self.team_dir();
        self.load()
            .into_iter()
            .filter(|r| r.expires.is_none_or(|e| e > now) && dir.join(&r.name).is_file())
            .collect()
    }

    /// Olvida un recordado. `false` si no estaba.
    pub fn forget(&self, name: &str) -> io::Result<bool> {
        check_name(name)?;
        let mut list = self.load();
        let before = list.len();
        list.retain(|r| r.name != name);
        let had = list.len() != before;
        if let Err(e) = std::fs::remove_file(self.team_dir().join(name))
            && e.kind() != io::ErrorKind::NotFound
        {
            return Err(e);
        }
        if had {
            self.store(&list)?;
        }
        Ok(had)
    }

    /// Los secretos sin recordar de una sesión, fuera (se cerró).
    pub fn forget_session(&self, session: &str) {
        if check_session(session).is_ok() {
            let _ = std::fs::remove_dir_all(self.sessions_dir().join(session));
        }
    }

    /// Fuera lo caducado: los recordados vencidos, las carpetas de sesiones que ya no existen
    /// (`live` dice cuáles sí) y los secretos sin recordar de más de 24 h.
    pub fn prune(&self, now: u64, live: impl Fn(&str) -> bool) {
        let (keep, gone): (Vec<_>, Vec<_>) = self
            .load()
            .into_iter()
            .partition(|r| r.expires.is_none_or(|e| e > now));
        for r in &gone {
            let _ = std::fs::remove_file(self.team_dir().join(&r.name));
        }
        if !gone.is_empty() {
            let _ = self.store(&keep);
        }
        let Ok(dirs) = std::fs::read_dir(self.sessions_dir()) else {
            return;
        };
        for d in dirs.flatten() {
            let id = d.file_name().to_string_lossy().into_owned();
            if !live(&id) {
                let _ = std::fs::remove_dir_all(d.path());
                continue;
            }
            for f in std::fs::read_dir(d.path()).into_iter().flatten().flatten() {
                let old =
                    mtime_ms(&f.path()).is_some_and(|t| now.saturating_sub(t) > SESSION_KEEP_MS);
                if old {
                    let _ = std::fs::remove_file(f.path());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_names() {
        for ok in [
            "github-token",
            "a",
            "API_KEY.prod",
            ".env",
            "x".repeat(64).as_str(),
        ] {
            assert!(valid_name(ok), "{ok}");
        }
        for bad in [
            "",
            ".",
            "..",
            "a/b",
            "a\\b",
            "con espacio",
            "ñandú",
            &"x".repeat(65),
        ] {
            assert!(!valid_name(bad), "{bad}");
        }
    }

    #[test]
    fn remember_parses_and_expires() {
        let r: Remember = serde_json::from_str("\"2h\"").unwrap();
        assert_eq!(r, Remember::TwoHours);
        assert_eq!(
            serde_json::to_string(&Remember::SevenDays).unwrap(),
            "\"7d\""
        );
        assert!(serde_json::from_str::<Remember>("\"1h\"").is_err());
        assert_eq!(
            Remember::TwoHours.expires(1_000),
            Some(1_000 + 2 * 3600 * 1000)
        );
        assert_eq!(Remember::SevenDays.expires(0), Some(7 * 24 * 3600 * 1000));
        assert_eq!(Remember::Always.expires(5), None);
    }

    fn store() -> (tempfile::TempDir, Store) {
        let tmp = tempfile::tempdir().unwrap();
        let s = Store::new(tmp.path().join("secrets"));
        (tmp, s)
    }

    #[test]
    fn session_secret_is_private_and_only_for_it() {
        let (_tmp, s) = store();
        let p = s.save_session("abc12345", "tok", b"s3cr3t").unwrap();
        assert_eq!(p, s.dir().join("sesiones/abc12345/tok"));
        assert_eq!(std::fs::read(&p).unwrap(), b"s3cr3t");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&p), 0o600);
            assert_eq!(mode(s.dir()), 0o700);
            assert_eq!(mode(&s.dir().join("sesiones")), 0o700);
            assert_eq!(mode(&s.dir().join("sesiones/abc12345")), 0o700);
        }
        // No es de todo el equipo.
        assert_eq!(s.remembered("tok", 0), None);
        assert!(s.save_session("../x", "tok", b"v").is_err());
        assert!(s.save_session("abc12345", "../x", b"v").is_err());
        s.forget_session("abc12345");
        assert!(!p.exists());
    }

    #[test]
    fn remembered_secrets_expire_and_keep_who_asked() {
        let (_tmp, s) = store();
        let p = s
            .save_team("gh", b"uno", Remember::TwoHours, "api", 1_000)
            .unwrap();
        assert_eq!(p, s.dir().join("equipo/gh"));
        assert_eq!(s.remembered("gh", 1_000), Some(p.clone()));
        let list = s.list(1_000);
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].by, "api");
        assert_eq!(list[0].expires, Some(1_000 + 2 * 3600 * 1000));
        // El índice no lleva el valor.
        let index = std::fs::read_to_string(s.dir().join("equipo.json")).unwrap();
        assert!(!index.contains("uno"), "{index}");
        // Otra sesión lo pide: se apunta.
        s.touch("gh", "web", 2_000).unwrap();
        assert_eq!(s.list(2_000)[0].by, "web");
        assert_eq!(s.list(2_000)[0].at, 2_000);
        // Caducado: ya no vale, y `prune` lo borra.
        let later = 1_000 + 2 * 3600 * 1000;
        assert_eq!(s.remembered("gh", later), None);
        assert!(s.list(later).is_empty());
        s.prune(later, |_| true);
        assert!(!p.exists());
        // Siempre: no caduca; sustituirlo cambia el valor y la caducidad.
        s.save_team("gh", b"dos", Remember::Always, "api", 3_000)
            .unwrap();
        s.save_team("gh", b"tres", Remember::SevenDays, "api", 4_000)
            .unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"tres");
        assert_eq!(s.list(4_000).len(), 1);
        assert!(s.forget("gh").unwrap());
        assert!(!s.forget("gh").unwrap());
        assert!(!p.exists() && s.list(4_000).is_empty());
    }

    #[test]
    fn prune_drops_old_and_orphan_session_secrets() {
        let (_tmp, s) = store();
        let viejo = s.save_session("aaaa1111", "a", b"1").unwrap();
        let nuevo = s.save_session("bbbb2222", "b", b"2").unwrap();
        let huerfano = s.save_session("cccc3333", "c", b"3").unwrap();
        let now = std::time::SystemTime::now();
        std::fs::File::options()
            .write(true)
            .open(&viejo)
            .unwrap()
            .set_modified(now - Duration::from_secs(25 * 3600))
            .unwrap();
        let now_ms = now
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        s.prune(now_ms, |id| id != "cccc3333");
        assert!(!viejo.exists());
        assert!(nuevo.exists());
        assert!(!huerfano.exists() && !huerfano.parent().unwrap().exists());
    }

    #[test]
    fn answer_goes_to_each_session_or_to_the_machine() {
        let (_tmp, s) = store();
        let sessions = vec!["aaaa1111".to_owned(), "bbbb2222".to_owned()];
        let paths = s
            .save_answer("tok", &sessions, b"v", Remember::No, false, "api, web", 1)
            .unwrap();
        assert_eq!(paths["aaaa1111"], s.dir().join("sesiones/aaaa1111/tok"));
        assert_eq!(paths["bbbb2222"], s.dir().join("sesiones/bbbb2222/tok"));
        let paths = s
            .save_answer(
                "tok",
                &sessions,
                b"v",
                Remember::Always,
                false,
                "api, web",
                1,
            )
            .unwrap();
        assert_eq!(paths["aaaa1111"], s.dir().join("equipo/tok"));
        assert_eq!(paths["bbbb2222"], s.dir().join("equipo/tok"));
        // Con `force` y sin recordar: el recordado ya no valía, fuera.
        s.save_answer("tok", &sessions[..1], b"w", Remember::No, true, "api", 2)
            .unwrap();
        assert_eq!(s.remembered("tok", 2), None);
    }
}
