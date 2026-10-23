//! Claude Code dentro de una sesión: encontrarlo, saber qué conversación tiene abierta y en
//! qué estado está, y las órdenes para lanzarlo o retomarlo.
//!
//! Claude Code escribe `<config>/sessions/<pid>.json` (`sessionId`, `cwd`, `name`, `status`…).
//! Es un formato interno: solo se leen esos campos, y si cambia se sigue con lo último conocido.
//! Los `.key` de ese directorio no se tocan.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::registry::ClaudeLink;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaudeInfo {
    pub session_id: String,
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    #[serde(default)]
    pub name: Option<String>,
    /// `busy`, `idle`… (lo que ponga Claude Code).
    #[serde(default)]
    pub status: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FoundClaude {
    pub pid: u32,
    pub info: ClaudeInfo,
    /// El `CLAUDE_CONFIG_DIR` con el que corre; `None` si es el de siempre (`~/.claude`).
    pub config_dir: Option<PathBuf>,
}

/// Acceso a `/proc` (en las pruebas, uno falso).
pub struct Procfs {
    root: PathBuf,
}

impl Default for Procfs {
    fn default() -> Self {
        Self {
            root: "/proc".into(),
        }
    }
}

impl Procfs {
    pub fn at(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// En macOS no hay `/proc`: se pregunta al núcleo (`sysctl`).
    #[cfg(target_os = "macos")]
    fn native(&self) -> bool {
        self.root == Path::new("/proc")
    }

    /// `pid → ppid` de todos los procesos.
    fn parents(&self) -> HashMap<u32, u32> {
        #[cfg(target_os = "macos")]
        if self.native() {
            return mac::parents();
        }
        let mut map = HashMap::new();
        let Ok(dir) = std::fs::read_dir(&self.root) else {
            return map;
        };
        for entry in dir.flatten() {
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|n| n.parse::<u32>().ok())
            else {
                continue;
            };
            let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
                continue;
            };
            // "pid (comm) estado ppid …": el comm puede llevar espacios y paréntesis.
            let Some(after) = stat.rfind(')').map(|i| &stat[i + 1..]) else {
                continue;
            };
            if let Some(ppid) = after.split_whitespace().nth(1).and_then(|p| p.parse().ok()) {
                map.insert(pid, ppid);
            }
        }
        map
    }

    /// El proceso y todos sus descendientes, del más cercano al más lejano.
    pub fn tree(&self, root: u32) -> Vec<u32> {
        let parents = self.parents();
        let mut out = vec![root];
        let mut i = 0;
        while i < out.len() {
            let p = out[i];
            let mut kids: Vec<u32> = parents
                .iter()
                .filter(|&(_, &pp)| pp == p)
                .map(|(&c, _)| c)
                .collect();
            kids.sort_unstable();
            out.extend(kids);
            i += 1;
        }
        out
    }

    pub fn comm(&self, pid: u32) -> Option<String> {
        #[cfg(target_os = "macos")]
        if self.native() {
            return mac::comm(pid);
        }
        let s = std::fs::read_to_string(self.root.join(pid.to_string()).join("comm")).ok()?;
        Some(s.trim_end().to_owned())
    }

    pub fn env_var(&self, pid: u32, key: &str) -> Option<String> {
        #[cfg(target_os = "macos")]
        if self.native() {
            return mac::env_var(pid, key);
        }
        let raw = std::fs::read(self.root.join(pid.to_string()).join("environ")).ok()?;
        raw.split(|&b| b == 0).find_map(|kv| {
            let kv = std::str::from_utf8(kv).ok()?;
            kv.strip_prefix(key)?.strip_prefix('=').map(str::to_owned)
        })
    }
}

/// Lo mismo que `/proc` en macOS, con `sysctl`: la lista de procesos (`KERN_PROC_ALL`) y los
/// argumentos y el entorno de uno (`KERN_PROCARGS2`, solo de los procesos del mismo usuario).
#[cfg(target_os = "macos")]
mod mac {
    use std::collections::HashMap;

