//! Las llamadas de una cuenta de Claude: entrar (OAuth con PKCE, el mismo camino que
//! `claude /login` cuando no puede abrir el navegador: se abre un enlace, se entra y se pega el
//! código que enseña la página), renovar el token, el perfil y el uso.
//!
//! Nada de esto escribe tokens en el registro ni los devuelve en errores.

use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

/// El cliente OAuth de Claude Code (público: va en el propio `claude`).
pub const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const AUTHORIZE_URL: &str = "https://claude.com/cai/oauth/authorize";
const TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
/// La página que enseña el código para pegarlo (`código#estado`).
const REDIRECT_URI: &str = "https://platform.claude.com/oauth/code/callback";
const PROFILE_URL: &str = "https://api.anthropic.com/api/oauth/profile";
const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
const BETA: &str = "oauth-2025-04-20";
/// Los mismos permisos que pide `claude /login` con una cuenta de claude.ai.
const SCOPES: &str = "org:create_api_key user:profile user:inference user:sessions:claude_code \
                      user:mcp_servers user:file_upload user:plugins";
const UA: &str = concat!("nido/", env!("CARGO_PKG_VERSION"));

pub fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(UA)
        .timeout(Duration::from_secs(20))
        .build()
        .expect("cliente HTTP")
}

fn random_b64(n: usize) -> String {
    let mut buf = vec![0u8; n];
    getrandom::fill(&mut buf).expect("números aleatorios");
    URL_SAFE_NO_PAD.encode(buf)
}

/// Un inicio de sesión a medias: el enlace para abrir y lo que hace falta para acabarlo.
pub struct Start {
    pub url: String,
    pub state: String,
    pub verifier: String,
}

pub fn start(login_hint: Option<&str>) -> Start {
    let verifier = random_b64(32);
    let state = random_b64(32);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let mut params = vec![
        ("code", "true"),
        ("client_id", CLIENT_ID),
        ("response_type", "code"),
        ("redirect_uri", REDIRECT_URI),
        ("scope", SCOPES),
        ("code_challenge", &challenge),
        ("code_challenge_method", "S256"),
        ("state", &state),
    ];
    if let Some(h) = login_hint.filter(|h| !h.is_empty()) {
        params.push(("login_hint", h));
    }
    let url = reqwest::Url::parse_with_params(AUTHORIZE_URL, &params)
        .expect("URL fija")
        .to_string();
    Start {
        url,
        state,
        verifier,
    }
}

/// Lo que se pega: `código#estado` (lo que enseña la página), solo el código, o la dirección
/// entera de la página. Devuelve el código y, si viene, el estado.
pub fn parse_pasted(input: &str) -> Result<(String, Option<String>), String> {
    let s = input.trim();
    if s.is_empty() {
        return Err("falta el código".into());
    }
    if s.starts_with("http://") || s.starts_with("https://") {
        let url = reqwest::Url::parse(s).map_err(|_| "esa dirección no se entiende".to_owned())?;
        let get = |k: &str| {
            url.query_pairs()
                .find(|(n, _)| n == k)
                .map(|(_, v)| v.into_owned())
        };
        let code = get("code").ok_or("en esa dirección no hay código")?;
        return Ok((code, get("state")));
    }
    let (code, state) = match s.split_once('#') {
        Some((c, st)) => (c, Some(st.to_owned())),
        None => (s, None),
    };
    if code.is_empty() || code.chars().any(char::is_whitespace) {
        return Err("el código no está completo: cópialo entero".into());
    }
    Ok((code.to_owned(), state.filter(|s| !s.is_empty())))
}

