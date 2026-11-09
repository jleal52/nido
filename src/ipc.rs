//! El canal entre el guardián de una sesión y sus clientes: un socket unix (`<runtime>/<id>.sock`)
//! o, en Windows, una tubería con nombre (`\\.\pipe\nido-…`, sacado de esa misma ruta).

use std::io;
use std::path::Path;

pub use imp::{Listener, Stream, connect, probe};

pub type ReadHalf = tokio::io::ReadHalf<Stream>;
pub type WriteHalf = tokio::io::WriteHalf<Stream>;

pub fn split(s: Stream) -> (ReadHalf, WriteHalf) {
    tokio::io::split(s)
}

/// Pide al guardián que pare la sesión (la trama `Stop`). Se queda conectado hasta que el
/// guardián cierra (o unos segundos): si colgara enseguida, el guardián, que primero manda la
/// pantalla, vería que el cliente se ha ido antes de leer la orden.
pub fn send_stop(socket: &Path) -> io::Result<()> {
    use crate::proto::{Frame, Hello, Role, encode};
    let mut buf = encode(&Frame::Hello(Hello {
        role: Role::Viewer,
        cols: 80,
        rows: 24,
    }))?;
    buf.extend(encode(&Frame::Stop)?);
    imp::send_and_drain(socket, buf, std::time::Duration::from_secs(4))
}

#[cfg(unix)]
mod imp {
    use std::io::{self, Write};
    use std::path::Path;

    pub type Stream = tokio::net::UnixStream;

    pub struct Listener(tokio::net::UnixListener);

    impl Listener {
        /// Socket en un directorio solo nuestro; si queda uno viejo sin nadie detrás, se quita.
        pub async fn bind(socket: &Path) -> io::Result<Self> {
            if let Some(dir) = socket.parent() {
                std::fs::create_dir_all(dir)?;
                crate::sys::private_dir(dir)?;
            }
            if socket.exists() {
                if Stream::connect(socket).await.is_ok() {
                    let msg = format!("ya hay un guardián en {}", socket.display());
                    return Err(io::Error::new(io::ErrorKind::AddrInUse, msg));
                }
                std::fs::remove_file(socket)?;
            }
            let l = tokio::net::UnixListener::bind(socket)?;
            crate::sys::private_file(socket)?;
            Ok(Self(l))
        }

        pub async fn accept(&mut self) -> io::Result<Stream> {
            self.0.accept().await.map(|(s, _)| s)
        }
    }

    pub async fn connect(socket: &Path) -> io::Result<Stream> {
        Stream::connect(socket).await
    }

    /// ¿Hay alguien atendiendo? (sin async: lo usan las órdenes de consola y el lanzador).
    pub fn probe(socket: &Path) -> bool {
        std::os::unix::net::UnixStream::connect(socket).is_ok()
    }

    pub fn send_and_drain(
        socket: &Path,
        buf: Vec<u8>,
        wait: std::time::Duration,
    ) -> io::Result<()> {
        let mut s = std::os::unix::net::UnixStream::connect(socket)?;
        s.write_all(&buf)?;
        s.flush()?;
        s.set_read_timeout(Some(wait))?;
        let _ = io::copy(&mut s, &mut io::sink());
        Ok(())
    }
}

#[cfg(windows)]
mod imp {
    use std::io::{self, Write};
    use std::path::Path;
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use std::time::Duration;

    use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
    use tokio::net::windows::named_pipe::{
        ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions,
    };

    /// ERROR_PIPE_BUSY: todas las instancias ocupadas; hay que reintentar.
    const PIPE_BUSY: i32 = 231;

