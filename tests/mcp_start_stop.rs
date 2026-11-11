//! `start` y `stop` del MCP de nido de punta a punta, en un directorio temporal. Sin Claude de
//! verdad: las sesiones «de Claude» se apuntan en el registro con su `ClaudeLink` y corren con una
//! «shell» falsa que se queda dormida (como en `mcp_api.rs`).

// Con sh, sockets Unix y señales: solo en Unix.
#![cfg(unix)]

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use nido::client::Api;
use nido::launch::{Launcher, Paths};
use nido::registry::{ClaudeLink, Session};
use nido::server::{self, AppState};
use serde_json::{Value, json};

const TOKEN: &str = "0123456789abcdef0123456789abcdef";

/// Una herramienta, como la llama `nido mcp`: sin `Accept-Language` (en inglés) y con la sesión
/// que llama en `X-Nido-Session`.
async fn tool(addr: SocketAddr, caller: Option<&str>, name: &str, args: Value) -> (u16, Value) {
    let caller = caller.map(str::to_owned);
    let path = format!("/api/v1/mcp/{name}");
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
            "POST",
            &path,
            Some(&args),
            &headers,
            Some(Duration::from_secs(30)),
        )
        .unwrap()
    })
    .await
    .unwrap()
}

/// Al acabar (también si la prueba falla), mata los guardianes que queden.
struct KillHolders(PathBuf);

impl Drop for KillHolders {
    fn drop(&mut self) {
        let Ok(dir) = std::fs::read_dir(&self.0) else {
            return;
        };
        for e in dir.flatten() {
            if e.path().extension().is_some_and(|x| x == "pid")
                && let Some(pid) = std::fs::read_to_string(e.path())
                    .ok()
                    .and_then(|p| p.trim().parse::<i32>().ok())
            {
                unsafe {
                    libc::kill(pid, libc::SIGKILL);
                }
            }
        }
    }
}

