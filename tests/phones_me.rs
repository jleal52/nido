//! Un dispositivo emparejado (teléfono o web) se da de baja él mismo: «Cerrar sesión».

// Con el NIDO_HOME del proceso: en su propio binario de pruebas.
#![cfg(unix)]

use std::net::SocketAddr;
use std::time::Duration;

use nido::client::Api;
use nido::launch::{Launcher, Paths};
use nido::server::{self, AppState};

const TOKEN: &str = "0123456789abcdef0123456789abcdef";

/// Una llamada a la API como la de la web o la app: sin `Accept-Language` (en inglés).
async fn call(addr: SocketAddr, token: &str, method: &str, path: &str) -> (u16, serde_json::Value) {
    let (token, method, path) = (token.to_owned(), method.to_owned(), path.to_owned());
    tokio::task::spawn_blocking(move || {
        Api {
            addr,
            token: Some(token),
        }
        .request_with(&method, &path, None, &[], Some(Duration::from_secs(30)))
        .unwrap()
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn a_device_signs_itself_out_of_every_machine() {
    let _limit = nido::watchdog::limit(Duration::from_secs(60));
    let tmp = tempfile::tempdir().unwrap();
    unsafe { std::env::set_var("NIDO_HOME", tmp.path()) };
    let state = AppState::new(
        Paths::from_env(),
        Launcher::Detached,
        Some(TOKEN.into()),
        true,
    )
    .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let st = state.clone();
    tokio::spawn(async move {
        let app = server::router(st).into_make_service_with_connect_info::<SocketAddr>();
        axum::serve(listener, app).await.unwrap()
    });

    // Dos dispositivos, en el grupo y con su token.
    let add = |k: u8, name: &str| {
        state.group.add_device(&[k; 32], name);
        state
            .phones
            .lock()
            .unwrap()
            .add(&[k; 32], name, None)
            .unwrap()
            .token
    };
    let web = add(7, "Firefox");
    let phone = add(8, "iPhone");

    // El token principal no es un dispositivo.
    let (code, e) = call(addr, TOKEN, "DELETE", "/api/v1/phones/me").await;
    assert_eq!(code, 400, "{e}");
    assert_eq!(
        e["error"],
        "only a paired phone or browser can sign itself out"
    );

    // La web se da de baja: fuera de la lista y su token ya no vale; el teléfono sigue.
    let (code, e) = call(addr, &web, "DELETE", "/api/v1/phones/me").await;
    assert_eq!(code, 204, "{e}");
    let (code, list) = call(addr, &phone, "GET", "/api/v1/phones").await;
    assert_eq!(code, 200, "{list}");
    let names: Vec<_> = list
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["name"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(names, ["iPhone"]);
    let (code, _) = call(addr, &web, "DELETE", "/api/v1/phones/me").await;
    assert_eq!(code, 401);
    // Y en el grupo (lo que reparte la baja a los demás equipos).
    assert!(!state.group.has_device(&[7u8; 32]));
    assert!(state.group.has_device(&[8u8; 32]));
}