#[derive(Debug, Clone, Deserialize)]
pub struct TokenAccount {
    pub uuid: String,
    #[serde(default)]
    pub email_address: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TokenOrg {
    pub uuid: String,
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Deserialize)]
pub struct Tokens {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    pub expires_in: u64,
    #[serde(default)]
    pub refresh_token_expires_in: Option<u64>,
    #[serde(default)]
    pub scope: Option<String>,
    #[serde(default)]
    pub account: Option<TokenAccount>,
    #[serde(default)]
    pub organization: Option<TokenOrg>,
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

impl Tokens {
    /// Pone lo nuevo en la credencial de Claude (`claudeAiOauth`), dejando el resto.
    pub fn apply(&self, oauth: &mut Value) {
        if !oauth.is_object() {
            *oauth = Value::Object(Default::default());
        }
        let now = now_ms();
        oauth["accessToken"] = self.access_token.clone().into();
        oauth["expiresAt"] = (now + self.expires_in * 1000).into();
        if let Some(r) = &self.refresh_token {
            oauth["refreshToken"] = r.clone().into();
        }
        if let Some(s) = self.refresh_token_expires_in {
            oauth["refreshTokenExpiresAt"] = (now + s * 1000).into();
        }
        if let Some(scope) = &self.scope {
            let list: Vec<Value> = scope.split_whitespace().map(|s| s.into()).collect();
            oauth["scopes"] = list.into();
        }
    }
}

/// La respuesta de error de OAuth (`{"error": "invalid_grant", …}`), sin nada más.
fn oauth_error(body: &str) -> Option<String> {
    serde_json::from_str::<Value>(body)
        .ok()?
        .get("error")?
        .as_str()
        .map(str::to_owned)
}

/// Cambia el código pegado por los tokens.
pub async fn exchange(
    http: &reqwest::Client,
    code: &str,
    state: &str,
    verifier: &str,
) -> Result<Tokens, String> {
    let body = serde_json::json!({
        "grant_type": "authorization_code",
        "code": code,
        "redirect_uri": REDIRECT_URI,
        "client_id": CLIENT_ID,
        "code_verifier": verifier,
        "state": state,
    });
    let resp = http
        .post(TOKEN_URL)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("no se pudo hablar con Claude: {}", e.without_url()))?;
    let status = resp.status();
    if !status.is_success() {
        let err = oauth_error(&resp.text().await.unwrap_or_default());
        return Err(match (status.as_u16(), err.as_deref()) {
            (400 | 401, Some("invalid_grant")) | (401, _) => {
                "Claude no acepta ese código (¿caducado o ya usado?): vuelve a empezar".into()
            }
            (_, Some(e)) => format!("Claude no ha dado la sesión ({status}, {e})"),
            _ => format!("Claude no ha dado la sesión ({status})"),
        });
    }
    resp.json()
        .await
        .map_err(|_| "Claude ha contestado algo que no se entiende".into())
}

pub enum RefreshError {
    /// La sesión ya no vale (Claude rechaza el token de renovación): hay que volver a entrar.
    Dead,
    /// Red, servidor… se puede reintentar.
    Transient(String),
}

pub async fn refresh(http: &reqwest::Client, refresh_token: &str) -> Result<Tokens, RefreshError> {
    // Sin `scope`: se renuevan los que ya tenía (pedir uno nuevo puede fallar con un token viejo).
    let body = serde_json::json!({
        "grant_type": "refresh_token",
        "refresh_token": refresh_token,
        "client_id": CLIENT_ID,
    });
    let resp = http
        .post(TOKEN_URL)
        .json(&body)
        .send()
        .await
        .map_err(|e| RefreshError::Transient(e.without_url().to_string()))?;
    let status = resp.status();
    if !status.is_success() {
        let err = oauth_error(&resp.text().await.unwrap_or_default());
        return Err(match refresh_failure(status.as_u16(), err.as_deref()) {
            RefreshError::Transient(_) => RefreshError::Transient(format!("{status}")),
            dead => dead,
        });
    }
    resp.json()
        .await
        .map_err(|_| RefreshError::Transient("respuesta que no se entiende".into()))
}

