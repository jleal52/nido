//! Para crear sesiones desde la web y la app sin escribir rutas a ciegas: las carpetas de este
//! equipo (solo carpetas, solo leer) y las órdenes que tiene sentido proponer (Claude y los
//! intérpretes de órdenes instalados).

use std::path::{Path, PathBuf};

use serde::Serialize;

#[derive(Debug, Serialize, PartialEq)]
pub struct Dir {
    pub name: String,
    /// Oculta (empieza por `.` o, en Windows, lleva la marca de oculta).
    pub hidden: bool,
}

#[derive(Debug, Serialize)]
pub struct Listing {
    /// La carpeta, entera y como la escribe este sistema (`C:\Users\ana`, `/home/ana`).
    pub path: String,
    /// La de arriba (ninguna en la raíz).
    pub parent: Option<String>,
    pub dirs: Vec<Dir>,
    /// Hay más de las que caben.
    pub truncated: bool,
}

const MAX: usize = 3000;

/// `~`, `~/x` (o `~\x`) y vacío, desde la carpeta de inicio.
pub fn expand(home: &Path, p: &str) -> PathBuf {
    let p = p.trim();
    if p.is_empty() || p == "~" {
        return home.to_owned();
    }
    if let Some(rest) = p.strip_prefix("~/").or_else(|| p.strip_prefix("~\\")) {
        return home.join(rest);
    }
    let mut path = PathBuf::from(p);
    // `C:` a secas es «la carpeta actual de C:»: aquí se quiere la raíz.
    if cfg!(windows) && p.len() == 2 && p.ends_with(':') {
        path.push("\\");
    }
    path
}

pub fn list(home: &Path, p: &str) -> Result<Listing, String> {
    let path = expand(home, p);
    if !path.is_absolute() {
        return Err(format!("«{p}» no es una ruta completa"));
    }
    let path = crate::sys::canonical(&path);
    let rd = std::fs::read_dir(&path).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => format!("no existe {}", path.display()),
        std::io::ErrorKind::PermissionDenied => format!("sin permiso para ver {}", path.display()),
        _ => format!("no se puede ver {}: {e}", path.display()),
    })?;
    let mut dirs = Vec::new();
    let mut truncated = false;
    for entry in rd.flatten() {
        // Los enlaces a carpetas cuentan como carpetas.
        let is_dir = entry.file_type().is_ok_and(|t| t.is_dir())
            || entry.path().metadata().is_ok_and(|m| m.is_dir());
        if !is_dir {
            continue;
        }
        if dirs.len() >= MAX {
            truncated = true;
            break;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        let hidden = name.starts_with('.') || hidden_attr(&entry);
        dirs.push(Dir { name, hidden });
    }
    dirs.sort_by_cached_key(|d| d.name.to_lowercase());
    Ok(Listing {
        parent: path.parent().map(|p| p.display().to_string()),
        path: path.display().to_string(),
        dirs,
        truncated,
    })
}

#[cfg(windows)]
fn hidden_attr(entry: &std::fs::DirEntry) -> bool {
    use std::os::windows::fs::MetadataExt;
    const HIDDEN: u32 = 0x2;
    const SYSTEM: u32 = 0x4;
    entry.metadata().is_ok_and(|m| m.file_attributes() & (HIDDEN | SYSTEM) != 0)
}

#[cfg(not(windows))]
fn hidden_attr(_: &std::fs::DirEntry) -> bool {
    false
}

/// Las unidades (`C:\`, `D:\`…) en Windows; en los demás, la raíz.
pub fn roots() -> Vec<String> {
    #[cfg(windows)]
    {
        // SAFETY: sin argumentos; devuelve una máscara de bits.
        let mask = unsafe { windows_sys::Win32::Storage::FileSystem::GetLogicalDrives() };
        (0..26u8)
            .filter(|i| mask & (1 << i) != 0)
            .map(|i| format!("{}:\\", (b'A' + i) as char))
            .collect()
    }
    #[cfg(not(windows))]
    {
        vec!["/".into()]
    }
}

