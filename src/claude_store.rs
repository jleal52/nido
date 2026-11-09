//! Dónde guarda Claude Code la cuenta con la que trabaja: la credencial (`~/.claude/.credentials.json`
//! o, en macOS, el llavero) y quién es (`oauthAccount` en `~/.claude.json`).
//!
//! Claude reescribe estos ficheros a menudo (renueva el token, guarda su configuración), así que
//! se cambian como él: con sus mismos cerrojos (directorios `*.lock`, como `proper-lockfile`),
//! leyendo justo antes y escribiendo de golpe (temporal y renombrar). De la credencial solo se toca
//! `claudeAiOauth`; lo demás (las sesiones de MCP…) se queda.

use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use serde_json::Value;

#[derive(Clone)]
pub struct Live {
    /// `~/.claude`.
    config_home: PathBuf,
    home: PathBuf,
}

/// Un cerrojo de Claude tomado (un directorio); se suelta al soltar el valor.
pub struct Lock(PathBuf);

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir(&self.0);
    }
}

/// Toma un cerrojo de Claude: crear el directorio es el cerrojo. Uno más viejo que `stale` es de
/// alguien que murió con él (los vivos lo tocan cada 5 s).
fn lock(path: &Path, stale: Duration) -> io::Result<Lock> {
    let deadline = Instant::now() + Duration::from_secs(9);
    loop {
        match std::fs::create_dir(path) {
            Ok(()) => return Ok(Lock(path.to_owned())),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                let age = std::fs::metadata(path)
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| SystemTime::now().duration_since(t).ok());
                if age.is_some_and(|a| a > stale) {
                    let _ = std::fs::remove_dir(path);
                    continue;
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                if let Some(dir) = path.parent() {
                    std::fs::create_dir_all(dir)?;
                    continue;
                }
                return Err(e);
            }
            Err(e) => return Err(e),
        }
        if Instant::now() > deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("Claude tiene ocupado {}", path.display()),
            ));
        }
        std::thread::sleep(Duration::from_millis(150));
    }
}

