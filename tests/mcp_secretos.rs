//! Secretos, portapapeles y ficheros (`secret`, `clipboard`, `send_file`) de punta a punta por
//! HTTP, en un directorio temporal y sin Claude: la prueba hace de `nido mcp` (con la sesión en
//! `X-Nido-Session`) y de la web (contesta y descarga por la API, que es donde desembocan los
//! flujos del canal cifrado).

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

/// Una petición de la web: con el token y en español.
async fn web(addr: SocketAddr, method: &str, path: &str, body: Option<Value>) -> (u16, Value) {
    let (method, path) = (method.to_owned(), path.to_owned());
    tokio::task::spawn_blocking(move || {
        let api = Api {
            addr,
            token: Some(TOKEN.into()),
        };
        api.request_with(
            &method,
            &path,
            body.as_ref(),
            &[("Accept-Language", "es")],
            Some(Duration::from_secs(30)),
        )
        .unwrap()
    })
    .await
    .unwrap()
}

/// Lo que baja la web: el código, las cabeceras (en minúsculas) y el cuerpo.
async fn download(addr: SocketAddr, path: &str) -> (u16, String, Vec<u8>) {
    use std::io::{Read, Write};
    let req =
        format!("GET {path} HTTP/1.0\r\nHost: {addr}\r\nAuthorization: Bearer {TOKEN}\r\n\r\n");
    let raw = tokio::task::spawn_blocking(move || {
        let mut s = std::net::TcpStream::connect(addr).unwrap();
        s.write_all(req.as_bytes()).unwrap();
        let mut out = Vec::new();
        s.read_to_end(&mut out).unwrap();
        out
    })
    .await
    .unwrap();
    let at = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    let head = String::from_utf8_lossy(&raw[..at]).to_lowercase();
    let code = head.split_whitespace().nth(1).unwrap().parse().unwrap();
    (code, head, raw[at + 4..].to_vec())
}

/// El valor de una cabecera de `head` (en minúsculas).
fn header<'a>(head: &'a str, name: &str) -> &'a str {
    head.lines()
        .find_map(|l| l.strip_prefix(&format!("{name}: ")))
        .unwrap_or("")
}

/// La sesión `id` tal como la ve la web (la lista se pide otra vez cada vez).
async fn listed(addr: SocketAddr, id: &str) -> Value {
    let (_, list) = web(addr, "GET", "/api/v1/sessions", None).await;
    list.as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == id)
        .cloned()
        .unwrap()
}