/// Lo que se propone en el campo «Orden»: Claude y los intérpretes de órdenes de este equipo
/// (el del usuario primero).
pub fn commands() -> Vec<String> {
    let mut out = vec!["claude".to_owned()];
    #[cfg(windows)]
    {
        let sys = std::env::var_os("SystemRoot")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\Windows"))
            .join("System32");
        for (name, path) in [
            ("cmd.exe", Some(sys.join("cmd.exe"))),
            ("powershell.exe", Some(sys.join(r"WindowsPowerShell\v1.0\powershell.exe"))),
            ("pwsh.exe", in_path("pwsh.exe")),
            ("wsl.exe", Some(sys.join("wsl.exe"))),
        ] {
            if path.is_some_and(|p| p.is_file()) {
                out.push(name.into());
            }
        }
    }
    #[cfg(not(windows))]
    {
        let mine = std::env::var("SHELL").ok();
        let listed = std::fs::read_to_string("/etc/shells").unwrap_or_default();
        let shells = shells(mine.as_deref(), &listed, |p| Path::new(p).is_file());
        out.extend(shells);
    }
    out
}

/// De `/etc/shells`, los que existen, por su nombre y sin repetir (el del usuario primero). Fuera
/// los que no son para trabajar (`nologin`, `git-shell`, `tmux`…).
#[cfg_attr(windows, allow(dead_code))]
fn shells(mine: Option<&str>, etc_shells: &str, exists: impl Fn(&str) -> bool) -> Vec<String> {
    const SKIP: [&str; 7] = ["nologin", "false", "git-shell", "tmux", "screen", "rbash", "true"];
    let mut out: Vec<String> = Vec::new();
    let lines = mine.into_iter().chain(
        etc_shells
            .lines()
            .map(str::trim)
            .filter(|l| l.starts_with('/')),
    );
    for path in lines {
        let name = path.rsplit('/').next().unwrap_or(path);
        if name.is_empty() || SKIP.contains(&name) || out.iter().any(|o| o == name) || !exists(path) {
            continue;
        }
        out.push(name.to_owned());
    }
    out
}

#[cfg(windows)]
fn in_path(exe: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|d| d.join(exe))
        .find(|p| p.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shells_from_etc() {
        let etc = "# comentario\n/bin/sh\n/bin/bash\n/usr/bin/bash\n/usr/bin/zsh\n/usr/bin/tmux\n/usr/sbin/nologin\n/bin/fish\n";
        let got = shells(Some("/usr/bin/zsh"), etc, |p| p != "/bin/fish");
        assert_eq!(got, ["zsh", "sh", "bash"]);
    }

    #[cfg(unix)]
    #[test]
    fn lists_only_dirs_sorted() {
        let root = std::env::temp_dir().join(format!("nido-browse-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for d in ["b", "A", ".oculta"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        std::fs::write(root.join("fichero"), "x").unwrap();
        std::os::unix::fs::symlink(root.join("b"), root.join("enlace")).unwrap();
        // En macOS el temporal es un enlace (`/var` → `/private/var`): se compara con la ruta real.
        let root = std::fs::canonicalize(&root).unwrap();
        let l = list(Path::new("/nada"), root.to_str().unwrap()).unwrap();
        let names: Vec<_> = l.dirs.iter().map(|d| (d.name.as_str(), d.hidden)).collect();
        assert_eq!(names, [(".oculta", true), ("A", false), ("b", false), ("enlace", false)]);
        assert_eq!(l.parent.as_deref(), root.parent().and_then(|p| p.to_str()));
        assert!(list(&root, "~/b").unwrap().path.ends_with("/b"));
        assert!(list(&root, "relativa").is_err());
        assert!(list(&root, "~/no-existe").unwrap_err().starts_with("no existe"));
        let _ = std::fs::remove_dir_all(&root);
    }
}
