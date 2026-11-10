//! La bandeja del MCP (`send`, `GET /api/v1/mcp/inbox` y su `ack`) de punta a punta, en un
//! directorio temporal y por HTTP de verdad. Hace de `nido mcp` la propia prueba: recoge la
//! bandeja con la sesión de destino en `X-Nido-Session`. Las sesiones que abre `open` corren una
//! «shell» falsa que se queda dormida.

// Con sh, sockets Unix y señales: solo en Unix.
#![cfg(unix)]

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use nido::client::Api;
use nido::launch::{Launcher, Paths};
use nido::registry::{ClaudeLink, Session};
use nido::server::{self, AppState};
use serde_json::{Value, json};

const TOKEN: &str = "0123456789abcdef0123456789abcdef";
/// Lo que espera una recogida vacía en esta prueba (de verdad, 25 s).
const POLL: Duration = Duration::from_secs(3);

/// Una petición como las de `nido mcp`: sin `Accept-Language` (en inglés) y con la sesión en
/// `X-Nido-Session` (`None`: sin la cabecera). Devuelve también cuánto tardó.
async fn request(
    addr: SocketAddr,
    method: &'static str,
    path: &str,
    caller: Option<&str>,
    body: Option<Value>,
) -> (u16, Value, Duration) {
    let caller = caller.map(str::to_owned);
    let path = path.to_owned();
    tokio::task::spawn_blocking(move || {
        let api = Api {
            addr,
            token: Some(TOKEN.into()),
        };
        let headers: Vec<(&str, &str)> = caller
            .as_deref()
            .map(|c| vec![("X-Nido-Session", c)])
            .unwrap_or_default();
        let start = Instant::now();
        let (code, v) = api
            .request_with(
                method,
                &path,
                body.as_ref(),
                &headers,
                Some(Duration::from_secs(30)),
            )
            .unwrap();
        (code, v, start.elapsed())
    })
    .await
    .unwrap()
}

async fn tool(addr: SocketAddr, caller: &str, name: &str, args: Value) -> (u16, Value) {
    let (code, v, _) = request(
        addr,
        "POST",
        &format!("/api/v1/mcp/{name}"),
        Some(caller),
        Some(args),
    )
    .await;
    (code, v)
}

async fn send(addr: SocketAddr, caller: &str, to: &str, text: &str) -> (u16, Value) {
    tool(addr, caller, "send", json!({"session": to, "text": text})).await
}

/// La recogida de la bandeja de `id`: los mensajes y cuánto tardó.
async fn poll(addr: SocketAddr, id: &str) -> (Vec<Value>, Duration) {
    let (code, v, took) = request(addr, "GET", "/api/v1/mcp/inbox", Some(id), None).await;
    assert_eq!(code, 200, "{v}");
    (v["messages"].as_array().unwrap().clone(), took)
}

async fn ack(addr: SocketAddr, id: &str, ids: &[&str]) -> Value {
    let (code, v, _) = request(
        addr,
        "POST",
        "/api/v1/mcp/inbox/ack",
        Some(id),
        Some(json!({ "ids": ids })),
    )
    .await;
    assert_eq!(code, 200, "{v}");
    v
}

fn texts(msgs: &[Value]) -> Vec<&str> {
    msgs.iter().map(|m| m["text"].as_str().unwrap()).collect()
}

