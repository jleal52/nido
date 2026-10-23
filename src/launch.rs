//! Arrancar y parar guardianes, y decidir con qué orden arranca cada sesión.

use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use crate::claude;
use crate::holder::HolderConfig;
use crate::registry::Session;

/// Dónde vive cada cosa. Con `NIDO_HOME` (pruebas), todo debajo de ese directorio.
#[derive(Debug, Clone)]
pub struct Paths {
    pub runtime: PathBuf,
    pub data: PathBuf,
    pub state: PathBuf,
    /// Configuración (`~/.config/nido`): el token, la clave de los avisos de Apple…
    pub config: PathBuf,
    pub home: PathBuf,
}

impl Paths {
    pub fn from_env() -> Self {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| "/".into());
        if let Some(root) = std::env::var_os("NIDO_HOME").map(PathBuf::from) {
            return Self {
                runtime: root.join("run"),
                data: root.join("data"),
                state: root.join("state"),
                config: root.join("config"),
                home,
            };
        }
        let xdg = |var: &str, default: &str| {
            std::env::var_os(var)
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(default))
                .join("nido")
        };
        let runtime = std::env::var_os("XDG_RUNTIME_DIR")
            .map(|d| PathBuf::from(d).join("nido"))
            .unwrap_or_else(|| {
                std::env::temp_dir().join(format!("nido-{}", unsafe { libc::getuid() }))
            });
        Self {
            runtime,
            data: xdg("XDG_DATA_HOME", ".local/share"),
            state: xdg("XDG_STATE_HOME", ".local/state"),
            config: xdg("XDG_CONFIG_HOME", ".config"),
            home,
        }
    }

    pub fn registry(&self) -> PathBuf {
        self.data.join("sessions.json")
    }

    pub fn socket(&self, id: &str) -> PathBuf {
        self.runtime.join(format!("{id}.sock"))
    }

    pub fn pidfile(&self, id: &str) -> PathBuf {
        self.runtime.join(format!("{id}.pid"))
    }

    pub fn session_state(&self, id: &str) -> PathBuf {
        self.state.join("sessions").join(id)
    }

    pub fn history(&self, id: &str) -> PathBuf {
        self.session_state(id).join("history.ans")
    }
}

/// Cómo se lanzan los guardianes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Launcher {
    /// Unidad transitoria de systemd por sesión: sobrevive al gestor y tiene límite de memoria.
    Systemd,
    /// Proceso suelto en su propia sesión (`setsid`): sin systemd, y en las pruebas.
    Detached,
}

