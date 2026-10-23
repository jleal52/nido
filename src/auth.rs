//! Control de acceso a la API de nido. No hay web: la API es para la consola de este equipo y
//! para los teléfonos y navegadores emparejados, que llegan por el canal cifrado.
//!
//! - Desde fuera de este equipo solo se atiende `/api/v1/e2e` (el canal cifrado, que se
//!   identifica con Noise). El resto de la API, solo desde loopback: la consola y los flujos del
//!   túnel, que entran por la escucha interna en 127.0.0.1.
//! - Siempre con `Authorization: Bearer <token>`: el principal (`~/.config/nido/token`, la
//!   consola) o el propio de un dispositivo emparejado. Una página web no puede poner esa cabecera
//!   en otro origen sin un preflight CORS, que aquí no se contesta, ni en un WebSocket.
//! - Escuchando solo en loopback, además, `Host` tiene que ser local (contra DNS rebinding).

use std::io::{self, Write};
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;

use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::server::AppState;

/// Lee el token del fichero, o lo crea (0600) si no existe.
pub fn load_or_create_token(path: &Path) -> io::Result<String> {
    if let Ok(t) = std::fs::read_to_string(path) {
        let t = t.trim();
        if t.len() >= 32 && t.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Ok(t.to_owned());
        }
    }
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(io::Error::other)?;
    let token: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
    opts.open(path)?.write_all(token.as_bytes())?;
    Ok(token)
}

pub fn ct_eq(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0u8, |acc, (x, y)| acc | (x ^ y))
            == 0
}

fn header(h: &HeaderMap, name: header::HeaderName) -> Option<&str> {
    h.get(name).and_then(|v| v.to_str().ok())
}

pub fn host_is_loopback(host: &str) -> bool {
    let name = match host.strip_prefix('[') {
        Some(rest) => rest.split(']').next().unwrap_or(""),
        None => host.split(':').next().unwrap_or(""),
    };
    matches!(name, "localhost" | "127.0.0.1" | "::1")
}

fn bearer(h: &HeaderMap) -> Option<&str> {
    header(h, header::AUTHORIZATION)?.strip_prefix("Bearer ")
}

fn unauthorized() -> Response {
    let body = serde_json::json!({ "error": "falta el token o no es válido" });
    (StatusCode::UNAUTHORIZED, axum::Json(body)).into_response()
}

pub async fn guard(State(st): State<Arc<AppState>>, req: Request, next: Next) -> Response {
    let h = req.headers();
    let path = req.uri().path();
    // El canal cifrado se identifica él mismo (Noise, con la clave del dispositivo).
    if path == "/api/v1/e2e" {
        return next.run(req).await;
    }
    let local = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .is_some_and(|c| c.0.ip().is_loopback());
    if !local {
        return (
            StatusCode::FORBIDDEN,
            "La API de nido solo se atiende en este equipo",
        )
            .into_response();
    }
    if st.loopback_only && !header(h, header::HOST).is_some_and(host_is_loopback) {
        return (StatusCode::FORBIDDEN, "Host no permitido").into_response();
    }
    if let Some(token) = &st.token {
        let Some(given) = bearer(h) else {
            return unauthorized();
        };
        // El token principal o el propio de un dispositivo emparejado.
        if !ct_eq(given, token) && !st.phones.lock().expect("teléfonos").token_ok(given) {
            return unauthorized();
        }
    }
    next.run(req).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_hosts() {
        assert!(host_is_loopback("localhost:7690"));
        assert!(host_is_loopback("127.0.0.1"));
        assert!(host_is_loopback("[::1]:7690"));
        assert!(!host_is_loopback("evil.com:7690"));
        assert!(!host_is_loopback("localhost.evil.com"));
    }
}
