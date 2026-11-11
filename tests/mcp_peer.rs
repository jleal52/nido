//! El canal «peer» entre equipos (`MODE_PEER`) contra un nido de verdad, en un directorio
//! temporal y sin hub: el otro equipo es una identidad apuntada en el grupo que habla con
//! `e2e::serve` directamente. Las sesiones que abre `open` corren con una «shell» falsa que apunta
//! sus argumentos y se queda dormida (como en `mcp_api.rs`).

// Con sh, sockets Unix y señales: solo en Unix.
#![cfg(unix)]

use std::io::Write;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use nido::client::Api;
use nido::e2e::{self, Host, Identity};
use nido::launch::{Launcher, Paths};
use nido::registry::{ClaudeLink, Session};
use nido::server::{self, AppState};
use serde_json::{Value, json};

const TOKEN: &str = "0123456789abcdef0123456789abcdef";

/// Una herramienta por la API local, desde la sesión `caller` de este equipo.
async fn tool(addr: SocketAddr, caller: &str, name: &str, args: Value) -> (u16, Value) {
    let caller = caller.to_owned();
    let path = format!("/api/v1/mcp/{name}");
    tokio::task::spawn_blocking(move || {
        let api = Api {
            addr,
            token: Some(TOKEN.into()),
        };
        api.request_with(
            "POST",
            &path,
            Some(&args),
            &[("X-Nido-Session", caller.as_str())],
            Some(Duration::from_secs(30)),
        )
        .unwrap()
    })
    .await
    .unwrap()
}

/// Lo que contesta el nido a otro equipo (`from`) por el canal «peer»: `{ok}` o `{error}`, o
/// `None` si no contesta.
async fn ask(state: &Arc<AppState>, from: &Identity, method: &str, params: Value) -> Option<Value> {
    let (to_nido, nido_rx) = tokio::sync::mpsc::channel(4);
    let (nido_tx, mut from_nido) = tokio::sync::mpsc::channel(4);
    tokio::spawn(e2e::serve(state.clone(), nido_rx, nido_tx));
    let payload =
        json!({"method": method, "params": params, "at": nido::group::now_ms()}).to_string();
    let key = state.identity().public;
    let (dial, first) =
        e2e::Dial::start_mode(from, &key, None, e2e::MODE_PEER, payload.as_bytes()).unwrap();
    to_nido.send(first).await.unwrap();
    let reply = tokio::time::timeout(Duration::from_secs(20), from_nido.recv())
        .await
        .expect("a tiempo")?;
    Some(serde_json::from_slice(&dial.finish(&reply).unwrap()).unwrap())
}

/// Los parámetros con la sesión que llama desde el otro equipo.
fn with_caller(id: &str, mut params: Value) -> Value {
    params["caller"] = json!({"id": id, "name": "padre"});
    params
}

fn ok(r: Option<Value>) -> Value {
    let r = r.expect("contesta");
    assert!(r.get("error").is_none(), "{r}");
    r["ok"].clone()
}

fn err(r: Option<Value>) -> String {
    failed(r).1
}

/// El error y su código (el estado HTTP que tendría en la API local).
fn failed(r: Option<Value>) -> (u64, String) {
    let r = r.expect("contesta");
    let msg = r["error"].as_str().unwrap_or_else(|| panic!("{r}"));
    let code = r["code"]
        .as_u64()
        .unwrap_or_else(|| panic!("sin código: {r}"));
    (code, msg.to_owned())
}

