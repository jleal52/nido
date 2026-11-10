//! El MCP con varios equipos del grupo, sin hub: dos nidos de verdad en el mismo proceso, cada
//! uno con su directorio, que se hablan llamando directamente al `mcp_api::peer_call` del otro
//! (lo que haría el canal «peer»). «Aquí» es el de quien llama y el «Mac», el otro. Hay también
//! equipos de mentira: uno caído, uno lento y uno que contesta lo que diga la prueba.
//!
//! El Mac es el único que arranca sesiones: los guardianes toman `NIDO_HOME` del entorno, y el
//! entorno es el suyo. Aquí solo hay sesiones apuntadas en el registro.

// Con sh, sockets Unix y señales: solo en Unix.
#![cfg(unix)]

use std::io::Write;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use nido::client::Api;
use nido::e2e::{Host, PeerResult};
use nido::group::Member;
use nido::launch::{Launcher, Paths};
use nido::registry::{ClaudeLink, Session};
use nido::server::{self, AppState, PeerLink};
use serde_json::{Value, json};

const TOKEN: &str = "0123456789abcdef0123456789abcdef";
const NO_PEER: &str = "sin conexión o versión antigua";

async fn request(
    addr: SocketAddr,
    token: &str,
    method: &'static str,
    path: &str,
    caller: Option<&str>,
    body: Option<Value>,
) -> (u16, Value) {
    let caller = caller.map(str::to_owned);
    let (path, token) = (path.to_owned(), token.to_owned());
    tokio::task::spawn_blocking(move || {
        let api = Api {
            addr,
            token: Some(token),
        };
        let headers: Vec<(&str, &str)> = caller
            .as_deref()
            .map(|c| vec![("X-Nido-Session", c)])
            .unwrap_or_default();
        api.request_with(
            method,
            &path,
            body.as_ref(),
            &headers,
            Some(Duration::from_secs(60)),
        )
        .unwrap()
    })
    .await
    .unwrap()
}

/// Una herramienta por la API local de `addr`, desde la sesión `caller` (en inglés, como `nido
/// mcp`).
async fn tool(addr: SocketAddr, caller: &str, name: &str, args: Value) -> (u16, Value) {
    let path = format!("/api/v1/mcp/{name}");
    request(addr, TOKEN, "POST", &path, Some(caller), Some(args)).await
}

async fn serve(state: Arc<AppState>) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let app = server::router(state).into_make_service_with_connect_info::<SocketAddr>();
        axum::serve(listener, app).await.unwrap()
    });
    addr
}

/// Conectado al hub con ese id (sin hub de verdad: solo lo que se ve de él).
fn hub_id(st: &AppState, id: &str) {
    st.hub.status.send_modify(|s| {
        s.url = Some("wss://hub.invalid".into());
        s.id = Some(id.into());
        s.connected = true;
    });
}

/// Cómo contesta cada equipo de mentira.
#[derive(Clone)]
enum Peer {
    /// Un nido de verdad: su `peer_call`, con quien pide apuntado como este miembro.
    Real(Arc<AppState>, Member),
    Down,
    Slow,
    /// Lo que diga la prueba en cada momento.
    Script(Arc<Mutex<PeerResult>>),
    /// Una respuesta por llamada, en orden (la última se repite).
    Queue(Arc<Mutex<Vec<PeerResult>>>),
    /// Como `Real`, pero estos métodos no contestan.
    Partial(Arc<AppState>, Member, &'static [&'static str]),
    /// Como `Real`, pero estos métodos tardan 30 s (y luego no contestan).
    SlowOn(Arc<AppState>, Member, &'static [&'static str]),
}

/// Lo que ve cada nido de los demás equipos, por su id, y cuántas veces se le pidió cada método.
#[derive(Clone, Default)]
struct Net {
    peers: Arc<Mutex<Vec<(String, Peer)>>>,
    calls: Arc<Mutex<Vec<(String, String)>>>,
}

impl Net {
    fn set(&self, id: &str, p: Peer) {
        let mut peers = self.peers.lock().unwrap();
        peers.retain(|(i, _)| i != id);
        peers.push((id.into(), p));
    }

    fn count(&self, id: &str, method: &str) -> usize {
        let calls = self.calls.lock().unwrap();
        calls.iter().filter(|c| c.0 == id && c.1 == method).count()
    }

