//! Importar los Claude que hoy corren en tmux: su conversación, carpeta, nombre y perfil.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::claude::{Procfs, default_config_dir};
use crate::registry::valid_name;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub name: String,
    pub cwd: PathBuf,
    pub session_id: String,
    pub config_dir: Option<PathBuf>,
    /// `sesión:@ventana.%panel` de tmux.
    pub tmux: String,
    pub pid: u32,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Raw {
    session_id: String,
    cwd: Option<PathBuf>,
    name: Option<String>,
    tmux: Option<String>,
}

/// Un nombre válido para nido a partir del que tenga en Claude o en tmux.
pub fn sanitize(name: &str) -> String {
    let mut out = String::new();
    for c in name.trim().chars() {
        let ok = c.is_alphanumeric() || matches!(c, '-' | '_' | '.');
        if ok {
            out.push(c);
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let out: String = out.trim_matches(['-', '.']).chars().take(64).collect();
    if valid_name(&out) {
        out
    } else {
        "claude".into()
    }
}

/// El nombre con el que se retomó (`claude --resume proyecto-api`), si no es un uuid.
fn resume_arg(procfs_root: &Path, pid: u32) -> Option<String> {
    let raw = std::fs::read(procfs_root.join(pid.to_string()).join("cmdline")).ok()?;
    let args: Vec<String> = raw
        .split(|&b| b == 0)
        .map(|a| String::from_utf8_lossy(a).into_owned())
        .collect();
    let i = args.iter().position(|a| a == "--resume" || a == "-r")?;
    let v = args.get(i + 1)?;
    let is_uuid = v.len() == 36 && v.chars().filter(|&c| c == '-').count() == 4;
    (!is_uuid && !v.starts_with('-')).then(|| v.clone())
}

/// Los Claude vivos que corren en tmux, de cualquier perfil (los de cswap incluidos).
pub fn scan(procfs_root: &Path, home: &Path) -> Vec<Found> {
    let procfs = Procfs::at(procfs_root);
    let mut out = Vec::new();
    let Ok(dir) = std::fs::read_dir(procfs_root) else {
        return out;
    };
    let mut pids: Vec<u32> = dir
        .flatten()
        .filter_map(|e| e.file_name().to_str()?.parse().ok())
        .collect();
    pids.sort_unstable();
    for pid in pids {
        if procfs.comm(pid).as_deref() != Some("claude") {
            continue;
        }
        let custom = procfs
            .env_var(pid, "CLAUDE_CONFIG_DIR")
            .filter(|v| !v.is_empty());
        let dir = custom
            .as_ref()
            .map_or_else(|| default_config_dir(home), PathBuf::from);
        let Ok(bytes) = std::fs::read(dir.join("sessions").join(format!("{pid}.json"))) else {
            continue;
        };
        let Ok(raw) = serde_json::from_slice::<Raw>(&bytes) else {
            continue;
        };
        let Some(tmux) = raw.tmux else { continue };
        let name = resume_arg(procfs_root, pid)
            .or(raw.name)
            .unwrap_or_else(|| "claude".into());
        out.push(Found {
            name: sanitize(&name),
            cwd: raw.cwd.unwrap_or_else(|| home.to_owned()),
            session_id: raw.session_id,
            config_dir: custom
                .map(PathBuf::from)
                .filter(|d| *d != default_config_dir(home)),
            tmux,
            pid,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizes_names() {
        assert_eq!(sanitize("proyecto-api"), "proyecto-api");
        assert_eq!(sanitize("Gestor tmux en Rust"), "Gestor-tmux-en-Rust");
        assert_eq!(sanitize("  ¿Qué tal?  "), "Qué-tal");
        assert_eq!(sanitize("..."), "claude");
    }

    #[test]
    fn finds_claude_in_tmux_with_its_resume_name() {
        let tmp = tempfile::tempdir().unwrap();
        let proc_root = tmp.path().join("proc");
        let home = tmp.path().join("home");
        let mk = |pid: u32, comm: &str, cmdline: &[&str], env: &str| {
            let d = proc_root.join(pid.to_string());
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("stat"), format!("{pid} ({comm}) S 1 0")).unwrap();
            std::fs::write(d.join("comm"), format!("{comm}\n")).unwrap();
            std::fs::write(d.join("cmdline"), cmdline.join("\0")).unwrap();
            std::fs::write(d.join("environ"), env).unwrap();
        };
        mk(10, "claude", &["claude", "--resume", "proyecto-api"], "");
        mk(11, "claude", &["claude"], "");
        mk(12, "bash", &["bash"], "");
        let sessions = home.join(".claude/sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        std::fs::write(
            sessions.join("10.json"),
            r#"{"sessionId":"s-10","cwd":"/w/sip","name":"proyecto-api","tmux":"trabajo:@1.%1"}"#,
        )
        .unwrap();
        // Sin campo tmux (no corre en tmux): no se importa.
        std::fs::write(
            sessions.join("11.json"),
            r#"{"sessionId":"s-11","cwd":"/w"}"#,
        )
        .unwrap();

        let found = scan(&proc_root, &home);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "proyecto-api");
        assert_eq!(found[0].session_id, "s-10");
        assert_eq!(found[0].cwd, PathBuf::from("/w/sip"));
        assert_eq!(found[0].config_dir, None);
    }
}