    /// Todos los pids, y de cada uno su padre y su nombre (libproc).
    fn all() -> Vec<(u32, u32, String)> {
        // SAFETY: con buffer nulo, proc_listallpids devuelve cuántos hay.
        let n = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
        if n <= 0 {
            return Vec::new();
        }
        let mut pids = vec![0 as libc::pid_t; n as usize + 64];
        let bytes = (pids.len() * std::mem::size_of::<libc::pid_t>()) as libc::c_int;
        // SAFETY: el buffer tiene sitio para `pids.len()` pids.
        let n = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), bytes) };
        if n <= 0 {
            return Vec::new();
        }
        pids.truncate(n as usize);
        pids.into_iter()
            .filter_map(|pid| info(pid as u32))
            .collect()
    }

    fn info(pid: u32) -> Option<(u32, u32, String)> {
        // SAFETY: proc_bsdinfo es POD; proc_pidinfo escribe como mucho su tamaño.
        let mut bsd: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
        let r = unsafe {
            libc::proc_pidinfo(
                pid as libc::c_int,
                libc::PROC_PIDTBSDINFO,
                0,
                (&mut bsd as *mut libc::proc_bsdinfo).cast(),
                size,
            )
        };
        if r != size {
            return None;
        }
        let name: Vec<u8> = bsd
            .pbi_comm
            .iter()
            .take_while(|&&c| c != 0)
            .map(|&c| c as u8)
            .collect();
        Some((pid, bsd.pbi_ppid, String::from_utf8(name).ok()?))
    }

    pub fn parents() -> HashMap<u32, u32> {
        all().into_iter().map(|(p, pp, _)| (p, pp)).collect()
    }

    pub fn comm(pid: u32) -> Option<String> {
        info(pid).map(|(_, _, n)| n)
    }

    /// `argc`, la ruta del ejecutable, los argumentos y el entorno, separados por ceros.
    fn procargs(pid: u32) -> Option<Vec<u8>> {
        let mut mib = [libc::CTL_KERN, libc::KERN_ARGMAX];
        let mut argmax: libc::c_int = 0;
        let mut size = std::mem::size_of::<libc::c_int>();
        // SAFETY: lee un entero.
        if unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                2,
                (&mut argmax as *mut libc::c_int).cast(),
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        } != 0
        {
            return None;
        }
        let mut buf = vec![0u8; argmax.max(4096) as usize];
        let mut size = buf.len();
        let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid as libc::c_int];
        // SAFETY: el buffer tiene `size` bytes.
        if unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                3,
                buf.as_mut_ptr().cast(),
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        } != 0
        {
            return None;
        }
        buf.truncate(size);
        Some(buf)
    }

    pub fn env_var(pid: u32, key: &str) -> Option<String> {
        let raw = procargs(pid)?;
        let argc = i32::from_ne_bytes(raw.get(..4)?.try_into().ok()?) as usize;
        // Tras argc: la ruta, ceros de relleno, los argc argumentos y luego el entorno.
        let mut parts = raw[4..].split(|&b| b == 0).filter(|p| !p.is_empty());
        parts.next()?; // la ruta del ejecutable
        for _ in 0..argc {
            parts.next()?;
        }
        parts.find_map(|kv| {
            let kv = std::str::from_utf8(kv).ok()?;
            kv.strip_prefix(key)?.strip_prefix('=').map(str::to_owned)
        })
    }
}

pub fn default_config_dir(home: &Path) -> PathBuf {
    home.join(".claude")
}

pub fn live_info(config_dir: &Path, pid: u32) -> Option<ClaudeInfo> {
    let raw = std::fs::read(config_dir.join("sessions").join(format!("{pid}.json"))).ok()?;
    serde_json::from_slice(&raw).ok()
}

/// El primer Claude en el árbol del proceso de la sesión (él mismo o un descendiente).
pub fn find_claude(procfs: &Procfs, root_pid: u32, home: &Path) -> Option<FoundClaude> {
    procfs.tree(root_pid).into_iter().find_map(|pid| {
        if procfs.comm(pid).as_deref() != Some("claude") {
            return None;
        }
        let custom = procfs
            .env_var(pid, "CLAUDE_CONFIG_DIR")
            .filter(|v| !v.is_empty());
        let dir = custom
            .as_ref()
            .map_or_else(|| default_config_dir(home), PathBuf::from);
        let info = live_info(&dir, pid)?;
        let config_dir = custom
            .map(PathBuf::from)
            .filter(|d| *d != default_config_dir(home));
        Some(FoundClaude {
            pid,
            info,
            config_dir,
        })
    })
}

fn with_account(account: Option<&str>, claude_args: Vec<String>) -> Vec<String> {
    let mut argv = Vec::new();
    if let Some(acct) = account {
        argv.extend(["cswap".into(), "run".into(), acct.to_owned(), "--".into()]);
    }
    argv.push("claude".into());
    argv.extend(claude_args);
    argv
}

/// Un Claude nuevo con la conversación ya identificada (`--session-id`) y con nombre.
pub fn new_argv(name: &str, session_id: &str, account: Option<&str>) -> Vec<String> {
    let args = vec![
        "--session-id".into(),
        session_id.into(),
        "-n".into(),
        name.into(),
    ];
    with_account(account, args)
}