fn session(name: &str, cwd: &Path, claude: Option<ClaudeLink>) -> Session {
    Session {
        id: nido::registry::new_id(),
        name: name.into(),
        cwd: cwd.to_owned(),
        argv: vec!["true".into()],
        env: vec![],
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

/// `start` y `stop` de punta a punta: parar, ver `since`, arrancar, y los «no».
#[tokio::test(flavor = "multi_thread")]
async fn start_and_stop_end_to_end() {
    let _limit = nido::watchdog::limit(std::time::Duration::from_secs(120));
    let tmp = tempfile::tempdir().unwrap();
    let _cleanup = KillHolders(tmp.path().join("nido/run"));
    // La «shell» de las sesiones: se queda dormida. Así no arranca ningún Claude.
    let bin = tmp.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let shell = bin.join("shell");
    std::fs::write(&shell, "#!/bin/sh\nexec sleep 60\n").unwrap();
    std::fs::set_permissions(&shell, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    // Esta prueba es su propio proceso (un fichero de tests/): cambiar el entorno no toca a otras.
    unsafe {
        std::env::set_var("NIDO_HOME", tmp.path().join("nido"));
        std::env::set_var("NIDO_EXE", env!("CARGO_BIN_EXE_nido"));
        std::env::set_var("NIDO_NAME", "Prueba");
        std::env::set_var("SHELL", &shell);
    }
    let paths = Paths::from_env();
    let state = AppState::new(paths.clone(), Launcher::Detached, Some(TOKEN.into()), true).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    {
        let state = state.clone();
        tokio::spawn(async move {
            let app = server::router(state).into_make_service_with_connect_info::<SocketAddr>();
            axum::serve(listener, app).await.unwrap()
        });
    }

    // Dos sesiones «de Claude» con la shell que duerme: la que llama y otra. Sin transcripción.
    let link = ClaudeLink {
        session_id: "conv-1".into(),
        config_dir: Some(tmp.path().join("claude")),
        account: None,
        inside_shell: false,
    };
    let (me_id, other) = {
        let mut reg = state.registry.lock().await;
        let a = reg
            .add(session("llama", tmp.path(), Some(link.clone())))
            .unwrap()
            .id;
        let b = reg
            .add(session("otra", tmp.path(), Some(link.clone())))
            .unwrap()
            .id;
        (a, b)
    };
    // `server::wait_alive` es del crate: aquí, a mano.
    let alive_soon = |id: String| {
        let paths = paths.clone();
        async move {
            for _ in 0..100 {
                if nido::launch::is_alive(&paths, &id) {
                    return true;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            false
        }
    };
    for id in [&me_id, &other] {
        nido::launch::start(&paths, Launcher::Detached, id).unwrap();
        assert!(alive_soon(id.clone()).await);
    }
    let me = Some(me_id.as_str());
    let listed = |list: &Value, id: &str| {
        list["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["id"] == id)
            .cloned()
            .unwrap()
    };
    let t0 = nido::registry::now_ms();

    // En marcha: `since` es cuando arrancó (antes de ahora, hace poco).
    let (_, list) = tool(addr, me, "sessions", json!({})).await;
    let s = listed(&list, &other);
    assert_ne!(s["state"], "stopped");
    let since = s["since"].as_u64().unwrap();
    // La hora del pidfile puede ir unos ms por delante de `t0`: margen a los dos lados.
    assert!((since as i64 - t0 as i64).abs() < 60_000, "{s}");

    // Una sesión no se para a sí misma.
    let (code, e) = tool(addr, me, "stop", json!({"session": me_id})).await;
    assert_eq!(
        (code, e["error"].as_str()),
        (400, Some("a session cannot stop itself"))
    );
    // Tampoco escrita con su equipo.
    let (code, e) = tool(
        addr,
        me,
        "stop",
        json!({"session": format!("Prueba/{me_id}")}),
    )
    .await;
    assert_eq!(
        (code, e["error"].as_str()),
        (400, Some("a session cannot stop itself"))
    );
    assert!(nido::launch::is_alive(&paths, &me_id));
    // Sin Claude a la vista, la de la shell falsa está «arrancando»: cuenta como trabajando, así
    // que sin `force` no se para.
    assert_eq!(s["state"], "working");
    let (code, e) = tool(addr, me, "stop", json!({"session": other})).await;
    assert_eq!(code, 409, "{e}");
    assert_eq!(
        e["error"],
        "it is working (or has a pending question): to stop it anyway, force: true"
    );
    assert!(nido::launch::is_alive(&paths, &other));
    // Parar: parada, y `since` es la hora a la que se paró.
    let (code, r) = tool(
        addr,
        me,
        "stop",
        json!({"session": format!("Prueba/{other}"), "force": true}),
    )
    .await;
    assert_eq!((code, r["state"].as_str()), (200, Some("stopped")), "{r}");
    assert!(!nido::launch::is_alive(&paths, &other));
    assert!(!state.registry.lock().await.get(&other).unwrap().autostart);
    let (_, list) = tool(addr, me, "sessions", json!({})).await;
    let s = listed(&list, &other);
    assert_eq!(s["state"], "stopped");
    let stopped_at = s["since"].as_u64().unwrap();
    assert!(stopped_at >= t0, "{s}");
    let (code, r) = tool(addr, me, "stop", json!({"session": other})).await;
    assert_eq!(
        (code, r["note"].as_str()),
        (200, Some("it was already stopped"))
    );

    // Arrancar: en marcha otra vez; y otra vez, «ya estaba en marcha».
    let (code, r) = tool(addr, me, "start", json!({"session": other})).await;
    assert_eq!(code, 200, "{r}");
    assert_ne!(r["state"], "stopped");
    assert!(r.get("note").is_none(), "{r}");
    assert!(nido::launch::is_alive(&paths, &other));
    assert!(state.registry.lock().await.get(&other).unwrap().autostart);
    // En marcha otra vez: `since` es cuando volvió a arrancar, no la parada.
    let (_, list) = tool(addr, me, "sessions", json!({})).await;
    let s = listed(&list, &other);
    assert_ne!(s["state"], "stopped");
    let started_at = s["since"].as_u64().unwrap();
    assert!(started_at >= stopped_at, "{s} (parada: {stopped_at})");
    let (code, r) = tool(addr, me, "start", json!({"session": other})).await;
    assert_eq!(
        (code, r["note"].as_str()),
        (200, Some("it was already running"))
    );

    // Se para sola (sin nido): sigue con autoarranque. `stop` dice que ya estaba parada, pero
    // apunta que se queda así: no vuelve a arrancar con el equipo.
    nido::launch::stop(&paths, Launcher::Detached, &other).unwrap();
    let mut gone = false;
    for _ in 0..100 {
        if !nido::launch::is_alive(&paths, &other) {
            gone = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(gone);
    assert!(state.registry.lock().await.get(&other).unwrap().autostart);
    // Y su pidfile se quedó viejo: el número es ya de otro proceso, que no se toca.
    let mut stranger = std::process::Command::new("sleep")
        .arg("60")
        .spawn()
        .unwrap();
    std::fs::write(paths.pidfile(&other), stranger.id().to_string()).unwrap();
    let (code, r) = tool(addr, me, "stop", json!({"session": other})).await;
    assert_eq!(
        (code, r["note"].as_str()),
        (200, Some("it was already stopped"))
    );
    assert!(!state.registry.lock().await.get(&other).unwrap().autostart);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let untouched = stranger.try_wait().unwrap().is_none();
    let _ = stranger.kill();
    let _ = stranger.wait();
    let _ = std::fs::remove_file(paths.pidfile(&other));
    assert!(untouched, "stop mató a un proceso que no era de nido");

    // Una que no existe, o que no es de Claude: no.
    let (code, _) = tool(addr, me, "start", json!({"session": "ffffffff"})).await;
    assert_eq!(code, 404);
    let shell_only = state
        .registry
        .lock()
        .await
        .add(session("shell", tmp.path(), None))
        .unwrap()
        .id;
    let (code, _) = tool(addr, me, "stop", json!({"session": shell_only})).await;
    assert_eq!(code, 404);
}