    fn link(&self) -> PeerLink {
        let net = self.clone();
        Box::new(move |m: Member, method: String, params: Value| {
            let net = net.clone();
            Box::pin(async move {
                net.calls
                    .lock()
                    .unwrap()
                    .push((m.id.clone(), method.clone()));
                let peer = net
                    .peers
                    .lock()
                    .unwrap()
                    .iter()
                    .find(|(i, _)| *i == m.id)
                    .map(|(_, p)| p.clone());
                match peer {
                    Some(Peer::Real(st, from)) => {
                        // Como por el canal: el JSON de ida y el de vuelta.
                        let params = serde_json::from_str(&params.to_string()).unwrap();
                        nido::mcp_api::peer_call(&st, &from, &method, params).await
                    }
                    Some(Peer::Slow) => {
                        tokio::time::sleep(Duration::from_secs(30)).await;
                        Err((502, NO_PEER.to_owned()))
                    }
                    Some(Peer::Script(r)) => r.lock().unwrap().clone(),
                    Some(Peer::Queue(q)) => {
                        let mut q = q.lock().unwrap();
                        if q.len() > 1 {
                            q.remove(0)
                        } else {
                            q[0].clone()
                        }
                    }
                    Some(Peer::Partial(_, _, failing)) if failing.contains(&method.as_str()) => {
                        Err((502, NO_PEER.to_owned()))
                    }
                    Some(Peer::Partial(st, from, _)) => {
                        nido::mcp_api::peer_call(&st, &from, &method, params).await
                    }
                    Some(Peer::SlowOn(_, _, slow)) if slow.contains(&method.as_str()) => {
                        tokio::time::sleep(Duration::from_secs(30)).await;
                        Err((502, NO_PEER.to_owned()))
                    }
                    Some(Peer::SlowOn(st, from, _)) => {
                        nido::mcp_api::peer_call(&st, &from, &method, params).await
                    }
                    Some(Peer::Down) | None => Err((502, NO_PEER.to_owned())),
                }
            })
        })
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

fn user(text: &str) -> Value {
    json!({"type":"user","uuid":nido::registry::new_id(),"message":{"content":text}})
}

fn assistant(text: &str) -> Value {
    json!({"type":"assistant","uuid":nido::registry::new_id(),
        "message":{"content":[{"type":"text","text":text}]}})
}

/// Una conversación de Claude con estas líneas, en `config`.
fn conversation(config: &Path, id: &str, lines: &[Value]) -> (ClaudeLink, PathBuf) {
    let dir = config.join("projects").join("-tmp-x");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{id}.jsonl"));
    append(&path, lines);
    let link = ClaudeLink {
        session_id: id.into(),
        config_dir: Some(config.to_owned()),
        account: None,
        inside_shell: false,
    };
    (link, path)
}

fn texts(read: &Value) -> Vec<String> {
    read["turns"]
        .as_array()
        .unwrap_or_else(|| panic!("{read}"))
        .iter()
        .map(|t| t["text"].as_str().unwrap().to_owned())
        .collect()
}

fn find<'a>(list: &'a Value, id: &str) -> &'a Value {
    list["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["id"] == id)
        .unwrap_or_else(|| panic!("{id} no está en {list}"))
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

// Las dos partes cambian el entorno del proceso (`NIDO_HOME`): una detrás de otra.
#[tokio::test(flavor = "multi_thread")]
async fn mcp_across_the_group() {
    let _limit = nido::watchdog::limit(std::time::Duration::from_secs(120));
    the_mcp_reaches_every_machine_of_the_group().await;
    a_child_does_not_alert_while_its_parent_lives().await;
}

async fn the_mcp_reaches_every_machine_of_the_group() {
    let tmp = tempfile::tempdir().unwrap();
    let _cleanup = KillHolders(tmp.path().join("mac/run"));
    let bin = tmp.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let shell = bin.join("shell");
    std::fs::write(
        &shell,
        "#!/bin/sh\nif [ -x ./program ]; then exec ./program; fi\nexec sleep 60\n",
    )
    .unwrap();
    std::fs::set_permissions(&shell, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    // Esta prueba es su propio proceso (un fichero de tests/): cambiar el entorno no toca a otras.
    // Los dos equipos se llaman igual de por sí (como WSL y su Windows); en el grupo, como se diga.
    unsafe {
        std::env::set_var("NIDO_HOME", tmp.path().join("mac"));
        std::env::set_var("NIDO_EXE", env!("CARGO_BIN_EXE_nido"));
        std::env::set_var("NIDO_NAME", "Prueba");
        std::env::set_var("SHELL", &shell);
    }
    let mac = AppState::new(
        Paths::from_env(),
        Launcher::Detached,
        Some(TOKEN.into()),
        true,
    )
    .unwrap();
    let root = tmp.path().join("aqui");
    let here_paths = Paths {
        runtime: root.join("run"),
        data: root.join("data"),
        state: root.join("state"),
        config: root.join("config"),
        home: Paths::from_env().home,
    };
    let here = AppState::new(here_paths, Launcher::Detached, Some(TOKEN.into()), true).unwrap();
    hub_id(&here, "aqui-id");
    hub_id(&mac, "mac-id");
    let key = |st: &AppState| B64.encode(st.identity().public);
    let member = |id: &str, st: &AppState, name: &str| Member {
        id: id.into(),
        key: key(st),
        name: name.into(),
        at: 0,
        removed: false,
    };
    // Cada uno apunta al otro en su grupo; aquí, el Mac se llama «Mac».
    here.group.add_member("mac-id", &key(&mac), "Mac");
    mac.group.add_member("aqui-id", &key(&here), "Prueba");
    let (from_here, from_mac) = (Net::default(), Net::default());
    from_here.set(
        "mac-id",
        Peer::Real(mac.clone(), member("aqui-id", &here, "Prueba")),
    );
    from_mac.set(
        "aqui-id",
        Peer::Real(here.clone(), member("mac-id", &mac, "Mac")),
    );
    assert!(here.set_peer_link(from_here.link()));
    assert!(mac.set_peer_link(from_mac.link()));
    assert!(!here.set_peer_link(from_here.link()), "solo una vez");
    let addr = serve(here.clone()).await;

    // Aquí: la sesión que llama y una de Claude, parada. En el Mac, otra de Claude, parada.
    let (link, _) = conversation(&root.join("claude"), "conv-aqui", &[user("uno")]);
    let (padre, aqui) = {
        let mut reg = here.registry.lock().await;
        let padre = reg.add(session("padre", tmp.path(), None)).unwrap().id;
        let aqui = reg.add(session("aqui", tmp.path(), Some(link))).unwrap().id;
        (padre, aqui)
    };
    let mac_claude = tmp.path().join("mac-claude");
    let (link, transcript) =
        conversation(&mac_claude, "conv-1", &[user("hola"), assistant("Hecho.")]);
    let otra = {
        let mut reg = mac.registry.lock().await;
        reg.add(session("otra", tmp.path(), Some(link))).unwrap().id
    };
    let at_mac = format!("Mac/{otra}");

    // `sessions`: las de los dos, cada una con su dirección como se escribe aquí.
    let (code, list) = tool(addr, &padre, "sessions", json!({})).await;
    assert_eq!(code, 200, "{list}");
    assert!(list.get("unreachable").is_none(), "{list}");
    assert_eq!(list["sessions"].as_array().unwrap().len(), 2, "{list}");
    let e = find(&list, &aqui);
    assert_eq!(e["session"], format!("Prueba/{aqui}"));
    assert_eq!(
        (e["machine"].as_str(), e["machine_id"].as_str()),
        (Some("Prueba"), Some("aqui-id"))
    );
    let e = find(&list, &otra);
    assert_eq!(e["session"], at_mac.as_str());
    assert_eq!(e["machine"], "Mac");
    assert_eq!(e["machine_id"], "mac-id");
    assert_eq!(e["state"], "stopped");
    assert_eq!(e["mine"], false);
    // Al Mac se le pidió con quien llama; él no pregunta a nadie más (ni vuelve aquí).
    assert_eq!(from_here.count("mac-id", "sessions"), 1);
    assert_eq!(from_mac.count("aqui-id", "sessions"), 0);
    let theirs = nido::mcp_api::peer_call(
        &mac,
        &member("aqui-id", &here, "Prueba"),
        "sessions",
        json!({"caller": {"id": padre, "name": "padre"}}),
    )
    .await
    .unwrap();
    assert_eq!(theirs["sessions"].as_array().unwrap().len(), 1, "{theirs}");

    // Uno caído y uno lento: salen en `unreachable`, y el lento no hace esperar más de 5 s.
    here.group.add_member("caido-id", "Y2FpZG8=", "Sobremesa");
    here.group.add_member("lento-id", "bGVudG8=", "Lento");
    from_here.set("caido-id", Peer::Down);
    from_here.set("lento-id", Peer::Slow);
    let start = Instant::now();
    let (code, list) = tool(addr, &padre, "sessions", json!({})).await;
    let took = start.elapsed();
    assert_eq!(code, 200, "{list}");
    assert!(
        took >= Duration::from_secs(5) && took < Duration::from_secs(20),
        "{took:?}"
    );
    assert_eq!(list["sessions"].as_array().unwrap().len(), 2, "{list}");
    let mut down: Vec<Value> = list["unreachable"].as_array().unwrap().clone();
    down.sort_by_key(|u| u["machine_id"].as_str().unwrap().to_owned());
    assert_eq!(
        down,
        [
            json!({"machine": "Sobremesa", "machine_id": "caido-id",
                "error": "no connection or old version"}),
            json!({"machine": "Lento", "machine_id": "lento-id",
                "error": "the other machine does not answer"}),
        ]
    );
    assert!(here.group.remove_member("lento-id"));

    // `read` en el Mac: allí lleva su posición (la de quien llama aquí).
    let (code, r) = tool(addr, &padre, "read", json!({"session": at_mac})).await;
    assert_eq!(code, 200, "{r}");
    assert_eq!(r["state"], "stopped");
    assert_eq!(texts(&r), ["hola", "Hecho."]);
    let (_, r) = tool(addr, &padre, "read", json!({"session": at_mac})).await;
    assert_eq!(texts(&r), Vec::<String>::new());
    // Por el id del Mac, igual; `last` también pasa.
    let (_, r) = tool(
        addr,
        &padre,
        "read",
        json!({"session": format!("mac-id/{otra}"), "last": 1}),
    )
    .await;
    assert_eq!(texts(&r), ["Hecho."]);
    // Los errores de allí, con su código.
    let (code, e) = tool(addr, &padre, "read", json!({"session": "Mac/zzzz"})).await;
    assert_eq!((code, e["error"].as_str()), (404, Some("no such session")));
    let (code, e) = tool(addr, &padre, "read", json!({"session": "Nadie/zzzz"})).await;
    assert_eq!(
        (code, e["error"].as_str()),
        (404, Some("“Nadie” is not a machine in the group"))
    );
    let (code, e) = tool(
        addr,
        &padre,
        "read",
        json!({"session": format!("Sobremesa/{otra}")}),
    )
    .await;
    assert_eq!(
        (code, e["error"].as_str()),
        (502, Some("no connection or old version"))
    );

    // `send` al Mac: a la cola de allí, con quién lo manda y el id del mensaje.
    let (code, r) = tool(
        addr,
        &padre,
        "send",
        json!({"session": at_mac, "text": "¿qué tal?"}),
    )
    .await;
    assert_eq!(code, 200, "{r}");
    assert_eq!(r["status"], "queued", "{r}");
    let msg = r["msg_id"].as_str().unwrap().to_owned();
    let queued = mac.inbox.pending(&otra);
    assert_eq!(queued.len(), 1);
    assert_eq!(queued[0].id, msg);
    assert_eq!(queued[0].from_id, format!("aqui-id/{padre}"));
    assert_eq!(queued[0].machine, "Prueba");
    assert_eq!(queued[0].text, "¿qué tal?");

    // `status`: si lo recogió y si contestó (solo cuenta lo que escriba Claude después).
    let status = |m: &str| tool(addr, &padre, "status", json!({"session": at_mac, "msg": m}));
    let (code, s) = status(&msg).await;
    assert_eq!(code, 200, "{s}");
    assert_eq!(
        s,
        json!({"state": "stopped", "taken": false, "replied": false})
    );
    assert_eq!(mac.inbox.ack(&otra, std::slice::from_ref(&msg)), 1);
    append(&transcript, &[user("¿qué tal?")]);
    assert_eq!(
        status(&msg).await.1,
        json!({"state": "stopped", "taken": true, "replied": false})
    );
    append(&transcript, &[assistant("Bien.")]);
    assert_eq!(
        status(&msg).await.1,
        json!({"state": "stopped", "taken": true, "replied": true})
    );
    // Sin mensaje, solo el estado.
    let (_, s) = tool(addr, &padre, "status", json!({"session": at_mac})).await;
    assert_eq!(s, json!({"state": "stopped"}));
    // Tras `/clear` (otra transcripción): lo que escriba Claude en la nueva.
    let (_, r) = tool(
        addr,
        &padre,
        "send",
        json!({"session": at_mac, "text": "otra"}),
    )
    .await;
    let msg2 = r["msg_id"].as_str().unwrap().to_owned();
    let (link2, transcript2) = conversation(&mac_claude, "conv-2", &[user("otra")]);
    mac.registry
        .lock()
        .await
        .update(&otra, |s| s.claude = Some(link2.clone()))
        .unwrap();
    assert_eq!(status(&msg2).await.1["replied"], false);
    append(&transcript2, &[assistant("Nueva.")]);
    assert_eq!(status(&msg2).await.1["replied"], true);
    // El de antes ya contestó: se queda así.
    assert_eq!(status(&msg).await.1["replied"], true);

    // `send` con `wait` al Mac: parada, no se espera a que lo recoja; y luego un `read` de allí.
    let start = Instant::now();
    let (code, r) = tool(
        addr,
        &padre,
        "send",
        json!({"session": at_mac, "text": "y ahora", "wait": 60}),
    )
    .await;
    assert_eq!(code, 200, "{r}");
    assert!(start.elapsed() < Duration::from_secs(5));
    assert_eq!(r["status"], "queued");
    assert_eq!(r["read"]["state"], "stopped", "{r}");
    assert_eq!(texts(&r["read"]), ["otra", "Nueva."]);
    // `read` con `wait`: igual.
    let (code, r) = tool(
        addr,
        &padre,
        "read",
        json!({"session": at_mac, "wait": true}),
    )
    .await;
    assert_eq!((code, r["state"].as_str()), (200, Some("stopped")), "{r}");
    // Con el Mac caído desde el principio: el error enseguida, sin esperar al tope.
    from_here.set("mac-id", Peer::Down);
    let start = Instant::now();
    let (code, e) = tool(
        addr,
        &padre,
        "read",
        json!({"session": at_mac, "wait": true, "timeout_s": 600}),
    )
    .await;
    assert_eq!(
        (code, e["error"].as_str()),
        (502, Some("no connection or old version"))
    );
    let took = start.elapsed();
    assert!(took < Duration::from_secs(5), "{took:?}");
    let (code, list) = tool(addr, &padre, "sessions", json!({})).await;
    assert_eq!(code, 200);
    assert_eq!(list["sessions"].as_array().unwrap().len(), 1, "{list}");
    assert_eq!(list["unreachable"].as_array().unwrap().len(), 2, "{list}");
    from_here.set(
        "mac-id",
        Peer::Real(mac.clone(), member("aqui-id", &here, "Prueba")),
    );

    // Un `status` que no vuelve se corta a los 10 s (o a lo que quede de espera): sin ninguna
    // respuesta, el error.
    from_here.set("mac-id", Peer::Slow);
    for (timeout, cut) in [(600, 10), (3, 3)] {
        let start = Instant::now();
        let (code, e) = tool(
            addr,
            &padre,
            "read",
            json!({"session": at_mac, "wait": true, "timeout_s": timeout}),
        )
        .await;
        assert_eq!(
            (code, e["error"].as_str()),
            (504, Some("the other machine does not answer")),
            "{timeout}"
        );
        let took = start.elapsed();
        assert!(
            took + Duration::from_millis(500) >= Duration::from_secs(cut)
                && took < Duration::from_secs(cut + 15),
            "{timeout}: {took:?}"
        );
    }
    // Lo que no se entiende (un estado que no existe) acaba la espera: no se reintenta.
    let state = |s: &str| Ok(json!({"state": s}));
    let script = Arc::new(Mutex::new(vec![
        state("working"),
        state("dormida"),
        state("idle"),
    ]));
    from_here.set("mac-id", Peer::Queue(script));
    let before = from_here.count("mac-id", "status");
    let (code, e) = tool(
        addr,
        &padre,
        "read",
        json!({"session": at_mac, "wait": true}),
    )
    .await;
    assert_eq!(
        (code, e["error"].as_str()),
        (502, Some("invalid reply from the other machine"))
    );
    assert_eq!(from_here.count("mac-id", "status") - before, 2);
    // `send` con `wait`: si ya está en la cola y la espera falla, se dice en `read`, sin perder
    // lo demás.
    from_here.set(
        "mac-id",
        Peer::Partial(
            mac.clone(),
            member("aqui-id", &here, "Prueba"),
            &["status", "read"],
        ),
    );
    let (code, r) = tool(
        addr,
        &padre,
        "send",
        json!({"session": at_mac, "text": "¿sigues?", "wait": 60}),
    )
    .await;
    assert_eq!(code, 200, "{r}");
    assert_eq!(r["status"], "queued", "{r}");
    let sent = r["msg_id"].as_str().unwrap().to_owned();
    assert!(mac.inbox.pending(&otra).iter().any(|m| m.id == sent));
    assert_eq!(r["read"], json!({"error": "no connection or old version"}));
    // La lectura del final de la espera tampoco se queda colgada: 10 s como mucho.
    from_here.set(
        "mac-id",
        Peer::SlowOn(mac.clone(), member("aqui-id", &here, "Prueba"), &["read"]),
    );
    let start = Instant::now();
    let (code, e) = tool(
        addr,
        &padre,
        "read",
        json!({"session": at_mac, "wait": true}),
    )
    .await;
    assert_eq!(
        (code, e["error"].as_str()),
        (504, Some("the other machine does not answer"))
    );
    let took = start.elapsed();
    assert!(
        took + Duration::from_millis(500) >= Duration::from_secs(10)
            && took < Duration::from_secs(25),
        "{took:?}"
    );
    from_here.set(
        "mac-id",
        Peer::Real(mac.clone(), member("aqui-id", &here, "Prueba")),
    );

    // Lo que pide otro equipo para un tercero, tampoco con `status` ni `open`.
    let from = member("aqui-id", &here, "Prueba");
    let who = json!({"id": padre, "name": "padre"});
    let r = nido::mcp_api::peer_call(
        &mac,
        &from,
        "status",
        json!({"session": format!("Otro/{otra}"), "caller": who}),
    )
    .await;
    assert_eq!(r, Err((400, "esa sesión no es de este equipo".into())));
    let r = nido::mcp_api::peer_call(
        &mac,
        &from,
        "open",
        json!({"machine": "aqui-id", "folder": tmp.path(), "prompt": "x", "caller": who}),
    )
    .await;
    assert_eq!(r, Err((400, "«aqui-id» no es este equipo".into())));

    // `answer` y `close` en una que no abrió: el 403 de allí sigue siendo 403.
    let (code, e) = tool(
        addr,
        &padre,
        "answer",
        json!({"session": at_mac, "option": "1"}),
    )
    .await;
    assert_eq!(code, 403, "{e}");
    assert_eq!(e["error"], "you can only answer sessions you opened");
    let (code, e) = tool(addr, &padre, "close", json!({"session": at_mac})).await;
    assert_eq!(code, 403, "{e}");
    assert_eq!(e["error"], "you can only close sessions you opened");

    // `open` en el Mac: allí, abierta por la dirección de quien llama aquí, y suya.
    let work = tmp.path().join("work");
    let (code, e) = tool(
        addr,
        &padre,
        "open",
        json!({"machine": "Mac", "folder": work, "prompt": "di hola"}),
    )
    .await;
    assert_eq!(code, 400, "la carpeta, en el Mac: {e}");
    assert!(
        e["error"].as_str().unwrap().contains("does not exist"),
        "{e}"
    );
    std::fs::create_dir_all(&work).unwrap();
    let (code, o) = tool(
        addr,
        &padre,
        "open",
        json!({"machine": "mac", "folder": work, "prompt": "di hola"}),
    )
    .await;
    assert_eq!(code, 200, "{o}");
    let hija = o["id"].as_str().unwrap().to_owned();
    assert_eq!(o["session"], format!("Mac/{hija}"));
    assert_eq!(o["machine_id"], "mac-id");
    assert_eq!(o["opened_by"], format!("aqui-id/{padre}"));
    assert_eq!(o["mine"], true);
    let reg_hija = mac.registry.lock().await.get(&hija).cloned().unwrap();
    assert_eq!(reg_hija.opened_by, Some(format!("aqui-id/{padre}")));
    let (_, list) = tool(addr, &padre, "sessions", json!({})).await;
    assert_eq!(find(&list, &hija)["mine"], true);
    // Suya: sin pregunta en pantalla, lo dice allí (409).
    let (code, e) = tool(
        addr,
        &padre,
        "answer",
        json!({"session": format!("mac-id/{hija}"), "option": "1"}),
    )
    .await;
    assert_eq!(
        (code, e["error"].as_str()),
        (409, Some("no question is pending"))
    );

    // Una espera en el Mac que acaba en una pregunta.
    let menu = tmp.path().join("menu");
    std::fs::create_dir_all(&menu).unwrap();
    let program = menu.join("program");
    std::fs::write(
        &program,
        "#!/bin/sh\nprintf '\\033[2J\\033[H'; cat screen\nexec sleep 60\n",
    )
    .unwrap();
    std::fs::set_permissions(
        &program,
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    )
    .unwrap();
    let permiso = std::fs::read_to_string(format!(
        "{}/tests/pantallas/permiso.txt",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap();
    std::fs::write(menu.join("screen"), permiso).unwrap();
    let (code, o) = tool(
        addr,
        &padre,
        "open",
        json!({"machine": "Mac", "folder": menu, "prompt": "pregunta"}),
    )
    .await;
    assert_eq!(code, 200, "{o}");
    let pregunta = o["session"].as_str().unwrap().to_owned();
    let (code, r) = tool(
        addr,
        &padre,
        "read",
        json!({"session": pregunta, "wait": true, "timeout_s": 30}),
    )
    .await;
    assert_eq!(code, 200, "{r}");
    assert_eq!(r["state"], "question", "{r}");
    assert_eq!(r["question"]["kind"], "permission", "{r}");
    let (code, _) = tool(addr, &padre, "close", json!({"session": pregunta})).await;
    assert_eq!(code, 200);

    // La madre de la hija es de aquí: mientras viva, lo que pregunte la hija no avisa. Vive si no
    // está parada, sea o no de Claude (como las de aquí): esta, parada.
    let s = nido::mcp_api::peer_call(
        &here,
        &member("mac-id", &mac, "Mac"),
        "status",
        json!({"session": padre, "caller": {"id": hija, "name": "work"}}),
    )
    .await;
    assert_eq!(s, Ok(json!({"state": "stopped"})));
    let by = format!("aqui-id/{padre}");
    assert!(!nido::mcp_api::parent_alive(&mac, &by, &hija, "work", false).await);
    // Una de Claude aquí, parada: tampoco.
    let by_aqui = format!("aqui-id/{aqui}");
    assert!(!nido::mcp_api::parent_alive(&mac, &by_aqui, &hija, "work", false).await);

    // `close` de la hija desde aquí.
    let (code, r) = tool(
        addr,
        &padre,
        "close",
        json!({"session": format!("Mac/{hija}")}),
    )
    .await;
    assert_eq!(code, 200, "{r}");
    assert!(mac.registry.lock().await.get(&hija).is_none());

    // Los dos se llaman igual en el grupo (WSL y su Windows): por el id, y el nombre no vale.
    here.group.add_member("mac-id", &key(&mac), "Prueba");
    let (_, list) = tool(addr, &padre, "sessions", json!({})).await;
    assert_eq!(find(&list, &aqui)["session"], format!("aqui-id/{aqui}"));
    assert_eq!(find(&list, &otra)["session"], format!("mac-id/{otra}"));
    assert_eq!(find(&list, &otra)["machine"], "Prueba");
    let (code, e) = tool(
        addr,
        &padre,
        "read",
        json!({"session": format!("Prueba/{otra}")}),
    )
    .await;
    assert_eq!(code, 400, "{e}");
    assert_eq!(
        e["error"],
        "there are several machines “Prueba” in the group: use its id"
    );
    let (code, _) = tool(
        addr,
        &padre,
        "read",
        json!({"session": format!("mac-id/{otra}")}),
    )
    .await;
    assert_eq!(code, 200);
    let (code, _) = tool(
        addr,
        &padre,
        "read",
        json!({"session": format!("aqui-id/{aqui}")}),
    )
    .await;
    assert_eq!(code, 200);
    let (code, _) = tool(
        addr,
        &padre,
        "open",
        json!({"machine": "prueba", "folder": work, "prompt": "x"}),
    )
    .await;
    assert_eq!(code, 400);
    here.group.add_member("mac-id", &key(&mac), "Mac");

    // Las alertas de todos (`nido alerts`): con el token principal, sin sesión.
    {
        let mut reg = here.registry.lock().await;
        reg.add_alert(&aqui, "aquí listo", 1_000).unwrap();
    }
    {
        let mut reg = mac.registry.lock().await;
        reg.add_alert(&otra, "en el Mac", 2_000).unwrap();
        reg.add_alert(&otra, "primera", 500).unwrap();
    }
    let (code, a) = request(addr, TOKEN, "GET", "/api/v1/mcp/alerts", None, None).await;
    assert_eq!(code, 200, "{a}");
    assert_eq!(
        a["alerts"],
        json!([
            {"machine": "Mac", "machine_id": "mac-id", "session": format!("Mac/{otra}"),
             "name": "otra", "text": "primera", "at": 500},
            {"machine": "Prueba", "machine_id": "aqui-id", "session": format!("Prueba/{aqui}"),
             "name": "aqui", "text": "aquí listo", "at": 1000},
            {"machine": "Mac", "machine_id": "mac-id", "session": format!("Mac/{otra}"),
             "name": "otra", "text": "en el Mac", "at": 2000},
        ])
    );
    assert_eq!(
        a["unreachable"],
        json!([{"machine": "Sobremesa", "machine_id": "caido-id",
            "error": "no connection or old version"}])
    );
    // Lo que da el Mac por el canal: las suyas, sin sesión que llame.
    let theirs = nido::mcp_api::peer_call(
        &mac,
        &member("aqui-id", &here, "Prueba"),
        "alerts",
        json!({}),
    )
    .await
    .unwrap();
    assert_eq!(theirs["alerts"].as_array().unwrap().len(), 2, "{theirs}");
    // Con el token de un teléfono, no.
    let phone = here
        .phones
        .lock()
        .unwrap()
        .add(&[7u8; 32], "teléfono", None)
        .unwrap();
    let (code, _) = request(addr, &phone.token, "GET", "/api/v1/mcp/alerts", None, None).await;
    assert_eq!(code, 403);
}

async fn a_child_does_not_alert_while_its_parent_lives() {
    let tmp = tempfile::tempdir().unwrap();
    let _cleanup = KillHolders(tmp.path().join("nido/run"));
    let bin = tmp.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let shell = bin.join("shell");
    std::fs::write(
        &shell,
        "#!/bin/sh\nif [ -x ./program ]; then exec ./program; fi\nexec sleep 60\n",
    )
    .unwrap();
    std::fs::set_permissions(&shell, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    unsafe {
        std::env::set_var("NIDO_HOME", tmp.path().join("nido"));
        std::env::set_var("NIDO_EXE", env!("CARGO_BIN_EXE_nido"));
        std::env::set_var("NIDO_NAME", "Prueba");
        std::env::set_var("SHELL", &shell);
    }
    let paths = Paths::from_env();
    let st = AppState::new(paths.clone(), Launcher::Detached, Some(TOKEN.into()), true).unwrap();
    hub_id(&st, "k-aqui");
    let net = Net::default();
    assert!(st.set_peer_link(net.link()));
    let hija = nido::registry::new_id();

    // Madre de aquí: apuntada y en marcha, vive; parada o borrada, no.
    let madre = {
        let mut reg = st.registry.lock().await;
        reg.add(session("madre", tmp.path(), None)).unwrap().id
    };
    for by in [format!("k-aqui/{madre}"), format!("Prueba/{madre}")] {
        assert!(
            !nido::mcp_api::parent_alive(&st, &by, &hija, "hija", false).await,
            "{by}"
        );
    }
    nido::launch::start(&paths, Launcher::Detached, &madre).unwrap();
    for _ in 0..100 {
        if nido::launch::is_alive(&paths, &madre) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(nido::launch::is_alive(&paths, &madre), "no arrancó");
    for by in [format!("k-aqui/{madre}"), format!("Prueba/{madre}")] {
        assert!(
            nido::mcp_api::parent_alive(&st, &by, &hija, "hija", false).await,
            "{by}"
        );
    }
    assert!(!nido::mcp_api::parent_alive(&st, "k-aqui/nadie", &hija, "hija", false).await);
    // Lo que ve otro equipo de una sesión que no es de Claude: solo si vive, nunca lo que tenga en
    // pantalla (aquí, un menú), ni si recogió un mensaje.
    let concha = tmp.path().join("concha");
    std::fs::create_dir_all(&concha).unwrap();
    let program = concha.join("program");
    std::fs::write(
        &program,
        "#!/bin/sh\nprintf '\\033[2J\\033[H'; cat screen\nexec sleep 60\n",
    )
    .unwrap();
    std::fs::set_permissions(
        &program,
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    )
    .unwrap();
    let permiso = std::fs::read_to_string(format!(
        "{}/tests/pantallas/permiso.txt",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap();
    std::fs::write(concha.join("screen"), permiso).unwrap();
    let shell_id = {
        let mut reg = st.registry.lock().await;
        reg.add(session("concha", &concha, None)).unwrap().id
    };
    nido::launch::start(&paths, Launcher::Detached, &shell_id).unwrap();
    for _ in 0..100 {
        if nido::launch::is_alive(&paths, &shell_id) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    // Que le dé tiempo a pintar el menú.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let mac = Member {
        id: "mac-id".into(),
        key: "bWFj".into(),
        name: "Mac".into(),
        at: 0,
        removed: false,
    };
    for args in [
        json!({"session": shell_id}),
        json!({"session": shell_id, "msg": "m1"}),
    ] {
        let mut args = args;
        args["caller"] = json!({"id": hija, "name": "hija"});
        let s = nido::mcp_api::peer_call(&st, &mac, "status", args).await;
        assert_eq!(s, Ok(json!({"state": "working"})));
    }
    st.registry.lock().await.remove(&madre).unwrap();
    assert!(
        !nido::mcp_api::parent_alive(&st, &format!("k-aqui/{madre}"), &hija, "hija", false).await
    );
    assert_eq!(
        net.calls.lock().unwrap().len(),
        0,
        "de aquí, sin preguntar a nadie"
    );

    // Madre de otro equipo: se le pregunta su `status` (con la hija como quien llama).
    let answer = Arc::new(Mutex::new(Ok(json!({"state": "working"}))));
    st.group.add_member("mac-id", "bWFj", "Mac");
    net.set("mac-id", Peer::Script(answer.clone()));
    assert!(nido::mcp_api::parent_alive(&st, "mac-id/p1", &hija, "hija", false).await);
    // Se recuerda un rato: aunque ya esté parada, no se vuelve a preguntar.
    *answer.lock().unwrap() = Ok(json!({"state": "stopped"}));
    assert!(nido::mcp_api::parent_alive(&st, "mac-id/p1", &hija, "hija", false).await);
    assert_eq!(net.count("mac-id", "status"), 1);
    // Otra madre: se pregunta, y parada no vive.
    assert!(!nido::mcp_api::parent_alive(&st, "mac-id/p2", &hija, "hija", false).await);
    assert_eq!(net.count("mac-id", "status"), 2);
    for state in ["idle", "question"] {
        *answer.lock().unwrap() = Ok(json!({"state": state}));
        let by = format!("mac-id/p-{state}");
        assert!(
            nido::mcp_api::parent_alive(&st, &by, &hija, "hija", false).await,
            "{state}"
        );
    }
    // Un error (ya no existe) o sin respuesta: no vive, y se avisa como siempre.
    *answer.lock().unwrap() = Err((404, "no existe esa sesión".into()));
    assert!(!nido::mcp_api::parent_alive(&st, "mac-id/p3", &hija, "hija", false).await);
    net.set("mac-id", Peer::Down);
    assert!(!nido::mcp_api::parent_alive(&st, "mac-id/p4", &hija, "hija", false).await);
    // Uno que no contesta no hace esperar más de 5 s.
    net.set("mac-id", Peer::Slow);
    let start = Instant::now();
    assert!(!nido::mcp_api::parent_alive(&st, "mac-id/p5", &hija, "hija", false).await);
    let took = start.elapsed();
    assert!(
        took >= Duration::from_secs(5) && took < Duration::from_secs(20),
        "{took:?}"
    );
    // De un equipo que no es del grupo, o una dirección sin equipo: no se pregunta.
    let before = net.calls.lock().unwrap().len();
    assert!(!nido::mcp_api::parent_alive(&st, "otro-id/p1", &hija, "hija", false).await);
    assert!(!nido::mcp_api::parent_alive(&st, "p1", &hija, "hija", false).await);
    assert_eq!(net.calls.lock().unwrap().len(), before);
}