impl Launcher {
    pub fn detect() -> Self {
        let ok = Command::new("systemctl")
            .args(["--user", "is-system-running"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success() || s.code() == Some(1))
            .unwrap_or(false);
        if ok && std::env::var_os("NIDO_HOME").is_none() {
            Launcher::Systemd
        } else {
            Launcher::Detached
        }
    }
}

pub fn unit_name(id: &str) -> String {
    format!("nido-s-{id}")
}

/// Límite de memoria por sesión: un proceso desbocado se mata solo a sí mismo.
const MEMORY_MAX: &str = "4G";

/// Variables del entorno del gestor que el guardián necesita (systemd da un PATH mínimo, sin
/// `~/.local/bin`, donde están `claude` y `cswap`).
fn passthrough_env() -> Vec<(String, String)> {
    [
        "PATH",
        "HOME",
        "USER",
        "SHELL",
        "LANG",
        "LC_ALL",
        "NIDO_HOME",
        "NIDO_EXE",
    ]
    .iter()
    .filter_map(|k| std::env::var(k).ok().map(|v| (k.to_string(), v)))
    .collect()
}

pub fn start(paths: &Paths, launcher: Launcher, id: &str) -> io::Result<()> {
    // NIDO_EXE: en las pruebas, el binario de nido (current_exe sería el del test).
    let exe = match std::env::var_os("NIDO_EXE") {
        Some(p) => PathBuf::from(p),
        None => std::env::current_exe()?,
    };
    match launcher {
        Launcher::Systemd => {
            let mut cmd = Command::new("systemd-run");
            cmd.args(["--user", "--quiet", "--collect"])
                .arg(format!("--unit={}", unit_name(id)))
                .arg(format!("--property=MemoryMax={MEMORY_MAX}"))
                // SIGTERM al guardián (que cuelga al proceso de la sesión); SIGKILL a lo que quede.
                .arg("--property=KillMode=mixed")
                .arg("--property=TimeoutStopSec=10");
            for (k, v) in passthrough_env() {
                cmd.arg(format!("--setenv={k}={v}"));
            }
            let out = cmd.arg(&exe).args(["holder", id]).output()?;
            if !out.status.success() {
                let err = String::from_utf8_lossy(&out.stderr);
                return Err(io::Error::other(format!("systemd-run: {}", err.trim())));
            }
        }
        Launcher::Detached => {
            let dir = paths.session_state(id);
            std::fs::create_dir_all(&dir)?;
            let log = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(dir.join("holder.log"))?;
            let mut cmd = Command::new(&exe);
            cmd.args(["holder", id])
                .stdin(Stdio::null())
                .stdout(log.try_clone()?)
                .stderr(log);
            // Su propia sesión: no le llegan las señales del terminal del gestor.
            unsafe {
                std::os::unix::process::CommandExt::pre_exec(&mut cmd, || {
                    libc::setsid();
                    Ok(())
                });
            }
            let mut child = cmd.spawn()?;
            // Recogerlo al terminar para no dejar un zombi mientras viva el gestor.
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
    }
    Ok(())
}

/// Hay un guardián atendiendo en el socket de la sesión.
pub fn is_alive(paths: &Paths, id: &str) -> bool {
    std::os::unix::net::UnixStream::connect(paths.socket(id)).is_ok()
}

pub fn holder_pid(paths: &Paths, id: &str) -> Option<i32> {
    std::fs::read_to_string(paths.pidfile(id))
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// El proceso del guardián sigue existiendo (aunque ya no atienda en el socket).
pub fn holder_running(paths: &Paths, id: &str) -> bool {
    holder_pid(paths, id).is_some_and(|pid| unsafe { libc::kill(pid, 0) } == 0)
}

/// Para la sesión: SIGTERM al guardián (cuelga al proceso; si no termina, SIGKILL a los 5 s).
pub fn stop(paths: &Paths, launcher: Launcher, id: &str) -> io::Result<()> {
    if launcher == Launcher::Systemd {
        let _ = Command::new("systemctl")
            .args(["--user", "stop", "--no-block", &unit_name(id)])
            .status();
    }
    if let Some(pid) = holder_pid(paths, id) {
        unsafe {
            libc::kill(pid, libc::SIGTERM);
        }
    }
    Ok(())
}

pub fn wait_until(mut cond: impl FnMut() -> bool, timeout: Duration) -> bool {
    let start = std::time::Instant::now();
    while start.elapsed() < timeout {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    cond()
}

/// ¿Existe ya la conversación? (si no, es un Claude nuevo y aún no se puede retomar).
pub fn transcript_exists(config_dir: &Path, session_id: &str) -> bool {
    let Ok(projects) = std::fs::read_dir(config_dir.join("projects")) else {
        return false;
    };
    let file = format!("{session_id}.jsonl");
    projects.flatten().any(|p| p.path().join(&file).is_file())
}

/// La orden pasa por la shell del usuario como login interactiva (`bash -lic 'exec "$@"' …`):
/// así tiene el mismo entorno que en una terminal normal (nvm, bun, go… en `.bashrc` y
/// `.profile`), y con `exec` la shell se convierte en el programa, sin quedarse en medio.
pub fn login_shell_wrap(argv: Vec<String>) -> Vec<String> {
    let shell = std::env::var("SHELL")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "/bin/bash".into());
    let mut out = vec![shell, "-lic".into(), "exec \"$@\"".into(), "nido".into()];
    out.extend(argv);
    out
}

/// La configuración con la que arranca el guardián de esta sesión: la orden tal cual, o, si
/// dentro hay un Claude con conversación, retomarla (directamente o escribiéndolo en la shell).
pub fn holder_config(paths: &Paths, session: &Session, cols: u16, rows: u16) -> HolderConfig {
    let mut argv = session.argv.clone();
    let mut env = session.env.clone();
    let mut initial_input = None;
    if let Some(link) = &session.claude {
        let config_dir = link
            .config_dir
            .clone()
            .unwrap_or_else(|| claude::default_config_dir(&paths.home));
        if transcript_exists(&config_dir, &link.session_id) {
            if link.inside_shell {
                initial_input = Some(format!("{}\r", claude::resume_line(link)).into_bytes());
            } else {
                let (resume, resume_env) = claude::resume_argv(link);
                argv = resume;
                env.extend(resume_env);
            }
        }
    }
    HolderConfig {
        id: session.id.clone(),
        cwd: session.cwd.clone(),
        argv: login_shell_wrap(argv),
        env,
        socket: paths.socket(&session.id),
        history_file: Some(paths.history(&session.id)),
        history_max: 8 * 1024 * 1024,
        cols,
        rows,
        initial_input,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{ClaudeLink, new_id, now};

    fn paths(root: &Path) -> Paths {
        Paths {
            runtime: root.join("run"),
            data: root.join("data"),
            state: root.join("state"),
            config: root.join("config"),
            home: root.join("home"),
        }
    }

    fn claude_session(inside_shell: bool) -> Session {
        Session {
            id: new_id(),
            name: "proyecto".into(),
            cwd: "/w".into(),
            argv: if inside_shell {
                vec!["bash".into()]
            } else {
                claude::new_argv("proyecto", "sid-1", None)
            },
            env: vec![],
            autostart: true,
            created: now(),
            folder: None,
            finished: 0,
            seen: 0,
            claude: Some(ClaudeLink {
                session_id: "sid-1".into(),
                config_dir: None,
                account: None,
                inside_shell,
            }),
        }
    }

    #[test]
    fn new_claude_runs_as_is_until_its_conversation_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let p = paths(tmp.path());
        let s = claude_session(false);
        let cfg = holder_config(&p, &s, 80, 24);
        assert!(
            cfg.argv.ends_with(&s.argv),
            "sin transcripción: se lanza nuevo: {:?}",
            cfg.argv
        );

        let proj = p.home.join(".claude/projects/-w");
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(proj.join("sid-1.jsonl"), "{}").unwrap();
        let cfg = holder_config(&p, &s, 80, 24);
        assert!(
            cfg.argv
                .ends_with(&["claude".to_string(), "--resume".into(), "sid-1".into()])
        );
        assert_eq!(cfg.argv[1..4], ["-lic", "exec \"$@\"", "nido"]);
        assert!(cfg.initial_input.is_none());
    }

    #[test]
    fn claude_inside_a_shell_is_typed_into_the_shell() {
        let tmp = tempfile::tempdir().unwrap();
        let p = paths(tmp.path());
        let proj = p.home.join(".claude/projects/-w");
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(proj.join("sid-1.jsonl"), "{}").unwrap();
        let s = claude_session(true);
        let cfg = holder_config(&p, &s, 80, 24);
        assert!(cfg.argv.ends_with(&["bash".to_string()]));
        assert_eq!(
            cfg.initial_input.as_deref(),
            Some(&b"claude --resume sid-1\r"[..])
        );
    }
}