/// Escribe de golpe: un temporal al lado (solo para el usuario) que pasa a ser el fichero.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::io::Write;
    let tmp = path.with_extension(format!("nido-{}.tmp", std::process::id()));
    {
        let mut f = std::fs::File::create(&tmp)?;
        crate::sys::private_file(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

impl Live {
    pub fn new(home: &Path) -> Self {
        Self {
            config_home: crate::claude::default_config_dir(home),
            home: home.to_owned(),
        }
    }

    fn credentials_file(&self) -> PathBuf {
        self.config_home.join(".credentials.json")
    }

    /// `~/.claude.json` o, si Claude aún usa el antiguo, `~/.claude/.config.json`.
    fn global_config(&self) -> PathBuf {
        let legacy = self.config_home.join(".config.json");
        match std::fs::metadata(&legacy) {
            Ok(m) if m.is_file() && m.len() > 0 => legacy,
            _ => self.home.join(".claude.json"),
        }
    }

    /// Los dos cerrojos con los que Claude renueva el token: con ellos, una renovación suya no
    /// pisa el cambio (y la que venga después ve la cuenta nueva).
    pub fn credentials_lock(&self) -> io::Result<(Lock, Lock)> {
        let stale = Duration::from_secs(60);
        let primary = lock(&self.config_home.join(".oauth_refresh.lock"), stale)?;
        let mut legacy = self.config_home.clone().into_os_string();
        legacy.push(".lock");
        let legacy = lock(Path::new(&legacy), stale)?;
        Ok((primary, legacy))
    }

    /// Toda la credencial (`claudeAiOauth`, `mcpOAuth`…), o nada si no hay.
    pub fn read_credentials(&self) -> io::Result<Option<Value>> {
        #[cfg(target_os = "macos")]
        if let Some(raw) = keychain::read()? {
            return Ok(serde_json::from_str(&raw).ok());
        }
        match std::fs::read(self.credentials_file()) {
            Ok(b) => Ok(serde_json::from_slice(&b).ok()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub fn write_credentials(&self, v: &Value) -> io::Result<()> {
        let raw = serde_json::to_string(v)?;
        #[cfg(target_os = "macos")]
        {
            // En macOS manda el llavero; si quedaba el fichero de reserva, también al día.
            keychain::write(&raw)?;
            if !self.credentials_file().exists() {
                return Ok(());
            }
        }
        std::fs::create_dir_all(&self.config_home)?;
        write_atomic(&self.credentials_file(), raw.as_bytes())
    }

    /// Solo la parte de Claude (`claudeAiOauth`).
    pub fn read_oauth(&self) -> io::Result<Option<Value>> {
        Ok(self
            .read_credentials()?
            .and_then(|mut v| v.get_mut("claudeAiOauth").map(Value::take))
            .filter(Value::is_object))
    }

    /// Quién es la cuenta de ahora (`oauthAccount`).
    pub fn read_identity(&self) -> Option<Value> {
        let raw = std::fs::read(self.global_config()).ok()?;
        let mut v: Value = serde_json::from_slice(&raw).ok()?;
        v.get_mut("oauthAccount")
            .map(Value::take)
            .filter(Value::is_object)
    }

    /// Cambia `oauthAccount` en `~/.claude.json` y deja todo lo demás.
    pub fn write_identity(&self, account: &Value) -> io::Result<()> {
        let path = self.global_config();
        let mut lock_path = path.clone().into_os_string();
        lock_path.push(".lock");
        let _l = lock(Path::new(&lock_path), Duration::from_secs(10))?;
        let mut v: Value = match std::fs::read(&path) {
            Ok(b) => serde_json::from_slice(&b).map_err(|e| {
                io::Error::new(io::ErrorKind::InvalidData, format!("{}: {e}", path.display()))
            })?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => serde_json::json!({}),
            Err(e) => return Err(e),
        };
        if !v.is_object() {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "~/.claude.json no es un objeto"));
        }
        v["oauthAccount"] = account.clone();
        write_atomic(&path, serde_json::to_string_pretty(&v)?.as_bytes())
    }

    /// Pone esta credencial como la de Claude, dentro de sus cerrojos. Devuelve la que había
    /// (para guardarla: es la más reciente de esa cuenta, Claude la habrá renovado).
    pub fn swap_oauth(&self, oauth: &Value) -> io::Result<Option<Value>> {
        let _locks = self.credentials_lock()?;
        self.swap_oauth_locked(oauth)
    }

    /// Lo mismo, con los cerrojos ya tomados.
    pub fn swap_oauth_locked(&self, oauth: &Value) -> io::Result<Option<Value>> {
        let mut all = self.read_credentials()?.unwrap_or_else(|| serde_json::json!({}));
        if !all.is_object() {
            all = serde_json::json!({});
        }
        let old = all
            .get_mut("claudeAiOauth")
            .map(Value::take)
            .filter(Value::is_object);
        all["claudeAiOauth"] = oauth.clone();
        self.write_credentials(&all)?;
        Ok(old)
    }
}

#[cfg(target_os = "macos")]
mod keychain {
    //! El llavero de macOS con `security`: el secreto entra por la entrada estándar (no se ve en
    //! la lista de procesos) salvo que no quepa en su línea de 4 KiB.
    use std::io::{self, Write};
    use std::process::{Command, Stdio};

    const SERVICE: &str = "Claude Code-credentials";
    const SECURITY: &str = "/usr/bin/security";

    fn user() -> String {
        std::env::var("USER").unwrap_or_default()
    }

    pub fn read() -> io::Result<Option<String>> {
        let out = Command::new(SECURITY)
            .args(["find-generic-password", "-a", &user(), "-s", SERVICE, "-w"])
            .stdin(Stdio::null())
            .output()?;
        match out.status.code() {
            Some(0) => {
                let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
                if s.ends_with('\n') {
                    s.pop();
                }
                Ok(Some(s))
            }
            // 44: no está.
            Some(44) => Ok(None),
            _ => Err(io::Error::other(format!(
                "el llavero no deja leer la cuenta de Claude: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ))),
        }
    }

    fn quote(s: &str) -> String {
        format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
    }

    pub fn write(value: &str) -> io::Result<()> {
        let hex: String = value.bytes().map(|b| format!("{b:02x}")).collect();
        let line = format!(
            "add-generic-password -U -a {} -s {} -X {hex}\n",
            quote(&user()),
            quote(SERVICE)
        );
        let out = if line.len() <= 4000 {
            let mut child = Command::new(SECURITY)
                .arg("-i")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()?;
            child
                .stdin
                .take()
                .expect("stdin")
                .write_all(line.as_bytes())?;
            child.wait_with_output()?
        } else {
            Command::new(SECURITY)
                .args(["add-generic-password", "-U", "-a", &user(), "-s", SERVICE, "-X", &hex])
                .stdin(Stdio::null())
                .output()?
        };
        // `security -i` acaba bien aunque falle la orden: lo dice por stderr.
        let err = String::from_utf8_lossy(&out.stderr);
        if !out.status.success() || !err.trim().is_empty() {
            return Err(io::Error::other(format!(
                "el llavero no deja guardar la cuenta de Claude: {}",
                err.trim()
            )));
        }
        Ok(())
    }
}

#[cfg(all(test, not(target_os = "macos")))]
mod tests {
    use super::*;

    #[test]
    fn swap_keeps_the_rest_and_returns_the_old_one() {
        let dir = std::env::temp_dir().join(format!("nido-live-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let live = Live::new(&dir);
        assert!(live.read_oauth().unwrap().is_none());
        std::fs::create_dir_all(dir.join(".claude")).unwrap();
        std::fs::write(
            dir.join(".claude/.credentials.json"),
            r#"{"claudeAiOauth":{"accessToken":"viejo"},"mcpOAuth":{"x":1}}"#,
        )
        .unwrap();
        std::fs::write(
            dir.join(".claude.json"),
            r#"{"theme":"dark","oauthAccount":{"accountUuid":"u1"}}"#,
        )
        .unwrap();
        let old = live
            .swap_oauth(&serde_json::json!({"accessToken": "nuevo"}))
            .unwrap();
        assert_eq!(old.unwrap()["accessToken"], "viejo");
        let all = live.read_credentials().unwrap().unwrap();
        assert_eq!(all["claudeAiOauth"]["accessToken"], "nuevo");
        assert_eq!(all["mcpOAuth"]["x"], 1);
        assert_eq!(live.read_identity().unwrap()["accountUuid"], "u1");
        live.write_identity(&serde_json::json!({"accountUuid": "u2"})).unwrap();
        let cfg: Value = serde_json::from_slice(&std::fs::read(dir.join(".claude.json")).unwrap()).unwrap();
        assert_eq!(cfg["theme"], "dark");
        assert_eq!(cfg["oauthAccount"]["accountUuid"], "u2");
        // Sin cerrojos ni temporales olvidados.
        let left: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .chain(std::fs::read_dir(dir.join(".claude")).unwrap())
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains("lock") || n.contains("tmp"))
            .collect();
        assert!(left.is_empty(), "{left:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
