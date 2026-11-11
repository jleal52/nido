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
use axum::body::Bytes;
use axum::extract::{Path as UrlPath, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::mcp_api::{Fail, Reply, args_of, fail};
use crate::outbox::{self, MAX_TEXT};
use crate::registry::{NoteKind, Session, now_ms};
use crate::secrets::{self, Outcome, Remember};
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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SecretArgs {
    name: String,
    reason: String,
    #[serde(default)]
    force: bool,
}

/// Lo que devuelve `secret` según cómo acabó la pregunta.
fn outcome_reply(o: Outcome, session: &str, name: &str) -> Reply {
    match o {
        Outcome::Saved(paths) => {
            paths
                .get(session)
                .map(|p| json!({ "path": p }))
                .ok_or_else(|| {
                    fail(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "no se pudo guardar el secreto: sin ruta",
                    )
                })
        }
        Outcome::Failed(e) => Err(fail(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("no se pudo guardar el secreto: {e}"),
        )),
        Outcome::Rejected => Err(fail(
            StatusCode::FORBIDDEN,
            format!("el usuario no quiso dar «{name}»"),
        )),
        Outcome::Expired => Err(fail(
            StatusCode::REQUEST_TIMEOUT,
            "nadie contestó en 15 minutos",
        )),
        Outcome::Closed => Err(fail(
            StatusCode::GONE,
            "la sesión se cerró mientras esperaba",
        )),
    }
}

/// `secret`: un recordado vigente, al momento; si no, el diálogo en la web y la app, y espera
/// (hasta 15 min) a que el usuario conteste. Dos sesiones que piden el mismo nombre comparten
/// diálogo; una sesión, una pregunta.
pub(crate) async fn tool_secret(st: &Arc<AppState>, caller: &Session, args: Value) -> Reply {
    let a: SecretArgs = args_of(args)?;
    if !secrets::valid_name(&a.name) {
        return Err(fail(
            StatusCode::BAD_REQUEST,
            format!(
                "nombre de secreto no válido: {:?} (letras, números, «.», «_» o «-»; hasta 64)",
                a.name
            ),
        ));
    }
    let Some(reason) = server::alert_text(&a.reason) else {
        return Err(fail(StatusCode::BAD_REQUEST, "falta el motivo"));
    };
    let now = now_ms();
    if !a.force
        && let Some(path) = st.secrets.remembered(&a.name, now)
    {
        // Sin diálogo, pero se apunta quién lo pidió.
        let _ = st.secrets.touch(&a.name, &caller.name, now);
        return Ok(json!({ "path": path }));
    }
    if !st.has_devices() {
        return Err(fail(
            StatusCode::CONFLICT,
            "no hay dónde preguntar: ni teléfono ni web emparejados con nido",
        ));
    }
    let (mut waiter, new) = st
        .asks
        .join_forced(&a.name, &reason, &caller.id, now, a.force)
        .map_err(|_| fail(StatusCode::CONFLICT, "esta sesión ya espera otro secreto"))?;
    if new {
        st.secret_push(&caller.id, &a.name).await;
    } else {
        st.refresh.notify_one();
    }
    let (outcome, unanswered) = waiter.wait().await;
    drop(waiter);
    st.refresh.notify_one();
    for id in &unanswered {
        let text = format!("{}: sin contestar", a.name);
        let _ = st
            .session_note(id, Some(NoteKind::Secret), &text, None)
            .await;
    }
    outcome_reply(outcome, &caller.id, &a.name)
}

/// Lo que manda la web o la app al contestar. Sin `Debug`: lleva el secreto.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SecretAnswer {
    ask: String,
    #[serde(default)]
    value: Option<String>,
    #[serde(default)]
    remember: Option<Remember>,
    #[serde(default)]
    reject: bool,
}

