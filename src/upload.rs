//! Subidas desde el navegador (imágenes pegadas, archivos arrastrados o elegidos en el móvil).
//!
//! Una terminal solo recibe texto: el archivo se guarda en este equipo y la web escribe su
//! ruta en la terminal, como hace un emulador al soltar un archivo encima (Claude Code, por
//! ejemplo, adjunta la imagen de una ruta pegada).

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use crate::server::AppState;

/// Máximo por archivo.
pub const MAX_BYTES: usize = 64 * 1024 * 1024;
/// Lo subido se borra pasada una semana.
const KEEP: Duration = Duration::from_secs(7 * 24 * 3600);

pub fn default_dir() -> PathBuf {
    let cache = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .unwrap_or_else(std::env::temp_dir);
    cache.join("nido").join("uploads")
}

/// Nombre seguro para el disco y para la terminal: sin rutas, espacios ni caracteres que
/// una shell interprete. Sin nombre, uno a partir del tipo (`pegado.png`).
pub fn safe_name(name: Option<&str>, content_type: Option<&str>) -> String {
    let base = name.unwrap_or("").rsplit(['/', '\\']).next().unwrap_or("");
    let mut out: String = base
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect();
    out = out.trim_start_matches(['.', '_', '-']).to_owned();
    if out.len() > 80 {
        // Recorta por delante y conserva la extensión.
        out = out[out.len() - 80..].to_owned();
    }
    if out.is_empty() || !out.contains(|c: char| c.is_ascii_alphanumeric()) {
        let ext = match content_type.unwrap_or("") {
            "image/png" => "png",
            "image/jpeg" => "jpg",
            "image/gif" => "gif",
            "image/webp" => "webp",
            "image/heic" => "heic",
            "text/plain" => "txt",
            "application/pdf" => "pdf",
            _ => "bin",
        };
        out = format!("pegado.{ext}");
    }
    out
}

fn cleanup(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .is_ok_and(|t| now.duration_since(t).unwrap_or_default() > KEEP);
        if old {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

fn save(dir: &Path, name: &str, data: &[u8]) -> io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    cleanup(dir);
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let mut rand = [0u8; 3];
    getrandom::fill(&mut rand).map_err(io::Error::other)?;
    let tag: String = rand.iter().map(|b| format!("{b:02x}")).collect();
    let path = dir.join(format!("{secs}-{tag}-{name}"));
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
    opts.open(&path)?.write_all(data)?;
    Ok(path)
}

#[derive(Deserialize)]
pub struct UploadQuery {
    name: Option<String>,
}

/// `POST /api/v1/upload?name=captura.png`, cuerpo = el archivo. Devuelve `{"path": "…"}`.
pub async fn api_upload(
    State(st): State<Arc<AppState>>,
    Query(q): Query<UploadQuery>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if body.is_empty() {
        let msg = serde_json::json!({ "error": "archivo vacío" });
        return (StatusCode::BAD_REQUEST, Json(msg)).into_response();
    }
    let ctype = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok());
    let name = safe_name(q.name.as_deref(), ctype);
    let dir = st.uploads.clone();
    match tokio::task::spawn_blocking(move || save(&dir, &name, &body)).await {
        Ok(Ok(path)) => Json(serde_json::json!({ "path": path })).into_response(),
        Ok(Err(e)) => {
            let msg = serde_json::json!({ "error": format!("no se pudo guardar: {e}") });
            (StatusCode::INTERNAL_SERVER_ERROR, Json(msg)).into_response()
        }
        Err(e) => {
            let msg = serde_json::json!({ "error": e.to_string() });
            (StatusCode::INTERNAL_SERVER_ERROR, Json(msg)).into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_safe_for_disk_and_shell() {
        assert_eq!(
            safe_name(Some("Captura de pantalla.png"), None),
            "Captura_de_pantalla.png"
        );
        assert_eq!(safe_name(Some("../../etc/passwd"), None), "passwd");
        assert_eq!(safe_name(Some("C:\\fotos\\IMG 1.HEIC"), None), "IMG_1.HEIC");
        assert_eq!(safe_name(Some("$(rm -rf ~).txt"), None), "rm_-rf___.txt");
        assert_eq!(safe_name(Some(".bashrc"), None), "bashrc");
        assert_eq!(safe_name(None, Some("image/png")), "pegado.png");
        assert_eq!(safe_name(Some("..."), Some("image/jpeg")), "pegado.jpg");
        let long = format!("{}.png", "a".repeat(200));
        let short = safe_name(Some(&long), None);
        assert!(short.len() == 80 && short.ends_with(".png"));
    }
}