/// Espera a que la sesión tenga una pregunta en la lista.
async fn asked(addr: SocketAddr, id: &str) -> Value {
    for _ in 0..100 {
        let s = listed(addr, id).await;
        if !s["secret"].is_null() {
            return s["secret"].clone();
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("la pregunta no sale en la lista");
}

#[tokio::test(flavor = "multi_thread")]
async fn secrets_clipboard_and_files_end_to_end() {
    let _limit = nido::watchdog::limit(Duration::from_secs(120));
    let tmp = tempfile::tempdir().unwrap();
    let _cleanup = KillHolders(tmp.path().join("nido/run"));
    // La «shell» de las sesiones: se queda dormida.
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
    // La sesión que pide, en marcha (si no, `scan` le cierra la pregunta): la shell que duerme.
    let caller = {
        let mut s = session("api", tmp.path(), None);
        s.argv = vec![];
        state.registry.lock().await.add(s).unwrap().id
    };
    nido::launch::start(&paths, Launcher::Detached, &caller).unwrap();
    let mut alive = false;
    for _ in 0..100 {
        if nido::launch::is_alive(&paths, &caller) {
            alive = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(alive);
    let me = Some(caller.as_str());

    // Sin dispositivos: no hay dónde preguntar.
    let (code, e) = tool(
        addr,
        me,
        "secret",
        json!({"name": "tok", "reason": "subir"}),
    )
    .await;
    assert_eq!(code, 409, "{e}");
    assert_eq!(
        e["error"],
        "nowhere to ask: no phone or browser is paired with nido"
    );
    state.group.add_device(&[7u8; 32], "teléfono de prueba");
    let (code, e) = tool(addr, me, "secret", json!({"name": "a/b", "reason": "x"})).await;
    assert_eq!(code, 400, "{e}");
    let (code, e) = tool(addr, me, "secret", json!({"name": "tok", "reason": "  "})).await;
    assert_eq!(
        (code, e["error"].as_str()),
        (400, Some("the reason is missing"))
    );

    // Pregunta: sale en la lista; la web contesta; la sesión recibe la ruta.
    let ask_in_bg = |args: Value| {
        let c = caller.clone();
        tokio::spawn(async move { tool(addr, Some(c.as_str()), "secret", args).await })
    };
    let asking = ask_in_bg(json!({"name": "tok", "reason": "subir"}));
    let ask = asked(addr, &caller).await;
    assert_eq!(
        (ask["name"].as_str(), ask["reason"].as_str()),
        (Some("tok"), Some("subir"))
    );
    // Un segundo secreto en la misma sesión, no.
    let (code, e) = tool(addr, me, "secret", json!({"name": "otro", "reason": "x"})).await;
    assert_eq!(
        (code, e["error"].as_str()),
        (
            409,
            Some("this session is already waiting for another secret")
        )
    );
    let answer = format!("/api/v1/sessions/{caller}/secret");
    let (code, e) = web(
        addr,
        "POST",
        &answer,
        Some(json!({"ask": ask["id"], "value": ""})),
    )
    .await;
    assert_eq!(
        (code, e["error"].as_str()),
        (400, Some("el secreto está vacío"))
    );
    // Un error de la respuesta no cita el valor.
    let (code, e) = web(
        addr,
        "POST",
        &answer,
        Some(json!({"ask": ask["id"], "value": "s3cr3t-de-prueba", "otro": 1})),
    )
    .await;
    assert_eq!(code, 400);
    assert!(!e.to_string().contains("s3cr3t"), "{e}");
    let (code, _) = web(
        addr,
        "POST",
        &answer,
        Some(json!({"ask": ask["id"], "value": "s3cr3t-de-prueba", "remember": "2h"})),
    )
    .await;
    assert_eq!(code, 200);
    // La segunda respuesta (otro dispositivo) ya no vale.
    let (code, _) = web(
        addr,
        "POST",
        &answer,
        Some(json!({"ask": ask["id"], "value": "otro"})),
    )
    .await;
    assert_eq!(code, 404);
    let (code, got) = asking.await.unwrap();
    assert_eq!(code, 200, "{got}");
    let path = std::path::PathBuf::from(got["path"].as_str().unwrap());
    assert_eq!(got["nido"], env!("CARGO_BIN_EXE_nido"));
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "s3cr3t-de-prueba");
    assert!(path.starts_with(tmp.path().join("nido/state/secrets/equipo")));
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    assert!(listed(addr, &caller).await["secret"].is_null());
    // Recordado: al momento, sin diálogo; y sale en la lista sin el valor.
    let (code, again) = tool(
        addr,
        me,
        "secret",
        json!({"name": "tok", "reason": "otra vez"}),
    )
    .await;
    assert_eq!((code, &again["path"]), (200, &got["path"]));
    assert_eq!(again["nido"], env!("CARGO_BIN_EXE_nido"));
    let (_, list) = web(addr, "GET", "/api/v1/secrets", None).await;
    assert_eq!(list["secrets"][0]["name"], "tok");
    assert_eq!(list["secrets"][0]["by"], "api");
    assert!(!list.to_string().contains("s3cr3t"), "{list}");

    // `force`: pregunta otra vez; rechazado → error para la sesión.
    let asking = ask_in_bg(json!({"name": "tok", "reason": "caducó", "force": true}));
    let ask = asked(addr, &caller).await;
    let (code, _) = web(
        addr,
        "POST",
        &answer,
        Some(json!({"ask": ask["id"], "reject": true})),
    )
    .await;
    assert_eq!(code, 200);
    let (code, e) = asking.await.unwrap();
    assert_eq!(
        (code, e["error"].as_str()),
        (403, Some("the user declined to give “tok”"))
    );

    // Sin recordar: solo para esta sesión, en su carpeta.
    let asking = ask_in_bg(json!({"name": "solo", "reason": "x"}));
    let ask = asked(addr, &caller).await;
    let (code, _) = web(
        addr,
        "POST",
        &answer,
        Some(json!({"ask": ask["id"], "value": "v"})),
    )
    .await;
    assert_eq!(code, 200);
    let (code, got) = asking.await.unwrap();
    assert_eq!(code, 200, "{got}");
    let own = tmp
        .path()
        .join(format!("nido/state/secrets/sesiones/{caller}"));
    assert_eq!(got["path"], own.join("solo").to_str().unwrap());

    // Sin contestar: con la espera corta, error y la nota en la sesión.
    state.asks.set_wait(Duration::from_millis(300));
    let (code, e) = tool(addr, me, "secret", json!({"name": "npm", "reason": "x"})).await;
    assert_eq!(
        (code, e["error"].as_str()),
        (408, Some("no answer within 15 minutes"))
    );
    let notes = listed(addr, &caller).await["alerts"].clone();
    assert!(
        notes
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n["kind"] == "secret" && n["text"] == "npm: sin contestar"),
        "{notes}"
    );
    state.asks.set_wait(Duration::from_secs(60));

    // Olvidarlo.
    let (code, _) = web(addr, "DELETE", "/api/v1/secrets/tok", None).await;
    assert_eq!(code, 200);
    let (code, _) = web(addr, "DELETE", "/api/v1/secrets/tok", None).await;
    assert_eq!(code, 404);
    assert!(!path.exists());

    // clipboard y send_file: nota en la sesión, lista y contenido.
    let (code, r) = tool(
        addr,
        me,
        "clipboard",
        json!({"text": "git push origin main", "title": "la orden"}),
    )
    .await;
    assert_eq!((code, r["status"].as_str()), (200, Some("sent")));
    std::fs::write(tmp.path().join("informe.pdf"), b"%PDF-1.7 prueba").unwrap();
    let (code, r) = tool(addr, me, "send_file", json!({"path": "informe.pdf"})).await;
    assert_eq!(code, 200, "{r}");
    assert_eq!(
        (r["name"].as_str(), r["size"].as_u64()),
        (Some("informe.pdf"), Some(15))
    );
    std::fs::write(tmp.path().join("dibujo.svg"), b"<svg/>").unwrap();
    let (code, r) = tool(addr, me, "send_file", json!({"path": "dibujo.svg"})).await;
    assert_eq!(code, 200, "{r}");
    let (code, e) = tool(addr, me, "send_file", json!({"path": "no-existe.txt"})).await;
    assert_eq!(code, 400);
    assert!(e["error"].as_str().unwrap().starts_with("file "), "{e}");
    // Nada de la carpeta de secretos.
    let (code, e) = tool(addr, me, "send_file", json!({"path": own.join("solo")})).await;
    assert_eq!(code, 400);
    assert!(
        e["error"].as_str().unwrap().contains("secrets folder"),
        "{e}"
    );
    let s = listed(addr, &caller).await;
    let kinds: Vec<_> = s["alerts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["kind"].clone())
        .collect();
    assert!(
        kinds.contains(&json!("clipboard")) && kinds.contains(&json!("file")),
        "{s}"
    );
    let (_, items) = web(
        addr,
        "GET",
        &format!("/api/v1/sessions/{caller}/items"),
        None,
    )
    .await;
    let items = items["items"].as_array().unwrap().clone();
    assert_eq!(items.len(), 3, "{items:?}");
    let item = |i: usize| {
        format!(
            "/api/v1/sessions/{caller}/items/{}",
            items[i]["id"].as_str().unwrap()
        )
    };
    let (code, head, body) = download(addr, &item(0)).await;
    assert_eq!(
        (code, header(&head, "content-type"), body.as_slice()),
        (
            200,
            "text/plain; charset=utf-8",
            &b"git push origin main"[..]
        )
    );
    let (code, head, body) = download(addr, &item(1)).await;
    assert_eq!(
        (code, header(&head, "content-type"), body.as_slice()),
        (200, "application/pdf", &b"%PDF-1.7 prueba"[..])
    );
    assert!(
        header(&head, "content-disposition").starts_with("attachment;"),
        "{head}"
    );
    assert_eq!(header(&head, "x-content-type-options"), "nosniff");
    // Un svg, nunca como imagen (podría llevar código): como bytes.
    let (code, head, _) = download(addr, &item(2)).await;
    assert_eq!(
        (code, header(&head, "content-type")),
        (200, "application/octet-stream")
    );
    let (code, _, _) = download(addr, &format!("/api/v1/sessions/{caller}/items/ffffffff")).await;
    assert_eq!(code, 404);
    // `seen` las marca vistas, como las alertas.
    web(
        addr,
        "POST",
        &format!("/api/v1/sessions/{caller}/seen"),
        None,
    )
    .await;
    assert_eq!(listed(addr, &caller).await["alerts"], json!([]));

    // Otro equipo no puede usarlas (no están en el canal «peer»).
    assert!(
        !nido::mcp_api::PEER_TOOLS
            .iter()
            .any(|t| ["secret", "clipboard", "send_file"].contains(t))
    );

    // Cerrar la sesión: fuera su pregunta, sus secretos y sus envíos.
    assert!(own.exists());
    let asking = ask_in_bg(json!({"name": "ultimo", "reason": "x"}));
    asked(addr, &caller).await;
    let (code, _) = web(addr, "DELETE", &format!("/api/v1/sessions/{caller}"), None).await;
    assert_eq!(code, 200);
    let (code, e) = asking.await.unwrap();
    assert_eq!(code, 410, "{e}");
    assert!(
        !tmp.path()
            .join(format!("nido/state/outbox/{caller}"))
            .exists()
    );
    assert!(!own.exists());
}