/// Lee y comprueba una respuesta. Los errores son textos fijos: ni el de `serde` (que podría
/// citar el valor) ni nada del valor.
fn read_answer(body: &[u8]) -> Result<SecretAnswer, &'static str> {
    let Ok(req) = serde_json::from_slice::<SecretAnswer>(body) else {
        return Err("petición no válida");
    };
    if !req.reject {
        let value = req.value.as_deref().unwrap_or_default();
        if value.trim().is_empty() {
            return Err("el secreto está vacío");
        }
        if value.len() > secrets::MAX_VALUE {
            return Err("el secreto pasa de 16 KB");
        }
    }
    Ok(req)
}

/// `POST /api/v1/sessions/{id}/secret` `{ask, value, remember}` o `{ask, reject: true}`: la
/// respuesta del usuario a la pregunta `ask` que espera la sesión `id`. La primera vale; las
/// demás, 404 (la web y la app cierran el diálogo).
pub async fn api_secret_answer(
    State(st): State<Arc<AppState>>,
    UrlPath(id): UrlPath<String>,
    body: Bytes,
) -> Response {
    let req = match read_answer(&body) {
        Ok(r) => r,
        Err(msg) => return error(StatusCode::BAD_REQUEST, msg),
    };
    let s = match server::find_session(&st, &id).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    let Some(taken) = st.asks.take(&req.ask, &s.id) else {
        return error(StatusCode::NOT_FOUND, "ese secreto ya no espera respuesta");
    };
    st.refresh.notify_one();
    if req.reject {
        taken.finish(Outcome::Rejected);
        return Json(json!({ "ok": true })).into_response();
    }
    let by = {
        let reg = st.registry.lock().await;
        let names: Vec<String> = taken
            .sessions
            .iter()
            .filter_map(|id| reg.get(id).map(|s| s.name.clone()))
            .collect();
        names.join(", ")
    };
    let value = req.value.unwrap_or_default();
    let remember = req.remember.unwrap_or(Remember::No);
    let (name, sessions, forced) = (taken.name.clone(), taken.sessions.clone(), taken.forced);
    let store_st = st.clone();
    let saved = tokio::task::spawn_blocking(move || {
        store_st.secrets.save_answer(
            &name,
            &sessions,
            value.as_bytes(),
            remember,
            forced,
            &by,
            now_ms(),
        )
    })
    .await;
    match saved {
        Ok(Ok(paths)) => {
            taken.finish(Outcome::Saved(paths));
            Json(json!({ "ok": true })).into_response()
        }
        Ok(Err(e)) => {
            taken.finish(Outcome::Failed(e.to_string()));
            error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("no se pudo guardar el secreto: {e}"),
            )
        }
        Err(e) => {
            taken.finish(Outcome::Failed(e.to_string()));
            error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
        }
    }
}

/// `GET /api/v1/secrets`: los secretos recordados de este equipo, sin su valor.
pub async fn api_secrets(State(st): State<Arc<AppState>>) -> Response {
    Json(json!({ "secrets": st.secrets.list(now_ms()) })).into_response()
}