fn id_of(m: &Value) -> &str {
    m["id"].as_str().unwrap()
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
        seen: 0,
        opened_by: None,
        alerts: vec![],
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn mcp_inbox_end_to_end() {
    let tmp = tempfile::tempdir().unwrap();
    let _cleanup = KillHolders(tmp.path().join("nido/run"));
    // La «shell» de las sesiones: duerme. Así `open` no arranca ningún Claude.
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
    state.inbox.set_poll_wait(POLL);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    {
        let state = state.clone();
        tokio::spawn(async move {
            let app = server::router(state).into_make_service_with_connect_info::<SocketAddr>();
            axum::serve(listener, app).await.unwrap()
        });
    }

    // La que llama (no hace falta que corra ni que sea de Claude) y otra de Claude, parada.
    let link = ClaudeLink {
        session_id: "conv-1".into(),
        config_dir: Some(tmp.path().join("claude")),
        account: None,
        inside_shell: false,
    };
    let (padre, parada) = {
        let mut reg = state.registry.lock().await;
        let padre = reg.add(session("padre", tmp.path(), None)).unwrap();
        let parada = reg.add(session("parada", tmp.path(), Some(link))).unwrap();
        (padre.id, parada.id)
    };

    // Sin sesión, no hay bandeja.
    let (code, e, _) = request(addr, "GET", "/api/v1/mcp/inbox", None, None).await;
    assert_eq!(code, 403, "{e}");
    assert_eq!(e["error"], "not called from a nido session");

    // A sí misma, no (aunque no sea de Claude: antes de buscarla).
    for to in [padre.clone(), format!("Prueba/{padre}")] {
        let (code, e) = send(addr, &padre, &to, "hola").await;
        assert_eq!(code, 400, "{e}");
        assert_eq!(e["error"], "you cannot send a message to yourself");
    }
    let (code, e) = send(addr, &padre, &parada, "   ").await;
    assert_eq!(code, 400, "{e}");
    assert_eq!(e["error"], "the text is missing");
    let (code, e) = send(addr, &padre, &parada, &"a".repeat(24 * 1024 + 1)).await;
    assert_eq!(code, 400, "{e}");
    assert_eq!(e["error"], "the text is longer than 24 KB");
    let (code, e) = send(addr, &padre, &format!("Otro/{parada}"), "hola").await;
    assert_eq!(code, 400, "{e}");
    assert_eq!(
        e["error"],
        "session is on another machine (not supported yet)"
    );
    let (code, _) = send(addr, &padre, "Prueba/zzzz", "hola").await;
    assert_eq!(code, 404);

    // Parada y del usuario: a la cola, y decide ella.
    let (code, r) = send(addr, &padre, &format!("Prueba/{parada}"), "  ¿sigues?  ").await;
    assert_eq!(code, 200, "{r}");
    assert_eq!(
        r,
        json!({"status": "queued", "note": "queued; it will be delivered when the session \
            starts, and the session decides whether to act on it"})
    );
    let queued = state.inbox.pending(&parada);
    assert_eq!(queued.len(), 1);
    assert_eq!(queued[0].text, "¿sigues?");
    assert_eq!(queued[0].from, "padre");
    assert_eq!(queued[0].from_id, format!("Prueba/{padre}"));
    assert_eq!(queued[0].machine, "Prueba");

    // Una hija recién abierta: viva, sin channel aún, en su arranque → a la cola.
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let (code, hija) = tool(
        addr,
        &padre,
        "open",
        json!({"machine": "Prueba", "folder": work, "prompt": "espera"}),
    )
    .await;
    assert_eq!(code, 200, "{hija}");
    assert_eq!(hija["channel"], false);
    let hija = hija["id"].as_str().unwrap().to_owned();
    let (code, r) = send(addr, &padre, &hija, "uno").await;
    assert_eq!(code, 200, "{r}");
    assert_eq!(
        r,
        json!({"status": "queued",
            "note": "queued; it will be delivered when the session has finished starting"})
    );

    // Su `nido mcp` recoge: sale enseguida, con quién lo manda.
    let (msgs, took) = poll(addr, &hija).await;
    assert!(took < Duration::from_secs(2), "{took:?}");
    assert_eq!(msgs.len(), 1);
    let m = &msgs[0];
    assert_eq!(
        m.as_object().unwrap().keys().collect::<Vec<_>>(),
        ["at", "from", "from_id", "id", "machine", "text"]
    );
    assert_eq!(m["text"], "uno");
    assert_eq!(m["from"], "padre");
    assert_eq!(m["from_id"], format!("Prueba/{padre}"));
    assert_eq!(m["machine"], "Prueba");
    assert!(m["at"].as_u64().unwrap() > 0);
    let first = id_of(m).to_owned();

    // Ya tiene channel: entregado, sin nota (es suya).
    let (_, list) = tool(addr, &padre, "sessions", json!({})).await;
    let channel = |id: &str| {
        list.as_array()
            .unwrap()
            .iter()
            .find(|s| s["id"] == id)
            .unwrap()["channel"]
            .clone()
    };
    assert_eq!(channel(&hija), true);
    assert_eq!(channel(&parada), false);
    let (code, r) = send(addr, &padre, &hija, "dos").await;
    assert_eq!(code, 200, "{r}");
    assert_eq!(r, json!({"status": "sent"}));

    // Sin confirmar, vuelve a salir (del más antiguo al más nuevo).
    let (msgs, _) = poll(addr, &hija).await;
    assert_eq!(texts(&msgs), ["uno", "dos"]);
    assert_eq!(id_of(&msgs[0]), first);
    let second = id_of(&msgs[1]).to_owned();
    // Se confirma lo suyo; lo de otra bandeja no se toca.
    let other = state.inbox.pending(&parada)[0].id.clone();
    let r = ack(addr, &hija, &[&first, &other]).await;
    assert_eq!(r, json!({"ok": true, "removed": 1}));
    assert_eq!(state.inbox.pending(&parada).len(), 1);
    let (msgs, _) = poll(addr, &hija).await;
    assert_eq!(texts(&msgs), ["dos"]);
    ack(addr, &hija, &[&second]).await;

    // Vacía: espera y vuelve sin nada.
    let (msgs, took) = poll(addr, &hija).await;
    assert!(msgs.is_empty());
    assert!(took >= POLL && took < POLL * 2, "{took:?}");

    // Esperando, se despierta en cuanto llega algo.
    let waiting = {
        let hija = hija.clone();
        tokio::spawn(async move { poll(addr, &hija).await })
    };
    tokio::time::sleep(Duration::from_millis(500)).await;
    let (code, r) = send(addr, &padre, &hija, "tres").await;
    assert_eq!(code, 200, "{r}");
    assert_eq!(r["status"], "sent");
    let (msgs, took) = waiting.await.unwrap();
    assert_eq!(texts(&msgs), ["tres"]);
    assert!(took < Duration::from_secs(2), "{took:?}");

    // Viva, sin channel y arrancada hace rato.
    let (code, vieja) = tool(
        addr,
        &padre,
        "open",
        json!({"machine": "Prueba", "folder": work, "prompt": "espera"}),
    )
    .await;
    assert_eq!(code, 200, "{vieja}");
    let vieja = vieja["id"].as_str().unwrap().to_owned();
    std::fs::File::options()
        .write(true)
        .open(paths.pidfile(&vieja))
        .unwrap()
        .set_modified(SystemTime::now() - Duration::from_secs(300))
        .unwrap();
    // `nido serve` acaba de arrancar: su `nido mcp` aún puede volver a recoger → a la cola.
    let (code, r) = send(addr, &padre, &vieja, "tras reiniciar").await;
    assert_eq!(code, 200, "{r}");
    assert_eq!(r["status"], "queued");
    // Con `nido serve` en marcha desde hace rato: no admite mensajes, y no se encola nada.
    state.inbox.backdate(Duration::from_secs(300));
    let (code, e) = send(addr, &padre, &vieja, "hola").await;
    assert_eq!(code, 409, "{e}");
    assert_eq!(
        e["error"],
        "this session does not accept messages (no channel)"
    );
    assert_eq!(texts_of(&state, &vieja), ["tras reiniciar"]);

    // Como mucho 100 en cola por sesión.
    for i in 1..100 {
        let (code, r) = send(addr, &padre, &parada, &format!("m{i}")).await;
        assert_eq!(code, 200, "{i}: {r}");
    }
    let (code, e) = send(addr, &padre, &parada, "uno más").await;
    assert_eq!(code, 409, "{e}");
    assert_eq!(e["error"], "the inbox of that session is full");

    // Tras reiniciar nido, lo no confirmado sigue ahí.
    let again = AppState::new(paths.clone(), Launcher::Detached, Some(TOKEN.into()), true).unwrap();
    assert_eq!(again.inbox.pending(&parada).len(), 100);
    assert_eq!(again.inbox.pending(&hija), state.inbox.pending(&hija));
    assert_eq!(texts_of(&again, &hija), ["tres"]);
    drop(again);

    // Al cerrarla, fuera su bandeja (también del fichero).
    for id in [&hija, &vieja] {
        let (code, r) = tool(addr, &padre, "close", json!({"session": id})).await;
        assert_eq!(code, 200, "{r}");
    }
    assert!(state.inbox.pending(&hija).is_empty());
    let file = std::fs::read_to_string(paths.state.join("inbox.json")).unwrap();
    assert!(!file.contains(&hija), "{file}");
    assert!(file.contains(&parada));
}

fn texts_of(st: &AppState, id: &str) -> Vec<String> {
    st.inbox.pending(id).into_iter().map(|m| m.text).collect()
}
