//! El guardián de punta a punta, con bash en un directorio temporal.

use std::path::Path;
use std::time::Duration;

use nido::holder::{self, HolderConfig};
use nido::proto::{self, Frame, Hello, HistoryRequest, Role, Size};
use tokio::net::UnixStream;

struct Client {
    stream: UnixStream,
    seen: String,
}

impl Client {
    async fn connect(socket: &Path, role: Role, cols: u16, rows: u16) -> (Client, String) {
        let mut stream = None;
        for _ in 0..100 {
            if let Ok(s) = UnixStream::connect(socket).await {
                stream = Some(s);
                break;
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
        let mut stream = stream.expect("el guardián no abrió su socket");
        proto::write_frame(&mut stream, &Frame::Hello(Hello { role, cols, rows }))
            .await
            .unwrap();
        let snapshot = match proto::read_frame(&mut stream).await.unwrap() {
            Some(Frame::Snapshot(b)) => String::from_utf8_lossy(&b).into_owned(),
            other => panic!("se esperaba Snapshot, llegó {other:?}"),
        };
        (
            Client {
                stream,
                seen: String::new(),
            },
            snapshot,
        )
    }

    async fn send(&mut self, frame: Frame) {
        proto::write_frame(&mut self.stream, &frame).await.unwrap();
    }

    async fn type_line(&mut self, line: &str) {
        self.send(Frame::Input(format!("{line}\r").into_bytes()))
            .await;
    }

    /// Lee tramas hasta que la salida acumulada contenga `needle`.
    async fn wait_for(&mut self, needle: &str) {
        let res = tokio::time::timeout(Duration::from_secs(10), async {
            while !self.seen.contains(needle) {
                match proto::read_frame(&mut self.stream).await.unwrap() {
                    Some(Frame::Output(b)) => self.seen.push_str(&String::from_utf8_lossy(&b)),
                    Some(_) => {}
                    None => panic!("el guardián cerró sin mostrar {needle:?}"),
                }
            }
        })
        .await;
        assert!(
            res.is_ok(),
            "no apareció {needle:?}; llegó: {:?}",
            self.seen
        );
    }

    async fn history(&mut self, all: bool) -> String {
        self.send(Frame::HistoryRequest(HistoryRequest { all, screen: false }))
            .await;
        loop {
            match proto::read_frame(&mut self.stream).await.unwrap() {
                Some(Frame::History(b)) => return String::from_utf8_lossy(&b).into_owned(),
                Some(_) => {}
                None => panic!("cerró sin historial"),
            }
        }
    }

    async fn wait_exit(&mut self) -> Option<i32> {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                match proto::read_frame(&mut self.stream).await.unwrap() {
                    Some(Frame::Exit(e)) => return e.code,
                    Some(_) => {}
                    None => panic!("cerró sin Exit"),
                }
            }
        })
        .await
        .expect("no llegó el Exit")
    }
}

fn config(dir: &Path) -> HolderConfig {
    HolderConfig {
        id: "prueba".into(),
        cwd: dir.to_owned(),
        argv: vec!["bash".into(), "--norc".into(), "--noprofile".into()],
        env: vec![("PS1".into(), "$ ".into())],
        socket: dir.join("run/prueba.sock"),
        history_file: Some(dir.join("state/history.ans")),
        history_max: 1 << 20,
        cols: 80,
        rows: 24,
        initial_input: None,
    }
}

#[tokio::test]
async fn a_late_client_sees_what_the_first_one_did() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = config(dir.path());
    let socket = cfg.socket.clone();
    let holder = tokio::spawn(holder::run(cfg));

    let (mut a, _) = Client::connect(&socket, Role::Driver, 80, 24).await;
    a.type_line("echo hola-$((40+2))").await;
    a.wait_for("hola-42").await;

    let (_b, snapshot) = Client::connect(&socket, Role::Viewer, 80, 24).await;
    assert!(
        snapshot.contains("hola-42"),
        "el snapshot no trae la pantalla: {snapshot:?}"
    );

    a.type_line("exit 3").await;
    assert_eq!(a.wait_exit().await, Some(3));
    assert_eq!(holder.await.unwrap().unwrap(), Some(3));
    assert!(!socket.exists(), "el socket debería haberse borrado");
}

#[tokio::test]
async fn only_drivers_change_the_size() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = config(dir.path());
    let socket = cfg.socket.clone();
    let holder = tokio::spawn(holder::run(cfg));

    let (mut driver, _) = Client::connect(&socket, Role::Driver, 80, 24).await;
    let (mut viewer, _) = Client::connect(&socket, Role::Viewer, 40, 10).await;
    viewer.send(Frame::Resize(Size { cols: 30, rows: 5 })).await;
    // Un viewer que escribe tampoco impone su tamaño.
    viewer.type_line("stty size; echo fin-$((0+1))").await;
    driver.wait_for("fin-1").await;
    assert!(driver.seen.contains("24 80"), "{:?}", driver.seen);

    driver
        .send(Frame::Resize(Size {
            cols: 100,
            rows: 30,
        }))
        .await;
    driver.type_line("stty size; echo fin-$((1+1))").await;
    driver.wait_for("fin-2").await;
    assert!(driver.seen.contains("30 100"), "{:?}", driver.seen);

    driver.type_line("exit").await;
    assert_eq!(driver.wait_exit().await, Some(0));
    holder.await.unwrap().unwrap();
}

#[tokio::test]
async fn history_survives_clear_and_restart() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = config(dir.path());
    let socket = cfg.socket.clone();
    let holder = tokio::spawn(holder::run(cfg));

    let (mut c, _) = Client::connect(&socket, Role::Driver, 80, 24).await;
    c.type_line("seq 1 200 | sed 's/^/n/'; printf '\\033[3J'; echo listo-$((6*7))")
        .await;
    c.wait_for("listo-42").await;
    let all = c.history(true).await;
    assert!(
        all.contains("n1\n") && all.contains("n200"),
        "falta historial: {all:?}"
    );
    assert!(all.contains("\u{1}reset "), "falta la marca del ESC[3J");
    let current = c.history(false).await;
    assert!(
        !current.contains("n1\n"),
        "tras ESC[3J la vista actual no debería traer n1"
    );
    c.type_line("exit").await;
    c.wait_exit().await;
    holder.await.unwrap().unwrap();

    // Otra vez la misma sesión (como tras un reinicio): el historial viejo sigue ahí.
    let cfg = config(dir.path());
    let holder = tokio::spawn(holder::run(cfg));
    let (mut c, _) = Client::connect(&socket, Role::Driver, 80, 24).await;
    let all = c.history(true).await;
    assert!(
        all.contains("n200") && all.contains("\u{1}restart "),
        "{all:?}"
    );
    c.type_line("exit").await;
    c.wait_exit().await;
    holder.await.unwrap().unwrap();
}
