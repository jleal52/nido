//! `nido attach`: engancharse a una sesión desde una terminal (Windows Terminal), como `tmux attach`.
//!
//! El núcleo (`relay`) no sabe nada de terminales: recibe una entrada y una salida cualquiera, y
//! así se prueba. `run` añade lo de la terminal de verdad: modo crudo, tamaño y SIGWINCH.

use std::io;
use std::path::Path;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::mpsc;

use crate::proto::{self, Frame, Hello, Role, Size};

/// Ctrl+\: desengancharse sin parar la sesión (como `prefijo d` en tmux).
pub const DETACH_KEY: u8 = 0x1c;

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// El usuario se desenganchó; la sesión sigue.
    Detached,
    /// El programa de la sesión terminó.
    Exited(Option<i32>),
    /// Se cortó la conexión con el guardián.
    Lost,
}

/// Lo que se manda al salir para dejar la terminal como estaba: sin ratón, sin pegado entre
/// corchetes, cursor visible, colores normales y fuera de la pantalla alternativa.
pub const RESTORE: &[u8] =
    b"\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?2004l\x1b[?25h\x1b[m\x1b[?1049l";

pub async fn relay<I, O>(
    socket: &Path,
    mut input: I,
    mut output: O,
    size: Size,
    mut resizes: mpsc::Receiver<Size>,
    detach: u8,
) -> io::Result<Outcome>
where
    I: AsyncRead + Unpin,
    O: AsyncWrite + Unpin,
{
    let stream = UnixStream::connect(socket).await?;
    let (mut rd, mut wr) = stream.into_split();
    let hello = Hello {
        role: Role::Driver,
        cols: size.cols,
        rows: size.rows,
    };
    proto::write_frame(&mut wr, &Frame::Hello(hello)).await?;
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        tokio::select! {
            frame = proto::read_frame(&mut rd) => match frame? {
                Some(Frame::Output(b) | Frame::Snapshot(b)) => {
                    output.write_all(&b).await?;
                    output.flush().await?;
                }
                Some(Frame::Exit(e)) => return Ok(Outcome::Exited(e.code)),
                Some(_) => {}
                None => return Ok(Outcome::Lost),
            },
            n = input.read(&mut buf) => {
                let n = n?;
                if n == 0 {
                    return Ok(Outcome::Detached);
                }
                let data = &buf[..n];
                if let Some(i) = data.iter().position(|&b| b == detach) {
                    if i > 0 {
                        proto::write_frame(&mut wr, &Frame::Input(data[..i].to_vec())).await?;
                    }
                    return Ok(Outcome::Detached);
                }
                proto::write_frame(&mut wr, &Frame::Input(data.to_vec())).await?;
            }
            Some(size) = resizes.recv() => {
                proto::write_frame(&mut wr, &Frame::Resize(size)).await?;
            }
        }
    }
}

/// Tamaño de la terminal (columnas, filas).
pub fn terminal_size() -> Size {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    let ok = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) } == 0;
    if ok && ws.ws_col > 0 && ws.ws_row > 0 {
        Size {
            cols: ws.ws_col,
            rows: ws.ws_row,
        }
    } else {
        Size { cols: 80, rows: 24 }
    }
}

/// Pone la terminal en modo crudo y la deja como estaba al soltarse (también si algo falla).
pub struct RawMode {
    saved: libc::termios,
}

impl RawMode {
    pub fn enable() -> io::Result<Self> {
        let mut t: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(libc::STDIN_FILENO, &mut t) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let saved = t;
        unsafe { libc::cfmakeraw(&mut t) };
        if unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &t) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { saved })
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        unsafe {
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &self.saved);
        }
    }
}

