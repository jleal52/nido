//! Protocolo entre el guardián de una sesión y sus clientes (gestor, `nido attach`).
//!
//! Trama: `u32` big-endian con la longitud de lo que sigue, un `u8` con el tipo y la carga.
//! Los mensajes de control van en JSON; los de terminal, en bytes tal cual.

use std::io;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Ninguna trama legítima se acerca: el historial se recorta muy por debajo.
pub const MAX_FRAME: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// Escribe, y el último que escribe o cambia de tamaño fija el tamaño del PTY.
    Driver,
    /// Solo mira: nunca cambia el tamaño (la vista de texto, el móvil leyendo).
    Viewer,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    pub role: Role,
    pub cols: u16,
    pub rows: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Size {
    pub cols: u16,
    pub rows: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExitInfo {
    /// `None` si el proceso murió por una señal.
    pub code: Option<i32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryRequest {
    /// Todo el historial, o solo desde la última vez que el programa lo mandó borrar.
    pub all: bool,
    /// Solo la pantalla actual (sin historial): menús, permisos, el modo de Claude…
    #[serde(default)]
    pub screen: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    Hello(Hello),
    Input(Vec<u8>),
    Resize(Size),
    Output(Vec<u8>),
    Snapshot(Vec<u8>),
    Exit(ExitInfo),
    HistoryRequest(HistoryRequest),
    History(Vec<u8>),
}

impl Frame {
    fn kind(&self) -> u8 {
        match self {
            Frame::Hello(_) => 1,
            Frame::Input(_) => 2,
            Frame::Resize(_) => 3,
            Frame::Output(_) => 4,
            Frame::Snapshot(_) => 5,
            Frame::Exit(_) => 6,
            Frame::HistoryRequest(_) => 7,
            Frame::History(_) => 8,
        }
    }

    fn payload(&self) -> Vec<u8> {
        let json =
            |r: serde_json::Result<Vec<u8>>| r.expect("los mensajes de control se serializan");
        match self {
            Frame::Hello(v) => json(serde_json::to_vec(v)),
            Frame::Resize(v) => json(serde_json::to_vec(v)),
            Frame::Exit(v) => json(serde_json::to_vec(v)),
            Frame::HistoryRequest(v) => json(serde_json::to_vec(v)),
            Frame::Input(b) | Frame::Output(b) | Frame::Snapshot(b) | Frame::History(b) => {
                b.clone()
            }
        }
    }

    fn decode(kind: u8, payload: Vec<u8>) -> io::Result<Frame> {
        fn json<T: for<'de> Deserialize<'de>>(p: &[u8]) -> io::Result<T> {
            serde_json::from_slice(p).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
        }
        Ok(match kind {
            1 => Frame::Hello(json(&payload)?),
            2 => Frame::Input(payload),
            3 => Frame::Resize(json(&payload)?),
            4 => Frame::Output(payload),
            5 => Frame::Snapshot(payload),
            6 => Frame::Exit(json(&payload)?),
            7 => Frame::HistoryRequest(json(&payload)?),
            8 => Frame::History(payload),
            k => {
                let msg = format!("tipo de trama desconocido: {k}");
                return Err(io::Error::new(io::ErrorKind::InvalidData, msg));
            }
        })
    }
}

pub async fn write_frame<W: AsyncWrite + Unpin>(w: &mut W, frame: &Frame) -> io::Result<()> {
    let payload = frame.payload();
    let len = u32::try_from(payload.len() + 1)
        .ok()
        .filter(|&l| l as usize <= MAX_FRAME)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "trama demasiado grande"))?;
    let mut buf = Vec::with_capacity(5 + payload.len());
    buf.extend_from_slice(&len.to_be_bytes());
    buf.push(frame.kind());
    buf.extend_from_slice(&payload);
    w.write_all(&buf).await?;
    w.flush().await
}

/// `None` si el otro lado cerró limpiamente entre tramas.
pub async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> io::Result<Option<Frame>> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len).await {
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_be_bytes(len) as usize;
    if len == 0 || len > MAX_FRAME {
        let msg = format!("longitud de trama no válida: {len}");
        return Err(io::Error::new(io::ErrorKind::InvalidData, msg));
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf).await?;
    let payload = buf.split_off(1);
    Frame::decode(buf[0], payload).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn round_trip_every_kind() {
        let frames = vec![
            Frame::Hello(Hello {
                role: Role::Viewer,
                cols: 80,
                rows: 24,
            }),
            Frame::Input(b"ls\r".to_vec()),
            Frame::Resize(Size {
                cols: 120,
                rows: 40,
            }),
            Frame::Output(vec![0x1b, b'[', b'm', 0, 255]),
            Frame::Snapshot(b"\x1b[2J".to_vec()),
            Frame::Exit(ExitInfo { code: Some(3) }),
            Frame::Exit(ExitInfo { code: None }),
            Frame::HistoryRequest(HistoryRequest {
                all: true,
                screen: false,
            }),
            Frame::History(Vec::new()),
        ];
        let (mut a, mut b) = tokio::io::duplex(1 << 16);
        for f in &frames {
            write_frame(&mut a, f).await.unwrap();
        }
        drop(a);
        for f in &frames {
            assert_eq!(read_frame(&mut b).await.unwrap().as_ref(), Some(f));
        }
        assert_eq!(read_frame(&mut b).await.unwrap(), None);
    }

    #[tokio::test]
    async fn rejects_oversized_and_unknown_frames() {
        let (mut a, mut b) = tokio::io::duplex(64);
        a.write_all(&(u32::MAX).to_be_bytes()).await.unwrap();
        assert!(read_frame(&mut b).await.is_err());

        let (mut a, mut b) = tokio::io::duplex(64);
        a.write_all(&[0, 0, 0, 1, 99]).await.unwrap();
        assert!(read_frame(&mut b).await.is_err());

        let (mut a, _b) = tokio::io::duplex(64);
        let big = Frame::Output(vec![0; MAX_FRAME + 1]);
        assert!(write_frame(&mut a, &big).await.is_err());
    }
}
