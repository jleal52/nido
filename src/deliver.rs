//! Lo que las sesiones piden y mandan al usuario con el MCP de nido: secretos (`secret`), textos
//! para copiar (`clipboard`) y ficheros (`send_file`).
//!
//! - Las herramientas, solo para las sesiones de este equipo: no están en `PEER_TOOLS` (otro
//!   equipo no puede sacar un diálogo de secreto aquí ni mandar nada en nombre de nadie).
//! - La API de la web y la app (que llegan por el canal cifrado): contestar un secreto, la lista
//!   de envíos de una sesión y su contenido, y los secretos recordados.
//!
//! Lo que escribe el usuario no se apunta en ningún sitio: ni en el log ni en los errores.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::Json;
use axum::extract::{Path as UrlPath, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::mcp_api::{Fail, Reply, args_of, fail};
use crate::outbox::{self, MAX_TEXT};
use crate::registry::{NoteKind, Session, now_ms};
use crate::server::{self, AppState, error};

/// La ruta de `send_file`: absoluta, o relativa a la carpeta de la sesión.
fn resolve(cwd: &Path, path: &str) -> PathBuf {
    let p = Path::new(path);
    if p.is_absolute() {
        p.to_owned()
    } else {
        cwd.join(p)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ClipboardArgs {
    text: String,
    title: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SendFileArgs {
    path: String,
    title: Option<String>,
}

/// `clipboard`: un texto para que el usuario lo copie con un botón.
pub(crate) async fn tool_clipboard(st: &Arc<AppState>, caller: &Session, args: Value) -> Reply {
    let a: ClipboardArgs = args_of(args)?;
    if a.text.trim().is_empty() {
        return Err(fail(StatusCode::BAD_REQUEST, "el texto está vacío"));
    }
    if a.text.len() > MAX_TEXT {
        return Err(fail(
            StatusCode::BAD_REQUEST,
            format!("el texto pasa de {} KB", MAX_TEXT / 1024),
        ));
    }
    let item = {
        let (st, id) = (st.clone(), caller.id.clone());
        tokio::task::spawn_blocking(move || {
            st.outbox
                .add_text(&id, &a.text, a.title.as_deref(), now_ms())
        })
        .await
        .map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .map_err(|e| {
            fail(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("no se pudo guardar la copia: {e}"),
            )
        })?
    };
    st.session_note(
        &caller.id,
        Some(NoteKind::Clipboard),
        &item.title,
        Some(&item.id),
    )
    .await
    .map_err(Fail::from)?;
    Ok(json!({ "status": "sent" }))
}

/// `send_file`: una copia de un fichero (hasta 64 MB) para que el usuario la abra o la guarde.
pub(crate) async fn tool_send_file(st: &Arc<AppState>, caller: &Session, args: Value) -> Reply {
    let a: SendFileArgs = args_of(args)?;
    let path = resolve(&caller.cwd, &a.path);
    let item = {
        let (st, id) = (st.clone(), caller.id.clone());
        tokio::task::spawn_blocking(move || {
            st.outbox.add_file(&id, &path, a.title.as_deref(), now_ms())
        })
        .await
        .map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .map_err(send_fail)?
    };
    st.session_note(
        &caller.id,
        Some(NoteKind::File),
        &item.title,
        Some(&item.id),
    )
    .await
    .map_err(Fail::from)?;
    Ok(json!({ "status": "sent", "name": item.name, "size": item.size }))
}

/// El error de `send_file`: lo que no se puede mandar, 400; no poder guardar la copia, 500.
fn send_fail(e: outbox::SendError) -> Fail {
    let code = match e {
        outbox::SendError::Io(_) => StatusCode::INTERNAL_SERVER_ERROR,
        _ => StatusCode::BAD_REQUEST,
    };
    fail(code, e.to_string())
}

/// `GET /api/v1/sessions/{id}/items`: los envíos de la sesión (también los caducados).
pub async fn api_items(State(st): State<Arc<AppState>>, UrlPath(id): UrlPath<String>) -> Response {
    let s = match server::find_session(&st, &id).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    Json(json!({ "items": st.outbox.list(&s.id) })).into_response()
}

/// `GET /api/v1/sessions/{id}/items/{item}`: el texto o el fichero, para copiarlo, abrirlo o
/// guardarlo. Siempre como adjunto y nunca como algo que el navegador ejecute (ver
/// `outbox::content_type`).
pub async fn api_item(
    State(st): State<Arc<AppState>>,
    UrlPath((id, item)): UrlPath<(String, String)>,
) -> Response {
    let s = match server::find_session(&st, &id).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    let Some((it, path)) = st.outbox.open(&s.id, &item) else {
        let listed = st.outbox.list(&s.id).into_iter().any(|i| i.id == item);
        return if listed {
            error(StatusCode::GONE, "ese envío ha caducado")
        } else {
            error(StatusCode::NOT_FOUND, "no existe ese envío")
        };
    };
    let bytes = match tokio::fs::read(&path).await {
        Ok(b) => b,
        Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };
    let name = it.name.clone().unwrap_or_else(|| format!("{}.txt", it.id));
    (
        [
            (header::CONTENT_TYPE, item_type(&it).to_owned()),
            (header::CONTENT_DISPOSITION, outbox::disposition(&name)),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff".to_owned()),
            (header::CACHE_CONTROL, "no-store".to_owned()),
        ],
        bytes,
    )
        .into_response()
}

/// Con qué tipo se sirve un envío: un texto, como texto; un fichero, según su nombre.
fn item_type(it: &outbox::Item) -> &'static str {
    match (it.kind, &it.name) {
        (NoteKind::File, Some(name)) => outbox::content_type(name),
        (NoteKind::File, None) => "application/octet-stream",
        _ => "text/plain; charset=utf-8",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn send_file_paths_are_relative_to_the_session() {
        let cwd = Path::new("/w/proyecto");
        assert_eq!(
            resolve(cwd, "informe.pdf"),
            Path::new("/w/proyecto/informe.pdf")
        );
        assert_eq!(resolve(cwd, "/tmp/x.png"), Path::new("/tmp/x.png"));
    }

    #[test]
    fn items_are_never_served_as_something_that_runs() {
        let it = |kind, name: Option<&str>| outbox::Item {
            id: "it1".into(),
            kind,
            title: "t".into(),
            name: name.map(str::to_owned),
            size: 1,
            at: 1,
            expired: false,
        };
        assert_eq!(
            item_type(&it(NoteKind::Clipboard, None)),
            "text/plain; charset=utf-8"
        );
        assert_eq!(
            item_type(&it(NoteKind::File, Some("a.pdf"))),
            "application/pdf"
        );
        for risky in ["a.html", "a.svg", "a.xhtml", "a.htm"] {
            assert_eq!(
                item_type(&it(NoteKind::File, Some(risky))),
                "application/octet-stream",
                "{risky}"
            );
        }
    }

    /// Cada error de `send_file` sale con su código y traducido (si no, al modelo le llegaría en
    /// español).
    #[test]
    fn send_file_errors_have_their_code_and_english() {
        use outbox::SendError;
        let p = || PathBuf::from("/w/a.bin");
        let en = |e: SendError| {
            let f = send_fail(e);
            (f.0, crate::i18n::english(&f.1))
        };
        let bad = StatusCode::BAD_REQUEST;
        assert_eq!(
            en(SendError::NotFound(p())),
            (bad, "file /w/a.bin does not exist".into())
        );
        assert_eq!(
            en(SendError::Unreadable(
                p(),
                std::io::Error::other("Permission denied")
            )),
            (bad, "cannot read /w/a.bin: Permission denied".into())
        );
        assert_eq!(
            en(SendError::IsDir(p())),
            (bad, "/w/a.bin is a folder".into())
        );
        assert_eq!(
            en(SendError::NotAFile(p())),
            (bad, "/w/a.bin is not a regular file".into())
        );
        assert_eq!(
            en(SendError::Secret(p())),
            (bad, "/w/a.bin is in nido's secrets folder: not sent".into())
        );
        assert_eq!(
            en(SendError::TooBig(p(), 70 * 1024 * 1024)),
            (bad, "/w/a.bin is over 64 MB (70.0 MB)".into())
        );
        assert_eq!(
            en(SendError::Io(std::io::Error::other("disco lleno"))),
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "could not save the copy: disco lleno".into()
            )
        );
        for (es, en) in [
            ("el texto está vacío", "the text is empty"),
            ("el texto pasa de 64 KB", "the text is longer than 64 KB"),
            ("ese envío ha caducado", "that item has expired"),
            ("no existe ese envío", "no such item"),
        ] {
            assert_eq!(crate::i18n::english(es), en);
        }
    }
}