/// `DELETE /api/v1/secrets/{name}`: olvida uno (la próxima vez se pregunta).
pub async fn api_forget_secret(
    State(st): State<Arc<AppState>>,
    UrlPath(name): UrlPath<String>,
) -> Response {
    if !secrets::valid_name(&name) {
        return error(
            StatusCode::BAD_REQUEST,
            format!(
                "nombre de secreto no válido: {name:?} (letras, números, «.», «_» o «-»; hasta 64)"
            ),
        );
    }
    match st.secrets.forget(&name) {
        Ok(true) => Json(json!({ "ok": true })).into_response(),
        Ok(false) => error(
            StatusCode::NOT_FOUND,
            format!("no hay ningún secreto recordado {name:?}"),
        ),
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_outcome_is_a_path_or_an_error() {
        use crate::secrets::Outcome;
        let p = |o| outcome_reply(o, "aaaa1111", "tok").map_err(|f| (f.0, f.1));
        let saved: std::collections::HashMap<_, _> =
            [("aaaa1111".to_owned(), PathBuf::from("/s/tok"))].into();
        assert_eq!(p(Outcome::Saved(saved)).unwrap(), json!({"path": "/s/tok"}));
        assert_eq!(
            p(Outcome::Rejected).unwrap_err(),
            (
                StatusCode::FORBIDDEN,
                "el usuario no quiso dar «tok»".to_owned()
            )
        );
        assert_eq!(
            p(Outcome::Expired).unwrap_err().0,
            StatusCode::REQUEST_TIMEOUT
        );
        assert_eq!(p(Outcome::Closed).unwrap_err().0, StatusCode::GONE);
        assert_eq!(
            p(Outcome::Failed("disco lleno".into())).unwrap_err().1,
            "no se pudo guardar el secreto: disco lleno"
        );
        // Y en inglés, para el modelo.
        for (es, en) in [
            (
                "el usuario no quiso dar «tok»",
                "the user declined to give “tok”",
            ),
            (
                "nadie contestó en 15 minutos",
                "no answer within 15 minutes",
            ),
            (
                "la sesión se cerró mientras esperaba",
                "the session was closed while waiting",
            ),
            (
                "no se pudo guardar el secreto: x",
                "could not save the secret: x",
            ),
            (
                "no hay dónde preguntar: ni teléfono ni web emparejados con nido",
                "nowhere to ask: no phone or browser is paired with nido",
            ),
            (
                "esta sesión ya espera otro secreto",
                "this session is already waiting for another secret",
            ),
            ("falta el motivo", "the reason is missing"),
            (
                "nombre de secreto no válido: \"a/b\" (letras, números, «.», «_» o «-»; hasta 64)",
                "invalid secret name: \"a/b\" (letters, digits, “.”, “_” or “-”; up to 64)",
            ),
        ] {
            assert_eq!(crate::i18n::english(es), en);
        }
    }

    /// Lo que contesta el usuario: sus errores son textos fijos, nunca con nada del valor.
    #[test]
    fn answer_errors_never_carry_the_value() {
        let err = |body: &str| read_answer(body.as_bytes()).err();
        let secret = "s3cr3t-de-prueba";
        for (body, want) in [
            (
                format!(r#"{{"ask":"q1","value":"{secret}","otro":1}}"#),
                "petición no válida",
            ),
            (
                format!(r#"{{"ask":"q1","value":"{secret}","remember":"1y"}}"#),
                "petición no válida",
            ),
            (
                format!(r#"{{"ask":"q1","value":"{secret}""#),
                "petición no válida",
            ),
            (
                r#"{"ask":"q1","value":"  "}"#.to_owned(),
                "el secreto está vacío",
            ),
            (r#"{"ask":"q1"}"#.to_owned(), "el secreto está vacío"),
            (
                format!(
                    r#"{{"ask":"q1","value":"{}"}}"#,
                    "x".repeat(crate::secrets::MAX_VALUE + 1)
                ),
                "el secreto pasa de 16 KB",
            ),
        ] {
            assert_eq!(err(&body), Some(want), "{body:.60}");
        }
        let ok = read_answer(
            format!(
                r#"{{"ask":"q1","value":"{}","remember":"7d"}}"#,
                "x".repeat(crate::secrets::MAX_VALUE)
            )
            .as_bytes(),
        )
        .ok()
        .unwrap();
        assert_eq!(
            (ok.ask.as_str(), ok.remember, ok.reject),
            ("q1", Some(Remember::SevenDays), false)
        );
        // Rechazar no lleva valor.
        assert!(read_answer(br#"{"ask":"q1","reject":true}"#).is_ok());
        for (es, en) in [
            ("petición no válida", "invalid request"),
            ("el secreto está vacío", "the secret is empty"),
            ("el secreto pasa de 16 KB", "the secret is over 16 KB"),
            (
                "ese secreto ya no espera respuesta",
                "that secret is no longer waiting for an answer",
            ),
        ] {
            assert_eq!(crate::i18n::english(es), en);
        }
    }

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