/// La lista de `sessions` que ve otro equipo (`{sessions, truncated?}`).
fn listed(r: Option<Value>) -> Vec<Value> {
    ok(r)["sessions"].as_array().unwrap().clone()
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

fn append(path: &Path, lines: &[Value]) {
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap();
    for l in lines {
        writeln!(f, "{l}").unwrap();
    }
}

fn texts(read: &Value) -> Vec<String> {
    read["turns"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["text"].as_str().unwrap().to_owned())
        .collect()
}

async fn wait_file(path: &Path) -> bool {
    for _ in 0..100 {
        if std::fs::metadata(path).is_ok_and(|m| m.len() > 0) {
            tokio::time::sleep(Duration::from_millis(100)).await;
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}

#[tokio::test(flavor = "multi_thread")]
async fn peer_calls_end_to_end() {
    let _limit = nido::watchdog::limit(std::time::Duration::from_secs(120));
    let tmp = tempfile::tempdir().unwrap();
    let _cleanup = KillHolders(tmp.path().join("nido/run"));
    let bin = tmp.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let shell = bin.join("shell");
    std::fs::write(
        &shell,
        "#!/bin/sh\nprintf '%s\\0' \"$@\" > args.tmp && mv args.tmp args\nexec sleep 60\n",
    )
    .unwrap();
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

    // El grupo: el Mac (vivo), uno que ya salió y uno de fuera.
    let key =
        |name: &str| Identity::load_or_create(&tmp.path().join(format!("{name}.key"))).unwrap();
    let (mac, viejo, extra) = (key("mac"), key("viejo"), key("extra"));
    state
        .group
        .add_member("mac-id", &B64.encode(mac.public), "Mac");
    state
        .group
        .add_member("viejo-id", &B64.encode(viejo.public), "Viejo");
    assert!(state.group.remove_member("viejo-id"));

    // La sesión que llama aquí y otra de Claude, parada, con su conversación.
    let config = tmp.path().join("claude");
    let transcript_dir = config.join("projects").join("-tmp-x");
    std::fs::create_dir_all(&transcript_dir).unwrap();
    let transcript = transcript_dir.join("conv-1.jsonl");
    append(
        &transcript,
        &[
            json!({"type":"user","uuid":"u1","message":{"content":"hola"}}),
            json!({"type":"assistant","uuid":"a1","message":{"content":[{"type":"text","text":"Hecho."}]}}),
        ],
    );
    let link = ClaudeLink {
        session_id: "conv-1".into(),
        config_dir: Some(config.clone()),
        account: None,
        inside_shell: false,
    };
    let (local, other) = {
        let mut reg = state.registry.lock().await;
        let local = reg.add(session("padre", tmp.path(), None)).unwrap();
        let other = reg.add(session("otra", tmp.path(), Some(link))).unwrap();
        (local.id, other.id)
    };
    // La sesión del Mac que llama tiene el mismo id que la de aquí: solo la dirección las separa.
    let remote = local.clone();
    let remote_addr = format!("mac-id/{remote}");
    let p = |params: Value| with_caller(&remote, params);

    // Solo a los miembros vivos: a los demás, ni respuesta.
    assert_eq!(ask(&state, &viejo, "sessions", p(json!({}))).await, None);
    assert_eq!(ask(&state, &extra, "sessions", p(json!({}))).await, None);

    // Solo la lista cerrada de métodos; `alert` va siempre al nido de quien la manda.
    for m in ["alert", "inbox", "nada"] {
        let (code, e) = failed(ask(&state, &mac, m, p(json!({"text": "x"}))).await);
        assert_eq!(code, 400);
        assert!(
            e.contains("no se puede pedir desde otro equipo"),
            "{m}: {e}"
        );
    }
    // Quién llama: hace falta, y con un id como los de nido.
    let (code, e) = failed(ask(&state, &mac, "sessions", json!({})).await);
    assert_eq!(code, 400);
    assert!(e.contains("falta la sesión que llama"), "{e}");
    for bad in ["", "a/b", "../x", "x y", "ñ"] {
        let e = err(ask(&state, &mac, "sessions", with_caller(bad, json!({}))).await);
        assert!(e.contains("sesión que llama no válida"), "{bad:?}: {e}");
    }

    // `sessions`: la de Claude, sin abrir por nadie.
    let r = ok(ask(&state, &mac, "sessions", p(json!({}))).await);
    assert!(r.get("truncated").is_none(), "{r}");
    let list = listed(Some(json!({"ok": r})));
    assert_eq!(list.len(), 1, "{list:?}");
    assert_eq!(list[0]["session"], format!("Prueba/{other}"));
    assert_eq!(list[0]["mine"], false);

    // `read`: cada una lleva su posición, aunque tengan el mismo id.
    let (code, r) = tool(addr, &local, "read", json!({"session": other})).await;
    assert_eq!(code, 200, "{r}");
    assert_eq!(texts(&r), ["hola", "Hecho."]);
    let r = ok(ask(&state, &mac, "read", p(json!({"session": other}))).await);
    assert_eq!(texts(&r), ["hola", "Hecho."]);
    let r = ok(ask(
        &state,
        &mac,
        "read",
        p(json!({"session": format!("Prueba/{other}")})),
    )
    .await);
    assert_eq!(texts(&r), Vec::<String>::new());
    // Nada de pasar a un tercer equipo (ni volver al de quien llama).
    for s in [format!("Otro/{other}"), format!("mac-id/{other}")] {
        let e = failed(ask(&state, &mac, "read", p(json!({"session": s}))).await);
        assert_eq!(e, (400, "esa sesión no es de este equipo".into()));
    }
    assert_eq!(
        nido::i18n::english("esa sesión no es de este equipo"),
        "that session is not on this machine"
    );

    // `send`: el mensaje dice de dónde viene (la dirección canónica del Mac). `wait` no se
    // espera aquí: lo hace el nido de quien llama.
    let start = Instant::now();
    let r = ok(ask(
        &state,
        &mac,
        "send",
        p(json!({"session": other, "text": "¿qué tal?", "wait": 60})),
    )
    .await);
    assert!(start.elapsed() < Duration::from_secs(10));
    assert_eq!(r["status"], "queued", "{r}");
    assert!(r.get("read").is_none(), "{r}");
    let queued = state.inbox.pending(&other);
    let m = queued.last().unwrap();
    assert_eq!(m.from_id, remote_addr);
    assert_eq!(m.machine, "Mac");
    assert_eq!(m.from, "padre");
    assert_eq!(m.text, "¿qué tal?");
    // A sí misma no: su dirección no es de este equipo.
    let e = err(ask(
        &state,
        &mac,
        "send",
        p(json!({"session": remote_addr, "text": "x"})),
    )
    .await);
    assert_eq!(e, "esa sesión no es de este equipo");

    // `open` desde el Mac: abierta por su dirección, y es suya.
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let e = err(ask(
        &state,
        &mac,
        "open",
        p(json!({"machine": "Mac", "folder": work, "prompt": "di hola"})),
    )
    .await);
    assert_eq!(e, "«Mac» no es este equipo");
    let opened = ok(ask(
        &state,
        &mac,
        "open",
        p(json!({"machine": "Prueba", "folder": work, "prompt": "di hola"})),
    )
    .await);
    assert_eq!(opened["opened_by"], remote_addr.as_str(), "{opened}");
    assert_eq!(opened["mine"], true);
    let a = opened["id"].as_str().unwrap().to_owned();
    let reg_a = state.registry.lock().await.get(&a).cloned().unwrap();
    assert_eq!(reg_a.opened_by.as_deref(), Some(remote_addr.as_str()));
    assert!(wait_file(&work.join("args")).await, "la sesión no arrancó");
    let args = String::from_utf8_lossy(&std::fs::read(work.join("args")).unwrap()).into_owned();
    assert!(
        args.contains(&format!(
            "[nido] You were opened by the session \"padre\" on Mac ({remote_addr})."
        )),
        "{args}"
    );

    // Topes de `open` desde un mismo equipo: 10 en marcha y 50 apuntadas. En otra carpeta, para
    // no pisar los argumentos de la primera.
    let limited = tmp.path().join("topes");
    std::fs::create_dir_all(&limited).unwrap();
    let open_one = p(json!({"machine": "Prueba", "folder": limited, "prompt": "otra"}));
    let too_many = (
        409,
        "demasiadas sesiones abiertas desde ese equipo; cierra alguna".to_owned(),
    );
    assert_eq!(
        nido::i18n::english("demasiadas sesiones abiertas desde ese equipo; cierra alguna"),
        "too many sessions opened from that machine; close some"
    );
    // En marcha: con la primera, 10; la siguiente no.
    let mut running = vec![];
    for _ in 0..9 {
        let o = ok(ask(&state, &mac, "open", open_one.clone()).await);
        running.push(o["id"].as_str().unwrap().to_owned());
    }
    assert_eq!(
        failed(ask(&state, &mac, "open", open_one.clone()).await),
        too_many
    );
    // Las de aquí llevan su propia cuenta: las del Mac no las dejan fuera.
    let (code, local_one) = tool(
        addr,
        &local,
        "open",
        json!({"machine": "Prueba", "folder": limited, "prompt": "x"}),
    )
    .await;
    assert_eq!(code, 200, "{local_one}");
    // Cómo se apuntan las de aquí («<este equipo>/»), para rellenar luego.
    let here_prefix = {
        let reg = state.registry.lock().await;
        let by = reg
            .get(local_one["id"].as_str().unwrap())
            .unwrap()
            .opened_by
            .clone()
            .unwrap();
        format!("{}/", by.rsplit_once('/').unwrap().0)
    };
    let (code, _) = tool(addr, &local, "close", json!({"session": local_one["id"]})).await;
    assert_eq!(code, 200);
    // Cerrada una, vuelve a haber sitio.
    let gone = running.pop().unwrap();
    ok(ask(&state, &mac, "close", p(json!({"session": gone}))).await);
    let o = ok(ask(&state, &mac, "open", open_one.clone()).await);
    running.push(o["id"].as_str().unwrap().to_owned());
    for id in running.drain(..) {
        ok(ask(&state, &mac, "close", p(json!({"session": id}))).await);
    }
    // Paradas no cuentan para las 10 (las huérfanas no dejan fuera al equipo)…
    let mut filler: Vec<String> = {
        let mut reg = state.registry.lock().await;
        let mut ids = vec![];
        for i in 0..49 {
            let mut s = session(&format!("relleno-{i}"), tmp.path(), None);
            s.opened_by = Some(format!("mac-id/zz{i}"));
            ids.push(reg.add(s).unwrap().id);
        }
        // Las de otro equipo con un id que empieza igual no cuentan para nada.
        let mut s = session("de-otro", tmp.path(), None);
        s.opened_by = Some("mac-id-2/zz".into());
        ids.push(reg.add(s).unwrap().id);
        ids
    };
    // …pero sí para las 50 apuntadas: con la primera, 50.
    assert_eq!(
        failed(ask(&state, &mac, "open", open_one.clone()).await),
        too_many
    );
    state
        .registry
        .lock()
        .await
        .remove(&filler.remove(0))
        .unwrap();
    let o = ok(ask(&state, &mac, "open", open_one.clone()).await);
    ok(ask(&state, &mac, "close", p(json!({"session": o["id"]}))).await);
    {
        let mut reg = state.registry.lock().await;
        for id in &filler {
            reg.remove(id).unwrap();
        }
    }
    // Las de aquí también tienen tope: con 50 apuntadas desde este equipo, no se abre otra.
    let filler: Vec<String> = {
        let mut reg = state.registry.lock().await;
        (0..50)
            .map(|i| {
                let mut s = session(&format!("aqui-{i}"), tmp.path(), None);
                s.opened_by = Some(format!("{here_prefix}zz{i}"));
                reg.add(s).unwrap().id
            })
            .collect()
    };
    let (code, e) = tool(
        addr,
        &local,
        "open",
        json!({"machine": "Prueba", "folder": limited, "prompt": "x"}),
    )
    .await;
    assert_eq!(code, 409, "{e}");
    assert_eq!(
        e["error"], "too many sessions opened with open on this machine; close some",
        "{e}"
    );
    {
        let mut reg = state.registry.lock().await;
        for id in &filler {
            reg.remove(id).unwrap();
        }
    }
    // Con `wait`, tampoco espera (trabajando: sin Claude que se apunte).
    let start = Instant::now();
    let r = ok(ask(
        &state,
        &mac,
        "read",
        p(json!({"session": a, "wait": true, "timeout_s": 60})),
    )
    .await);
    assert_eq!(r["state"], "working", "{r}");
    assert!(start.elapsed() < Duration::from_secs(10));

    // La de aquí con el mismo id no la abrió: ni la ve suya, ni la contesta, ni la cierra.
    let (_, list) = tool(addr, &local, "sessions", json!({})).await;
    let entry = list["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["id"] == a.as_str())
        .unwrap();
    assert_eq!(entry["mine"], false);
    let (code, _) = tool(addr, &local, "answer", json!({"session": a, "option": "1"})).await;
    assert_eq!(code, 403);
    let (code, _) = tool(addr, &local, "close", json!({"session": a})).await;
    assert_eq!(code, 403);
    // Otra sesión del Mac, tampoco.
    let e = failed(
        ask(
            &state,
            &mac,
            "close",
            with_caller("otra2", json!({"session": a})),
        )
        .await,
    );
    assert_eq!(
        e,
        (403, "solo puedes cerrar las sesiones que abriste".into())
    );
    // La del Mac que la abrió, sí: pasa del permiso (no hay pregunta en pantalla).
    let list = listed(ask(&state, &mac, "sessions", p(json!({}))).await);
    let entry = list.iter().find(|e| e["id"] == a.as_str()).unwrap();
    assert_eq!(entry["mine"], true);
    let e = failed(
        ask(
            &state,
            &mac,
            "answer",
            p(json!({"session": a, "option": "1"})),
        )
        .await,
    );
    assert_eq!(e, (409, "no hay ninguna pregunta pendiente".into()));

    // Y al revés: la que abre la de aquí no es del Mac.
    let (code, b) = tool(
        addr,
        &local,
        "open",
        json!({"machine": "Prueba", "folder": work, "prompt": "y tú"}),
    )
    .await;
    assert_eq!(code, 200, "{b}");
    assert_eq!(b["opened_by"], format!("Prueba/{local}"));
    let b = b["id"].as_str().unwrap().to_owned();
    let e = err(ask(
        &state,
        &mac,
        "answer",
        p(json!({"session": b, "option": "1"})),
    )
    .await);
    assert_eq!(e, "solo puedes contestar las sesiones que abriste");
    let e = err(ask(&state, &mac, "close", p(json!({"session": b}))).await);
    assert_eq!(e, "solo puedes cerrar las sesiones que abriste");
    let (code, _) = tool(addr, &local, "close", json!({"session": b})).await;
    assert_eq!(code, 200);

    // El Mac cierra la suya.
    assert_eq!(
        ok(ask(&state, &mac, "close", p(json!({"session": a}))).await),
        json!({"ok": true})
    );
    assert!(state.registry.lock().await.get(&a).is_none());

    // Muchas sesiones: la lista que va a otro equipo se recorta para que quepa.
    {
        let mut reg = state.registry.lock().await;
        let link = reg.get(&other).unwrap().claude.clone();
        let deep = tmp.path().join("carpeta".repeat(20));
        for i in 0..400 {
            reg.add(session(&format!("muchas-{i}"), &deep, link.clone()))
                .unwrap();
        }
    }
    let r = ok(ask(&state, &mac, "sessions", p(json!({}))).await);
    assert_eq!(
        r["truncated"],
        true,
        "{}",
        r["sessions"].as_array().unwrap().len()
    );
    let n = r["sessions"].as_array().unwrap().len();
    assert!(n > 10 && n < 401, "{n}");
    assert!(serde_json::to_vec(&r).unwrap().len() <= 48 * 1024);

    // Del lado del que pide: sin hub, «sin conexión o versión antigua» (502); a uno que no es
    // del grupo, ni se intenta.
    let no_peer = Err((502, "sin conexión o versión antigua".to_owned()));
    assert_eq!(state.peer("mac-id", "sessions", json!({})).await, no_peer);
    assert_eq!(state.peer("Mac", "sessions", json!({})).await, no_peer);
    let (code, e) = state
        .peer("viejo-id", "sessions", json!({}))
        .await
        .unwrap_err();
    assert_eq!(code, 404);
    assert!(e.contains("no es ningún equipo del grupo"), "{e}");
    // Dos con el mismo nombre: por el nombre no, por el id sí.
    for (id, name) in [("g1", "Gemelo"), ("g2", "gemelo"), ("Gemelo", "Otro")] {
        state
            .group
            .add_member(id, &B64.encode(key(id).public), name);
    }
    let (code, e) = state
        .peer("gemelo", "sessions", json!({}))
        .await
        .unwrap_err();
    assert_eq!(code, 400);
    assert_eq!(e, "hay varios equipos «gemelo» en el grupo: usa su id");
    assert_eq!(
        nido::i18n::english("hay varios equipos «gemelo» en el grupo: usa su id"),
        "there are several machines “gemelo” in the group: use its id"
    );
    assert_eq!(state.peer("g2", "sessions", json!({})).await, no_peer);
    // Un id que es exactamente el pedido gana a los nombres.
    assert_eq!(state.peer("Gemelo", "sessions", json!({})).await, no_peer);
}
