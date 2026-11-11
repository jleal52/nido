//! Lanzar Claude con el MCP de nido, de punta a punta en un directorio temporal: las opciones y
//! `NIDO_CHANNEL` que recibe la sesión, la pantalla de aviso de los channels (se acepta solo la
//! de nido) y `nido mcp on|off`. Sin Claude de verdad: una «shell» falsa apunta lo que recibe y
//! corre un programa que enseña la pantalla y apunta la tecla que le llega.

// Con sh, sockets Unix y señales: solo en Unix.
#![cfg(unix)]

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use nido::client::Api;
use nido::launch::{Launcher, Paths};
use nido::prompt::Startup;
use nido::registry::Session;
use nido::server::{self, AppState};
use serde_json::{Value, json};

const TOKEN: &str = "0123456789abcdef0123456789abcdef";

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

/// Enseña `before` 3 s (si lo hay) y luego `screen`; apunta en `picked` las teclas que le
/// lleguen, tal cual (Enter es `\r`): la primera, y tras ella, quitada la pantalla (y puesta
/// `after`, si la hay), las demás.
const PROGRAM: &str = r#"#!/bin/sh
stty -icanon -echo -icrnl min 1 time 0
if [ -f before ]; then printf '\033[2J\033[H'; cat before; sleep 3; fi
printf '\033[2J\033[H'; cat screen
dd bs=1 count=1 of=picked.tmp 2>/dev/null && mv picked.tmp picked
printf '\033[2J\033[H'
if [ -f after ]; then cat after; else printf 'listo\n'; fi
exec cat >> picked
"#;

