//! Los secretos que piden las sesiones de Claude con el MCP de nido (`secret`): el usuario los
//! escribe en la web o en la app, llegan cifrados por el canal de siempre y se guardan en un
//! fichero solo suyo (0600) bajo `<state>/secrets/` (0700; en Windows, en el perfil del
//! usuario). La sesión recibe la ruta, nunca el valor.
//!
//! - Recordados: `equipo/<nombre>`, para todas las sesiones de este equipo. Su caducidad y quién
//!   lo pidió por última vez van aparte, en `equipo.json` (sin el valor).
//! - Sin recordar: `sesiones/<id>/<nombre>`, solo para esa sesión; se borra al cerrarla o a las
//!   24 h.
//!
//! Las preguntas en curso ([`Asks`]) viven en memoria: una por nombre (dos sesiones que piden el
//! mismo comparten diálogo) y una por sesión.

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Cuánto espera una pregunta a que el usuario conteste.
pub const ASK_WAIT: Duration = Duration::from_secs(15 * 60);
/// Lo que vive, como mucho, un secreto sin recordar (ms).
pub const SESSION_KEEP_MS: u64 = 24 * 3600 * 1000;
/// Lo más que ocupa un secreto.
pub const MAX_VALUE: usize = 16 * 1024;

const HOUR_MS: u64 = 3600 * 1000;

/// Un nombre de secreto: `[A-Za-z0-9._-]`, de 1 a 64, sin `.` ni `..` solos. Va tal cual en una
/// ruta: nada más.
pub fn valid_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name != "."
        && name != ".."
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// Cuánto recordar lo que contesta el usuario.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Remember {
    #[serde(rename = "no")]
    No,
    #[serde(rename = "2h")]
    TwoHours,
    #[serde(rename = "7d")]
    SevenDays,
    #[serde(rename = "always")]
    Always,
}

impl Remember {
    /// Hasta cuándo (ms) vale si se recuerda; `None`, siempre.
    pub fn expires(self, now: u64) -> Option<u64> {
        match self {
            Remember::No => Some(now),
            Remember::TwoHours => Some(now + 2 * HOUR_MS),
            Remember::SevenDays => Some(now + 7 * 24 * HOUR_MS),
            Remember::Always => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_names() {
        for ok in [
            "github-token",
            "a",
            "API_KEY.prod",
            ".env",
            "x".repeat(64).as_str(),
        ] {
            assert!(valid_name(ok), "{ok}");
        }
        for bad in [
            "",
            ".",
            "..",
            "a/b",
            "a\\b",
            "con espacio",
            "ñandú",
            &"x".repeat(65),
        ] {
            assert!(!valid_name(bad), "{bad}");
        }
    }

    #[test]
    fn remember_parses_and_expires() {
        let r: Remember = serde_json::from_str("\"2h\"").unwrap();
        assert_eq!(r, Remember::TwoHours);
        assert_eq!(
            serde_json::to_string(&Remember::SevenDays).unwrap(),
            "\"7d\""
        );
        assert!(serde_json::from_str::<Remember>("\"1h\"").is_err());
        assert_eq!(
            Remember::TwoHours.expires(1_000),
            Some(1_000 + 2 * 3600 * 1000)
        );
        assert_eq!(Remember::SevenDays.expires(0), Some(7 * 24 * 3600 * 1000));
        assert_eq!(Remember::Always.expires(5), None);
    }
}
