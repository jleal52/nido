//! El gestor de punta a punta: guardianes de verdad (el binario nido), sin systemd, en un
//! directorio temporal.

use std::net::SocketAddr;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use nido::client::Api;
use nido::launch::{Launcher, Paths};
use nido::server::{self, AppState};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

const TOKEN: &str = "0123456789abcdef0123456789abcdef";

async fn call(
    addr: SocketAddr,
    method: &str,
    path: &str,
    body: Option<serde_json::Value>,
) -> (u16, serde_json::Value) {
    let (method, path) = (method.to_owned(), path.to_owned());
    tokio::task::spawn_blocking(move || {
        let api = Api {
            addr,
            token: Some(TOKEN.into()),
        };
        api.request(&method, &path, body.as_ref()).unwrap()
    })
    .await
    .unwrap()
}

async fn wait_for(mut cond: impl AsyncFnMut() -> bool) -> bool {
    for _ in 0..100 {
        if cond().await {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}

async fn alive(addr: SocketAddr, name: &str) -> Option<bool> {
    let (_, list) = call(addr, "GET", "/api/v1/sessions", None).await;
    list.as_array()?
        .iter()
        .find(|s| s["name"] == name)
        .map(|s| s["alive"] == true)
}

async fn history(addr: SocketAddr, id: &str, all: bool) -> String {
    let (code, body) = call(
        addr,
        "GET",
        &format!("/api/v1/sessions/{id}/history?all={all}"),
        None,
    )
    .await;
    assert_eq!(code, 200);
    body.as_str().unwrap_or_default().to_owned()
}

/// Al acabar (también si la prueba falla), mata los guardianes que queden.
struct KillHolders(std::path::PathBuf);

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

#[tokio::test(flavor = "multi_thread")]
async fn sessions_end_to_end() {
    let tmp = tempfile::tempdir().unwrap();
    let _cleanup = KillHolders(tmp.path().join("run"));
    // El guardián (otro proceso) lee las mismas rutas por NIDO_HOME.
    unsafe {
        std::env::set_var("NIDO_HOME", tmp.path());
        std::env::set_var("NIDO_EXE", env!("CARGO_BIN_EXE_nido"));
    }
    let paths = Paths::from_env();
    let state = AppState::new(paths.clone(), Launcher::Detached, Some(TOKEN.into()), true).unwrap();
    server::spawn_scanner(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let app = server::router(state).into_make_service_with_connect_info::<SocketAddr>();
        axum::serve(listener, app).await.unwrap()
    });

    // Crear una sesión con bash.
    let body = serde_json::json!({
        "name": "prueba", "cwd": tmp.path(), "command": ["bash", "--norc", "--noprofile"],
    });
    let (code, created) = call(addr, "POST", "/api/v1/sessions", Some(body)).await;
    assert_eq!(code, 201, "{created}");
    let id = created["id"].as_str().unwrap().to_owned();
    assert_eq!(alive(addr, "prueba").await, Some(true));
    let (code, _) = call(
        addr,
        "POST",
        "/api/v1/sessions",
        Some(serde_json::json!({"name": "prueba", "command": ["true"]})),
    )
    .await;
    assert_eq!(code, 400, "nombre repetido");

    // Terminal por WebSocket.
    let url = format!("ws://{addr}/api/v1/sessions/{id}/term?role=driver&cols=80&rows=24");
    let mut req = url.into_client_request().unwrap();
    req.headers_mut()
        .insert("Authorization", format!("Bearer {TOKEN}").parse().unwrap());
    let (mut ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();
    ws.send(Message::Binary("echo hola-$((40+2))\r".into()))
        .await
        .unwrap();
    let mut seen = String::new();
    let ok = tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(Ok(msg)) = ws.next().await {
            if let Message::Binary(b) = msg {
                seen.push_str(&String::from_utf8_lossy(&b));
                if seen.contains("hola-42") {
                    return true;
                }
            }
        }
        false
    })
    .await
    .unwrap_or(false);
    assert!(ok, "la terminal no devolvió la salida: {seen:?}");

    // En vivo: sin preguntar, llega la pantalla en cuanto cambia (y el chat, vacío: no es Claude).
    let url = format!("ws://{addr}/api/v1/sessions/{id}/live");
    let mut req = url.into_client_request().unwrap();
    req.headers_mut()
        .insert("Authorization", format!("Bearer {TOKEN}").parse().unwrap());
    let (mut live, _) = tokio_tungstenite::connect_async(req).await.unwrap();
    ws.send(Message::Binary("echo en-vivo-$((6*7))\r".into()))
        .await
        .unwrap();
    let (mut chat_ok, mut screen_ok) = (false, false);
    let got = tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(Ok(msg)) = live.next().await {
            if let Message::Text(t) = msg {
                let v: serde_json::Value = serde_json::from_str(t.as_str()).unwrap();
                match v["type"].as_str() {
                    Some("chat") => chat_ok = v["session_id"].is_null(),
                    Some("screen") => {
                        screen_ok |= v["text"].as_str().unwrap_or("").contains("en-vivo-42")
                    }
                    _ => {}
                }
                if chat_ok && screen_ok {
                    return true;
                }
            }
        }
        false
    })
    .await
    .unwrap_or(false);
    assert!(
        got,
        "el WebSocket en vivo no mandó chat y pantalla (chat={chat_ok}, pantalla={screen_ok})"
    );
    drop(live);
    drop(ws);
    assert!(history(addr, &id, true).await.contains("hola-42"));
    // Solo la pantalla (la franja en vivo de la vista chat).
    let (code, screen) = call(
        addr,
        "GET",
        &format!("/api/v1/sessions/{id}/history?screen=true"),
        None,
    )
    .await;
    assert_eq!(code, 200);
    assert!(screen.as_str().unwrap_or_default().contains("hola-42"));
    // Sin Claude no hay chat.
    let (code, _) = call(addr, "GET", &format!("/api/v1/sessions/{id}/chat"), None).await;
    assert_eq!(code, 404);

    // Reiniciar: vuelve a estar viva y el historial sigue, con su marca.
    let (code, _) = call(
        addr,
        "POST",
        &format!("/api/v1/sessions/{id}/restart"),
        None,
    )
    .await;
    assert_eq!(code, 200);
    assert_eq!(alive(addr, "prueba").await, Some(true));
    let all = history(addr, &id, true).await;
    assert!(
        all.contains("hola-42") && all.contains("\u{1}restart "),
        "{all:?}"
    );

    // Parar: queda registrada, parada, y su historial se lee del disco.
    let (code, _) = call(addr, "POST", &format!("/api/v1/sessions/{id}/stop"), None).await;
    assert_eq!(code, 200);
    assert!(wait_for(async || alive(addr, "prueba").await == Some(false)).await);
    assert!(history(addr, &id, true).await.contains("hola-42"));

    // Arrancar otra vez y borrar: se para y desaparece, también su estado.
    let (code, _) = call(addr, "POST", &format!("/api/v1/sessions/{id}/start"), None).await;
    assert_eq!(code, 200);
    assert_eq!(alive(addr, "prueba").await, Some(true));
    let (code, _) = call(addr, "DELETE", &format!("/api/v1/sessions/{id}"), None).await;
    assert_eq!(code, 200);
    assert_eq!(alive(addr, "prueba").await, None);
    assert!(!paths.session_state(&id).exists());
    assert!(!paths.socket(&id).exists());
    // Ningún guardián vivo.
    let leftover = std::fs::read_dir(&paths.runtime)
        .map(|d| {
            d.flatten()
                .filter(|e| e.path().extension().is_some_and(|x| x == "pid"))
                .count()
        })
        .unwrap_or(0);
    assert_eq!(leftover, 0, "quedó algún guardián");
}