/// Qué hacer cuando Claude no renueva la sesión. Un 400/401/403 es que no acepta ese token de
/// renovación (con `invalid_grant` o sin decir por qué, p. ej. un «400 Bad Request» a secas):
/// reintentar no lo arregla, hay que volver a entrar. Salvo que el fallo sea de nido y no de la
/// cuenta (`invalid_client`, `unsupported_grant_type`). Lo demás (límites, servidor) se reintenta.
fn refresh_failure(status: u16, err: Option<&str>) -> RefreshError {
    match (status, err) {
        (400 | 401 | 403, Some("invalid_client" | "unsupported_grant_type")) => {
            RefreshError::Transient(String::new())
        }
        (400 | 401 | 403, _) => RefreshError::Dead,
        _ => RefreshError::Transient(String::new()),
    }
}

/// El perfil de la cuenta (`account`, `organization`), tal cual.
pub async fn profile(http: &reqwest::Client, access_token: &str) -> Result<Value, String> {
    let resp = http
        .get(PROFILE_URL)
        .bearer_auth(access_token)
        .header("anthropic-beta", BETA)
        .send()
        .await
        .map_err(|e| format!("no se pudo pedir el perfil: {}", e.without_url()))?;
    if !resp.status().is_success() {
        return Err(format!("no se pudo pedir el perfil ({})", resp.status()));
    }
    resp.json()
        .await
        .map_err(|_| "el perfil no se entiende".into())
}

/// Lo que sabe Claude Code de la cuenta (`oauthAccount` en `~/.claude.json`), a partir del perfil
/// y de lo que vino con los tokens. Claude completa el resto él solo.
pub fn identity(profile: &Value, tokens: &Tokens) -> Option<Value> {
    let acc = &profile["account"];
    let org = &profile["organization"];
    let uuid = acc["uuid"]
        .as_str()
        .or(tokens.account.as_ref().map(|a| a.uuid.as_str()))?;
    let email = acc["email"].as_str().or(tokens
        .account
        .as_ref()
        .and_then(|a| a.email_address.as_deref()))?;
    let mut v = serde_json::json!({ "accountUuid": uuid, "emailAddress": email });
    let org_uuid = org["uuid"]
        .as_str()
        .or(tokens.organization.as_ref().map(|o| o.uuid.as_str()));
    let org_name = org["name"]
        .as_str()
        .or(tokens.organization.as_ref().and_then(|o| o.name.as_deref()));
    for (k, val) in [
        ("organizationUuid", org_uuid.map(Value::from)),
        ("organizationName", org_name.map(Value::from)),
        ("displayName", acc.get("display_name").cloned()),
        ("fullName", acc.get("full_name").cloned()),
        ("accountCreatedAt", acc.get("created_at").cloned()),
        ("billingType", org.get("billing_type").cloned()),
        (
            "hasExtraUsageEnabled",
            org.get("has_extra_usage_enabled").cloned(),
        ),
        (
            "subscriptionCreatedAt",
            org.get("subscription_created_at").cloned(),
        ),
        ("organizationType", org.get("organization_type").cloned()),
        (
            "organizationRateLimitTier",
            org.get("rate_limit_tier").cloned(),
        ),
    ] {
        if let Some(val) = val.filter(|v| !v.is_null()) {
            v[k] = val;
        }
    }
    Some(v)
}

/// El plan como lo guarda Claude en la credencial (`max`, `pro`…).
pub fn subscription_type(profile: &Value) -> Option<&'static str> {
    match profile["organization"]["organization_type"].as_str()? {
        "claude_max" => Some("max"),
        "claude_pro" => Some("pro"),
        "claude_enterprise" => Some("enterprise"),
        "claude_team" => Some("team"),
        _ => None,
    }
}

/// La credencial nueva, como la escribe `claude /login`.
pub fn credential(tokens: &Tokens, profile: &Value) -> Value {
    let mut oauth = serde_json::json!({});
    tokens.apply(&mut oauth);
    if let Some(t) = subscription_type(profile) {
        oauth["subscriptionType"] = t.into();
        oauth["subscriptionTypeSource"] = "sign_in".into();
    }
    if let Some(t) = profile["organization"]["rate_limit_tier"].as_str() {
        oauth["rateLimitTier"] = t.into();
    }
    oauth
}

