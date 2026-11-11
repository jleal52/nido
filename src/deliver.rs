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
use axum::body::{Body, Bytes};
use axum::extract::{Path as UrlPath, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::AsyncReadExt;

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
    text: Option<String>,
    /// En vez de `text`: un fichero (absoluto, o relativo a la carpeta de la sesión).
    path: Option<String>,
    title: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SendFileArgs {
    path: String,
    title: Option<String>,
}

/// `clipboard`: un texto para que el usuario lo copie con un botón; el texto, o un fichero
/// (`path`) de texto, con las comprobaciones de `send_file`.
pub(crate) async fn tool_clipboard(st: &Arc<AppState>, caller: &Session, args: Value) -> Reply {
    let a: ClipboardArgs = args_of(args)?;
    let text = match (a.text, a.path) {
        (Some(t), None) => t,
        (None, Some(p)) => {
            let path = resolve(&caller.cwd, &p);
            let st = st.clone();
            tokio::task::spawn_blocking(move || st.outbox.read_text(&path))
                .await
                .map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
                .map_err(send_fail)?
        }
        _ => {
            return Err(fail(
                StatusCode::BAD_REQUEST,
                "hace falta text o path (uno de los dos)",
            ));
        }
    };
    if text.trim().is_empty() {
        return Err(fail(StatusCode::BAD_REQUEST, "el texto está vacío"));
    }
    if text.len() > MAX_TEXT {
        return Err(fail(
            StatusCode::BAD_REQUEST,
            format!("el texto pasa de {} KB", MAX_TEXT / 1024),
        ));
    }
    let item = {
        let (st, id) = (st.clone(), caller.id.clone());
        tokio::task::spawn_blocking(move || {
            st.outbox.add_text(&id, &text, a.title.as_deref(), now_ms())
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
    forget_if_gone(st, std::slice::from_ref(&caller.id)).await;
    st.session_note(
        &caller.id,
        Some(NoteKind::Clipboard),
        &item.title,
        Some(&item.id),
    )
    .await
    .map_err(Fail::from)?;
    Ok(json!({ "status": "sent", "size": item.size }))
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
    forget_if_gone(st, std::slice::from_ref(&caller.id)).await;
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

/// Lo de las sesiones `ids` que se borraron mientras se guardaba algo suyo (secretos y envíos),
/// fuera. `delete_session` las quita del registro antes de borrar lo suyo: o lo borra él o lo ve
/// aquí.
async fn forget_if_gone(st: &Arc<AppState>, ids: &[String]) {
    let gone: Vec<String> = {
        let reg = st.registry.lock().await;
        ids.iter()
            .filter(|id| reg.get(id).is_none())
            .cloned()
            .collect()
    };
    if gone.is_empty() {
        return;
    }
    let st = st.clone();
    let _ = tokio::task::spawn_blocking(move || {
        for id in gone {
            st.secrets.forget_session(&id);
            st.outbox.forget(&id);
        }
    })
    .await;
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
    // Lee el índice del disco: fuera del hilo de tokio.
    let listed = tokio::task::spawn_blocking(move || st.outbox.list(&s.id)).await;
    match listed {
        Ok(items) => Json(json!({ "items": items })).into_response(),
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
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
    // El índice y la copia, en el disco: fuera del hilo de tokio. `Err(true)`: está en la
    // lista, pero ya no.
    let found = tokio::task::spawn_blocking(move || match st.outbox.open(&s.id, &item) {
        Some(found) => Ok(found),
        None => Err(st.outbox.list(&s.id).into_iter().any(|i| i.id == item)),
    })
    .await;
    match found {
        Ok(Ok((it, path))) => serve_item(&it, &path).await,
        Ok(Err(true)) => error(StatusCode::GONE, "ese envío ha caducado"),
        Ok(Err(false)) => error(StatusCode::NOT_FOUND, "no existe ese envío"),
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

/// Lo que se lee de una vez al mandar una copia.
const CHUNK: usize = 64 * 1024;

/// Manda la copia `path` del envío `it` a trozos (puede tener 64 MB: no entera en memoria).
async fn serve_item(it: &outbox::Item, path: &Path) -> Response {
    let file = match tokio::fs::File::open(path).await {
        Ok(f) => f,
        // Se borró entre mirar y abrir (caducó o pasó el tope).
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return error(StatusCode::GONE, "ese envío ha caducado");
        }
        Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };
    let len = file.metadata().await.ok().map(|m| m.len());
    // Hasta el final o el primer error (que corta la respuesta).
    let chunks = futures_util::stream::unfold(Some(file), |file| async move {
        let mut file = file?;
        let mut buf = vec![0u8; CHUNK];
        match file.read(&mut buf).await {
            Ok(0) => None,
            Ok(n) => {
                buf.truncate(n);
                Some((Ok(Bytes::from(buf)), Some(file)))
            }
            Err(e) => Some((Err::<Bytes, _>(e), None)),
        }
    });
    let name = it.name.clone().unwrap_or_else(|| format!("{}.txt", it.id));
    let mut r = (
        [
            (header::CONTENT_TYPE, item_type(it).to_owned()),
            (header::CONTENT_DISPOSITION, outbox::disposition(&name)),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff".to_owned()),
            (header::CACHE_CONTROL, "no-store".to_owned()),
        ],
        Body::from_stream(chunks),
    )
        .into_response();
    if let Some(n) = len {
        r.headers_mut().insert(header::CONTENT_LENGTH, n.into());
    }
    r
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

/// El motivo de `secret`, que el usuario ve en el diálogo: sin caracteres de control ni de
/// formato (los que no se ven o dan la vuelta al texto), como los títulos, pero con sus saltos
/// de línea. `None` si queda vacío.
fn clean_reason(s: &str) -> Option<String> {
    let s: String = s.chars().filter(|&c| !outbox::is_format(c)).collect();
    server::alert_text(&s)
}

/// Lo que devuelve `secret` según cómo acabó la pregunta.
fn outcome_reply(o: Outcome, session: &str, name: &str) -> Reply {
    match o {
        // Sin ruta para ella: ya no la esperaba (como cerrada).
        Outcome::Saved(paths) => paths
            .get(session)
            .map(|p| json!({ "path": p }))
            .ok_or_else(|| fail(StatusCode::GONE, "la sesión se cerró mientras esperaba")),
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
    let Some(reason) = clean_reason(&a.reason) else {
        return Err(fail(StatusCode::BAD_REQUEST, "falta el motivo"));
    };
    let now = now_ms();
    if !a.force {
        // Con el cerrojo de los secretos y escribiendo en el disco: fuera del hilo de tokio.
        let (st, name, by) = (st.clone(), a.name.clone(), caller.name.clone());
        let found = tokio::task::spawn_blocking(move || {
            let path = st.secrets.remembered(&name, now)?;
            // Sin diálogo, pero se apunta quién lo pidió.
            let _ = st.secrets.touch(&name, &by, now);
            Some(path)
        })
        .await
        .map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        if let Some(path) = found {
            return Ok(json!({ "path": path, "nido": nido_cmd() }));
        }
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
    let mut reply = outcome_reply(outcome, &caller.id, &a.name)?;
    reply["nido"] = nido_cmd().into();
    Ok(reply)
}

/// Cómo llamar a nido desde la sesión (para `nido secrets run`): su ruta completa, por si no
/// está en el `PATH` de la sesión.
fn nido_cmd() -> String {
    crate::launch::nido_exe().map_or_else(|_| "nido".to_owned(), |p| p.display().to_string())
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

/// Lo más que se lee del cuerpo de una respuesta (el secreto va en JSON, con sus escapes): no
/// los 2 MB de siempre.
pub const MAX_ANSWER: usize = 128 * 1024;

/// `POST /api/v1/sessions/{id}/secret` `{ask, value, remember}` o `{ask, reject: true}`: la
/// respuesta del usuario a la pregunta `ask` que espera la sesión `id`. La primera vale; las
/// demás, 404 (la web y la app cierran el diálogo).
///
/// Solo con el token de un dispositivo emparejado (un teléfono o una web, una persona): no con
/// el principal, que tiene el `nido mcp` de cada sesión. Si no, una sesión engañada podría
/// contestar la pregunta de otra o dejar un valor recordado para todo el equipo.
pub async fn api_secret_answer(
    State(st): State<Arc<AppState>>,
    UrlPath(id): UrlPath<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let device = crate::auth::bearer(&headers)
        .is_some_and(|t| st.phones.lock().expect("teléfonos").token_ok(t));
    if !device {
        return error(
            StatusCode::FORBIDDEN,
            "solo un teléfono o una web emparejados pueden contestar un secreto",
        );
    }
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
    let value = req.value.unwrap_or_default();
    let remember = req.remember.unwrap_or(Remember::No);
    // Se guarda y se avisa en una tarea aparte: si se corta esta petición (el dispositivo se va),
    // se guarda igual y la sesión que espera recibe su ruta.
    let saving = tokio::spawn(save_answer(st.clone(), taken, value, remember));
    match saving.await {
        Ok(Ok(())) => Json(json!({ "ok": true })).into_response(),
        Ok(Err(e)) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("no se pudo guardar el secreto: {e}"),
        ),
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

/// Guarda lo que contestó el usuario y avisa a las sesiones que esperan (`taken`).
async fn save_answer(
    st: Arc<AppState>,
    taken: secrets::Taken,
    value: String,
    remember: Remember,
) -> Result<(), String> {
    let by = {
        let reg = st.registry.lock().await;
        let names: Vec<String> = taken
            .sessions
            .iter()
            .filter_map(|id| reg.get(id).map(|s| s.name.clone()))
            .collect();
        names.join(", ")
    };
    let (name, sessions, forced) = (taken.name.clone(), taken.sessions.clone(), taken.forced);
    let store = st.secrets.clone();
    let saved = tokio::task::spawn_blocking(move || {
        store.save_answer(
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
            forget_if_gone(&st, &taken.sessions).await;
            taken.finish(Outcome::Saved(paths));
            Ok(())
        }
        Ok(Err(e)) => {
            let e = e.to_string();
            taken.finish(Outcome::Failed(e.clone()));
            Err(e)
        }
        Err(e) => {
            let e = e.to_string();
            taken.finish(Outcome::Failed(e.clone()));
            Err(e)
        }
    }
}

/// `GET /api/v1/secrets`: los secretos recordados de este equipo, sin su valor.
pub async fn api_secrets(State(st): State<Arc<AppState>>) -> Response {
    match tokio::task::spawn_blocking(move || st.secrets.list(now_ms())).await {
        Ok(list) => Json(json!({ "secrets": list })).into_response(),
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
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
    let (store, n) = (st.secrets.clone(), name.clone());
    match tokio::task::spawn_blocking(move || store.forget(&n)).await {
        Ok(Ok(true)) => Json(json!({ "ok": true })).into_response(),
        Ok(Ok(false)) => error(
            StatusCode::NOT_FOUND,
            format!("no hay ningún secreto recordado {name:?}"),
        ),
        Ok(Err(e)) => error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Un nido en `dir`, sin HTTP, con una sesión y un teléfono emparejado (su token).
    async fn nido_in(dir: &Path) -> (Arc<AppState>, String, String) {
        let paths = crate::launch::Paths {
            runtime: dir.join("run"),
            data: dir.join("data"),
            state: dir.join("state"),
            config: dir.join("config"),
            home: dir.to_owned(),
            server: None,
        };
        let st = AppState::new(paths, crate::launch::Launcher::Detached, None, true).unwrap();
        let session = Session {
            id: crate::registry::new_id(),
            name: "api".into(),
            cwd: dir.to_owned(),
            argv: vec!["true".into()],
            env: vec![],
            autostart: false,
            created: 0,
            claude: None,
            folder: None,
            finished: 0,
            stopped: 0,
            seen: 0,
            opened_by: None,
            alerts: vec![],
        };
        let id = st.registry.lock().await.add(session).unwrap().id;
        let token = st
            .phones
            .lock()
            .unwrap()
            .add(&[7u8; 32], "teléfono de prueba", None)
            .unwrap()
            .token;
        (st, id, token)
    }

    /// Si la sesión se borra mientras se guarda lo que contestó el usuario, no queda nada suyo
    /// (`delete_session` ya pasó y no lo volvería a mirar).
    #[tokio::test(flavor = "multi_thread")]
    async fn an_answer_for_a_session_deleted_meanwhile_leaves_nothing() {
        let _limit = crate::watchdog::limit(std::time::Duration::from_secs(60));
        let tmp = tempfile::tempdir().unwrap();
        let (st, id, _) = nido_in(tmp.path()).await;
        let (_waiter, _) = st.asks.join("tok", "x", &id, 1).unwrap();
        let ask = st.asks.view_for(&id).unwrap().id;
        let taken = st.asks.take(&ask, &id).unwrap();
        st.registry.lock().await.remove(&id).unwrap();
        save_answer(st.clone(), taken, "v".into(), Remember::No)
            .await
            .unwrap();
        assert!(!st.secrets.dir().join("sesiones").join(&id).exists());
    }

    /// Lo mismo con un envío: si la sesión ya no está, su copia tampoco.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_item_for_a_session_deleted_meanwhile_leaves_nothing() {
        let _limit = crate::watchdog::limit(std::time::Duration::from_secs(60));
        let tmp = tempfile::tempdir().unwrap();
        let (st, id, _) = nido_in(tmp.path()).await;
        let caller = st.registry.lock().await.remove(&id).unwrap();
        let _ = tool_clipboard(&st, &caller, json!({"text": "hola"})).await;
        std::fs::write(tmp.path().join("f.txt"), "hola").unwrap();
        let _ = tool_send_file(&st, &caller, json!({"path": "f.txt"})).await;
        assert!(!tmp.path().join("state/outbox").join(&id).exists());
    }

    /// `clipboard` con `path`: el fichero (relativo a la carpeta de la sesión, o absoluto), con
    /// las comprobaciones de `send_file` y las del texto; `text` o `path`, uno de los dos.
    #[tokio::test(flavor = "multi_thread")]
    async fn clipboard_takes_the_text_or_a_text_file() {
        let _limit = crate::watchdog::limit(std::time::Duration::from_secs(60));
        let tmp = tempfile::tempdir().unwrap();
        let (st, id, _) = nido_in(tmp.path()).await;
        let caller = st.registry.lock().await.get(&id).cloned().unwrap();
        let clip = |args: Value| {
            let (st, caller) = (st.clone(), caller.clone());
            async move {
                tool_clipboard(&st, &caller, args)
                    .await
                    .map_err(|f| (f.0, f.1))
            }
        };
        let last = || {
            let item = st.outbox.list(&id).into_iter().last().unwrap();
            let (item, path) = st.outbox.open(&id, &item.id).unwrap();
            (item.title, std::fs::read_to_string(path).unwrap())
        };
        std::fs::create_dir_all(tmp.path().join("docs")).unwrap();
        std::fs::write(
            tmp.path().join("docs/clave.pub"),
            "ssh-ed25519 AAAA prueba\n",
        )
        .unwrap();

        // Relativo a la carpeta de la sesión.
        let r = clip(json!({"path": "docs/clave.pub"})).await.unwrap();
        assert_eq!(r, json!({"status": "sent", "size": 24}));
        assert_eq!(last().1, "ssh-ed25519 AAAA prueba\n");
        // Absoluto, con título.
        let abs = tmp.path().join("docs/clave.pub");
        let r = clip(json!({"path": abs, "title": "la clave"}))
            .await
            .unwrap();
        assert_eq!(r["status"], "sent");
        assert_eq!(
            last(),
            (
                "la clave".to_owned(),
                "ssh-ed25519 AAAA prueba\n".to_owned()
            )
        );
        // El texto, como siempre.
        let r = clip(json!({"text": "git push"})).await.unwrap();
        assert_eq!(r, json!({"status": "sent", "size": 8}));

        // Uno de los dos.
        let both = json!({"text": "x", "path": "docs/clave.pub"});
        for args in [both, json!({}), json!({"title": "t"})] {
            assert_eq!(
                clip(args).await.unwrap_err(),
                (
                    StatusCode::BAD_REQUEST,
                    "hace falta text o path (uno de los dos)".to_owned()
                )
            );
        }
        // Binario, demasiado grande, vacío, inexistente, una carpeta: no.
        let bad = |name: &str, bytes: &[u8]| {
            std::fs::write(tmp.path().join(name), bytes).unwrap();
            json!({ "path": name })
        };
        let e = clip(bad("foto.png", b"\x89PNG\r\n\x1a\n\0\0"))
            .await
            .unwrap_err();
        assert_eq!(e.0, StatusCode::BAD_REQUEST);
        assert!(
            e.1.ends_with("foto.png no es texto (UTF-8): para mandarlo, usa send_file"),
            "{e:?}"
        );
        let e = clip(bad("latin1.txt", b"camion \xe1rbol"))
            .await
            .unwrap_err();
        assert!(
            e.1.ends_with("latin1.txt no es texto (UTF-8): para mandarlo, usa send_file"),
            "{e:?}"
        );
        let big = vec![b'a'; MAX_TEXT + 1];
        let e = clip(bad("grande.txt", &big)).await.unwrap_err();
        assert!(e.1.ends_with("grande.txt pasa de 64 KB"), "{e:?}");
        // Justo 64 KB, sí.
        let r = clip(bad("justo.txt", &big[..MAX_TEXT])).await.unwrap();
        assert_eq!(r["size"], MAX_TEXT);
        let e = clip(bad("vacio.txt", b" \n")).await.unwrap_err();
        assert_eq!(e.1, "el texto está vacío");
        let e = clip(json!({"path": "no-existe.txt"})).await.unwrap_err();
        assert!(e.1.starts_with("no existe el fichero "), "{e:?}");
        let e = clip(json!({"path": "docs"})).await.unwrap_err();
        assert!(e.1.ends_with("docs es una carpeta"), "{e:?}");
        // Nada de la carpeta de secretos.
        let secret = st.secrets.save_session(&id, "tok", b"secreto").unwrap();
        let e = clip(json!({ "path": secret })).await.unwrap_err();
        assert!(e.1.contains("carpeta de secretos"), "{e:?}");
        // Lo que no se mandó no dejó nada: los tres de antes y el de 64 KB.
        assert_eq!(st.outbox.list(&id).len(), 4);
    }

    /// Si se corta la petición de quien contesta mientras se guarda, se guarda igual y la sesión
    /// que espera recibe su ruta (no «se cerró»).
    #[tokio::test(flavor = "multi_thread")]
    async fn an_answer_is_saved_and_told_even_if_its_request_goes() {
        let _limit = crate::watchdog::limit(std::time::Duration::from_secs(60));
        let tmp = tempfile::tempdir().unwrap();
        let (st, id, token) = nido_in(tmp.path()).await;
        let (mut waiter, _) = st.asks.join("tok", "x", &id, 1).unwrap();
        let ask = st.asks.view_for(&id).unwrap().id;
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            format!("Bearer {token}").parse().unwrap(),
        );
        let body = json!({"ask": ask, "value": "v"}).to_string();
        let answering = api_secret_answer(
            State(st.clone()),
            UrlPath(id.clone()),
            headers,
            Bytes::from(body),
        );
        // Una vuelta y fuera, como si el dispositivo se fuera mientras se guarda.
        let _ = tokio::time::timeout(std::time::Duration::ZERO, answering).await;
        match waiter.wait().await.0 {
            Outcome::Saved(paths) => {
                assert_eq!(std::fs::read_to_string(&paths[&id]).unwrap(), "v")
            }
            other => panic!("{other:?}"),
        }
    }

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
        // Guardado, pero sin ruta para ella (ya no la esperaba): como cerrada.
        assert_eq!(
            p(Outcome::Saved(Default::default())).unwrap_err(),
            (
                StatusCode::GONE,
                "la sesión se cerró mientras esperaba".to_owned()
            )
        );
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

    /// El motivo lo ve el usuario en el diálogo: sin caracteres invisibles ni que den la vuelta
    /// al texto, como los títulos, pero con sus saltos de línea.
    #[test]
    fn the_reason_has_no_invisible_characters() {
        assert_eq!(
            clean_reason("subir\u{202E}txt.exe\n\u{2066}línea 2\u{2069}"),
            Some("subirtxt.exe\nlínea 2".to_owned())
        );
        assert_eq!(clean_reason("  \u{200B}\u{FEFF} "), None);
        assert_eq!(clean_reason("\u{1b}[31mrojo"), Some("rojo".to_owned()));
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

    /// La copia se manda tal cual, a trozos; si ya no está al ir a leerla (caducó entre mirar y
    /// abrir), «caducado» y no un error de nido.
    #[tokio::test]
    async fn an_item_is_streamed_and_one_gone_meanwhile_is_expired() {
        let _limit = crate::watchdog::limit(std::time::Duration::from_secs(60));
        let tmp = tempfile::tempdir().unwrap();
        let it = outbox::Item {
            id: "it1".into(),
            kind: NoteKind::File,
            title: "t".into(),
            name: Some("a.pdf".into()),
            size: 0,
            at: 1,
            expired: false,
        };
        let path = tmp.path().join("it1");
        let data: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(&path, &data).unwrap();
        let r = serve_item(&it, &path).await;
        assert_eq!(r.status(), StatusCode::OK);
        assert_eq!(r.headers()[header::CONTENT_TYPE], "application/pdf");
        assert_eq!(r.headers()[header::CONTENT_LENGTH], "200000");
        let body = axum::body::to_bytes(r.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(body.as_ref(), data.as_slice());
        std::fs::remove_file(&path).unwrap();
        assert_eq!(serve_item(&it, &path).await.status(), StatusCode::GONE);
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
            en(SendError::TextTooBig(p())),
            (bad, "/w/a.bin is over 64 KB".into())
        );
        assert_eq!(
            en(SendError::NotText(p())),
            (
                bad,
                "/w/a.bin is not text (UTF-8): to send it, use send_file".into()
            )
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