/// Retomar la conversación con la misma cuenta. Devuelve la orden y las variables de entorno.
pub fn resume_argv(link: &ClaudeLink) -> (Vec<String>, Vec<(String, String)>) {
    let argv = with_account(
        link.account.as_deref(),
        vec!["--resume".into(), link.session_id.clone()],
    );
    let env = match (&link.account, &link.config_dir) {
        // Con cuenta de cswap, el perfil lo pone cswap.
        (None, Some(dir)) => vec![(
            "CLAUDE_CONFIG_DIR".into(),
            dir.to_string_lossy().into_owned(),
        )],
        _ => vec![],
    };
    (argv, env)
}

/// Comillas simples de shell (para escribir la orden dentro de una shell al restaurar).
fn sh_quote(s: &str) -> String {
    if !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_./=:@".contains(&b))
    {
        s.to_owned()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

/// La línea que se escribe en la shell para retomar el Claude que corría dentro.
pub fn resume_line(link: &ClaudeLink) -> String {
    let (argv, env) = resume_argv(link);
    let mut parts: Vec<String> = env
        .iter()
        .map(|(k, v)| format!("{k}={}", sh_quote(v)))
        .collect();
    parts.extend(argv.iter().map(|a| sh_quote(a)));
    parts.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_proc(root: &Path, pid: u32, ppid: u32, comm: &str, env: &[&str]) {
        let d = root.join(pid.to_string());
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(
            d.join("stat"),
            format!("{pid} ({comm}) S {ppid} {pid} {pid} 0"),
        )
        .unwrap();
        std::fs::write(d.join("comm"), format!("{comm}\n")).unwrap();
        let mut environ = env.join("\0");
        environ.push('\0');
        std::fs::write(d.join("environ"), environ).unwrap();
    }

    #[test]
    fn finds_claude_inside_a_shell_with_its_profile() {
        let tmp = tempfile::tempdir().unwrap();
        let proc_root = tmp.path().join("proc");
        let home = tmp.path().join("home");
        let profile = tmp.path().join("perfil-2");
        fake_proc(&proc_root, 100, 1, "bash", &["HOME=/h"]);
        fake_proc(&proc_root, 150, 100, "sleep", &[]);
        fake_proc(
            &proc_root,
            200,
            100,
            "claude",
            &[&format!("CLAUDE_CONFIG_DIR={}", profile.display())],
        );
        // Un comm raro, con espacios y paréntesis, no rompe la lectura de los demás.
        fake_proc(&proc_root, 300, 1, "a) (b", &[]);
        std::fs::create_dir_all(profile.join("sessions")).unwrap();
        std::fs::write(
            profile.join("sessions/200.json"),
            r#"{"pid":200,"sessionId":"abc-123","cwd":"/w","name":"proyecto","status":"busy","extra":1}"#,
        )
        .unwrap();

        let procfs = Procfs::at(&proc_root);
        assert_eq!(procfs.tree(100), [100, 150, 200]);
        let found = find_claude(&procfs, 100, &home).expect("debería encontrar el claude");
        assert_eq!(found.pid, 200);
        assert_eq!(found.info.session_id, "abc-123");
        assert_eq!(found.info.status.as_deref(), Some("busy"));
        assert_eq!(found.config_dir, Some(profile));
        assert!(find_claude(&procfs, 150, &home).is_none());
    }

    #[test]
    fn launch_and_resume_commands() {
        assert_eq!(
            new_argv("proyecto", "u-1", None),
            ["claude", "--session-id", "u-1", "-n", "proyecto"]
        );
        assert_eq!(
            new_argv("proyecto", "u-1", Some("2")),
            [
                "cswap",
                "run",
                "2",
                "--",
                "claude",
                "--session-id",
                "u-1",
                "-n",
                "proyecto"
            ]
        );
        let link = ClaudeLink {
            session_id: "u-1".into(),
            config_dir: Some("/perfil con espacio".into()),
            account: None,
            inside_shell: true,
        };
        let (argv, env) = resume_argv(&link);
        assert_eq!(argv, ["claude", "--resume", "u-1"]);
        assert_eq!(
            env,
            [(
                "CLAUDE_CONFIG_DIR".to_string(),
                "/perfil con espacio".to_string()
            )]
        );
        assert_eq!(
            resume_line(&link),
            "CLAUDE_CONFIG_DIR='/perfil con espacio' claude --resume u-1"
        );
        let with_acct = ClaudeLink {
            account: Some("a@b.c".into()),
            ..link
        };
        assert_eq!(resume_argv(&with_acct).1, []);
        assert_eq!(
            resume_line(&with_acct),
            "cswap run a@b.c -- claude --resume u-1"
        );
    }
}
