//! Una sesión cuyo Claude pasa su conversación a un trabajo en segundo plano de Claude Code (con
//! una orden de barra), de punta a punta y sin Claude de verdad: un `claude` falso que se queda en
//! marcha y apunta su `sessions/<pid>.json`, y un proceso dormido que hace del trabajo, con el
//! suyo. Ids y rutas inventados.

// Con sh, señales y `/proc`: solo en Linux.
#![cfg(target_os = "linux")]

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use nido::client::Api;
use nido::launch::{Launcher, Paths};
use nido::registry::{ClaudeLink, Session};
use nido::server::{self, AppState};
use serde_json::{Value, json};

const TOKEN: &str = "0123456789abcdef0123456789abcdef";
const JOB: &str = "ab12cd34";

async fn call(
    addr: SocketAddr,
    method: &str,
    path: &str,
    caller: Option<&str>,
    body: Option<Value>,
) -> (u16, Value) {
    let caller = caller.map(str::to_owned);
    let (method, path) = (method.to_owned(), path.to_owned());
    tokio::task::spawn_blocking(move || {
        let api = Api {
            addr,
            token: Some(TOKEN.into()),
        };
        let headers: Vec<(&str, &str)> = caller
            .as_deref()
            .map(|c| vec![("X-Nido-Session", c)])
            .unwrap_or_default();
        api.request_with(
            &method,
            &path,
            body.as_ref(),
            &headers,
            Some(Duration::from_secs(60)),
        )
        .unwrap()
    })
    .await
    .unwrap()
}

async fn tool(addr: SocketAddr, caller: &str, name: &str, args: Value) -> (u16, Value) {
    call(
        addr,
        "POST",
        &format!("/api/v1/mcp/{name}"),
        Some(caller),
        Some(args),
    )
    .await
}

/// Al acabar (también si la prueba falla), mata los guardianes y los «trabajos» que queden.
struct Cleanup(PathBuf, Vec<u32>);

impl Drop for Cleanup {
    fn drop(&mut self) {
        let mut pids = self.1.clone();
        if let Ok(dir) = std::fs::read_dir(&self.0) {
            for e in dir.flatten() {
                if e.path().extension().is_some_and(|x| x == "pid")
                    && let Some(pid) = std::fs::read_to_string(e.path())
                        .ok()
                        .and_then(|p| p.trim().parse::<u32>().ok())
                {
                    pids.push(pid);
                }
            }
        }
        for pid in pids {
            unsafe {
                libc::kill(pid as i32, libc::SIGKILL);
            }
        }
    }
}

fn executable(path: &Path, text: &str) {
    std::fs::write(path, text).unwrap();
    std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
}

/// Un proceso dormido que hace del trabajo en segundo plano, con su `sessions/<pid>.json`. Se
/// recoge en cuanto muere (si no, quedaría zombi y seguiría «vivo» en `/proc`).
fn spawn_job(config: &Path, session_id: &str) -> u32 {
    let mut child = std::process::Command::new("sleep")
        .arg("120")
        .spawn()
        .unwrap();
    let pid = child.id();
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    std::fs::write(
        config.join(format!("sessions/{pid}.json")),
        json!({
            "pid": pid, "sessionId": session_id, "cwd": "/w", "kind": "bg",
            "name": "prueba", "jobId": JOB, "status": "busy"
        })
        .to_string(),
    )
    .unwrap();
    pid
}

fn alive(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|s| {
        !s.rsplit(')')
            .next()
            .unwrap_or("")
            .trim_start()
            .starts_with('Z')
    })
}

