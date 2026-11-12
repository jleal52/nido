//! `nido clip`: lo que llega por la entrada estándar (o un fichero) al portapapeles de la sesión
//! de `NIDO_SESSION`, por la API del MCP de un nido de prueba (en un directorio temporal), y sin
//! que el contenido salga por la salida.

// Como el resto de pruebas con el gestor: solo en Unix (Windows lo compila el CI).
#![cfg(unix)]

use std::io::Write;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::Arc;
use std::time::Duration;

use nido::launch::{Launcher, Paths};
use nido::registry::Session;
use nido::server::{self, AppState};

const TOKEN: &str = "0123456789abcdef0123456789abcdef";

struct Nido {
    rt: tokio::runtime::Runtime,
    state: Arc<AppState>,
    addr: SocketAddr,
    home: PathBuf,
    id: String,
}

impl Nido {
    /// Un nido en `root/nido` (el de `NIDO_HOME`, con su token) y una sesión en `root/sesion`.
    fn new(root: &Path) -> Self {
        let home = root.join("nido");
        let paths = Paths {
            runtime: home.join("run"),
            data: home.join("data"),
            state: home.join("state"),
            config: home.join("config"),
            home: root.to_owned(),
            server: None,
        };
        std::fs::create_dir_all(&paths.config).unwrap();
        std::fs::write(paths.token(), TOKEN).unwrap();
        let cwd = root.join("sesion");
        std::fs::create_dir_all(&cwd).unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let (state, addr, id) = rt.block_on(async {
            let state = AppState::new(paths, Launcher::Detached, Some(TOKEN.into()), true).unwrap();
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let app =
                server::router(state.clone()).into_make_service_with_connect_info::<SocketAddr>();
            tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            let session = Session {
                id: nido::registry::new_id(),
                name: "api".into(),
                cwd,
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
            let id = state.registry.lock().await.add(session).unwrap().id;
            (state, addr, id)
        });
        Nido {
            rt,
            state,
            addr,
            home,
            id,
        }
    }

    /// `nido clip args` en `dir`, como una orden de la sesión `session`, con `input` en la
    /// entrada, en una terminal en español.
    fn clip(&self, session: Option<&str>, dir: &Path, args: &[&str], input: &[u8]) -> Output {
        self.clip_in("es_ES.UTF-8", session, dir, args, input)
    }

    /// Lo mismo, con `LANG=lang`.
    fn clip_in(
        &self,
        lang: &str,
        session: Option<&str>,
        dir: &Path,
        args: &[&str],
        input: &[u8],
    ) -> Output {
        let mut c = Command::new(env!("CARGO_BIN_EXE_nido"));
        c.arg("clip")
            .args(args)
            .current_dir(dir)
            .env("NIDO_HOME", &self.home)
            .env("NIDO_SERVER", self.addr.to_string())
            .env_remove("NIDO_SESSION")
            .env_remove("LC_ALL")
            .env_remove("LC_MESSAGES")
            .env("LANG", lang)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(s) = session {
            c.env("NIDO_SESSION", s);
        }
        let mut child = c.spawn().unwrap();
        let mut stdin = child.stdin.take().unwrap();
        let input = input.to_vec();
        // Si lee menos (pasa de 64 KB), la tubería se cierra antes: da igual.
        let writer = std::thread::spawn(move || {
            let _ = stdin.write_all(&input);
        });
        let out = child.wait_with_output().unwrap();
        writer.join().unwrap();
        out
    }

    /// Los envíos de la sesión: (título, contenido).
    fn items(&self) -> Vec<(String, String)> {
        let outbox = &self.state.outbox;
        outbox
            .list(&self.id)
            .into_iter()
            .map(|i| {
                let (item, path) = outbox.open(&self.id, &i.id).unwrap();
                (item.title, std::fs::read_to_string(path).unwrap())
            })
            .collect()
    }

    /// Las notas de portapapeles de la sesión (lo que avisa a la web y la app).
    fn clipboard_alerts(&self) -> usize {
        self.rt.block_on(async {
            let reg = self.state.registry.lock().await;
            reg.get(&self.id)
                .unwrap()
                .alerts
                .iter()
                .filter(|a| a.kind == Some(nido::registry::NoteKind::Clipboard))
                .count()
        })
    }
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

#[test]
fn sends_stdin_or_a_file_to_the_session_clipboard() {
    let _limit = nido::watchdog::limit(Duration::from_secs(120));
    let tmp = tempfile::tempdir().unwrap();
    let nido = Nido::new(tmp.path());
    let me = Some(nido.id.as_str());
    // La orden corre en otra carpeta que la de la sesión: el fichero, relativo a la suya.
    let work = tmp.path().join("trabajo");
    std::fs::create_dir_all(&work).unwrap();

    // La entrada estándar.
    let o = nido.clip(me, &work, &[], b"diff --git a/x b/x\n+hola\n");
    assert!(o.status.success(), "{}", stderr(&o));
    assert_eq!(stdout(&o), "enviado al portapapeles (1 KB)\n");
    assert!(!stdout(&o).contains("hola") && !stderr(&o).contains("hola"));
    assert_eq!(
        nido.items().last().unwrap().1,
        "diff --git a/x b/x\n+hola\n"
    );
    // Con título, y el tamaño redondeado hacia arriba.
    let text = "x".repeat(2000);
    let o = nido.clip(me, &work, &["--title", "el diff"], text.as_bytes());
    assert!(o.status.success(), "{}", stderr(&o));
    assert_eq!(stdout(&o), "enviado al portapapeles (2 KB)\n");
    assert_eq!(
        nido.items().last().unwrap(),
        &("el diff".to_owned(), text.clone())
    );
    // Un fichero, relativo a donde corre la orden; lo de la entrada no cuenta.
    std::fs::write(work.join("clave.pub"), "ssh-ed25519 AAAA prueba\n").unwrap();
    let o = nido.clip(me, &work, &["clave.pub", "--title", "la clave"], b"no");
    assert!(o.status.success(), "{}", stderr(&o));
    assert_eq!(stdout(&o), "enviado al portapapeles (1 KB)\n");
    assert!(!stdout(&o).contains("AAAA"));
    assert_eq!(
        nido.items().last().unwrap(),
        &(
            "la clave".to_owned(),
            "ssh-ed25519 AAAA prueba\n".to_owned()
        )
    );
    // Y absoluto.
    let abs = work.join("clave.pub");
    let o = nido.clip(me, tmp.path(), &[abs.to_str().unwrap()], b"");
    assert!(o.status.success(), "{}", stderr(&o));
    // Justo 64 KB, sí.
    let o = nido.clip(me, &work, &[], &[b'a'; 64 * 1024]);
    assert!(o.status.success(), "{}", stderr(&o));
    assert_eq!(stdout(&o), "enviado al portapapeles (64 KB)\n");
    let sent = 5;
    assert_eq!(nido.items().len(), sent);
    assert_eq!(nido.clipboard_alerts(), sent);

    // Lo que no vale: error claro, código 1, nada en la salida ni en el portapapeles.
    let fails = |o: Output, msg: &str| {
        assert_eq!(o.status.code(), Some(1), "{}", stderr(&o));
        assert_eq!(stdout(&o), "");
        assert!(stderr(&o).contains(msg), "{}", stderr(&o));
    };
    // Más de 64 KB.
    let big = vec![b'a'; 64 * 1024 + 1];
    fails(nido.clip(me, &work, &[], &big), "el texto pasa de 64 KB");
    std::fs::write(work.join("grande.txt"), &big).unwrap();
    fails(
        nido.clip(me, &work, &["grande.txt"], b""),
        "grande.txt pasa de 64 KB",
    );
    // Binario (o texto que no es UTF-8): mejor `send_file`.
    let png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR";
    fails(nido.clip(me, &work, &[], png), "usa send_file");
    fails(nido.clip(me, &work, &[], b"\0\0\0"), "usa send_file");
    std::fs::write(work.join("foto.png"), png).unwrap();
    fails(
        nido.clip(me, &work, &["foto.png"], b""),
        "foto.png no es texto (UTF-8): para mandarlo, usa send_file",
    );
    // Vacío.
    fails(nido.clip(me, &work, &[], b""), "el texto está vacío");
    fails(nido.clip(me, &work, &[], b" \n\n"), "el texto está vacío");
    std::fs::write(work.join("vacio.txt"), "").unwrap();
    fails(
        nido.clip(me, &work, &["vacio.txt"], b""),
        "el texto está vacío",
    );
    fails(
        nido.clip(me, &work, &["no-existe.txt"], b""),
        "no existe el fichero",
    );
    // Nada de la carpeta de secretos.
    let secret = nido
        .state
        .secrets
        .save_session(&nido.id, "tok", b"secreto")
        .unwrap();
    let o = nido.clip(me, &work, &[secret.to_str().unwrap()], b"");
    assert!(!stderr(&o).contains("secreto\n") && !stdout(&o).contains("secreto"));
    fails(o, "carpeta de secretos");
    // Ni por un enlace, ni dando la vuelta con `..`.
    std::os::unix::fs::symlink(&secret, work.join("atajo")).unwrap();
    fails(nido.clip(me, &work, &["atajo"], b""), "carpeta de secretos");
    let rel = secret.strip_prefix(tmp.path()).unwrap();
    let around = format!("../trabajo/../{}", rel.display());
    fails(nido.clip(me, &work, &[&around], b""), "carpeta de secretos");
    // Fuera de una sesión de nido (o con una que no es de este nido).
    fails(
        nido.clip(None, &work, &[], b"hola"),
        "falta o no es válida NIDO_SESSION",
    );
    fails(
        nido.clip(Some("../x"), &work, &[], b"hola"),
        "falta o no es válida NIDO_SESSION",
    );
    fails(
        nido.clip(Some("aaaa1111"), &work, &[], b"hola"),
        "no se llamó desde una sesión de nido",
    );

    assert_eq!(nido.items().len(), sent);
    assert_eq!(nido.clipboard_alerts(), sent);

    // En una terminal que no es en español (lo normal donde corre Claude), en inglés: lo suyo y
    // lo del gestor.
    for lang in ["C.UTF-8", "en_US.UTF-8"] {
        let o = nido.clip_in(lang, me, &work, &["--title", "en"], b"hello");
        assert!(o.status.success(), "{}", stderr(&o));
        assert_eq!(stdout(&o), "sent to the clipboard (1 KB)\n");
    }
    let en = |o: Output, msg: &str| {
        assert_eq!(o.status.code(), Some(1), "{}", stderr(&o));
        assert_eq!(stderr(&o), format!("nido: {msg}\n"));
    };
    en(
        nido.clip_in("C.UTF-8", me, &work, &[], b""),
        "the text is empty",
    );
    en(
        nido.clip_in("C.UTF-8", me, &work, &[], png),
        "the input is not text (UTF-8): to send a binary file, use send_file",
    );
    en(
        nido.clip_in("C.UTF-8", None, &work, &[], b"x"),
        "NIDO_SESSION is missing or invalid: `nido clip` is for the commands of a nido session",
    );
    // La ruta, absoluta desde la carpeta de la orden tal como la da el sistema (en macOS,
    // `/private/var/…` aunque la temporal diga `/var/…`): basta con cómo acaba.
    let o = nido.clip_in("C.UTF-8", me, &work, &["foto.png"], b"");
    assert_eq!(o.status.code(), Some(1), "{}", stderr(&o));
    let msg = stderr(&o);
    let foto = std::path::Path::new("trabajo").join("foto.png");
    assert!(
        msg.starts_with("nido: ")
            && msg.ends_with(&format!(
                "{} is not text (UTF-8): to send it, use send_file\n",
                foto.display()
            )),
        "{msg}"
    );
    assert_eq!(nido.items().len(), sent + 2);
}