pub enum UsageError {
    /// Demasiadas peticiones: esperar (lo que diga `Retry-After`, si lo dice).
    RateLimited(Option<u64>),
    /// El token no vale.
    Unauthorized,
    Other(String),
}

pub async fn usage(http: &reqwest::Client, access_token: &str) -> Result<Value, UsageError> {
    let resp = http
        .get(USAGE_URL)
        .bearer_auth(access_token)
        .header("anthropic-beta", BETA)
        .send()
        .await
        .map_err(|e| UsageError::Other(e.without_url().to_string()))?;
    match resp.status().as_u16() {
        200..=299 => resp
            .json()
            .await
            .map_err(|_| UsageError::Other("respuesta que no se entiende".into())),
        401 => Err(UsageError::Unauthorized),
        429 => Err(UsageError::RateLimited(
            resp.headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.trim().parse::<f64>().ok())
                .map(|s| s.max(0.0) as u64),
        )),
        s => Err(UsageError::Other(format!("HTTP {s}"))),
    }
}

/// Una ventana de uso: el porcentaje gastado, cuándo se reinicia y, en las semanales, el ritmo.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
pub struct Usage {
    pub five_hour: Option<Window>,
    pub seven_day: Option<Window>,
    /// Límites semanales por modelo.
    #[serde(default)]
    pub models: Vec<Window>,
}

const WEEK_S: f64 = 7.0 * 86400.0;
/// Recién reiniciada la semana, el ritmo no dice nada.
const PACE_QUIET_S: f64 = 86400.0;
/// "Por encima del ritmo": tantos puntos por encima de lo esperado.
const AHEAD_PCT: f64 = 15.0;