async fn wait_for(mut cond: impl FnMut() -> bool) -> bool {
    for _ in 0..200 {
        if cond() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

fn session(
    name: &str,
    cwd: &Path,
    argv: &[&str],
    env: Vec<(String, String)>,
    claude: Option<ClaudeLink>,
) -> Session {
    Session {
        id: nido::registry::new_id(),
        name: name.into(),
        cwd: cwd.to_owned(),
        argv: argv.iter().map(|a| a.to_string()).collect(),
        env,
        autostart: false,
        created: 0,
        claude,
        folder: None,
        finished: 0,
        stopped: 0,
        seen: 0,
        opened_by: None,
        alerts: vec![],
    }
}

/// El `sessions/<pid>.json` del Claude falso de la sesión: con `parked`, aparcado en `JOB`.
fn viewer_file(config: &Path, pid: u32, parked: bool) {
    let mut v = json!({
        "pid": pid, "sessionId": "conv-vieja", "cwd": "/w", "kind": "interactive",
        "name": "prueba", "status": "idle"
    });
    if parked {
        v["parkedJobId"] = JOB.into();
    }
    std::fs::write(config.join(format!("sessions/{pid}.json")), v.to_string()).unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_session_parked_in_the_background() {
    let _limit = nido::watchdog::limit(Duration::from_secs(240));
    let tmp = tempfile::tempdir().unwrap();
    let mut cleanup = Cleanup(tmp.path().join("nido/run"), vec![]);
    let fake = tmp.path().join("fake");
    let config = tmp.path().join("claude");
    let bin = tmp.path().join("bin");
    for d in [&fake, &config.join("sessions"), &bin] {
        std::fs::create_dir_all(d).unwrap();
    }
    // La shell de login: `-lic 'exec "$@"' nido <orden>` → la orden.
    let shell = bin.join("shell");
    executable(&shell, "#!/bin/sh\nshift 3\nexec \"$@\"\n");
    // El `claude` falso: `--version` (lo pregunta nido para el channel), `stop <id>` (lo apunta
    // con su perfil y para el trabajo) y, si no, se queda en marcha como el Claude de la sesión,
    // apuntando su pid y su orden.
    executable(
        &bin.join("claude"),
        "#!/bin/sh\n\
         case \"$1\" in\n\
         --version) echo '2.1.329 (Claude Code)'; exit 0;;\n\
         stop) echo \"stop $2 ${CLAUDE_CONFIG_DIR:-}\" >> \"$FAKE_DIR/stops\";\n\
               for f in \"$FAKE_DIR\"/job-*.pid; do [ -f \"$f\" ] && kill \"$(cat \"$f\")\"; done; exit 0;;\n\
         esac\n\
         echo \"$*\" >> \"$FAKE_DIR/runs\"\n\
         echo $$ > \"$FAKE_DIR/viewer.pid\"\n\
         while :; do sleep 1; done\n",
    );
    // Esta prueba es su propio proceso (un fichero de tests/): cambiar el entorno no toca a otras.
    unsafe {
        let path = std::env::var_os("PATH").unwrap_or_default();
        let mut dirs = vec![bin.clone()];
        dirs.extend(std::env::split_paths(&path));
        std::env::set_var("PATH", std::env::join_paths(dirs).unwrap());
        std::env::set_var("NIDO_HOME", tmp.path().join("nido"));
        std::env::set_var("HOME", tmp.path().join("home"));
        std::env::set_var("NIDO_EXE", env!("CARGO_BIN_EXE_nido"));
        std::env::set_var("NIDO_NAME", "Prueba");
        std::env::set_var("SHELL", &shell);
        std::env::set_var("FAKE_DIR", &fake);
    }
    let paths = Paths::from_env();
    let st = AppState::new(paths.clone(), Launcher::Detached, Some(TOKEN.into()), true).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    {
        let st = st.clone();
        tokio::spawn(async move {
            let app = server::router(st).into_make_service_with_connect_info::<SocketAddr>();
            axum::serve(listener, app).await.unwrap()
        });
    }
    let env = vec![(
        "CLAUDE_CONFIG_DIR".to_owned(),
        config.to_string_lossy().into_owned(),
    )];
    let link = ClaudeLink {
        session_id: "conv-vieja".into(),
        config_dir: Some(config.clone()),
        account: None,
        inside_shell: false,
    };
    let (id, caller) = {
        let mut reg = st.registry.lock().await;
        let id = reg
            .add(session("prueba", tmp.path(), &["claude"], env, Some(link)))
            .unwrap()
            .id;
        let caller = reg
            .add(session("otra", tmp.path(), &["true"], vec![], None))
            .unwrap()
            .id;
        (id, caller)
    };
    let viewer_pid = || {
        std::fs::read_to_string(fake.join("viewer.pid"))
            .ok()
            .and_then(|p| p.trim().parse::<u32>().ok())
    };
    nido::launch::start(&paths, Launcher::Detached, &id).unwrap();
    assert!(wait_for(|| viewer_pid().is_some()).await, "no arrancó");
    let viewer = viewer_pid().unwrap();
    viewer_file(&config, viewer, false);
    let link_now = |st: &AppState| {
        let st = st.registry.try_lock().unwrap();
        st.get(&id).unwrap().claude.clone().unwrap().session_id
    };
    let listed = |list: &Value| {
        list["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["id"] == id.as_str())
            .cloned()
            .unwrap()
    };
    st.scan().await;
    assert_eq!(link_now(&st), "conv-vieja");
    assert_eq!(st.inbox.refusal(&id), None);

    // Pasa a segundo plano: la conversación, el estado y los mensajes, del trabajo.
    let job = spawn_job(&config, "conv-nueva");
    cleanup.1.push(job);
    std::fs::write(fake.join("job-1.pid"), job.to_string()).unwrap();
    viewer_file(&config, viewer, true);
    st.scan().await;
    assert_eq!(link_now(&st), "conv-nueva");
    let view = st
        .views
        .borrow()
        .sessions
        .iter()
        .find(|v| v.id == id)
        .cloned()
        .unwrap();
    let claude = view.claude.unwrap();
    assert_eq!(claude.session_id, "conv-nueva");
    assert_eq!(claude.status.as_deref(), Some("busy"));
    assert_eq!(st.inbox.refusal(&id), Some(nido::inbox::Refusal::Parked));
    let (code, list) = tool(addr, &caller, "sessions", json!({})).await;
    assert_eq!(code, 200, "{list}");
    let s = listed(&list);
    assert_eq!(s["channel"], false, "{s}");
    assert_eq!(s["channel_error"], "claude moved to the background", "{s}");
    assert_eq!(s["state"], "working", "{s}");
    // Un mensaje no se da por enviado: el trabajo no lo vería.
    let (code, e) = tool(
        addr,
        &caller,
        "send",
        json!({"session": id, "text": "hola"}),
    )
    .await;
    assert_eq!(code, 409, "{e}");
    assert!(
        e["error"]
            .as_str()
            .unwrap()
            .contains("moved its conversation to a background job"),
        "{e}"
    );

    // Vuelve al primer plano: lo del Claude de la sesión.
    viewer_file(&config, viewer, false);
    st.scan().await;
    assert_eq!(link_now(&st), "conv-vieja");
    assert_eq!(st.inbox.refusal(&id), None);
    let (_, list) = tool(addr, &caller, "sessions", json!({})).await;
    assert!(listed(&list).get("channel_error").is_none(), "{list}");
    assert!(alive(job));
}
