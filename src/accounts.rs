//! Las cuentas de Claude que reparte `cswap` (claude-swap) y lo que lleva gastado cada una.
//!
//! Siempre a través de `cswap list --json`: la API de uso de Anthropic admite muy pocas
//! peticiones por hora y cswap ya las reparte entre todas sus vistas con un almacén compartido.
//! De su salida solo se toman los campos que se enseñan (nada de credenciales ni identificadores
//! internos).

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio::process::Command;
use tokio::sync::Mutex;

/// Cuánto vale lo que dio la última llamada: cswap ya sirve de su almacén si el dato es reciente;
/// esto evita lanzar Python por cada navegador que mira.
const CACHE: Duration = Duration::from_secs(60);
/// Si cswap tiene que preguntar a la API puede tardar (o esperar a otro que pregunta).
const TIMEOUT: Duration = Duration::from_secs(30);
/// Cambiar puede tener que renovar el token de la cuenta de destino.
const SWITCH_TIMEOUT: Duration = Duration::from_secs(90);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all(deserialize = "camelCase"))]
pub struct Window {
    /// Solo en las ventanas por modelo (`Fable`…).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub pct: Option<f64>,
    pub resets_at: Option<String>,
    /// Dónde estaría el uso si se repartiera por igual en la semana.
    #[serde(default)]
    pub expected_pct: Option<f64>,
    #[serde(default)]
    pub ahead_of_pace: Option<bool>,
    #[serde(default)]
    pub will_last_to_reset: Option<bool>,
    #[serde(default)]
    pub projected_exhaustion_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all(deserialize = "camelCase"))]
pub struct Usage {
    pub five_hour: Option<Window>,
    pub seven_day: Option<Window>,
    /// Límites semanales por modelo.
    #[serde(default, rename(deserialize = "scoped"))]
    pub models: Vec<Window>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all(deserialize = "camelCase"))]
pub struct Account {
    pub number: u32,
    pub email: Option<String>,
    #[serde(default)]
    pub alias: Option<String>,
    #[serde(default, rename(deserialize = "organizationName"))]
    pub organization: Option<String>,
    #[serde(default)]
    pub active: bool,
    #[serde(default)]
    pub disabled: bool,
    /// `ok`, `token_expired`, `unavailable`…
    #[serde(default, rename(deserialize = "usageStatus"))]
    pub status: Option<String>,
    pub usage: Option<Usage>,
    #[serde(default, rename(deserialize = "usageAgeSeconds"))]
    pub age_s: Option<f64>,
    /// La última medida buena, cuando `usage` falta (dato viejo o cuenta con problemas).
    #[serde(default, rename(deserialize = "lastGoodUsage"))]
    pub last_good: Option<Usage>,
    #[serde(default, rename(deserialize = "lastGoodAgeSeconds"))]
    pub last_good_age_s: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Accounts {
    pub accounts: Vec<Account>,
}

/// `{"schemaVersion":1,"accounts":[…]}` o `{"schemaVersion":1,"error":{…}}`.
pub fn parse(json: &str) -> Result<Accounts, String> {
    let v: serde_json::Value = serde_json::from_str(json)
        .map_err(|e| format!("cswap devolvió algo que no es JSON: {e}"))?;
    check(&v)?;
    let accounts = serde_json::from_value(v.get("accounts").cloned().unwrap_or_default())
        .map_err(|e| format!("cswap: no se entiende la lista de cuentas: {e}"))?;
    Ok(Accounts { accounts })
}

/// `{"schemaVersion":1,"error":{…}}` → el mensaje; también una versión que no se conoce.
fn check(v: &serde_json::Value) -> Result<(), String> {
    if let Some(err) = v.get("error") {
        let msg = err
            .get("message")
            .and_then(|m| m.as_str())
            .map(str::to_owned)
            .unwrap_or_else(|| err.to_string());
        return Err(format!("cswap: {msg}"));
    }
    if v.get("schemaVersion").and_then(|s| s.as_u64()) != Some(1) {
        return Err("cswap: versión de salida desconocida".into());
    }
    Ok(())
}

/// `cswap <args…>`: su salida JSON (con error, cswap también deja en stdout un JSON que lo explica).
async fn cswap(args: &[&str], timeout: Duration) -> Result<String, String> {
    let mut cmd = Command::new("cswap");
    cmd.args(args)
        // La cuenta por defecto es la de `~/.claude`, no la de la sesión que lance esto.
        .env_remove("CLAUDE_CONFIG_DIR")
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    let out = tokio::time::timeout(timeout, cmd.output())
        .await
        .map_err(|_| "cswap tardó demasiado en contestar".to_owned())?
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => "no está cswap (claude-swap) en el PATH".to_owned(),
            _ => format!("no se pudo lanzar cswap: {e}"),
        })?;
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    if stdout.trim().is_empty() {
        return Err(format!("cswap terminó con {} sin decir nada", out.status));
    }
    Ok(stdout)
}

