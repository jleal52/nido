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
/// lleguen, tal cual (Enter es `\r`): la primera, y tras ella, quitada la pantalla, las demás.
const PROGRAM: &str = r#"#!/bin/sh
stty -icanon -echo -icrnl min 1 time 0
if [ -f before ]; then printf '\033[2J\033[H'; cat before; sleep 3; fi
printf '\033[2J\033[H'; cat screen
dd bs=1 count=1 of=picked.tmp 2>/dev/null && mv picked.tmp picked
printf '\033[2J\033[Hlisto\n'
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

const FLAGS: [&str; 4] = [
    "--dangerously-load-development-channels",
    "server:nido",
    "--allowedTools",
    "mcp__nido__*",
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
         if [ -x ./program ]; then exec ./program; fi\nexec sleep 60\n",
    )
    .unwrap();
    std::fs::set_permissions(&shell, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    // Esta prueba es su propio proceso (un fichero de tests/): cambiar el entorno no toca a otras.
    unsafe {
        std::env::set_var("NIDO_HOME", tmp.path().join("nido"));
        // Ni el `HOME` de verdad: nada de lo que se mire o escriba sale del temporal.
        std::env::set_var("HOME", tmp.path().join("home"));
        std::env::set_var("NIDO_EXE", env!("CARGO_BIN_EXE_nido"));
        std::env::set_var("NIDO_NAME", "Prueba");
        std::env::set_var("SHELL", &shell);
        // Aunque el gestor lo tuviera, a una sesión sin channel no le llega.
        std::env::set_var("NIDO_CHANNEL", "1");
    }
    let paths = Paths::from_env();
    let state = AppState::new(paths.clone(), Launcher::Detached, Some(TOKEN.into()), true).unwrap();
    server::spawn_startup_watch(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
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
    assert_eq!(argv[1..5], FLAGS, "{argv:?}");
    assert_eq!(argv[5], "--session-id");
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
    assert_eq!(r["removed"], 0);
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
    assert_eq!(r["removed"], 1);
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