/// Engancha esta terminal a la sesión hasta desengancharse o que termine.
pub async fn run(socket: &Path) -> io::Result<Outcome> {
    if unsafe { libc::isatty(libc::STDIN_FILENO) } == 0 {
        return Err(io::Error::other("nido attach necesita una terminal"));
    }
    let (tx, rx) = mpsc::channel(8);
    let mut winch = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change())?;
    tokio::spawn(async move {
        while winch.recv().await.is_some() {
            if tx.send(terminal_size()).await.is_err() {
                break;
            }
        }
    });
    let mut out = tokio::io::stdout();
    // Pantalla alternativa propia: al desengancharse vuelve lo que hubiera en la terminal.
    out.write_all(b"\x1b[?1049h").await?;
    out.flush().await?;
    let raw = RawMode::enable()?;
    let res = relay(
        socket,
        tokio::io::stdin(),
        tokio::io::stdout(),
        terminal_size(),
        rx,
        DETACH_KEY,
    )
    .await;
    drop(raw);
    out.write_all(RESTORE).await?;
    out.flush().await?;
    res
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::holder::{self, HolderConfig};
    use std::time::Duration;

    #[tokio::test]
    async fn types_sees_output_and_detaches() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("run/s.sock");
        let cfg = HolderConfig {
            id: "a".into(),
            cwd: dir.path().to_owned(),
            argv: vec!["bash".into(), "--norc".into(), "--noprofile".into()],
            env: vec![("PS1".into(), "$ ".into())],
            socket: socket.clone(),
            history_file: None,
            history_max: 1 << 20,
            cols: 80,
            rows: 24,
            initial_input: None,
        };
        let holder = tokio::spawn(holder::run(cfg));
        for _ in 0..100 {
            if socket.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        // Lo que "teclea" el usuario y lo que ve.
        let (mut keys, input) = tokio::io::duplex(1024);
        let (output, mut screen) = tokio::io::duplex(1 << 20);
        let (resize_tx, resize_rx) = mpsc::channel(4);
        let sock = socket.clone();
        let attach = tokio::spawn(async move {
            relay(
                &sock,
                input,
                output,
                Size {
                    cols: 100,
                    rows: 30,
                },
                resize_rx,
                DETACH_KEY,
            )
            .await
        });

        keys.write_all(b"stty size; echo hola-$((40+2))\r")
            .await
            .unwrap();
        let mut seen = String::new();
        let mut buf = vec![0u8; 4096];
        tokio::time::timeout(Duration::from_secs(10), async {
            while !seen.contains("hola-42") {
                let n = screen.read(&mut buf).await.unwrap();
                seen.push_str(&String::from_utf8_lossy(&buf[..n]));
            }
        })
        .await
        .expect("no apareció la salida");
        // Se engancha como driver: el PTY toma su tamaño.
        assert!(seen.contains("30 100"), "{seen:?}");

        resize_tx.send(Size { cols: 90, rows: 20 }).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        keys.write_all(b"stty size; echo fin-$((1+1))\r")
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            while !seen.contains("fin-2") {
                let n = screen.read(&mut buf).await.unwrap();
                seen.push_str(&String::from_utf8_lossy(&buf[..n]));
            }
        })
        .await
        .expect("no apareció la segunda salida");
        assert!(seen.contains("20 90"), "{seen:?}");

        // Ctrl+\ desengancha y la sesión sigue viva.
        keys.write_all(&[DETACH_KEY]).await.unwrap();
        assert_eq!(attach.await.unwrap().unwrap(), Outcome::Detached);
        assert!(
            !holder.is_finished(),
            "desengancharse no debe parar la sesión"
        );

        // Otra vez, y ahora el programa termina.
        let (mut keys, input) = tokio::io::duplex(1024);
        let (output, _screen) = tokio::io::duplex(1 << 20);
        let (_tx, rx) = mpsc::channel(1);
        let sock = socket.clone();
        let attach = tokio::spawn(async move {
            relay(
                &sock,
                input,
                output,
                Size { cols: 80, rows: 24 },
                rx,
                DETACH_KEY,
            )
            .await
        });
        keys.write_all(b"exit 7\r").await.unwrap();
        assert_eq!(attach.await.unwrap().unwrap(), Outcome::Exited(Some(7)));
        holder.await.unwrap().unwrap();
    }
}