async fn run_cswap() -> Result<Accounts, String> {
    parse(&cswap(&["list", "--json"], TIMEOUT).await?)
}

/// Número, correo o alias de una cuenta de cswap. Nunca empieza por `-` (no es una opción).
pub fn valid_target(t: &str) -> bool {
    !t.is_empty()
        && t.len() <= 254
        && !t.starts_with('-')
        && t.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"@._+-".contains(&b))
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AccountRef {
    pub number: u32,
    #[serde(default)]
    pub email: Option<String>,
}

/// Lo que contesta `cswap switch <cuenta> --json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Switched {
    pub switched: bool,
    pub from: Option<AccountRef>,
    pub to: Option<AccountRef>,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub warnings: Vec<String>,
}

pub fn parse_switch(json: &str) -> Result<Switched, String> {
    let v: serde_json::Value = serde_json::from_str(json)
        .map_err(|e| format!("cswap devolvió algo que no es JSON: {e}"))?;
    check(&v)?;
    serde_json::from_value(v).map_err(|e| format!("cswap: no se entiende la respuesta: {e}"))
}

/// Lo que se manda a la web: las cuentas (las últimas buenas, aunque la llamada falle) y, si la
/// última llamada falló, por qué.
#[derive(Debug, Clone, Default, Serialize)]
pub struct View {
    pub accounts: Vec<Account>,
    /// Segundos desde que se le preguntó a cswap.
    pub age_s: Option<u64>,
    pub error: Option<String>,
}

#[derive(Default)]
pub struct Cache {
    data: Arc<std::sync::Mutex<CacheInner>>,
    /// Solo una llamada a cswap a la vez; las demás esperan su resultado.
    running: Arc<Mutex<()>>,
}

#[derive(Default)]
struct CacheInner {
    good: Option<(Instant, Accounts)>,
    error: Option<(Instant, String)>,
}

impl CacheInner {
    /// Lo último (bueno o fallido) es de hace menos de `CACHE`.
    fn fresh(&self) -> bool {
        let fresh = |t: &Instant| t.elapsed() < CACHE;
        self.good.as_ref().is_some_and(|(t, _)| fresh(t))
            || self.error.as_ref().is_some_and(|(t, _)| fresh(t))
    }

    fn view(&self) -> View {
        View {
            accounts: self
                .good
                .as_ref()
                .map(|(_, a)| a.accounts.clone())
                .unwrap_or_default(),
            age_s: self.good.as_ref().map(|(t, _)| t.elapsed().as_secs()),
            error: self.error.as_ref().map(|(_, e)| e.clone()),
        }
    }
}

async fn refresh(data: Arc<std::sync::Mutex<CacheInner>>, running: Arc<Mutex<()>>) {
    let _one = running.lock().await;
    if data.lock().unwrap().fresh() {
        return; // lo acaba de traer otro
    }
    let res = run_cswap().await;
    let mut c = data.lock().unwrap();
    match res {
        Ok(a) => {
            c.good = Some((Instant::now(), a));
            c.error = None;
        }
        Err(e) => c.error = Some((Instant::now(), e)),
    }
}

