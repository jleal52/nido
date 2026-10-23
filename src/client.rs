//! Cliente mínimo de la API local para la CLI (`nido ls`, `nido new`…).

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

pub struct Api {
    pub addr: SocketAddr,
    pub token: Option<String>,
}

impl Api {
    /// Devuelve el código y el cuerpo. HTTP/1.0: respuesta entera, sin trozos.
    pub fn request(
        &self,
        method: &str,
        path: &str,
        body: Option<&serde_json::Value>,
    ) -> io::Result<(u16, serde_json::Value)> {
        let mut s =
            TcpStream::connect_timeout(&self.addr, Duration::from_secs(3)).map_err(|e| {
                io::Error::new(
                    e.kind(),
                    format!(
                        "no responde el gestor en {} ({e}); ¿está `nido serve` en marcha?",
                        self.addr
                    ),
                )
            })?;
        s.set_read_timeout(Some(Duration::from_secs(30)))?;
        let body = body.map(|b| b.to_string()).unwrap_or_default();
        let mut req = format!("{method} {path} HTTP/1.0\r\nHost: {}\r\n", self.addr);
        if let Some(t) = &self.token {
            req.push_str(&format!("Authorization: Bearer {t}\r\n"));
        }
        if !body.is_empty() {
            req.push_str("Content-Type: application/json\r\n");
        }
        req.push_str(&format!("Content-Length: {}\r\n\r\n{body}", body.len()));
        s.write_all(req.as_bytes())?;
        let mut raw = Vec::new();
        s.read_to_end(&mut raw)?;
        let text = String::from_utf8_lossy(&raw);
        let code = text
            .split_whitespace()
            .nth(1)
            .and_then(|c| c.parse().ok())
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "respuesta HTTP rara"))?;
        let body = text.split_once("\r\n\r\n").map(|(_, b)| b).unwrap_or("");
        let json = serde_json::from_str(body).unwrap_or(serde_json::Value::String(body.to_owned()));
        Ok((code, json))
    }

    /// Como `request`, pero un código de error se convierte en error con el mensaje del gestor.
    pub fn call(
        &self,
        method: &str,
        path: &str,
        body: Option<&serde_json::Value>,
    ) -> io::Result<serde_json::Value> {
        let (code, json) = self.request(method, path, body)?;
        if (200..300).contains(&code) {
            Ok(json)
        } else {
            let msg = json
                .get("error")
                .and_then(|e| e.as_str())
                .map(str::to_owned);
            Err(io::Error::other(
                msg.unwrap_or_else(|| format!("el gestor respondió {code}")),
            ))
        }
    }
}
