//! Cliente mínimo de la API local para la CLI (`nido ls`, `nido new`…).

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

/// Lo más que la CLI espera a que el gestor arranque, pare, reinicie o borre una sesión: con su
/// conversación en un trabajo en segundo plano de Claude Code, parar ese trabajo cuenta (ver
/// `server::SESSION_OP_WORST`, de menos de un minuto; esto, más del doble).
pub const SESSION_OP_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Clone)]
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
        // La terminal habla español: los mensajes del gestor, también (la API, por defecto, en
        // inglés para la app y la web). Lo mismo en `call_with`.
        self.request_with(
            method,
            path,
            body,
            &[("Accept-Language", "es")],
            Some(Duration::from_secs(30)),
        )
    }

    /// Como `request`, con cabeceras propias y otro límite de lectura (`None`: sin límite).
    pub fn request_with(
        &self,
        method: &str,
        path: &str,
        body: Option<&serde_json::Value>,
        headers: &[(&str, &str)],
        read_timeout: Option<Duration>,
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
        s.set_read_timeout(read_timeout)?;
        let body = body.map(|b| b.to_string()).unwrap_or_default();
        let mut req = format!("{method} {path} HTTP/1.0\r\nHost: {}\r\n", self.addr);
        for (k, v) in headers {
            req.push_str(&format!("{k}: {v}\r\n"));
        }
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
        self.call_with(method, path, body, &[])
    }

    /// Como `call`, con cabeceras de más (`X-Nido-Session` para hablar como una sesión, otro
    /// `Accept-Language`).
    pub fn call_with(
        &self,
        method: &str,
        path: &str,
        body: Option<&serde_json::Value>,
        headers: &[(&str, &str)],
    ) -> io::Result<serde_json::Value> {
        self.call_within(method, path, body, headers, Duration::from_secs(30))
    }

    /// Como `call`, para arrancar, parar, reiniciar o borrar una sesión: puede tardar más (ver
    /// `SESSION_OP_TIMEOUT`).
    pub fn call_session_op(
        &self,
        method: &str,
        path: &str,
        body: Option<&serde_json::Value>,
    ) -> io::Result<serde_json::Value> {
        self.call_within(method, path, body, &[], SESSION_OP_TIMEOUT)
    }

    fn call_within(
        &self,
        method: &str,
        path: &str,
        body: Option<&serde_json::Value>,
        headers: &[(&str, &str)],
        timeout: Duration,
    ) -> io::Result<serde_json::Value> {
        let mut all = headers.to_vec();
        if !headers
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case("Accept-Language"))
        {
            all.push(("Accept-Language", "es"));
        }
        let (code, json) = self.request_with(method, path, body, &all, Some(timeout))?;
        ok_or_message(code, json)
    }
}

fn ok_or_message(code: u16, json: serde_json::Value) -> io::Result<serde_json::Value> {
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