pub(crate) fn ts(s: &str) -> Option<f64> {
    // `2026-11-09T12:00:00.123456+00:00` o con `Z`.
    let s = s.trim();
    let (date, rest) = s.split_once('T')?;
    let mut d = date.split('-').map(|x| x.parse::<i64>());
    let (y, m, day) = (d.next()?.ok()?, d.next()?.ok()?, d.next()?.ok()?);
    let (time, offset_s) = if let Some(t) = rest.strip_suffix('Z') {
        (t, 0)
    } else if let Some(i) = rest.rfind(['+', '-']) {
        let (t, off) = rest.split_at(i);
        let sign = if off.starts_with('-') { -1 } else { 1 };
        let mut hm = off[1..].split(':').map(|x| x.parse::<i64>());
        let (oh, om) = (
            hm.next()?.ok()?,
            hm.next().and_then(Result::ok).unwrap_or(0),
        );
        (t, sign * (oh * 3600 + om * 60))
    } else {
        (rest, 0)
    };
    let mut t = time.split(':');
    let (hh, mm) = (
        t.next()?.parse::<i64>().ok()?,
        t.next()?.parse::<i64>().ok()?,
    );
    let ss = t.next().map_or(Ok(0.0), str::parse::<f64>).ok()?;
    // Días desde 1970 (algoritmo de Howard Hinnant).
    let (yy, mo) = if m <= 2 { (y - 1, m + 9) } else { (y, m - 3) };
    let era = yy.div_euclid(400);
    let yoe = yy - era * 400;
    let doy = (153 * mo + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    Some((days * 86400 + hh * 3600 + mm * 60 - offset_s) as f64 + ss)
}

fn iso(secs: f64) -> String {
    let s = secs.floor() as i64;
    let days = s.div_euclid(86400);
    let rem = s.rem_euclid(86400);
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}+00:00",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// El ritmo de una ventana semanal medida en `at` (segundos): lo esperado si el uso fuera
/// uniforme y, con esa misma recta, si llega al reinicio.
fn pace(w: &mut Window, at: f64) {
    let (Some(pct), Some(next)) = (w.pct, w.resets_at.as_deref().and_then(ts)) else {
        return;
    };
    let remaining = (next - at).rem_euclid(WEEK_S);
    let elapsed = if remaining == 0.0 {
        0.0
    } else {
        WEEK_S - remaining
    };
    if elapsed < PACE_QUIET_S {
        return;
    }
    let expected = (elapsed / WEEK_S * 100.0).min(100.0);
    w.expected_pct = Some(expected);
    w.ahead_of_pace = Some(pct - expected >= AHEAD_PCT);
    if pct <= 0.0 {
        w.will_last_to_reset = Some(true);
        return;
    }
    let rate = pct / elapsed;
    w.will_last_to_reset = Some(pct + rate * (WEEK_S - elapsed) <= 100.0);
    w.projected_exhaustion_at = Some(iso(at + (100.0 - pct).max(0.0) / rate));
}

/// La respuesta de `/api/oauth/usage`, medida en `at` (segundos desde 1970).
pub fn parse_usage(v: &Value, at: f64) -> Option<Usage> {
    let window = |w: &Value, pct_key: &str| -> Option<Window> {
        let pct = w.get(pct_key)?.as_f64()?;
        Some(Window {
            name: None,
            pct: Some(pct),
            resets_at: w["resets_at"].as_str().map(str::to_owned),
            expected_pct: None,
            ahead_of_pace: None,
            will_last_to_reset: None,
            projected_exhaustion_at: None,
        })
    };
    let five_hour = window(&v["five_hour"], "utilization");
    let mut seven_day = window(&v["seven_day"], "utilization");
    if let Some(w) = &mut seven_day {
        pace(w, at);
    }
    let mut models = Vec::new();
    for lim in v["limits"].as_array().into_iter().flatten() {
        let Some(name) = lim["scope"]["model"]["display_name"].as_str() else {
            continue;
        };
        let Some(mut w) = window(lim, "percent") else {
            continue;
        };
        w.name = Some(name.to_owned());
        pace(&mut w, at);
        models.push(w);
    }
    if five_hour.is_none() && seven_day.is_none() && models.is_empty() {
        return None;
    }
    Some(Usage {
        five_hour,
        seven_day,
        models,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_refused_refresh_asks_to_log_in_again() {
        let dead = |s, e| matches!(refresh_failure(s, e), RefreshError::Dead);
        // Lo de siempre: el token de renovación ya no vale.
        assert!(dead(400, Some("invalid_grant")));
        // Un 400/401/403 sin `invalid_grant` (p. ej. un «400 Bad Request» a secas) tampoco se
        // arregla reintentando: hay que volver a entrar.
        assert!(dead(400, None));
        assert!(dead(400, Some("invalid_request")));
        assert!(dead(401, None));
        assert!(dead(403, Some("access_denied")));
        // Lo que no es culpa de la cuenta sí se reintenta.
        assert!(!dead(400, Some("invalid_client")));
        assert!(!dead(400, Some("unsupported_grant_type")));
        assert!(!dead(429, None));
        assert!(!dead(500, None));
        assert!(!dead(503, Some("invalid_grant")));
    }

    #[test]
    fn authorize_url_has_pkce_and_hint() {
        let s = start(Some("ana@example.com"));
        let url = reqwest::Url::parse(&s.url).unwrap();
        let q: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(q["code"], "true");
        assert_eq!(q["client_id"], CLIENT_ID);
        assert_eq!(q["redirect_uri"], REDIRECT_URI);
        assert_eq!(q["state"], s.state);
        assert_eq!(q["code_challenge_method"], "S256");
        assert_eq!(
            q["code_challenge"],
            URL_SAFE_NO_PAD.encode(Sha256::digest(s.verifier.as_bytes()))
        );
        assert_eq!(q["login_hint"], "ana@example.com");
        assert!(q["scope"].contains("user:inference"));
        assert!(!start(None).url.contains("login_hint"));
    }

    #[test]
    fn pasted_codes() {
        assert_eq!(
            parse_pasted(" abc#xyz \n").unwrap(),
            ("abc".into(), Some("xyz".into()))
        );
        assert_eq!(parse_pasted("abc").unwrap(), ("abc".into(), None));
        assert_eq!(
            parse_pasted("https://platform.claude.com/oauth/code/callback?code=c1&state=s1")
                .unwrap(),
            ("c1".into(), Some("s1".into()))
        );
        assert!(parse_pasted("").is_err());
        assert!(parse_pasted("a b#c").is_err());
    }

    #[test]
    fn dates_round_trip() {
        let t = ts("2026-11-09T12:34:56.5+00:00").unwrap();
        assert_eq!(iso(t), "2026-11-09T12:34:56+00:00");
        assert_eq!(ts("2026-11-09T14:34:56+02:00").unwrap(), t.floor());
        assert_eq!(ts("2026-11-09T12:34:56Z").unwrap(), t.floor());
        assert_eq!(ts("1970-01-01T00:00:00Z").unwrap(), 0.0);
    }

    #[test]
    fn usage_with_pace() {
        // A mitad de semana (3,5 días), con el 70 % gastado: por encima del ritmo, no llega.
        let at = ts("2026-11-09T12:00:00Z").unwrap();
        let v = serde_json::json!({
            "five_hour": {"utilization": 12.0, "resets_at": "2026-11-09T14:00:00+00:00"},
            "seven_day": {"utilization": 70.0, "resets_at": "2026-11-13T00:00:00+00:00"},
            "limits": [
                {"scope": {"model": {"display_name": "Fable"}}, "percent": 10.0,
                 "resets_at": "2026-11-13T00:00:00+00:00"},
                {"scope": {}, "percent": 5.0}
            ]
        });
        let u = parse_usage(&v, at).unwrap();
        assert_eq!(u.five_hour.as_ref().unwrap().pct, Some(12.0));
        assert_eq!(u.five_hour.as_ref().unwrap().expected_pct, None);
        let w = u.seven_day.unwrap();
        assert_eq!(w.expected_pct, Some(50.0));
        assert_eq!(w.ahead_of_pace, Some(true));
        assert_eq!(w.will_last_to_reset, Some(false));
        assert!(w.projected_exhaustion_at.is_some());
        assert_eq!(u.models.len(), 1);
        assert_eq!(u.models[0].name.as_deref(), Some("Fable"));
        assert_eq!(u.models[0].will_last_to_reset, Some(true));
        assert!(parse_usage(&serde_json::json!({}), at).is_none());
    }

    #[test]
    fn identity_and_credential_from_login() {
        let tokens: Tokens = serde_json::from_value(serde_json::json!({
            "access_token": "a", "refresh_token": "r", "expires_in": 3600,
            "scope": "user:profile user:inference",
            "account": {"uuid": "u1", "email_address": "ana@example.com"},
            "organization": {"uuid": "o1", "name": "Ana"}
        }))
        .unwrap();
        let profile = serde_json::json!({
            "account": {"uuid": "u1", "email": "ana@example.com", "display_name": "Ana"},
            "organization": {"uuid": "o1", "organization_type": "claude_max",
                             "rate_limit_tier": "default_claude_max_20x", "billing_type": null}
        });
        let id = identity(&profile, &tokens).unwrap();
        assert_eq!(id["accountUuid"], "u1");
        assert_eq!(id["emailAddress"], "ana@example.com");
        assert_eq!(id["organizationName"], "Ana");
        assert_eq!(id["displayName"], "Ana");
        assert!(id.get("billingType").is_none());
        let c = credential(&tokens, &profile);
        assert_eq!(c["accessToken"], "a");
        assert_eq!(c["refreshToken"], "r");
        assert_eq!(c["subscriptionType"], "max");
        assert_eq!(c["rateLimitTier"], "default_claude_max_20x");
        assert_eq!(c["scopes"][1], "user:inference");
        assert!(c["expiresAt"].as_u64().unwrap() > now_ms());
    }
}