    /// La tubería de una sesión: su ruta de socket, con lo que no vale en un nombre cambiado por
    /// `-` (`\\.\pipe\nido-C--Users-ana-AppData-Local-Temp-nido-ana-1a2b3c4d.sock`).
    pub fn pipe_name(socket: &Path) -> String {
        let flat: String = socket
            .to_string_lossy()
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '.' || c == '_' {
                    c
                } else {
                    '-'
                }
            })
            .collect();
        let tail = if flat.len() > 200 {
            &flat[flat.len() - 200..]
        } else {
            &flat
        };
        format!(r"\\.\pipe\nido-{tail}")
    }

    pub enum Stream {
        Server(NamedPipeServer),
        Client(NamedPipeClient),
    }

    impl AsyncRead for Stream {
        fn poll_read(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            match self.get_mut() {
                Stream::Server(s) => Pin::new(s).poll_read(cx, buf),
                Stream::Client(c) => Pin::new(c).poll_read(cx, buf),
            }
        }
    }

    impl AsyncWrite for Stream {
        fn poll_write(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            match self.get_mut() {
                Stream::Server(s) => Pin::new(s).poll_write(cx, buf),
                Stream::Client(c) => Pin::new(c).poll_write(cx, buf),
            }
        }
        fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            match self.get_mut() {
                Stream::Server(s) => Pin::new(s).poll_flush(cx),
                Stream::Client(c) => Pin::new(c).poll_flush(cx),
            }
        }
        fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            match self.get_mut() {
                Stream::Server(s) => Pin::new(s).poll_shutdown(cx),
                Stream::Client(c) => Pin::new(c).poll_shutdown(cx),
            }
        }
    }

    /// Una instancia de la tubería esperando cliente; al llegar uno se crea la siguiente.
    pub struct Listener {
        name: String,
        next: NamedPipeServer,
    }

    impl Listener {
        pub async fn bind(socket: &Path) -> io::Result<Self> {
            let name = pipe_name(socket);
            if ClientOptions::new().open(&name).is_ok() {
                let msg = format!("ya hay un guardián en {name}");
                return Err(io::Error::new(io::ErrorKind::AddrInUse, msg));
            }
            // Solo clientes de este equipo (es lo predeterminado, pero que quede dicho).
            let next = ServerOptions::new()
                .first_pipe_instance(true)
                .reject_remote_clients(true)
                .create(&name)?;
            Ok(Self { name, next })
        }

        pub async fn accept(&mut self) -> io::Result<Stream> {
            loop {
                match self.next.connect().await {
                    Ok(()) => break,
                    // ERROR_NO_DATA: el cliente llegó y se fue (una sonda): instancia nueva.
                    Err(e) if e.raw_os_error() == Some(232) => {
                        self.next = ServerOptions::new()
                            .reject_remote_clients(true)
                            .create(&self.name)?;
                    }
                    Err(e) => return Err(e),
                }
            }
            let fresh = ServerOptions::new()
                .reject_remote_clients(true)
                .create(&self.name)?;
            Ok(Stream::Server(std::mem::replace(&mut self.next, fresh)))
        }
    }

    pub async fn connect(socket: &Path) -> io::Result<Stream> {
        let name = pipe_name(socket);
        for _ in 0..40 {
            match ClientOptions::new().open(&name) {
                Ok(c) => return Ok(Stream::Client(c)),
                Err(e) if e.raw_os_error() == Some(PIPE_BUSY) => {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                Err(e) => return Err(e),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "la tubería de la sesión está ocupada",
        ))
    }

    fn open_blocking(socket: &Path) -> io::Result<std::fs::File> {
        let name = pipe_name(socket);
        for _ in 0..40 {
            match std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&name)
            {
                Err(e) if e.raw_os_error() == Some(PIPE_BUSY) => {
                    std::thread::sleep(Duration::from_millis(50));
                }
                r => return r,
            }
        }
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "la tubería de la sesión está ocupada",
        ))
    }

    pub fn probe(socket: &Path) -> bool {
        match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(pipe_name(socket))
        {
            Ok(_) => true,
            // Ocupada: hay guardián, atendiendo a otro.
            Err(e) => e.raw_os_error() == Some(PIPE_BUSY),
        }
    }

    /// La lectura de una tubería no tiene plazo: se hace en un hilo y se espera como mucho
    /// `wait` (si se pasa, el hilo se queda hasta que el guardián cierre).
    pub fn send_and_drain(socket: &Path, buf: Vec<u8>, wait: Duration) -> io::Result<()> {
        let mut f = open_blocking(socket)?;
        f.write_all(&buf)?;
        f.flush()?;
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = io::copy(&mut f, &mut io::sink());
            let _ = tx.send(());
        });
        let _ = rx.recv_timeout(wait);
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        #[test]
        fn pipe_names_are_flat() {
            let n = super::pipe_name(std::path::Path::new(
                r"C:\Users\ana\AppData\Local\Temp\nido-ana\ab12.sock",
            ));
            assert_eq!(
                n,
                r"\\.\pipe\nido-C--Users-ana-AppData-Local-Temp-nido-ana-ab12.sock"
            );
        }
    }
}