fn fixture(name: &str) -> String {
    std::fs::read_to_string(format!(
        "{}/tests/pantallas/{name}.txt",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
}

/// Una carpeta con `program` y la pantalla que enseña.
fn folder(dir: &Path, screen: &str) {
    std::fs::create_dir_all(dir).unwrap();
    let program = dir.join("program");
    std::fs::write(&program, PROGRAM).unwrap();
    std::fs::set_permissions(
        &program,
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    )
    .unwrap();
    std::fs::write(dir.join("screen"), screen).unwrap();
}

fn session(name: &str, cwd: &Path, argv: &[&str]) -> Session {
    Session {
        id: nido::registry::new_id(),
        name: name.into(),
        cwd: cwd.to_owned(),
        argv: argv.iter().map(|s| s.to_string()).collect(),
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
    }
}

async fn wait_file(path: &Path, tries: u32) -> bool {
    for _ in 0..tries {
        if std::fs::metadata(path).is_ok_and(|m| m.len() > 0) {
            tokio::time::sleep(Duration::from_millis(100)).await;
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}

/// Los argumentos que recibió la «shell», desde `claude`.
fn claude_args(dir: &Path) -> Vec<String> {
    let raw = std::fs::read(dir.join("args")).unwrap();
    let args: Vec<String> = raw
        .split(|&b| b == 0)
        .filter(|a| !a.is_empty())
        .map(|a| String::from_utf8_lossy(a).into_owned())
        .collect();
    let from = args.iter().position(|a| a == "claude").expect("claude");
    args[from..].to_vec()
}

/// Las de nido: el channel y solo las herramientas que miran y hablan sin pedir permiso.
const FLAGS: [&str; 7] = [
    "--dangerously-load-development-channels",
    "server:nido",
    "--allowedTools",
    "mcp__nido__sessions",
    "mcp__nido__read",
    "mcp__nido__send",
    "mcp__nido__alert",
];

fn nido_mcp(home: &Path, on: bool) -> String {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_nido"))
        .args(["mcp", if on { "on" } else { "off" }])
        .env("NIDO_HOME", home.join("nido"))
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join("config"))
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    String::from_utf8(out.stdout).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn claude_launches_with_the_nido_channel() {
    let _limit = nido::watchdog::limit(Duration::from_secs(180));
    let tmp = tempfile::tempdir().unwrap();
    let _cleanup = KillHolders(tmp.path().join("nido/run"));
    let bin = tmp.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let shell = bin.join("shell");
    std::fs::write(
        &shell,
        "#!/bin/sh\nprintf '%s\\0' \"$@\" > args.tmp && mv args.tmp args\n\
         printf '%s' \"${NIDO_CHANNEL:-no}\" > channel\n\
         printf '%s' \"${NIDO_SERVER:-no}\" > server\n\
         if [ -x ./program ]; then exec ./program; fi\nexec sleep 60\n",
    )
    .unwrap();
    std::fs::set_permissions(&shell, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    // El `claude` que mira nido para saber si conoce los channels (`claude --version`): uno de
    // mentira, el primero del `PATH` (ver «Un Claude Code de antes de los channels»).
    let fake_claude = bin.join("claude");
    let claude_version = |v: &str, at: std::time::SystemTime| {
        std::fs::write(
            &fake_claude,
            format!("#!/bin/sh\necho '{v} (Claude Code)'\n"),
        )
        .unwrap();
        std::fs::set_permissions(
            &fake_claude,
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();
        // Otra fecha: nido se la vuelve a preguntar.
        std::fs::File::options()
            .write(true)
            .open(&fake_claude)
            .unwrap()
            .set_modified(at)
            .unwrap();
    };
    claude_version("2.1.322", std::time::SystemTime::now());
    // Esta prueba es su propio proceso (un fichero de tests/): cambiar el entorno no toca a otras.
    unsafe {
        let path = std::env::var_os("PATH").unwrap_or_default();
        let mut dirs = vec![bin.clone()];
        dirs.extend(std::env::split_paths(&path));
        std::env::set_var("PATH", std::env::join_paths(dirs).unwrap());
        std::env::set_var("NIDO_HOME", tmp.path().join("nido"));
        // Ni el `HOME` de verdad: nada de lo que se mire o escriba sale del temporal.
        std::env::set_var("HOME", tmp.path().join("home"));
        std::env::set_var("NIDO_EXE", env!("CARGO_BIN_EXE_nido"));
        std::env::set_var("NIDO_NAME", "Prueba");
        std::env::set_var("SHELL", &shell);
        // Aunque el gestor lo tuviera, a una sesión sin channel no le llega.
        std::env::set_var("NIDO_CHANNEL", "1");
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    // Como `nido serve`: sus sesiones, con su dirección (aunque no haya `NIDO_SERVER`).
    let paths = Paths {
        server: Some(addr),
        ..Paths::from_env()
    };
    let state = AppState::new(paths.clone(), Launcher::Detached, Some(TOKEN.into()), true).unwrap();
    server::spawn_startup_watch(state.clone());
    // Lo dado a los `nido mcp`, mirado en las transcripciones.
    server::spawn_delivery_watch(state.clone());
    {
        let state = state.clone();
        tokio::spawn(async move {
            let app = server::router(state).into_make_service_with_connect_info::<SocketAddr>();
            axum::serve(listener, app).await.unwrap()
        });
    }
    let caller = state
        .registry
        .lock()
        .await
        .add(session("padre", tmp.path(), &["true"]))
        .unwrap()
        .id;

    // `open` con el aviso de nido en pantalla: las opciones, `NIDO_CHANNEL=1` y un Enter.
    let aviso = tmp.path().join("aviso");
    folder(&aviso, &fixture("aviso_channels"));
    let (code, o) = tool(
        addr,
        &caller,
        "open",
        json!({"machine": "Prueba", "folder": aviso, "prompt": "di hola", "model": "haiku"}),
    )
    .await;
    assert_eq!(code, 200, "{o}");
    assert!(wait_file(&aviso.join("args"), 100).await, "no arrancó");
    let argv = claude_args(&aviso);
    assert_eq!(argv[1..8], FLAGS, "{argv:?}");
    assert_eq!(argv[8], "--session-id");
    assert!(
        argv.windows(2).any(|w| w == ["--model", "haiku"]),
        "{argv:?}"
    );
    // El mensaje inicial sigue siendo el mensaje (no otra herramienta permitida).
    let message = argv.last().unwrap();
    assert!(message.starts_with("[nido] You were opened by"), "{argv:?}");
    assert!(message.ends_with("\n\ndi hola"), "{message}");
    assert!(wait_file(&aviso.join("channel"), 50).await);
    assert_eq!(std::fs::read_to_string(aviso.join("channel")).unwrap(), "1");
    // Su `nido mcp` hablará con este nido.
    assert_eq!(
        std::fs::read_to_string(aviso.join("server")).unwrap(),
        addr.to_string()
    );
    assert!(
        wait_file(&aviso.join("picked"), 100).await,
        "no aceptó el aviso"
    );
    assert_eq!(std::fs::read_to_string(aviso.join("picked")).unwrap(), "\r");
    let (code, _) = tool(addr, &caller, "close", json!({"session": o["session"]})).await;
    assert_eq!(code, 200);

    // Dos que miran a la vez (p. ej. arrancada dos veces): un solo Enter.
    let doble = tmp.path().join("doble");
    folder(&doble, &fixture("aviso_channels"));
    let s = state
        .registry
        .lock()
        .await
        .add(session("doble", &doble, &["claude"]))
        .unwrap();
    nido::launch::start(&paths, Launcher::Detached, &s.id).unwrap();
    let (a, b) = tokio::join!(
        server::watch_startup(&state, &s.id, Duration::from_secs(4)),
        server::watch_startup(&state, &s.id, Duration::from_secs(4)),
    );
    assert!(a == Startup::Accept || b == Startup::Accept, "{a:?} {b:?}");
    assert_eq!(std::fs::read_to_string(doble.join("picked")).unwrap(), "\r");

    // Primero la pregunta de confiar en la carpeta (la contesta el usuario): el plazo vuelve a
    // empezar cuando se va, y luego se acepta el aviso.
    let tarde = tmp.path().join("tarde");
    folder(&tarde, &fixture("aviso_channels"));
    std::fs::write(tarde.join("before"), fixture("confianza")).unwrap();
    let s = state
        .registry
        .lock()
        .await
        .add(session("tarde", &tarde, &["claude"]))
        .unwrap();
    nido::launch::start(&paths, Launcher::Detached, &s.id).unwrap();
    let end = server::watch_startup(&state, &s.id, Duration::from_millis(1500)).await;
    assert_eq!(end, Startup::Accept);
    assert_eq!(std::fs::read_to_string(tarde.join("picked")).unwrap(), "\r");

    // Con otro channel en la lista, no se toca nada.
    let otro = tmp.path().join("otro");
    folder(&otro, &fixture("aviso_channels_otro"));
    let s = state
        .registry
        .lock()
        .await
        .add(session("otro", &otro, &["claude"]))
        .unwrap();
    nido::launch::start(&paths, Launcher::Detached, &s.id).unwrap();
    let end = server::watch_startup(&state, &s.id, Duration::from_secs(20)).await;
    assert_eq!(
        end,
        Startup::Foreign(vec!["server:nido".into(), "server:otro".into()])
    );
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!otro.join("picked").exists(), "tecleó en un aviso ajeno");
    // Con el aviso aún en pantalla, Claude tiraría lo que le llegue: la bandeja sigue retenida
    // (hasta que se conteste, o se pare la sesión).
    assert!(state.inbox.held(&s.id), "soltada con el aviso en pantalla");
    nido::launch::stop(&paths, Launcher::Detached, &s.id).unwrap();
    assert!(released(&state, &s.id).await, "no se soltó al pararla");

    // «no MCP server configured»: la sesión queda sin channel, aunque acabara de recoger.
    let sin = tmp.path().join("sin");
    folder(&sin, &fixture("channels_sin_servidor"));
    let s = state
        .registry
        .lock()
        .await
        .add(session("sin", &sin, &["claude"]))
        .unwrap();
    state.inbox.set_poll_wait(Duration::from_millis(50));
    state.inbox.poll(&s.id).await;
    assert!(state.inbox.channel(&s.id));
    nido::launch::start(&paths, Launcher::Detached, &s.id).unwrap();
    let end = server::watch_startup(&state, &s.id, Duration::from_secs(20)).await;
    assert_eq!(end, Startup::NoServer);
    assert!(!state.inbox.channel(&s.id));
    assert!(!sin.join("picked").exists());

    // La organización de la cuenta de Claude no permite channels: sin aviso, ya con su caja, y
    // Claude tiraría los mensajes. Sin channel, `send` dice por qué y no se le da nada.
    let bloq = tmp.path().join("bloqueado");
    folder(&bloq, &fixture("channels_bloqueados"));
    let s = state
        .registry
        .lock()
        .await
        .add(Session {
            // De Claude: a esas manda `send`.
            claude: Some(nido::registry::ClaudeLink {
                session_id: "conv-bloq".into(),
                config_dir: None,
                account: None,
                inside_shell: false,
            }),
            ..session("bloqueado", &bloq, &["claude"])
        })
        .unwrap();
    state
        .inbox
        .push(
            &s.id,
            nido::inbox::Msg {
                id: "b1".into(),
                from: "padre".into(),
                from_id: "Prueba/x".into(),
                machine: "Prueba".into(),
                text: "hola".into(),
                at: 1,
            },
        )
        .unwrap();
    nido::launch::start(&paths, Launcher::Detached, &s.id).unwrap();
    let end = server::watch_startup(&state, &s.id, Duration::from_secs(20)).await;
    assert_eq!(end, Startup::Blocked);
    assert_eq!(
        state.inbox.refusal(&s.id),
        Some(nido::inbox::Refusal::Blocked)
    );
    assert!(!state.inbox.channel(&s.id));
    let (code, r) = inbox(addr, &s.id, None, None).await;
    assert_eq!(code, 200, "{r}");
    assert_eq!(
        r["messages"],
        json!([]),
        "no se le da a un channel que se tira"
    );
    assert_eq!(state.inbox.pending(&s.id).len(), 1);
    let (code, e) = tool(
        addr,
        &caller,
        "send",
        json!({"session": s.id, "text": "hola"}),
    )
    .await;
    assert_eq!(code, 409, "{e}");
    assert!(
        e["error"].as_str().unwrap().contains("channelsEnabled"),
        "{e}"
    );
    let (_, list) = tool(addr, &caller, "sessions", json!({})).await;
    let me = list["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["id"] == s.id.as_str())
        .unwrap()
        .clone();
    assert_eq!(me["channel"], false, "{me}");
    assert_eq!(me["channel_error"], "blocked by org policy", "{me}");
    assert!(!bloq.join("picked").exists());
    nido::launch::stop(&paths, Launcher::Detached, &s.id).unwrap();

    // Retomada con mucha conversación: las filas de «blocked» ya no están en pantalla, pero sí
    // en lo que ha sacado desde que arrancó.
    let largo = tmp.path().join("bloqueado-largo");
    let fixture_b = fixture("channels_bloqueados");
    let (head, tail) = fixture_b.split_at(fixture_b.find('─').unwrap());
    let mut talk = String::from("> sigue con lo de ayer\n\n");
    for i in 0..80 {
        talk.push_str(&format!("● Paso {i}: hecho.\n"));
    }
    folder(&largo, &format!("{head}{talk}{tail}"));
    let s = state
        .registry
        .lock()
        .await
        .add(session("bloqueado-largo", &largo, &["claude"]))
        .unwrap();
    nido::launch::start(&paths, Launcher::Detached, &s.id).unwrap();
    let end = server::watch_startup(&state, &s.id, Duration::from_secs(20)).await;
    assert_eq!(end, Startup::Blocked);
    assert_eq!(
        state.inbox.refusal(&s.id),
        Some(nido::inbox::Refusal::Blocked)
    );
    nido::launch::stop(&paths, Launcher::Detached, &s.id).unwrap();

    // Parada, `send` la encola. Al volver a arrancarla, su `nido mcp` recoge enseguida, con el
    // aviso aún en pantalla, y Claude tira lo que le llegue antes de aceptarlo: la bandeja no da
    // nada hasta que se acepta y está su caja.
    let cola = tmp.path().join("cola");
    folder(&cola, &fixture("aviso_channels"));
    std::fs::write(cola.join("after"), fixture("normal")).unwrap();
    let s = state
        .registry
        .lock()
        .await
        .add(Session {
            claude: Some(nido::registry::ClaudeLink {
                session_id: "conv-cola".into(),
                config_dir: None,
                account: None,
                inside_shell: false,
            }),
            ..session("cola", &cola, &["claude"])
        })
        .unwrap();
    let start = format!("/api/v1/sessions/{}/start", s.id);
    let stop = format!("/api/v1/sessions/{}/stop", s.id);
    assert_eq!(rest(addr, &start).await, 200);
    assert!(wait_file(&cola.join("picked"), 100).await, "no aceptó");
    assert_eq!(rest(addr, &stop).await, 200);
    std::fs::remove_file(cola.join("picked")).unwrap();
    // Esta vez el aviso tarda en poder contestarse (3 s).
    std::fs::write(cola.join("before"), fixture("aviso_channels")).unwrap();
    // Una recogida larga en marcha al mandar (la del `nido mcp` que se muere): no se lo lleva.
    state.inbox.set_poll_wait(Duration::from_secs(3));
    let id_ = s.id.clone();
    let early = tokio::spawn(async move { inbox(addr, &id_, None, None).await });
    tokio::time::sleep(Duration::from_millis(300)).await;
    let (code, r) = tool(
        addr,
        &caller,
        "send",
        json!({"session": s.id, "text": "hola"}),
    )
    .await;
    assert_eq!(code, 200, "{r}");
    assert_eq!(r["status"], "queued", "{r}");
    let (code, r) = early.await.unwrap();
    assert_eq!(code, 200, "{r}");
    assert_eq!(r["messages"], json!([]), "se lo llevó parada");
    assert_eq!(state.inbox.pending(&s.id).len(), 1);
    // Y otra que empieza antes de arrancarla: tampoco (el aviso no se puede aceptar hasta
    // pasados 3 s).
    let id_ = s.id.clone();
    let before = tokio::spawn(async move { inbox(addr, &id_, None, None).await });
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(rest(addr, &start).await, 200);
    let (code, r) = before.await.unwrap();
    assert_eq!(code, 200, "{r}");
    assert_eq!(r["messages"], json!([]), "se lo llevó al arrancar");
    assert!(!cola.join("picked").exists());
    state.inbox.set_poll_wait(Duration::from_millis(300));
    // Como su `nido mcp`, recogiendo sin parar desde que arranca.
    let until = std::time::Instant::now() + Duration::from_secs(30);
    let got = loop {
        assert!(std::time::Instant::now() < until, "nunca se le dio");
        let accepted = cola.join("picked").exists();
        let (code, r) = inbox(addr, &s.id, None, None).await;
        assert_eq!(code, 200, "{r}");
        if r["messages"] != json!([]) {
            assert!(accepted, "se le dio con el aviso aún sin aceptar: {r}");
            break r;
        }
    };
    assert_eq!(got["messages"][0]["text"], "hola", "{got}");
    let id = got["messages"][0]["id"].as_str().unwrap().to_owned();
    let (code, r) = inbox(addr, &s.id, None, Some(json!({"ids": [id]}))).await;
    assert_eq!(code, 200, "{r}");
    assert_eq!(r["acked"], 1);
    assert_eq!(rest(addr, &stop).await, 200);

    // La de confiar en la carpeta es del usuario: mientras está, se sigue mirando, hasta el tope
    // (cinco plazos).
    let trust = tmp.path().join("confianza");
    folder(&trust, &fixture("confianza"));
    let s = state
        .registry
        .lock()
        .await
        .add(session("confianza", &trust, &["claude"]))
        .unwrap();
    nido::launch::start(&paths, Launcher::Detached, &s.id).unwrap();
    let start = std::time::Instant::now();
    let end = server::watch_startup(&state, &s.id, Duration::from_secs(1)).await;
    assert_eq!(end, Startup::Nothing);
    let took = start.elapsed();
    assert!(
        took >= Duration::from_secs(5) && took < Duration::from_secs(10),
        "{took:?}"
    );
    assert!(!trust.join("picked").exists());
    // Sigue la pregunta: retenida aún, hasta que la conteste o se pare.
    assert!(
        state.inbox.held(&s.id),
        "soltada con la pregunta en pantalla"
    );
    nido::launch::stop(&paths, Launcher::Detached, &s.id).unwrap();
    assert!(released(&state, &s.id).await, "no se soltó al pararla");
    // No la abrió nadie con `open`: no hay alerta (el usuario la tiene delante).
    let alerts = |id: &str| {
        let id = id.to_owned();
        let state = state.clone();
        async move { state.registry.lock().await.get(&id).unwrap().alerts.clone() }
    };
    assert!(alerts(&s.id).await.is_empty());

    // Una hija de `open` en esa pregunta: `open` no espera a que salga, `read` se lo dice a la
    // madre, `answer` no la contesta (es del usuario) y al usuario se le avisa con una alerta,
    // aunque viva su madre (una vez).
    let hija = tmp.path().join("confianza-hija");
    folder(&hija, &fixture("confianza"));
    let (code, o) = tool(
        addr,
        &caller,
        "open",
        json!({"machine": "Prueba", "folder": hija, "prompt": "haz algo"}),
    )
    .await;
    assert_eq!(code, 200, "{o}");
    assert!(o.get("note").is_none(), "{o}");
    let child = o["id"].as_str().unwrap().to_owned();
    let mut r = json!(null);
    for _ in 0..50 {
        let (code, got) = tool(addr, &caller, "read", json!({"session": child})).await;
        assert_eq!(code, 200, "{got}");
        r = got;
        if r.get("note").is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(r["state"], "question", "{r}");
    let note = r["note"].as_str().unwrap_or_default();
    assert!(
        note.starts_with("the session is waiting for the user to trust the folder")
            && note.contains("confianza-hija")
            && note.ends_with("(they have been told)"),
        "{r}"
    );
    let (code, e) = tool(
        addr,
        &caller,
        "answer",
        json!({"session": child, "option": "Yes, I trust this folder"}),
    )
    .await;
    assert_eq!(code, 403, "{e}");
    assert!(
        e["error"]
            .as_str()
            .unwrap()
            .starts_with("the session is waiting for the user to trust the folder"),
        "{e}"
    );
    assert!(!hija.join("picked").exists(), "contestada");
    let mut told = vec![];
    for _ in 0..50 {
        told = alerts(&child).await;
        if !told.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(told.len(), 1, "{told:?}");
    assert!(told[0].text.contains("confíes en la carpeta"), "{told:?}");
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(alerts(&child).await.len(), 1, "una vez");
    let (code, r) = tool(addr, &caller, "close", json!({"session": child})).await;
    assert_eq!(code, 200, "{r}");

    // Claude en otra pantalla todo el plazo (iniciar sesión, aprobar un MCP, una novedad…): no
    // se sabe si tira el channel. Retenida, hasta que se vea su caja o se cierre.
    let otra = tmp.path().join("otra-pantalla");
    folder(
        &otra,
        "Select login method:\n\n❯ 1. Claude account with subscription\n  2. Anthropic Console account\n",
    );
    let s = state
        .registry
        .lock()
        .await
        .add(session("otra-pantalla", &otra, &["claude"]))
        .unwrap();
    nido::launch::start(&paths, Launcher::Detached, &s.id).unwrap();
    let end = server::watch_startup(&state, &s.id, Duration::from_secs(1)).await;
    assert_eq!(end, Startup::Nothing);
    assert!(state.inbox.held(&s.id), "soltada sin mirar");
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(state.inbox.held(&s.id), "soltada sin mirar, después");
    nido::launch::stop(&paths, Launcher::Detached, &s.id).unwrap();
    assert!(released(&state, &s.id).await, "no se soltó al pararla");

    // Su guardián atiende pero no contesta en todo el plazo: tampoco se suelta. Cuando de
    // verdad no hay nadie, sí.
    let s = state
        .registry
        .lock()
        .await
        .add(session("mudo", tmp.path(), &["claude"]))
        .unwrap();
    let socket = paths.socket(&s.id);
    std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let (quiet_tx, mut quiet_rx) = tokio::sync::oneshot::channel::<()>();
    let mute = tokio::spawn(async move {
        let mut open = Vec::new();
        loop {
            tokio::select! {
                Ok((c, _)) = listener.accept() => open.push(c),
                _ = &mut quiet_rx => return,
            }
        }
    });
    let end = server::watch_startup(&state, &s.id, Duration::from_secs(1)).await;
    assert_eq!(end, Startup::Nothing);
    assert!(state.inbox.held(&s.id), "soltada sin mirar");
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(state.inbox.held(&s.id), "soltada sin mirar, después");
    let _ = quiet_tx.send(());
    mute.await.unwrap();
    std::fs::remove_file(&socket).unwrap();
    assert!(released(&state, &s.id).await, "no se soltó sin guardián");

    // Su Claude se va sin haber enseñado su caja, y la sesión sigue (p. ej. una shell de login
    // que no le hace `exec`): nadie recogerá la bandeja. Sin channel hasta que se vuelva a lanzar
    // (sin soltarla), y se deja de mirar; lo que espera sigue en la cola.
    let ida = tmp.path().join("sin-claude");
    std::fs::create_dir_all(&ida).unwrap();
    std::os::unix::fs::symlink("/bin/bash", ida.join("claude")).unwrap();
    std::fs::write(
        ida.join("screen"),
        "Select login method:\n\n❯ 1. Claude account with subscription\n",
    )
    .unwrap();
    // `sleep 5 & wait`, y no `sleep 5` a secas: con una sola orden, bash se convierte en ella
    // (`exec`) y ya no hay ningún proceso que se llame `claude`.
    std::fs::write(
        ida.join("program"),
        "#!/bin/sh\nprintf '\\033[2J\\033[H'; cat screen\n./claude -c 'sleep 5 & wait'\n\
         printf '\\033[2J\\033[H$ '\nexec sleep 60\n",
    )
    .unwrap();
    std::fs::set_permissions(
        ida.join("program"),
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    )
    .unwrap();
    let s = state
        .registry
        .lock()
        .await
        .add(Session {
            claude: Some(nido::registry::ClaudeLink {
                session_id: "conv-ida".into(),
                config_dir: None,
                account: None,
                inside_shell: false,
            }),
            ..session("sin-claude", &ida, &["claude"])
        })
        .unwrap();
    state
        .inbox
        .push(
            &s.id,
            nido::inbox::Msg {
                id: "i1".into(),
                from: "padre".into(),
                from_id: "Prueba/x".into(),
                machine: "Prueba".into(),
                text: "hola".into(),
                at: 1,
            },
        )
        .unwrap();
    nido::launch::start(&paths, Launcher::Detached, &s.id).unwrap();
    let end = server::watch_startup(&state, &s.id, Duration::from_secs(1)).await;
    assert_eq!(end, Startup::Nothing);
    assert!(state.inbox.held(&s.id), "soltada sin mirar");
    let mut gone = false;
    for _ in 0..150 {
        if state.inbox.refusal(&s.id) == Some(nido::inbox::Refusal::NoClaude) {
            gone = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(gone, "no vio que su Claude se fue");
    assert!(nido::launch::is_alive(&paths, &s.id), "la shell sigue");
    assert!(state.inbox.held(&s.id));
    assert!(!state.inbox.channel(&s.id));
    assert_eq!(state.inbox.pending(&s.id).len(), 1, "sigue en la cola");
    let (code, e) = tool(
        addr,
        &caller,
        "send",
        json!({"session": s.id, "text": "hola"}),
    )
    .await;
    assert_eq!(code, 409, "{e}");
    let why = e["error"].as_str().unwrap();
    assert!(
        why.contains("not running") || why.contains("no está en marcha"),
        "{e}"
    );
    let (_, list) = tool(addr, &caller, "sessions", json!({})).await;
    let me = list["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["id"] == s.id.as_str())
        .unwrap()
        .clone();
    assert_eq!(me["channel_error"], "claude not running", "{me}");
    // Ya no se mira: solo lo de este arranque la retiene.
    state.inbox.relaunched(&s.id);
    assert!(released(&state, &s.id).await, "se sigue mirando");
    assert_eq!(state.inbox.pending(&s.id).len(), 1);
    nido::launch::stop(&paths, Launcher::Detached, &s.id).unwrap();

    // Reiniciada mientras se mira su bandeja retenida (ya visto su Claude): lo que mira el
    // arranque de antes no toca el nuevo, aunque su Claude tarde en aparecer. El mensaje llega.
    let again = tmp.path().join("reiniciada");
    std::fs::create_dir_all(&again).unwrap();
    std::os::unix::fs::symlink("/bin/bash", again.join("claude")).unwrap();
    std::fs::write(
        again.join("screen"),
        "Select login method:\n\n❯ 1. Claude account with subscription\n",
    )
    .unwrap();
    std::fs::write(again.join("normal"), fixture("normal")).unwrap();
    // La segunda vez, 6 s sin Claude antes de enseñar su caja. (Con `& wait`: ver «sin-claude».)
    std::fs::write(
        again.join("program"),
        "#!/bin/sh\nif [ -f again ]; then sleep 6; printf '\\033[2J\\033[H'; cat normal; \
         exec ./claude -c 'sleep 60 & wait'; fi\ntouch again\n\
         printf '\\033[2J\\033[H'; cat screen\nexec ./claude -c 'sleep 60 & wait'\n",
    )
    .unwrap();
    std::fs::set_permissions(
        again.join("program"),
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    )
    .unwrap();
    let s = state
        .registry
        .lock()
        .await
        .add(Session {
            claude: Some(nido::registry::ClaudeLink {
                session_id: "conv-again".into(),
                config_dir: None,
                account: None,
                inside_shell: false,
            }),
            ..session("reiniciada", &again, &["claude"])
        })
        .unwrap();
    nido::launch::start(&paths, Launcher::Detached, &s.id).unwrap();
    let end = server::watch_startup(&state, &s.id, Duration::from_secs(1)).await;
    assert_eq!(end, Startup::Nothing);
    // Que `keep_held` vea su Claude.
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(state.inbox.held(&s.id));
    assert_eq!(state.inbox.refusal(&s.id), None);
    state
        .inbox
        .push(
            &s.id,
            nido::inbox::Msg {
                id: "r1".into(),
                from: "padre".into(),
                from_id: "Prueba/x".into(),
                machine: "Prueba".into(),
                text: "otra vez".into(),
                at: 1,
            },
        )
        .unwrap();
    let restart = format!("/api/v1/sessions/{}/restart", s.id);
    assert_eq!(rest(addr, &restart).await, 200);
    let until = std::time::Instant::now() + Duration::from_secs(30);
    let got = loop {
        assert!(std::time::Instant::now() < until, "nunca se le dio");
        assert_eq!(
            state.inbox.refusal(&s.id),
            None,
            "lo de antes rechazó el arranque nuevo"
        );
        let (code, r) = inbox(addr, &s.id, None, None).await;
        assert_eq!(code, 200, "{r}");
        if r["messages"] != json!([]) {
            break r;
        }
    };
    assert_eq!(got["messages"][0]["id"], "r1", "{got}");
    let (code, r) = inbox(addr, &s.id, None, Some(json!({"ids": ["r1"]}))).await;
    assert_eq!(code, 200, "{r}");
    assert_eq!(r["acked"], 1);
    assert_eq!(state.inbox.refusal(&s.id), None);
    let stop = format!("/api/v1/sessions/{}/stop", s.id);
    assert_eq!(rest(addr, &stop).await, 200);

    // Claude tira el primer evento del channel (le pasa justo tras saludar): el mensaje no se ve
    // en su transcripción, se vuelve a dar y el mismo `nido mcp` lo mete otra vez; al verse con
    // su id, fuera de la bandeja, y no se da más. Un «claude» de mentira (bash por un enlace
    // `claude`) con su `nido mcp` de verdad, hablándole por dos tuberías: el primer evento lo
    // tira, el segundo lo apunta en su transcripción como lo hace Claude. Retomada (ya tiene
    // transcripción) y nueva (aún no: la escribe al meter el segundo).
    state.inbox.set_confirm_wait(
        Duration::from_secs(3),
        Duration::from_millis(500),
        Duration::from_secs(1),
    );
    std::fs::write(paths.token(), TOKEN).unwrap();
    for (name, conv, msg, resumed) in [
        ("reintento", "conv-retry", "t1", true),
        ("reintento-nueva", "conv-fresh", "t2", false),
    ] {
        let retry = tmp.path().join(name);
        std::fs::create_dir_all(&retry).unwrap();
        std::os::unix::fs::symlink("/bin/bash", retry.join("claude")).unwrap();
        std::fs::write(retry.join("screen"), fixture("normal")).unwrap();
        std::fs::write(
            retry.join("program"),
            "#!/bin/sh\nprintf '\\033[2J\\033[H'; cat screen\nexec ./claude fake.sh\n",
        )
        .unwrap();
        std::fs::set_permissions(
            retry.join("program"),
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();
        std::fs::write(
            retry.join("fake.sh"),
            r#"T="$HOME/.claude/projects/p/CONV.jsonl"
rm -f in out; mkfifo in out
"$NIDO_EXE" mcp < in > out 2> mcp.err &
exec 3> in 4< out
echo '{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"falso","version":"1"}}}' >&3
echo '{"jsonrpc":"2.0","method":"notifications/initialized"}' >&3
echo '{"jsonrpc":"2.0","id":1,"method":"tools/list"}' >&3
n=0
while IFS= read -r l <&4; do
  case "$l" in
  *notifications/claude/channel*)
    n=$((n+1))
    printf '%s\n' "$l" >> events
    if [ "$n" -ge 2 ]; then
      id=$(printf '%s' "$l" | sed -n 's/.*"msg_id":"\([^"]*\)".*/\1/p')
      mkdir -p "$(dirname "$T")"
      printf '{"type":"user","origin":{"kind":"channel","server":"nido"},"message":{"role":"user","content":"<channel source=\\"nido\\" msg_id=\\"%s\\">\\nhola\\n</channel>"}}\n' "$id" >> "$T"
    fi;;
  esac
done
"#
            .replace("CONV", conv),
        )
        .unwrap();
        let transcript = tmp
            .path()
            .join(format!("home/.claude/projects/p/{conv}.jsonl"));
        if resumed {
            std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
            std::fs::write(
                &transcript,
                "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"empieza\"}}\n",
            )
            .unwrap();
        }
        let s = state
            .registry
            .lock()
            .await
            .add(Session {
                claude: Some(nido::registry::ClaudeLink {
                    session_id: conv.into(),
                    config_dir: None,
                    account: None,
                    inside_shell: false,
                }),
                ..session(name, &retry, &["claude"])
            })
            .unwrap();
        state
            .inbox
            .push(
                &s.id,
                nido::inbox::Msg {
                    id: msg.into(),
                    from: "padre".into(),
                    from_id: "Prueba/x".into(),
                    machine: "Prueba".into(),
                    text: "hola".into(),
                    at: 1,
                },
            )
            .unwrap();
        let start = format!("/api/v1/sessions/{}/start", s.id);
        assert_eq!(rest(addr, &start).await, 200);
        let until = std::time::Instant::now() + Duration::from_secs(40);
        while !state.inbox.pending(&s.id).is_empty() {
            // Como `spawn_scanner` (aquí no corre): si está viva y si Claude trabaja.
            state.scan().await;
            let events = std::fs::read_to_string(retry.join("events")).unwrap_or_default();
            let err = std::fs::read_to_string(retry.join("mcp.err")).unwrap_or_default();
            assert!(
                std::time::Instant::now() < until,
                "{name}: no se vio en su transcripción: {:?}; eventos: {events}; nido mcp: {err}",
                state.inbox.out(&s.id, msg)
            );
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        let events = || -> Vec<Value> {
            std::fs::read_to_string(retry.join("events"))
                .unwrap()
                .lines()
                .map(|l| serde_json::from_str(l).unwrap())
                .collect()
        };
        let got = events();
        assert_eq!(got.len(), 2, "{name}: {got:?}");
        for e in &got {
            assert_eq!(e["params"]["content"], "hola");
            assert_eq!(e["params"]["meta"]["msg_id"], msg);
        }
        assert!(transcript.is_file(), "{name}");
        // Visto: no se vuelve a dar.
        tokio::time::sleep(Duration::from_secs(5)).await;
        assert_eq!(events().len(), 2, "{name}");
        assert!(!state.inbox.has(&s.id, msg));
        let stop = format!("/api/v1/sessions/{}/stop", s.id);
        assert_eq!(rest(addr, &stop).await, 200);
    }

    // Un Claude Code de antes de los channels no conoce sus opciones (no arrancaría): sin ellas,
    // sin `NIDO_CHANNEL`, y `sessions` dice por qué no tiene channel.
    claude_version(
        "2.0.14",
        std::time::SystemTime::now() + Duration::from_secs(60),
    );
    let viejo = tmp.path().join("viejo");
    folder(&viejo, &fixture("normal"));
    let (code, o) = tool(
        addr,
        &caller,
        "open",
        json!({"machine": "Prueba", "folder": viejo, "prompt": "di hola"}),
    )
    .await;
    assert_eq!(code, 200, "{o}");
    assert!(wait_file(&viejo.join("args"), 100).await, "no arrancó");
    let argv = claude_args(&viejo);
    assert_eq!(argv[1], "--session-id", "{argv:?}");
    assert!(
        !argv.iter().any(|a| FLAGS.contains(&a.as_str())),
        "{argv:?}"
    );
    assert!(wait_file(&viejo.join("channel"), 50).await);
    assert_eq!(
        std::fs::read_to_string(viejo.join("channel")).unwrap(),
        "no"
    );
    let child = o["id"].as_str().unwrap().to_owned();
    let mut error = Value::Null;
    for _ in 0..50 {
        let (_, list) = tool(addr, &caller, "sessions", json!({})).await;
        error = list["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["id"] == child.as_str())
            .unwrap()["channel_error"]
            .clone();
        if !error.is_null() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(error, "claude too old for channels");
    let (code, r) = tool(addr, &caller, "close", json!({"session": child})).await;
    assert_eq!(code, 200, "{r}");
    claude_version(
        "2.1.322",
        std::time::SystemTime::now() + Duration::from_secs(120),
    );

    // Apagado: la sesión siguiente va sin opciones, sin `NIDO_CHANNEL` y sin que nadie teclee.
    assert!(nido_mcp(tmp.path(), false).contains("apagado"));
    assert!(paths.config.join("mcp-off").exists());
    let off = tmp.path().join("apagado");
    folder(&off, &fixture("aviso_channels"));
    let (code, o) = tool(
        addr,
        &caller,
        "open",
        json!({"machine": "Prueba", "folder": off, "prompt": "di hola"}),
    )
    .await;
    assert_eq!(code, 200, "{o}");
    assert!(wait_file(&off.join("args"), 100).await, "no arrancó");
    let argv = claude_args(&off);
    assert_eq!(argv[1], "--session-id", "{argv:?}");
    assert!(!argv.iter().any(|a| a == FLAGS[0]), "{argv:?}");
    assert!(wait_file(&off.join("channel"), 50).await);
    assert_eq!(std::fs::read_to_string(off.join("channel")).unwrap(), "no");
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(
        !off.join("picked").exists(),
        "con el MCP apagado no se acepta"
    );
    assert!(nido_mcp(tmp.path(), true).contains("encendido"));
    assert!(!paths.config.join("mcp-off").exists());

    // La bandeja es del Claude de la sesión: otro `claude` de dentro (con su `NIDO_SESSION` y
    // `NIDO_CHANNEL` heredados) recibe 409 y no se lleva los mensajes. Un «claude» de mentira
    // (bash por un enlace `claude`: es su nombre) con dos hijos: el `nido mcp` suyo y otro
    // `claude` con el suyo.
    state.inbox.set_poll_wait(Duration::from_millis(300));
    let dir = tmp.path().join("bandeja");
    std::fs::create_dir_all(&dir).unwrap();
    // Un enlace, no una copia: macOS mata la copia de un binario del sistema (su firma).
    std::os::unix::fs::symlink("/bin/bash", dir.join("claude")).unwrap();
    std::fs::write(
        dir.join("program"),
        "#!/bin/sh\nexec ./claude -c 'echo $$ > claude.pid; sleep 60 & echo $! > mine; \
         ./claude -c \"sleep 60 & echo \\$! > nested; wait\" & wait'\n",
    )
    .unwrap();
    std::fs::set_permissions(
        dir.join("program"),
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    )
    .unwrap();
    let s = state
        .registry
        .lock()
        .await
        .add(session("bandeja", &dir, &["claude"]))
        .unwrap();
    let msg = |id: &str| nido::inbox::Msg {
        id: id.into(),
        from: "padre".into(),
        from_id: "Prueba/x".into(),
        machine: "Prueba".into(),
        text: "hola".into(),
        at: 1,
    };
    state.inbox.push(&s.id, msg("m1")).unwrap();
    // Parada (o arrancando, o el `nido mcp` de antes de reiniciarla): sin saber cuál es su
    // Claude, ni mensajes ni channel, para nadie.
    let me = std::process::id();
    let (code, r) = inbox(addr, &s.id, Some(me), None).await;
    assert_eq!(code, 200, "{r}");
    assert_eq!(r["messages"], json!([]));
    let (code, r) = inbox(addr, &s.id, Some(me), Some(json!({"ids": ["m1"]}))).await;
    assert_eq!(code, 200, "{r}");
    assert_eq!(r["acked"], 0);
    assert!(!state.inbox.channel(&s.id));
    assert_eq!(state.inbox.pending(&s.id).len(), 1);

    nido::launch::start(&paths, Launcher::Detached, &s.id).unwrap();
    for f in ["claude.pid", "mine", "nested"] {
        assert!(wait_file(&dir.join(f), 100).await, "sin {f}");
    }
    let pid = |f: &str| -> u32 {
        std::fs::read_to_string(dir.join(f))
            .unwrap()
            .trim()
            .parse()
            .unwrap()
    };
    let (claude, mine, nested) = (pid("claude.pid"), pid("mine"), pid("nested"));
    // Lo que Claude apunta de sí mismo: así lo encuentra nido.
    let sessions = tmp.path().join("home/.claude/sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    std::fs::write(
        sessions.join(format!("{claude}.json")),
        r#"{"sessionId":"conv-b"}"#,
    )
    .unwrap();
    let (code, r) = inbox(addr, &s.id, Some(nested), None).await;
    assert_eq!(code, 409, "{r}");
    assert_eq!(r["error"], "not this session's claude");
    let (code, _) = inbox(addr, &s.id, Some(nested), Some(json!({"ids": ["m1"]}))).await;
    assert_eq!(code, 409, "tampoco confirma");
    assert!(
        !state.inbox.channel(&s.id),
        "rechazado, no marca el channel"
    );
    let (code, _) = inbox(addr, &s.id, Some(me), None).await;
    assert_eq!(code, 409, "sin Claude encima");
    let (code, r) = inbox(addr, &s.id, Some(mine), None).await;
    assert_eq!(code, 200, "{r}");
    assert_eq!(r["messages"][0]["id"], "m1");
    assert!(state.inbox.channel(&s.id));
    let (code, r) = inbox(addr, &s.id, Some(mine), Some(json!({"ids": ["m1"]}))).await;
    assert_eq!(code, 200, "{r}");
    assert_eq!(r["acked"], 1);
    // Sin lo que Claude apunta de sí mismo (otra versión, o en Windows con su perfil): el primer
    // Claude del árbol de la sesión, igual.
    std::fs::remove_file(sessions.join(format!("{claude}.json"))).unwrap();
    state.inbox.push(&s.id, msg("m2")).unwrap();
    let (code, r) = inbox(addr, &s.id, Some(nested), None).await;
    assert_eq!(code, 409, "{r}");
    let (code, r) = inbox(addr, &s.id, Some(mine), None).await;
    assert_eq!(code, 200, "{r}");
    assert_eq!(r["messages"][0]["id"], "m2");
    // Sin pid (un `nido mcp` que no lo manda), como siempre.
    let (code, _) = inbox(addr, &s.id, None, None).await;
    assert_eq!(code, 200);
    nido::launch::stop(&paths, Launcher::Detached, &s.id).unwrap();
    for p in [mine, nested] {
        unsafe {
            libc::kill(p as i32, libc::SIGKILL);
        }
    }
}

/// Espera (hasta 10 s) a que la bandeja de `id` deje de estar retenida.
async fn released(state: &AppState, id: &str) -> bool {
    for _ in 0..100 {
        if !state.inbox.held(id) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}

/// Un `POST` sin cuerpo a la API de la consola (arrancar, parar): su código.
async fn rest(addr: SocketAddr, path: &str) -> u16 {
    let path = path.to_owned();
    tokio::task::spawn_blocking(move || {
        let api = Api {
            addr,
            token: Some(TOKEN.into()),
        };
        api.request("POST", &path, None).unwrap().0
    })
    .await
    .unwrap()
}

/// La bandeja como la pide `nido mcp`: `GET` para recoger, o `POST …/ack` con `ack`.
async fn inbox(
    addr: SocketAddr,
    session: &str,
    pid: Option<u32>,
    ack: Option<Value>,
) -> (u16, Value) {
    let session = session.to_owned();
    tokio::task::spawn_blocking(move || {
        let api = Api {
            addr,
            token: Some(TOKEN.into()),
        };
        let pid = pid.map(|p| p.to_string());
        let mut headers = vec![("X-Nido-Session", session.as_str())];
        if let Some(p) = &pid {
            headers.push(("X-Nido-Mcp-Pid", p.as_str()));
        }
        let (method, path) = match &ack {
            Some(_) => ("POST", "/api/v1/mcp/inbox/ack"),
            None => ("GET", "/api/v1/mcp/inbox"),
        };
        api.request_with(
            method,
            path,
            ack.as_ref(),
            &headers,
            Some(Duration::from_secs(30)),
        )
        .unwrap()
    })
    .await
    .unwrap()
}

/// El `nido mcp` de una sesión de un nido con `NIDO_HOME` usa el token de ese nido: ni crea ni
/// lee el de verdad.
#[test]
fn the_console_token_follows_nido_home() {
    let _limit = nido::watchdog::limit(Duration::from_secs(60));
    let tmp = tempfile::tempdir().unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_nido"))
        .arg("mcp")
        .env("NIDO_HOME", tmp.path().join("nido"))
        .env("HOME", tmp.path().join("home"))
        .env("XDG_CONFIG_HOME", tmp.path().join("config"))
        .env_remove("NIDO_SESSION")
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert!(tmp.path().join("nido/config/token").exists(), "{out:?}");
    assert!(!tmp.path().join("config/nido/token").exists());
    assert!(!tmp.path().join("home/.config/nido/token").exists());
}
