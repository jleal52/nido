//! La API local del MCP (`/api/v1/mcp/<herramienta>`) de punta a punta, en un directorio
//! temporal. Sin Claude de verdad: las sesiones de Claude se apuntan en el registro con una
//! transcripción escrita a mano, y las que abre `open` corren con una «shell» falsa que apunta
//! sus argumentos y se queda dormida.

// Con sh, sockets Unix y señales: solo en Unix.
#![cfg(unix)]

use std::io::Write;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use nido::client::Api;
use nido::launch::{Launcher, Paths};
use nido::registry::{ClaudeLink, Session};
use nido::server::{self, AppState};
use serde_json::{Value, json};

const TOKEN: &str = "0123456789abcdef0123456789abcdef";

/// Una herramienta, como la llama `nido mcp`: sin `Accept-Language` (en inglés) y con la sesión
/// que llama en `X-Nido-Session` (`None`: sin la cabecera).
async fn tool(addr: SocketAddr, caller: Option<&str>, name: &str, args: Value) -> (u16, Value) {
    tool_as(addr, TOKEN, caller, name, args).await
}

/// Como `tool`, con otro token.
async fn tool_as(
    addr: SocketAddr,
    token: &str,
    caller: Option<&str>,
    name: &str,
    args: Value,
) -> (u16, Value) {
    let caller = caller.map(str::to_owned);
    let token = token.to_owned();
    let path = format!("/api/v1/mcp/{name}");
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
            "POST",
            &path,
            Some(&args),
            &headers,
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

fn roles_texts(read: &Value) -> Vec<(String, String)> {
    read["turns"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| {
            (
                t["role"].as_str().unwrap().to_owned(),
                t["text"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

/// Un programa que hace de Claude con un menú en pantalla: enseña `screen1`, apunta en `picked`
/// la tecla que le llega y, si hay `screen2` (el cursor ya en el campo de texto), la enseña y
/// apunta en `typed` lo que se escriba hasta Enter. Luego, otra pantalla sin menú.
const MENU_PROGRAM: &str = r#"#!/bin/sh
stty -icanon -echo min 1 time 0
printf '\033[2J\033[H'; cat screen1
k=$(dd bs=1 count=1 2>/dev/null)
printf '%s' "$k" > picked
if [ -f screen2 ]; then
  printf '\033[2J\033[H'; cat screen2
  cr=$(printf '\r')
  t=''
  while c=$(dd bs=1 count=1 2>/dev/null); [ -n "$c" ] && [ "$c" != "$cr" ]; do t="$t$c"; done
  printf '%s' "$t" > typed
fi
printf '\033[2J\033[Hlisto\n'
exec sleep 60
"#;

fn fixture(name: &str) -> String {
    std::fs::read_to_string(format!(
        "{}/tests/pantallas/{name}.txt",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
}

/// Una carpeta con `program` (ver `MENU_PROGRAM`) y sus pantallas.
fn menu_folder(dir: &Path, screen1: &str, screen2: Option<&str>) {
    std::fs::create_dir_all(dir).unwrap();
    let program = dir.join("program");
    std::fs::write(&program, MENU_PROGRAM).unwrap();
    std::fs::set_permissions(
        &program,
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    )
    .unwrap();
    std::fs::write(dir.join("screen1"), screen1).unwrap();
    if let Some(s) = screen2 {
        std::fs::write(dir.join("screen2"), s).unwrap();
    }
}

async fn read_file(path: &Path) -> Option<String> {
    wait_file(path)
        .await
        .then(|| std::fs::read_to_string(path).unwrap())
}

async fn wait_file(path: &Path) -> bool {
    for _ in 0..100 {
        if std::fs::metadata(path).is_ok_and(|m| m.len() > 0) {
            // Que haya terminado de escribirlo.
            tokio::time::sleep(Duration::from_millis(100)).await;
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}

#[tokio::test(flavor = "multi_thread")]
async fn mcp_api_end_to_end() {
    let tmp = tempfile::tempdir().unwrap();
    let _cleanup = KillHolders(tmp.path().join("nido/run"));
    // La «shell» de las sesiones: apunta sus argumentos en la carpeta de la sesión y duerme (o
    // corre el `program` que haya en ella). Así `open` no arranca ningún Claude.
    let bin = tmp.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let shell = bin.join("shell");
    std::fs::write(
        &shell,
        "#!/bin/sh\nprintf '%s\\0' \"$@\" > args.tmp && mv args.tmp args\n\
         if [ -x ./program ]; then exec ./program; fi\nexec sleep 60\n",
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

    // La sesión que llama (no hace falta que corra) y otra de Claude, parada, con su conversación.
    let config = tmp.path().join("claude");
    let transcript_dir = config.join("projects").join("-tmp-x");
    std::fs::create_dir_all(&transcript_dir).unwrap();
    let transcript = transcript_dir.join("conv-1.jsonl");
    append(
        &transcript,
        &[
            json!({"type":"user","uuid":"u1","message":{"content":"hola"}}),
            json!({"type":"assistant","uuid":"a1","message":{"content":[
                {"type":"text","text":"Voy a mirar."},
                {"type":"tool_use","id":"t-1","name":"Bash","input":{"command":"ls"}}]}}),
        ],
    );
    let link = ClaudeLink {
        session_id: "conv-1".into(),
        config_dir: Some(config.clone()),
        account: None,
        inside_shell: false,
    };
    let (caller, other) = {
        let mut reg = state.registry.lock().await;
        let caller = reg.add(session("padre", tmp.path(), None)).unwrap();
        let other = reg
            .add(session("otra", tmp.path(), Some(link.clone())))
            .unwrap();
        (caller.id, other.id)
    };
    let me = Some(caller.as_str());

    // Solo desde una sesión de nido.
    let (code, e) = tool(addr, None, "sessions", json!({})).await;
    assert_eq!(code, 403, "{e}");
    assert_eq!(e["error"], "not called from a nido session");
    let (code, _) = tool(addr, Some("nadie"), "sessions", json!({})).await;
    assert_eq!(code, 403);
    let (code, e) = tool(addr, me, "sessions", json!({"x": 1})).await;
    assert_eq!(code, 400, "{e}");
    let (code, _) = tool(addr, me, "nada", json!({})).await;
    assert_eq!(code, 404);
    // Con el token de un teléfono, no.
    let phone = state
        .phones
        .lock()
        .unwrap()
        .add(&[7u8; 32], "teléfono", None)
        .unwrap();
    let (code, e) = tool_as(addr, &phone.token, me, "sessions", json!({})).await;
    assert_eq!(code, 403, "{e}");
    assert_eq!(e["error"], "the MCP API only takes this machine's token");

    // `sessions`: solo las de Claude, con su dirección; parada. Sin más equipos en el grupo,
    // nadie sin contestar.
    let (code, list) = tool(addr, me, "sessions", json!({})).await;
    assert_eq!(code, 200, "{list}");
    assert_eq!(
        list.as_object().unwrap().keys().collect::<Vec<_>>(),
        ["sessions"],
        "{list}"
    );
    let list = list["sessions"].as_array().unwrap();
    assert_eq!(list.len(), 1, "{list:?}");
    let s = &list[0];
    assert_eq!(s["session"], format!("Prueba/{other}"));
    assert_eq!(s["machine"], "Prueba");
    assert_eq!(s["machine_id"], "Prueba", "sin hub, el nombre");
    assert_eq!(s["id"], other.as_str());
    assert_eq!(s["name"], "otra");
    assert_eq!(s["state"], "stopped");
    assert_eq!(s["mine"], false);
    assert_eq!(s["channel"], false);
    assert!(s["opened_by"].is_null());

    // `read`: la primera vez, todo; luego, solo lo nuevo.
    let addr_other = format!("Prueba/{other}");
    let (code, r) = tool(addr, me, "read", json!({"session": addr_other})).await;
    assert_eq!(code, 200, "{r}");
    assert_eq!(r["state"], "stopped");
    assert!(
        r.get("question").is_none() && r.get("truncated").is_none(),
        "{r}"
    );
    assert_eq!(
        roles_texts(&r),
        vec![
            ("user".into(), "hola".into()),
            ("assistant".into(), "Voy a mirar.".into()),
            ("tool".into(), "Bash: ls".into()),
        ]
    );
    let (_, r) = tool(addr, me, "read", json!({"session": addr_other})).await;
    assert_eq!(roles_texts(&r), vec![], "{r}");
    append(
        &transcript,
        &[
            json!({"type":"assistant","uuid":"a2","message":{"content":[{"type":"text","text":"Hecho."}]}}),
        ],
    );
    // Con el equipo en minúsculas o sin equipo, la misma.
    let (_, r) = tool(
        addr,
        me,
        "read",
        json!({"session": format!("prueba/{other}")}),
    )
    .await;
    assert_eq!(roles_texts(&r), vec![("assistant".into(), "Hecho.".into())]);
    let (_, r) = tool(addr, me, "read", json!({"session": other, "last": 2})).await;
    assert_eq!(
        roles_texts(&r),
        vec![
            ("tool".into(), "Bash: ls".into()),
            ("assistant".into(), "Hecho.".into()),
        ]
    );
    // `last` no mueve la posición: lo nuevo sigue siendo nuevo después.
    let a3 = json!({"type":"assistant","uuid":"a3","message":{"content":[{"type":"text","text":"Otra cosa."}]}});
    append(&transcript, &[a3]);
    let (_, r) = tool(addr, me, "read", json!({"session": other, "last": 1})).await;
    assert_eq!(
        roles_texts(&r),
        vec![("assistant".into(), "Otra cosa.".into())]
    );
    let (_, r) = tool(addr, me, "read", json!({"session": other})).await;
    assert_eq!(
        roles_texts(&r),
        vec![("assistant".into(), "Otra cosa.".into())]
    );
    // Dos lecturas a la vez: lo nuevo sale en una sola.
    for round in 0..5 {
        let line = json!({"type":"assistant","uuid":format!("p{round}"),"message":{"content":[{"type":"text","text":format!("a la vez {round}")}]}});
        append(&transcript, &[line]);
        let ((_, r1), (_, r2)) = tokio::join!(
            tool(addr, me, "read", json!({"session": other})),
            tool(addr, me, "read", json!({"session": other})),
        );
        let mut both = roles_texts(&r1);
        both.extend(roles_texts(&r2));
        assert_eq!(
            both,
            vec![("assistant".into(), format!("a la vez {round}"))]
        );
    }
    // Por nombre, no: solo `equipo/id` o el id.
    let (code, _) = tool(addr, me, "read", json!({"session": "otra"})).await;
    assert_eq!(code, 404);
    let (code, _) = tool(addr, me, "read", json!({"session": "Prueba/otra"})).await;
    assert_eq!(code, 404);
    // Con `wait` y parada, no espera.
    let (code, r) = tool(addr, me, "read", json!({"session": other, "wait": true})).await;
    assert_eq!(code, 200);
    assert_eq!(r["state"], "stopped");
    let (code, e) = tool(
        addr,
        me,
        "read",
        json!({"session": format!("Otro/{other}")}),
    )
    .await;
    assert_eq!(code, 404);
    assert_eq!(e["error"], "“Otro” is not a machine in the group");
    let (code, e) = tool(addr, me, "read", json!({"session": "Prueba/zzzz"})).await;
    assert_eq!(code, 404);
    assert_eq!(e["error"], "no such session");
    // La que llama no es de Claude: no se ve.
    let (code, _) = tool(addr, me, "read", json!({"session": caller})).await;
    assert_eq!(code, 404);

    // `answer` y `close`, solo en las que abrió quien llama.
    let (code, e) = tool(addr, me, "answer", json!({"session": other, "option": "1"})).await;
    assert_eq!(code, 403);
    assert_eq!(e["error"], "you can only answer sessions you opened");
    let (code, e) = tool(addr, me, "close", json!({"session": other})).await;
    assert_eq!(code, 403);
    assert_eq!(e["error"], "you can only close sessions you opened");
    let by = format!("Prueba/{caller}");
    state
        .registry
        .lock()
        .await
        .update(&other, |s| s.opened_by = Some(by.clone()))
        .unwrap();
    let (_, list) = tool(addr, me, "sessions", json!({})).await;
    assert_eq!(list["sessions"][0]["mine"], true);
    assert_eq!(list["sessions"][0]["opened_by"], by.as_str());
    let (code, e) = tool(addr, me, "answer", json!({"session": other, "option": "1"})).await;
    assert_eq!(code, 409, "{e}");
    assert_eq!(e["error"], "the session is not running");

    // `alert`: se apunta en la sesión que llama.
    let (code, r) = tool(addr, me, "alert", json!({"text": "  listo  "})).await;
    assert_eq!(code, 200, "{r}");
    assert_eq!(r["ok"], true);
    let alerts = state
        .registry
        .lock()
        .await
        .get(&caller)
        .unwrap()
        .alerts
        .clone();
    assert_eq!(alerts.len(), 1);
    assert_eq!(alerts[0].text, "listo");
    let (code, _) = tool(addr, me, "alert", json!({"text": "   "})).await;
    assert_eq!(code, 400);

    // `open`: la carpeta tiene que existir.
    let work = tmp.path().join("work");
    let (code, e) = tool(
        addr,
        me,
        "open",
        json!({"machine": "Prueba", "folder": work, "prompt": "di hola"}),
    )
    .await;
    assert_eq!(code, 400);
    assert!(
        e["error"].as_str().unwrap().contains("does not exist"),
        "{e}"
    );
    let (code, e) = tool(
        addr,
        me,
        "open",
        json!({"machine": "Otro", "folder": tmp.path(), "prompt": "di hola"}),
    )
    .await;
    assert_eq!(code, 404);
    assert_eq!(e["error"], "“Otro” is not a machine in the group");
    std::fs::create_dir_all(&work).unwrap();
    let count = || async { state.registry.lock().await.sessions().len() };
    let before = count().await;
    let bad = [
        (
            json!({"machine": "Prueba", "folder": "rel/x", "prompt": "di hola"}),
            "is not a full path",
        ),
        (
            json!({"machine": "Prueba", "folder": work, "prompt": "di hola", "model": "-x"}),
            "invalid model",
        ),
        (
            json!({"machine": "Prueba", "folder": work, "prompt": "a".repeat(24 * 1024 + 1)}),
            "the prompt is longer than 24 KB",
        ),
        (
            json!({"machine": "Prueba", "folder": work, "prompt": "di hola", "model": "m".repeat(129)}),
            "the model is longer than 128 characters",
        ),
    ];
    for (args, want) in bad {
        let (code, e) = tool(addr, me, "open", args).await;
        assert_eq!(code, 400, "{e}");
        assert!(e["error"].as_str().unwrap().contains(want), "{e}");
    }
    assert_eq!(count().await, before, "no se apuntó ninguna");
    // Justo en el límite, sí.
    let (code, max) = tool(
        addr,
        me,
        "open",
        json!({"machine": "Prueba", "folder": work, "prompt": "a".repeat(24 * 1024)}),
    )
    .await;
    assert_eq!(code, 200, "{max}");
    let (code, _) = tool(addr, me, "close", json!({"session": max["session"]})).await;
    assert_eq!(code, 200);
    let _ = std::fs::remove_file(work.join("args"));
    let (code, opened) = tool(
        addr,
        me,
        "open",
        json!({"machine": "prueba", "folder": work, "prompt": "di hola", "model": "haiku"}),
    )
    .await;
    assert_eq!(code, 200, "{opened}");
    assert_eq!(opened["name"], "work");
    assert_eq!(opened["machine"], "Prueba");
    assert_eq!(opened["opened_by"], by.as_str());
    assert_eq!(opened["mine"], true);
    assert_eq!(opened["state"], "working", "viva y sin Claude a la vista");
    assert_eq!(
        opened["channel"], false,
        "su nido mcp aún no recoge la bandeja"
    );
    let new_id = opened["id"].as_str().unwrap().to_owned();
    assert_eq!(opened["session"], format!("Prueba/{new_id}"));
    // Lo que recibió la «shell»: la orden de Claude con el modelo y el mensaje inicial al final.
    assert!(wait_file(&work.join("args")).await, "la sesión no arrancó");
    let raw = std::fs::read(work.join("args")).unwrap();
    let args: Vec<String> = raw
        .split(|&b| b == 0)
        .filter(|a| !a.is_empty())
        .map(|a| String::from_utf8_lossy(a).into_owned())
        .collect();
    let from = args.iter().position(|a| a == "claude").expect("claude");
    let argv = &args[from..];
    assert_eq!(&argv[..2], ["claude", "--session-id"]);
    assert!(argv.windows(2).any(|w| w == ["-n", "work"]), "{argv:?}");
    assert!(
        argv.windows(2).any(|w| w == ["--model", "haiku"]),
        "{argv:?}"
    );
    let message = argv.last().unwrap();
    assert!(
        message.starts_with(&format!(
            "[nido] You were opened by the session \"padre\" on Prueba (Prueba/{caller})."
        )),
        "{message}"
    );
    assert!(message.ends_with("\n\ndi hola"), "{message}");
    let reg_opened = state.registry.lock().await.get(&new_id).cloned().unwrap();
    assert_eq!(reg_opened.opened_by.as_deref(), Some(by.as_str()));

    // Otra en la misma carpeta: otro nombre.
    let (code, second) = tool(
        addr,
        me,
        "open",
        json!({"machine": "Prueba", "folder": work, "prompt": "y tú"}),
    )
    .await;
    assert_eq!(code, 200, "{second}");
    assert_eq!(second["name"], "work-2");
    let second_id = second["id"].as_str().unwrap().to_owned();

    // `close`: se para y desaparece.
    for id in [&new_id, &second_id] {
        let (code, r) = tool(
            addr,
            me,
            "close",
            json!({"session": format!("Prueba/{id}")}),
        )
        .await;
        assert_eq!(code, 200, "{r}");
        assert!(state.registry.lock().await.get(id).is_none());
        assert!(!nido::launch::is_alive(&paths, id));
    }
    let (code, _) = tool(addr, me, "close", json!({"session": new_id})).await;
    assert_eq!(code, 404);

    // `answer` de verdad, contra un programa con un menú en pantalla. Un permiso: por número.
    let menu = tmp.path().join("menu");
    menu_folder(&menu, &fixture("permiso"), None);
    // Una pregunta con campo de texto: solo con texto (se va al campo y escribe).
    let sola = fixture("pregunta_sola");
    let on_text = sola
        .replace("❯ 1. Rojo", "  1. Rojo")
        .replace("  3. Type something.", "❯ 3. Type something.");
    assert_ne!(on_text, sola);
    let texto = tmp.path().join("texto");
    menu_folder(&texto, &sola, Some(&on_text));
    let mut opened_ids = vec![];
    for (dir, answer, kind) in [
        (&menu, json!({"option": "1"}), "permission"),
        (&texto, json!({"text": "azul cielo"}), "question"),
    ] {
        let (code, o) = tool(
            addr,
            me,
            "open",
            json!({"machine": "Prueba", "folder": dir, "prompt": "pregunta"}),
        )
        .await;
        assert_eq!(code, 200, "{o}");
        let session = o["session"].as_str().unwrap().to_owned();
        opened_ids.push(session.clone());
        // Espera a que salga la pregunta.
        let (code, r) = tool(
            addr,
            me,
            "read",
            json!({"session": session, "wait": true, "timeout_s": 15}),
        )
        .await;
        assert_eq!(code, 200, "{r}");
        assert_eq!(r["state"], "question", "{r}");
        assert_eq!(r["question"]["kind"], kind, "{r}");
        let mut args = answer.clone();
        args["session"] = session.clone().into();
        let (code, a) = tool(addr, me, "answer", args).await;
        assert_eq!(code, 200, "{a}");
        assert_eq!(a["ok"], true);
    }
    assert_eq!(read_file(&menu.join("picked")).await.as_deref(), Some("1"));
    assert_eq!(read_file(&texto.join("picked")).await.as_deref(), Some("3"));
    assert_eq!(
        read_file(&texto.join("typed")).await.as_deref(),
        Some("azul cielo")
    );
    // Ya sin menú: no hay nada que contestar. (Sin un Claude que se apunte, sale «trabajando».)
    let (_, r) = tool(addr, me, "read", json!({"session": opened_ids[0]})).await;
    assert_eq!(r["state"], "working", "{r}");
    assert!(r.get("question").is_none(), "{r}");
    let (code, e) = tool(
        addr,
        me,
        "answer",
        json!({"session": opened_ids[0], "option": "1"}),
    )
    .await;
    assert_eq!(code, 409, "{e}");
    assert_eq!(e["error"], "no question is pending");
    for s in opened_ids {
        let (code, r) = tool(addr, me, "close", json!({"session": s})).await;
        assert_eq!(code, 200, "{r}");
    }
}