impl Cache {
    /// Cambia la cuenta de `~/.claude` (la de todas las sesiones sin cuenta propia) y vuelve a
    /// pedir el uso, que ahora tiene otra cuenta activa.
    pub async fn switch(&self, target: &str) -> Result<Switched, String> {
        if !valid_target(target) {
            return Err("cuenta no válida".into());
        }
        let res = {
            // Que no coincida con una lectura de la lista.
            let _one = self.running.lock().await;
            let out = cswap(&["switch", target, "--json"], SWITCH_TIMEOUT).await;
            let mut c = self.data.lock().unwrap();
            c.good = c.good.take().map(|(_, a)| (Instant::now() - CACHE, a));
            c.error = None;
            out.and_then(|o| parse_switch(&o))
        };
        tokio::spawn(refresh(self.data.clone(), self.running.clone()));
        res
    }

    /// Si lo guardado es viejo se pide de nuevo; mientras, si hay algo, se devuelve eso (cswap
    /// puede tardar decenas de segundos si tiene que preguntar a la API).
    pub async fn get(&self) -> View {
        let (fresh, have) = {
            let c = self.data.lock().unwrap();
            (c.fresh(), c.good.is_some())
        };
        if !fresh {
            let job = refresh(self.data.clone(), self.running.clone());
            if have {
                tokio::spawn(job);
            } else {
                job.await;
            }
        }
        self.data.lock().unwrap().view()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_only_known_fields() {
        let json = r#"{"schemaVersion":1,"activeAccountNumber":7,"accounts":[
            {"number":7,"email":"a@b.c","organizationName":"Org","organizationUuid":"x","active":true,
             "usageStatus":"ok","usageAgeSeconds":10.4,
             "usage":{"fiveHour":{"pct":17.0,"resetsAt":"r5","countdown":"3h","clock":"20:09"},
                      "sevenDay":{"pct":76.0,"resetsAt":"r7","expectedPct":18.3,"aheadOfPace":true,"willLastToReset":false},
                      "scoped":[{"name":"Fable","pct":53.0,"resetsAt":"r7"}]}},
            {"number":3,"email":"d@e.f","usageStatus":"token_expired","usage":null,"disabled":true,
             "lastGoodUsage":{"fiveHour":{"pct":1.0,"resetsAt":null},"sevenDay":null},"lastGoodAgeSeconds":900}],
            "unclaimedCredentials":["secreto"]}"#;
        let a = parse(json).unwrap();
        assert_eq!(a.accounts.len(), 2);
        let first = &a.accounts[0];
        assert!(first.active);
        assert_eq!(first.organization.as_deref(), Some("Org"));
        let u = first.usage.as_ref().unwrap();
        assert_eq!(u.seven_day.as_ref().unwrap().ahead_of_pace, Some(true));
        assert_eq!(u.models[0].name.as_deref(), Some("Fable"));
        let second = &a.accounts[1];
        assert!(second.disabled && second.usage.is_none());
        assert_eq!(
            second
                .last_good
                .as_ref()
                .unwrap()
                .five_hour
                .as_ref()
                .unwrap()
                .pct,
            Some(1.0)
        );
        let out = serde_json::to_string(&a).unwrap();
        assert!(!out.contains("secreto") && !out.contains("organizationUuid"));
        assert!(out.contains("\"five_hour\"") && out.contains("\"ahead_of_pace\":true"));
    }

    #[test]
    fn switch_result_and_targets() {
        let r = parse_switch(
            r#"{"schemaVersion":1,"switched":true,"from":{"number":7,"email":"a@b.c"},
               "to":{"number":3,"email":"d@e.f"},"strategy":"explicit","reason":"switched",
               "message":"Switched to Account-3 (d@e.f)","warnings":[]}"#,
        )
        .unwrap();
        assert!(r.switched);
        assert_eq!(r.to.unwrap().number, 3);
        assert!(
            parse_switch(r#"{"schemaVersion":1,"error":{"message":"No account found"}}"#).is_err()
        );
        for ok in ["3", "a.b+c@d-e.com", "dev_1"] {
            assert!(valid_target(ok), "{ok}");
        }
        for bad in ["", "-h", "--force", "3 4", "a;b", "$(x)"] {
            assert!(!valid_target(bad), "{bad}");
        }
    }

    #[test]
    fn reports_cswap_errors() {
        let e = parse(r#"{"schemaVersion":1,"error":{"code":"x","message":"sin cuentas"}}"#);
        assert_eq!(e.unwrap_err(), "cswap: sin cuentas");
        assert!(parse("Traceback…").is_err());
    }
}
