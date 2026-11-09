//! Las cuentas de Claude de este equipo: cuál usa Claude ahora, cambiar de una a otra, entrar en
//! una nueva (o volver a entrar) y lo que lleva gastado cada una.
//!
//! - Cada cuenta guardada tiene su credencial (`<data>/accounts/<n>.json`, solo para el usuario) y
//!   su identidad (`<data>/accounts.json`, sin secretos). La de la cuenta activa la lleva Claude
//!   (ver `claude_store`): mientras está activa, la nuestra no se toca ni se renueva (renovarla
//!   cambiaría el token que usan los Claude abiertos); al cambiar a otra, se guarda la suya.
//! - El uso se pregunta a Anthropic poco (la API admite muy pocas peticiones por hora): la activa
//!   cada pocos minutos y las demás menos; tras un «demasiadas peticiones», se espera más.
//! - La primera vez se traen las cuentas de cswap (claude-swap), si lo hay, con sus números.
//! - Cambio automático (por equipo, apagado de entrada): cuando la cuenta activa llega al
//!   [`AUTO_PCT`] de su ventana de 5 h o de la semanal, pasa a la siguiente que no esté agotada.
//!
//! A la web y a la app solo salen identidad y uso; nunca tokens.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::{Mutex, Notify};

use crate::claude_store::{Live, write_atomic};
use crate::oauth::{self, RefreshError, Usage, UsageError};

/// Cada cuánto se mira el uso de la cuenta activa, y el de las demás.
const ACTIVE_EVERY: Duration = Duration::from_secs(180);
const OTHERS_EVERY: Duration = Duration::from_secs(600);
/// Tras un «demasiadas peticiones»: al menos esto, y el doble cada vez hasta el máximo.
const AFTER_429: Duration = Duration::from_secs(360);
const AFTER_429_MAX: Duration = Duration::from_secs(1800);
/// Tras otro error (red…).
const AFTER_ERROR: Duration = Duration::from_secs(120);
/// Un dato más viejo que esto se enseña como «último bueno», no como el de ahora.
const FRESH_S: u64 = 20 * 60;
/// Renovar el token si caduca antes de esto.
const EXPIRY_MARGIN_MS: u64 = 5 * 60 * 1000;
/// Un inicio de sesión a medias vale esto.
const LOGIN_TTL: Duration = Duration::from_secs(15 * 60);
/// El cambio automático pasa de cuenta desde este uso (de la ventana de 5 h o de la semanal).
pub const AUTO_PCT: f64 = 99.0;
/// Una cuenta a la que no se pudo cambiar no se vuelve a probar hasta pasado esto.
const AUTO_RETRY: Duration = Duration::from_secs(600);

fn now_s() -> u64 {
    oauth::now_ms() / 1000
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Saved {
    number: u32,
    email: String,
    uuid: String,
    #[serde(default)]
    org_uuid: Option<String>,
    #[serde(default)]
    organization: Option<String>,
    #[serde(default)]
    alias: Option<String>,
    /// `oauthAccount` para `~/.claude.json`.
    #[serde(default)]
    identity: Value,
    /// Hay que volver a entrar: la sesión guardada ya no vale.
    #[serde(default)]
    relogin: bool,
    #[serde(default)]
    usage: Option<Usage>,
    /// Cuándo se midió `usage` (segundos desde 1970).
    #[serde(default)]
    usage_at: Option<u64>,
}

impl Saved {
    fn is(&self, uuid: &str, org: Option<&str>) -> bool {
        self.uuid == uuid
            && (org.is_none() || self.org_uuid.is_none() || self.org_uuid.as_deref() == org)
    }

    fn reference(&self) -> AccountRef {
        AccountRef {
            number: self.number,
            email: Some(self.email.clone()),
        }
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct SavedFile {
    accounts: Vec<Saved>,
    /// Ya se miró si había cuentas de cswap que traer.
    #[serde(default)]
    cswap_imported: bool,
    /// Cambio automático de cuenta al llegar al [`AUTO_PCT`].
    #[serde(default)]
    auto_switch: bool,
}

/// Una cuenta, como la ven la web y la app.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Account {
    pub number: u32,
    pub email: Option<String>,
    pub alias: Option<String>,
    pub organization: Option<String>,
    pub active: bool,
    pub disabled: bool,
    /// `ok`, `relogin_required` (hay que volver a entrar), `unavailable` (sin dato reciente).
    pub status: Option<String>,
    pub usage: Option<Usage>,
    pub age_s: Option<f64>,
    /// La última medida, cuando `usage` falta porque es vieja.
    pub last_good: Option<Usage>,
    pub last_good_age_s: Option<f64>,
    /// Por qué falló la última medida (si falló).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub problem: Option<String>,
}

/// Lo que se manda a la web.
#[derive(Debug, Clone, Default, Serialize)]
pub struct View {
    pub accounts: Vec<Account>,
    /// Segundos desde la medida de la cuenta activa.
    pub age_s: Option<u64>,
    pub error: Option<String>,
    /// Siempre: nido ya no necesita cswap (se queda para la web y la app de antes).
    pub installed: bool,
    /// Cambio automático de cuenta, y desde qué uso.
    pub auto_switch: bool,
    pub auto_pct: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AccountRef {
    pub number: u32,
    #[serde(default)]
    pub email: Option<String>,
}

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

/// Lo que devuelve acabar de entrar.
#[derive(Debug, Clone, Serialize)]
pub struct LoggedIn {
    pub number: u32,
    pub email: String,
    /// Cuenta nueva (si no, se ha vuelto a entrar en una que ya estaba).
    pub new: bool,
    /// Es la que usa Claude ahora.
    pub active: bool,
}

/// Número, correo o alias. Nunca empieza por `-`.
pub fn valid_target(t: &str) -> bool {
    !t.is_empty()
        && t.len() <= 254
        && !t.starts_with('-')
        && t.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"@._+-".contains(&b))
}

struct Pending {
    verifier: String,
    at: Instant,
}

#[derive(Default)]
struct Timing {
    next: HashMap<u32, Instant>,
    /// Espera tras el último «demasiadas peticiones» de cada cuenta.
    backoff: HashMap<u32, Duration>,
    /// La última medida falló (y por qué), para enseñarlo.
    failing: HashMap<u32, String>,
    /// Cuentas a las que el cambio automático no pudo pasar, hasta cuándo no probarlas.
    auto_skip: HashMap<u32, Instant>,
    /// Ya se dijo que no queda ninguna cuenta a la que pasar (para no repetirlo cada pasada).
    auto_stuck: bool,
}

struct Inner {
    file: SavedFile,
    timing: Timing,
}

pub struct Accounts {
    live: Live,
    /// `<data>/accounts.json`.
    file: PathBuf,
    /// `<data>/accounts/`.
    creds_dir: PathBuf,
    http: reqwest::Client,
    /// Todo lo que cambia cuentas o credenciales va de uno en uno.
    inner: Mutex<Inner>,
    view: std::sync::Mutex<View>,
    logins: std::sync::Mutex<HashMap<String, Pending>>,
    kick: Notify,
    /// Avisa cuando el cambio automático ha pasado de cuenta.
    auto_switched: Notify,
}

impl Accounts {
    pub fn new(home: &Path, data: &Path) -> Arc<Self> {
        let file = data.join("accounts.json");
        let saved: SavedFile = std::fs::read(&file)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        Arc::new(Self {
            live: Live::new(home),
            file,
            creds_dir: data.join("accounts"),
            http: oauth::client(),
            inner: Mutex::new(Inner {
                file: saved,
                timing: Timing::default(),
            }),
            view: std::sync::Mutex::new(View {
                installed: !disabled(),
                auto_pct: AUTO_PCT,
                ..Default::default()
            }),
            logins: Default::default(),
            kick: Notify::new(),
            auto_switched: Notify::new(),
        })
    }

    /// Mira el uso de vez en cuando, para siempre.
    pub fn spawn(self: &Arc<Self>, home: PathBuf) {
        if disabled() {
            return;
        }
        let me = self.clone();
        tokio::spawn(async move {
            {
                let mut inner = me.inner.lock().await;
                if !inner.file.cswap_imported {
                    let live_uuid = blocking({
                        let live = me.live.clone();
                        move || live.read_identity()
                    })
                    .await
                    .and_then(|v| v["accountUuid"].as_str().map(str::to_owned));
                    let n = me.import_cswap(&mut inner, &home, live_uuid.as_deref());
                    inner.file.cswap_imported = true;
                    if n > 0 {
                        eprintln!("nido: {n} cuentas de Claude traídas de cswap");
                    }
                    me.save(&inner);
                }
            }
            loop {
                me.pass().await;
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(30)) => {}
                    _ = me.kick.notified() => {}
                }
            }
        });
    }

    pub fn view(&self) -> View {
        self.view.lock().expect("vista").clone()
    }

    /// Se dispara cada vez que el cambio automático pasa de cuenta.
    pub fn auto_switched(&self) -> &Notify {
        &self.auto_switched
    }

    /// Enciende o apaga el cambio automático de este equipo.
    pub async fn set_auto(&self, on: bool) -> Result<View, String> {
        if disabled() {
            return Err(OFF.into());
        }
        {
            let mut inner = self.inner.lock().await;
            inner.file.auto_switch = on;
            inner.timing.auto_skip.clear();
            inner.timing.auto_stuck = false;
            self.save(&inner);
        }
        let (identity, _) = self.live_now().await;
        self.redraw(identity.as_ref().and_then(|v| v["accountUuid"].as_str()))
            .await;
        // Si ya va pasada del límite, que cambie ahora y no en la próxima medida.
        self.kick.notify_one();
        Ok(self.view())
    }

    fn save(&self, inner: &Inner) {
        let res = (|| {
            if let Some(dir) = self.file.parent() {
                std::fs::create_dir_all(dir)?;
            }
            write_atomic(&self.file, &serde_json::to_vec_pretty(&inner.file)?)
        })();
        if let Err(e) = res {
            eprintln!("nido: no se pudo guardar {}: {e}", self.file.display());
        }
    }

    fn creds_path(&self, number: u32) -> PathBuf {
        self.creds_dir.join(format!("{number}.json"))
    }

    fn load_creds(&self, number: u32) -> Option<Value> {
        let raw = std::fs::read(self.creds_path(number)).ok()?;
        serde_json::from_slice::<Value>(&raw)
            .ok()
            .filter(Value::is_object)
    }

    fn save_creds(&self, number: u32, oauth: &Value) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.creds_dir)?;
        crate::sys::private_dir(&self.creds_dir)?;
        write_atomic(&self.creds_path(number), &serde_json::to_vec(oauth)?)
    }

    /// La cuenta que usa Claude ahora (por su identidad en `~/.claude.json`) y su credencial.
    async fn live_now(&self) -> (Option<Value>, Option<Value>) {
        let live = self.live.clone();
        blocking(move || (live.read_identity(), live.read_oauth().ok().flatten())).await
    }

    /// Una pasada: apunta la cuenta activa si es nueva y mide las que tocan.
    async fn pass(&self) {
        let (identity, live_oauth) = self.live_now().await;
        let active_uuid = identity
            .as_ref()
            .and_then(|v| v["accountUuid"].as_str())
            .map(str::to_owned);
        let numbers: Vec<(u32, bool)> = {
            let mut inner = self.inner.lock().await;
            if let (Some(id), Some(_)) = (&identity, &live_oauth)
                && adopt(&mut inner.file, id).is_some_and(|(_, changed)| changed)
            {
                self.save(&inner);
            }
            let now = Instant::now();
            inner
                .file
                .accounts
                .iter()
                .map(|a| (a.number, Some(&a.uuid) == active_uuid.as_ref()))
                .filter(|(n, _)| inner.timing.next.get(n).is_none_or(|t| *t <= now))
                .collect()
        };
        for (number, active) in numbers {
            self.measure(number, active, live_oauth.as_ref()).await;
        }
        self.redraw(active_uuid.as_deref()).await;
        self.auto(active_uuid.as_deref()).await;
    }

    /// El cambio automático: si la cuenta activa (con un dato reciente) ha llegado al
    /// [`AUTO_PCT`], pasa a la siguiente que pueda.
    async fn auto(&self, active_uuid: Option<&str>) {
        let (from, to, pct) = {
            let mut inner = self.inner.lock().await;
            if !inner.file.auto_switch {
                return;
            }
            let now = now_s();
            let Some(cur) = active_uuid
                .and_then(|u| inner.file.accounts.iter().find(|a| a.uuid == u))
                .cloned()
            else {
                return;
            };
            let fresh = cur
                .usage_at
                .is_some_and(|t| now.saturating_sub(t) <= FRESH_S);
            let Some(pct) = cur
                .usage
                .as_ref()
                .filter(|_| fresh)
                .and_then(|u| spent(u, now as f64))
                .filter(|p| *p >= AUTO_PCT)
            else {
                inner.timing.auto_stuck = false;
                return;
            };
            let at = Instant::now();
            inner.timing.auto_skip.retain(|_, until| *until > at);
            let skip: Vec<u32> = inner.timing.auto_skip.keys().copied().collect();
            match next_account(&inner.file.accounts, cur.number, &skip, now as f64) {
                Some(to) => {
                    inner.timing.auto_stuck = false;
                    (cur.number, to, pct)
                }
                None => {
                    if !inner.timing.auto_stuck {
                        eprintln!(
                            "nido: la cuenta {} va al {pct:.0} % y no queda ninguna otra a la que pasar",
                            cur.number
                        );
                        inner.timing.auto_stuck = true;
                    }
                    return;
                }
            }
        };
        match self.switch(&to.to_string()).await {
            Ok(r) if r.switched => {
                eprintln!("nido: la cuenta {from} va al {pct:.0} %: cambio automático a la {to}");
                self.auto_switched.notify_one();
            }
            Ok(_) => {}
            Err(e) => {
                eprintln!("nido: el cambio automático no pudo pasar a la cuenta {to}: {e}");
                self.inner
                    .lock()
                    .await
                    .timing
                    .auto_skip
                    .insert(to, Instant::now() + AUTO_RETRY);
            }
        }
    }

    /// Mide el uso de una cuenta (renovando antes su token si es de las guardadas).
    async fn measure(&self, number: u32, active: bool, live_oauth: Option<&Value>) {
        let mut inner = self.inner.lock().await;
        let Some(acc) = inner
            .file
            .accounts
            .iter()
            .find(|a| a.number == number)
            .cloned()
        else {
            return;
        };
        // Si se reinició nido, no volver a preguntar lo que ya se sabe.
        if let (std::collections::hash_map::Entry::Vacant(slot), Some(at)) =
            (inner.timing.next.entry(number), acc.usage_at)
        {
            let every = if active { ACTIVE_EVERY } else { OTHERS_EVERY };
            let age = Duration::from_secs(now_s().saturating_sub(at));
            if age < every {
                slot.insert(Instant::now() + (every - age));
                return;
            }
        }
        let token = if active {
            // La de Claude: se lee. Si ha caducado, la renueva Claude; solo si no hay ningún
            // Claude en marcha (nadie la tiene en memoria) la renueva nido, en sus cerrojos.
            match live_oauth.filter(|o| !expiring(o, 60_000)) {
                Some(o) => o["accessToken"].as_str().map(str::to_owned),
                None => match self.refresh_live_if_idle(&mut inner, number).await {
                    Some(t) => Some(t),
                    None => {
                        inner
                            .timing
                            .next
                            .insert(number, Instant::now() + Duration::from_secs(60));
                        return;
                    }
                },
            }
        } else {
            if acc.relogin {
                inner
                    .timing
                    .next
                    .insert(number, Instant::now() + OTHERS_EVERY);
                return;
            }
            match self.fresh_token(&mut inner, number, false).await {
                Ok(t) => Some(t),
                Err(e) => {
                    inner.timing.failing.insert(number, e);
                    inner
                        .timing
                        .next
                        .insert(number, Instant::now() + AFTER_ERROR);
                    self.save(&inner);
                    return;
                }
            }
        };
        let Some(mut token) = token else { return };
        let mut retried = false;
        loop {
            match oauth::usage(&self.http, &token).await {
                Ok(v) => {
                    let at = now_s();
                    let usage = oauth::parse_usage(&v, at as f64);
                    if let Some(a) = inner.file.accounts.iter_mut().find(|a| a.number == number) {
                        if usage.is_some() {
                            a.usage = usage;
                            a.usage_at = Some(at);
                        }
                        a.relogin = false;
                    }
                    inner.timing.failing.remove(&number);
                    inner.timing.backoff.remove(&number);
                    let every = if active { ACTIVE_EVERY } else { OTHERS_EVERY };
                    inner.timing.next.insert(number, Instant::now() + every);
                    self.save(&inner);
                    return;
                }
                Err(UsageError::RateLimited(after)) => {
                    let prev = inner.timing.backoff.get(&number).copied();
                    let wait = prev
                        .map_or(AFTER_429, |p| (p * 2).min(AFTER_429_MAX))
                        .max(Duration::from_secs(after.unwrap_or(0)));
                    inner.timing.backoff.insert(number, wait);
                    inner.timing.next.insert(number, Instant::now() + wait);
                    if !inner.timing.failing.contains_key(&number) {
                        eprintln!(
                            "nido: Anthropic pide esperar con la cuenta {number} ({} s)",
                            wait.as_secs()
                        );
                    }
                    inner
                        .timing
                        .failing
                        .insert(number, "Anthropic pide esperar".into());
                    return;
                }
                Err(UsageError::Unauthorized) if !active && !retried => {
                    // El token de acceso ya no vale aunque no haya caducado: renovarlo una vez.
                    retried = true;
                    match self.fresh_token(&mut inner, number, true).await {
                        Ok(t) => token = t,
                        Err(e) => {
                            inner.timing.failing.insert(number, e);
                            inner
                                .timing
                                .next
                                .insert(number, Instant::now() + AFTER_ERROR);
                            self.save(&inner);
                            return;
                        }
                    }
                }
                Err(UsageError::Unauthorized) => {
                    // La activa: Claude la renovará. Las demás: ya se renovó y sigue sin valer.
                    if !active {
                        if let Some(a) = inner.file.accounts.iter_mut().find(|a| a.number == number)
                        {
                            a.relogin = true;
                        }
                        self.save(&inner);
                    }
                    inner
                        .timing
                        .next
                        .insert(number, Instant::now() + AFTER_ERROR);
                    return;
                }
                Err(UsageError::Other(e)) => {
                    if inner.timing.failing.get(&number) != Some(&e) {
                        eprintln!("nido: no se pudo medir el uso de la cuenta {number}: {e}");
                    }
                    inner.timing.failing.insert(number, e);
                    inner
                        .timing
                        .next
                        .insert(number, Instant::now() + AFTER_ERROR);
                    return;
                }
            }
        }
    }

    /// Renueva la sesión de Claude si ha caducado y no hay ningún Claude en marcha (en un equipo
    /// sin Claude abierto nadie la renueva, y sin ella no se sabe el uso). Dentro de los cerrojos
    /// de Claude: uno que arranque entretanto espera y lee la nueva.
    async fn refresh_live_if_idle(&self, inner: &mut Inner, number: u32) -> Option<String> {
        let live = self.live.clone();
        let (locks, oauth) = blocking(move || -> Option<_> {
            if crate::claude::any_running() {
                return None;
            }
            let locks = live.credentials_lock().ok()?;
            let oauth = live.read_oauth().ok()??;
            Some((locks, oauth))
        })
        .await?;
        if !expiring(&oauth, 60_000) {
            return oauth["accessToken"].as_str().map(str::to_owned);
        }
        let refresh = oauth["refreshToken"].as_str()?.to_owned();
        let tokens = match oauth::refresh(&self.http, &refresh).await {
            Ok(t) => t,
            Err(RefreshError::Dead) => {
                eprintln!(
                    "nido: la sesión de Claude de este equipo ha caducado: hay que volver a entrar"
                );
                set_relogin(inner, number, true);
                self.save(inner);
                return None;
            }
            Err(RefreshError::Transient(e)) => {
                eprintln!("nido: no se pudo renovar la sesión de Claude: {e}");
                return None;
            }
        };
        let mut fresh = oauth;
        tokens.apply(&mut fresh);
        let live = self.live.clone();
        let written = blocking(move || {
            let r = live.swap_oauth_locked(&fresh);
            drop(locks);
            r
        })
        .await;
        match written {
            Ok(_) => Some(tokens.access_token),
            Err(e) => {
                eprintln!("nido: no se pudo guardar la sesión renovada de Claude: {e}");
                None
            }
        }
    }

    /// El token de acceso de una cuenta guardada (no la activa), renovándolo si va a caducar (o
    /// si `force`). Lo renovado se guarda antes de usarlo: el token de renovación viejo deja de
    /// valer.
    async fn fresh_token(
        &self,
        inner: &mut Inner,
        number: u32,
        force: bool,
    ) -> Result<String, String> {
        let Some(mut creds) = self.load_creds(number) else {
            set_relogin(inner, number, true);
            return Err("no hay sesión guardada de esta cuenta".into());
        };
        if !force && !expiring(&creds, EXPIRY_MARGIN_MS) {
            return creds["accessToken"]
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| "la sesión guardada está incompleta".into());
        }
        let Some(refresh) = creds["refreshToken"].as_str().map(str::to_owned) else {
            set_relogin(inner, number, true);
            return Err("la sesión guardada no se puede renovar".into());
        };
        match oauth::refresh(&self.http, &refresh).await {
            Ok(t) => {
                t.apply(&mut creds);
                self.save_creds(number, &creds)
                    .map_err(|e| format!("no se pudo guardar la sesión renovada: {e}"))?;
                set_relogin(inner, number, false);
                Ok(t.access_token)
            }
            Err(RefreshError::Dead) => {
                set_relogin(inner, number, true);
                Err("la sesión ha caducado: hay que volver a entrar".into())
            }
            Err(RefreshError::Transient(e)) => Err(format!("no se pudo renovar la sesión: {e}")),
        }
    }

    /// Rehace la vista para la web.
    async fn redraw(&self, active_uuid: Option<&str>) {
        let inner = self.inner.lock().await;
        let now = now_s();
        let mut accounts: Vec<Account> = inner
            .file
            .accounts
            .iter()
            .map(|a| {
                let active = Some(a.uuid.as_str()) == active_uuid;
                let age = a.usage_at.map(|t| now.saturating_sub(t));
                let fresh = age.is_some_and(|s| s <= FRESH_S);
                let status = if a.relogin {
                    "relogin_required"
                } else if a.usage.is_some() && !fresh {
                    "unavailable"
                } else {
                    "ok"
                };
                Account {
                    number: a.number,
                    email: Some(a.email.clone()),
                    alias: a.alias.clone(),
                    organization: a.organization.clone(),
                    active,
                    disabled: false,
                    status: Some(status.into()),
                    usage: if fresh { a.usage.clone() } else { None },
                    age_s: if fresh { age.map(|s| s as f64) } else { None },
                    last_good: if fresh { None } else { a.usage.clone() },
                    last_good_age_s: if fresh { None } else { age.map(|s| s as f64) },
                    problem: inner.timing.failing.get(&a.number).cloned(),
                }
            })
            .collect();
        accounts.sort_by_key(|a| a.number);
        let age_s = inner
            .file
            .accounts
            .iter()
            .find(|a| Some(a.uuid.as_str()) == active_uuid)
            .and_then(|a| a.usage_at)
            .map(|t| now.saturating_sub(t));
        *self.view.lock().expect("vista") = View {
            accounts,
            age_s,
            error: None,
            installed: true,
            auto_switch: inner.file.auto_switch,
            auto_pct: AUTO_PCT,
        };
    }

    fn find<'a>(file: &'a SavedFile, target: &str) -> Option<&'a Saved> {
        let t = target.trim();
        if let Ok(n) = t.parse::<u32>() {
            return file.accounts.iter().find(|a| a.number == n);
        }
        file.accounts
            .iter()
            .find(|a| a.email.eq_ignore_ascii_case(t) || a.alias.as_deref() == Some(t))
    }

    /// Pasa Claude (`~/.claude`, el de todas las sesiones) a otra cuenta.
    pub async fn switch(&self, target: &str) -> Result<Switched, String> {
        if disabled() {
            return Err(OFF.into());
        }
        if !valid_target(target) {
            return Err("cuenta no válida".into());
        }
        let (identity, _) = self.live_now().await;
        let cur_uuid = identity
            .as_ref()
            .and_then(|v| v["accountUuid"].as_str())
            .map(str::to_owned);
        let mut inner = self.inner.lock().await;
        let to = Self::find(&inner.file, target)
            .cloned()
            .ok_or("no hay ninguna cuenta así")?;
        let from = cur_uuid
            .as_deref()
            .and_then(|u| inner.file.accounts.iter().find(|a| a.uuid == u))
            .cloned();
        if from.as_ref().is_some_and(|f| f.number == to.number) {
            return Ok(Switched {
                switched: false,
                from: Some(to.reference()),
                to: Some(to.reference()),
                message: Some(format!("La cuenta {} ya es la activa.", to.number)),
                warnings: vec![],
            });
        }
        if to.relogin {
            return Err(format!(
                "la sesión guardada de la cuenta {} ya no vale: vuelve a entrar en ella",
                to.number
            ));
        }
        // Antes de tocar nada de Claude: que la sesión de destino valga (renovarla si toca).
        self.fresh_token(&mut inner, to.number, false)
            .await
            .inspect_err(|_| self.save(&inner))?;
        let creds = self
            .load_creds(to.number)
            .ok_or("no hay sesión guardada de esa cuenta")?;
        let live = self.live.clone();
        let identity_to = to.identity.clone();
        let old = blocking(move || -> std::io::Result<Option<Value>> {
            let old = live.swap_oauth(&creds)?;
            live.write_identity(&identity_to)?;
            Ok(old)
        })
        .await
        .map_err(|e| format!("no se pudo cambiar la cuenta de Claude: {e}"))?;
        let mut warnings = Vec::new();
        // La sesión que tenía Claude es la más reciente de la cuenta que deja: se guarda.
        match (&from, old) {
            (Some(f), Some(old)) => {
                // Sale de Claude: vale seguro.
                set_relogin(&mut inner, f.number, false);
                if let Err(e) = self.save_creds(f.number, &old) {
                    warnings.push(format!(
                        "no se pudo guardar la sesión de la cuenta {}: {e}",
                        f.number
                    ));
                }
            }
            (None, Some(old)) if identity.is_some() => {
                // Una cuenta que no estaba apuntada (entró con `claude /login`): se apunta.
                let n = adopt(&mut inner.file, identity.as_ref().expect("identidad"));
                if let Some((n, _)) = n {
                    let _ = self.save_creds(n, &old);
                }
            }
            _ => {}
        }
        if let Some(a) = inner
            .file
            .accounts
            .iter_mut()
            .find(|a| a.number == to.number)
        {
            a.relogin = false;
        }
        // Las dos cuentas cambian de ritmo: medirlas ya.
        inner.timing.next.remove(&to.number);
        if let Some(f) = &from {
            inner.timing.next.remove(&f.number);
        }
        self.save(&inner);
        drop(inner);
        // La vista ya con la nueva activa (la web la pide en cuanto contesta esto).
        self.redraw(Some(&to.uuid)).await;
        self.kick.notify_one();
        Ok(Switched {
            switched: true,
            from: from.map(|f| f.reference()),
            to: Some(to.reference()),
            message: None,
            warnings,
        })
    }

    /// Empieza a entrar en una cuenta: el enlace que hay que abrir.
    pub fn login_start(&self, hint: Option<&str>) -> (String, String) {
        let s = oauth::start(hint);
        let mut logins = self.logins.lock().expect("entradas");
        logins.retain(|_, p| p.at.elapsed() < LOGIN_TTL);
        if logins.len() >= 8
            && let Some(k) = logins
                .iter()
                .min_by_key(|(_, p)| p.at)
                .map(|(k, _)| k.clone())
        {
            logins.remove(&k);
        }
        logins.insert(
            s.state.clone(),
            Pending {
                verifier: s.verifier,
                at: Instant::now(),
            },
        );
        (s.url, s.state)
    }

    /// Acaba de entrar con el código que enseñó la página. Si la cuenta ya estaba, se le pone la
    /// sesión nueva (y, si es la activa, también a Claude).
    pub async fn login_finish(
        &self,
        state: Option<&str>,
        pasted: &str,
    ) -> Result<LoggedIn, String> {
        if disabled() {
            return Err(OFF.into());
        }
        let (code, pasted_state) = oauth::parse_pasted(pasted)?;
        let state = match (pasted_state, state) {
            (Some(p), Some(s)) if p != s => {
                return Err(
                    "ese código es de otro inicio de sesión: abre el enlace de este".into(),
                );
            }
            (Some(p), _) => p,
            (None, Some(s)) => s.to_owned(),
            (None, None) => {
                return Err(
                    "falta el final del código (lo que va tras «#»): cópialo entero".into(),
                );
            }
        };
        let pending = self
            .logins
            .lock()
            .expect("entradas")
            .remove(&state)
            .filter(|p| p.at.elapsed() < LOGIN_TTL)
            .ok_or("ese inicio de sesión ya no vale (caducó o ya se usó): vuelve a empezar")?;
        let tokens = oauth::exchange(&self.http, &code, &state, &pending.verifier).await?;
        let profile = oauth::profile(&self.http, &tokens.access_token).await?;
        let identity =
            oauth::identity(&profile, &tokens).ok_or("Claude no dice de quién es la cuenta")?;
        let creds = oauth::credential(&tokens, &profile);
        let uuid = identity["accountUuid"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        let org = identity["organizationUuid"].as_str().map(str::to_owned);
        let email = identity["emailAddress"]
            .as_str()
            .unwrap_or_default()
            .to_owned();

        let (live_identity, live_oauth) = self.live_now().await;
        let live_uuid = live_identity
            .as_ref()
            .and_then(|v| v["accountUuid"].as_str())
            .map(str::to_owned);
        let mut inner = self.inner.lock().await;
        let existing = inner
            .file
            .accounts
            .iter()
            .position(|a| a.is(&uuid, org.as_deref()));
        let number = match existing {
            Some(i) => inner.file.accounts[i].number,
            None => {
                inner
                    .file
                    .accounts
                    .iter()
                    .map(|a| a.number)
                    .max()
                    .unwrap_or(0)
                    + 1
            }
        };
        let entry = Saved {
            number,
            email: email.clone(),
            uuid: uuid.clone(),
            org_uuid: org.clone(),
            organization: identity["organizationName"].as_str().map(str::to_owned),
            alias: existing.and_then(|i| inner.file.accounts[i].alias.clone()),
            identity: identity.clone(),
            relogin: false,
            usage: existing.and_then(|i| inner.file.accounts[i].usage.clone()),
            usage_at: existing.and_then(|i| inner.file.accounts[i].usage_at),
        };
        match existing {
            Some(i) => inner.file.accounts[i] = entry,
            None => inner.file.accounts.push(entry),
        }
        // Activa si es la que usa Claude o si Claude no tiene ninguna.
        let active = live_uuid.as_deref() == Some(uuid.as_str()) || live_oauth.is_none();
        let result = if active {
            let live = self.live.clone();
            let (c, id) = (creds.clone(), identity.clone());
            blocking(move || -> std::io::Result<()> {
                live.swap_oauth(&c)?;
                live.write_identity(&id)
            })
            .await
            .map_err(|e| format!("no se pudo poner la sesión a Claude: {e}"))
        } else {
            Ok(())
        };
        // Guardada también: si luego se cambia de cuenta, la de Claude la sustituye.
        let saved = self
            .save_creds(number, &creds)
            .map_err(|e| format!("no se pudo guardar la sesión: {e}"));
        inner.timing.next.remove(&number);
        self.save(&inner);
        drop(inner);
        result?;
        saved?;
        // La cuenta ya en la lista (su uso llega en cuanto se mida, en la pasada que se despierta).
        let now_active = if active {
            Some(uuid.as_str())
        } else {
            live_uuid.as_deref()
        };
        self.redraw(now_active).await;
        self.kick.notify_one();
        Ok(LoggedIn {
            number,
            email,
            new: existing.is_none(),
            active,
        })
    }

    /// Olvida una cuenta guardada (no la activa).
    pub async fn remove(&self, number: u32) -> Result<(), String> {
        if disabled() {
            return Err(OFF.into());
        }
        let (identity, _) = self.live_now().await;
        let live_uuid = identity
            .as_ref()
            .and_then(|v| v["accountUuid"].as_str())
            .map(str::to_owned);
        let mut inner = self.inner.lock().await;
        let i = inner
            .file
            .accounts
            .iter()
            .position(|a| a.number == number)
            .ok_or("no hay ninguna cuenta así")?;
        if Some(&inner.file.accounts[i].uuid) == live_uuid.as_ref() {
            return Err("es la que usa Claude ahora: cambia antes a otra".into());
        }
        inner.file.accounts.remove(i);
        let _ = std::fs::remove_file(self.creds_path(number));
        self.save(&inner);
        drop(inner);
        self.redraw(live_uuid.as_deref()).await;
        Ok(())
    }

    /// Las cuentas de cswap (claude-swap): sus números, identidades y sesiones guardadas. La
    /// activa no: su sesión buena es la de Claude.
    fn import_cswap(&self, inner: &mut Inner, home: &Path, live_uuid: Option<&str>) -> usize {
        let Some(root) = cswap_root(home) else {
            return 0;
        };
        let Ok(raw) = std::fs::read(root.join("sequence.json")) else {
            return 0;
        };
        let Ok(seq) = serde_json::from_slice::<Value>(&raw) else {
            return 0;
        };
        let mut n = 0;
        for (num, info) in seq["accounts"].as_object().into_iter().flatten() {
            let (Ok(number), Some(email), Some(uuid)) = (
                num.parse::<u32>(),
                info["email"].as_str(),
                info["uuid"].as_str(),
            ) else {
                continue;
            };
            let org = info["organizationUuid"].as_str();
            if inner
                .file
                .accounts
                .iter()
                .any(|a| a.number == number || a.is(uuid, org))
            {
                continue;
            }
            let identity = std::fs::read(
                root.join("configs")
                    .join(format!(".claude-config-{number}-{email}.json")),
            )
            .ok()
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
            .and_then(|mut v| v.get_mut("oauthAccount").map(Value::take))
            .filter(|v| v["accountUuid"].as_str() == Some(uuid))
            .unwrap_or_else(|| {
                serde_json::json!({
                    "accountUuid": uuid,
                    "emailAddress": email,
                    "organizationUuid": org,
                    "organizationName": info["organizationName"],
                })
            });
            let mut relogin = false;
            if live_uuid != Some(uuid) {
                match cswap_creds(&root, number, email) {
                    Some(c) => {
                        if let Err(e) = self.save_creds(number, &c) {
                            eprintln!(
                                "nido: no se pudo guardar la sesión de la cuenta {number}: {e}"
                            );
                            relogin = true;
                        }
                    }
                    None => relogin = true,
                }
            }
            inner.file.accounts.push(Saved {
                number,
                email: email.to_owned(),
                uuid: uuid.to_owned(),
                org_uuid: org.map(str::to_owned),
                organization: info["organizationName"].as_str().map(str::to_owned),
                alias: info["alias"].as_str().map(str::to_owned),
                identity,
                relogin,
                usage: None,
                usage_at: None,
            });
            n += 1;
        }
        n
    }
}

/// `NIDO_ACCOUNTS=off`: nido no toca las cuentas de Claude (ni las mide, ni las trae de cswap,
/// ni renueva sus sesiones). Para un nido de pruebas o de demostración junto al de verdad.
pub fn disabled() -> bool {
    std::env::var("NIDO_ACCOUNTS").is_ok_and(|v| v.eq_ignore_ascii_case("off"))
}

const OFF: &str = "las cuentas de Claude están desactivadas en este nido (NIDO_ACCOUNTS=off)";

/// Lo más gastado de una cuenta (su ventana de 5 h o la semanal), sin contar las ventanas que
/// ya se han reiniciado desde que se midió.
fn spent(u: &Usage, now: f64) -> Option<f64> {
    [&u.five_hour, &u.seven_day]
        .into_iter()
        .flatten()
        .filter(|w| {
            w.resets_at
                .as_deref()
                .and_then(oauth::ts)
                .is_none_or(|r| r > now)
        })
        .filter_map(|w| w.pct)
        .reduce(f64::max)
}

/// A qué cuenta pasa el cambio automático desde `from`: la siguiente por número (dando la
/// vuelta) que tenga sesión guardada válida, no esté en `skip` y no vaya ya al [`AUTO_PCT`].
fn next_account(accounts: &[Saved], from: u32, skip: &[u32], now: f64) -> Option<u32> {
    let mut ok: Vec<u32> = accounts
        .iter()
        .filter(|a| a.number != from && !a.relogin && !skip.contains(&a.number))
        .filter(|a| {
            a.usage
                .as_ref()
                .and_then(|u| spent(u, now))
                .is_none_or(|p| p < AUTO_PCT)
        })
        .map(|a| a.number)
        .collect();
    ok.sort_unstable();
    ok.iter()
        .find(|n| **n > from)
        .or_else(|| ok.first())
        .copied()
}

fn set_relogin(inner: &mut Inner, number: u32, v: bool) {
    if let Some(a) = inner.file.accounts.iter_mut().find(|a| a.number == number) {
        a.relogin = v;
    }
}

/// La credencial caduca dentro de menos de `margin_ms` (o no dice cuándo).
fn expiring(oauth: &Value, margin_ms: u64) -> bool {
    oauth["expiresAt"]
        .as_u64()
        .is_none_or(|t| t <= oauth::now_ms() + margin_ms)
}

/// Apunta la cuenta de esta identidad si no estaba (o le pone la identidad al día). Devuelve su
/// número y si algo ha cambiado.
fn adopt(file: &mut SavedFile, identity: &Value) -> Option<(u32, bool)> {
    let uuid = identity["accountUuid"].as_str()?;
    let org = identity["organizationUuid"].as_str();
    if let Some(a) = file.accounts.iter_mut().find(|a| a.is(uuid, org)) {
        // Claude completa la identidad con el tiempo: quedarse con la suya.
        let changed = a.identity != *identity;
        if changed {
            a.identity = identity.clone();
            if let Some(e) = identity["emailAddress"].as_str() {
                a.email = e.to_owned();
            }
            if let Some(o) = identity["organizationName"].as_str() {
                a.organization = Some(o.to_owned());
            }
        }
        return Some((a.number, changed));
    }
    let number = file.accounts.iter().map(|a| a.number).max().unwrap_or(0) + 1;
    file.accounts.push(Saved {
        number,
        email: identity["emailAddress"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
        uuid: uuid.to_owned(),
        org_uuid: org.map(str::to_owned),
        organization: identity["organizationName"].as_str().map(str::to_owned),
        alias: None,
        identity: identity.clone(),
        relogin: false,
        usage: None,
        usage_at: None,
    });
    Some((number, true))
}

fn cswap_root(home: &Path) -> Option<PathBuf> {
    let xdg = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| home.join(".local/share"));
    [xdg.join("claude-swap"), home.join(".claude-swap-backup")]
        .into_iter()
        .find(|d| d.join("sequence.json").is_file())
}

/// La sesión guardada de una cuenta de cswap: un fichero en base64 o, en macOS, el llavero.
fn cswap_creds(root: &Path, number: u32, email: &str) -> Option<Value> {
    use base64::Engine;
    let from_json = |raw: &str| -> Option<Value> {
        let mut v: Value = serde_json::from_str(raw).ok()?;
        v.get_mut("claudeAiOauth")
            .map(Value::take)
            .filter(Value::is_object)
    };
    let enc = root
        .join("credentials")
        .join(format!(".creds-{number}-{email}.enc"));
    if let Ok(b) = std::fs::read_to_string(&enc) {
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(b.trim())
            .ok()?;
        return from_json(&String::from_utf8(decoded).ok()?);
    }
    #[cfg(target_os = "macos")]
    {
        let out = std::process::Command::new("/usr/bin/security")
            .args([
                "find-generic-password",
                "-a",
                &format!("account-{number}-{email}"),
                "-s",
                "claude-swap",
                "-w",
            ])
            .stdin(std::process::Stdio::null())
            .output()
            .ok()?;
        if out.status.success() {
            return from_json(String::from_utf8_lossy(&out.stdout).trim_end_matches('\n'));
        }
    }
    None
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    tokio::task::spawn_blocking(f)
        .await
        .expect("tarea de fondo")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets() {
        assert!(valid_target("3"));
        assert!(valid_target("ana@example.com"));
        assert!(!valid_target("-3"));
        assert!(!valid_target("a b"));
        assert!(!valid_target(""));
    }

    fn acc(number: u32, five: f64, week: f64, resets: &str) -> Saved {
        let w = |pct| crate::oauth::Window {
            name: None,
            pct: Some(pct),
            resets_at: Some(resets.to_owned()),
            expected_pct: None,
            ahead_of_pace: None,
            will_last_to_reset: None,
            projected_exhaustion_at: None,
        };
        Saved {
            number,
            uuid: format!("u{number}"),
            usage: Some(Usage {
                five_hour: Some(w(five)),
                seven_day: Some(w(week)),
                models: vec![],
            }),
            ..Default::default()
        }
    }

    #[test]
    fn spent_ignores_windows_already_reset() {
        let now = 1_800_000_000.0; // 2027-01-15
        let later = "2027-01-16T00:00:00Z";
        let gone = "2027-01-01T00:00:00Z";
        assert_eq!(
            spent(acc(1, 40.0, 99.5, later).usage.as_ref().unwrap(), now),
            Some(99.5)
        );
        assert_eq!(
            spent(acc(1, 40.0, 99.5, gone).usage.as_ref().unwrap(), now),
            None
        );
    }

    #[test]
    fn next_account_goes_round_and_skips_spent_ones() {
        let now = 1_800_000_000.0;
        let r = "2027-01-16T00:00:00Z";
        let mut list = vec![
            acc(1, 99.0, 50.0, r),
            acc(2, 10.0, 99.2, r),
            acc(3, 20.0, 30.0, r),
            acc(5, 0.0, 0.0, r),
        ];
        assert_eq!(next_account(&list, 1, &[], now), Some(3), "la 2 va agotada");
        assert_eq!(next_account(&list, 3, &[], now), Some(5));
        assert_eq!(next_account(&list, 5, &[], now), Some(3), "da la vuelta");
        assert_eq!(
            next_account(&list, 1, &[3], now),
            Some(5),
            "la 3 falló hace poco"
        );
        list[3].relogin = true;
        assert_eq!(next_account(&list, 3, &[], now), None, "no queda ninguna");
        // Sin dato de uso, también vale.
        list.push(Saved {
            number: 9,
            uuid: "u9".into(),
            ..Default::default()
        });
        assert_eq!(next_account(&list, 3, &[], now), Some(9));
    }

    #[test]
    fn adopt_adds_once_and_keeps_numbers() {
        let mut f = SavedFile::default();
        f.accounts.push(Saved {
            number: 7,
            uuid: "u7".into(),
            email: "x@example.com".into(),
            ..Default::default()
        });
        let id = serde_json::json!({"accountUuid": "u1", "emailAddress": "ana@example.com", "organizationUuid": "o1"});
        assert_eq!(adopt(&mut f, &id), Some((8, true)));
        assert_eq!(adopt(&mut f, &id), Some((8, false)));
        assert_eq!(f.accounts.len(), 2);
        let id7 = serde_json::json!({"accountUuid": "u7", "emailAddress": "nuevo@example.com"});
        assert_eq!(adopt(&mut f, &id7), Some((7, true)));
        assert_eq!(f.accounts[0].email, "nuevo@example.com");
    }

    #[test]
    fn imports_cswap_accounts() {
        use base64::Engine;
        let home = std::env::temp_dir().join(format!("nido-cswap-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let root = home.join(".claude-swap-backup");
        std::fs::create_dir_all(root.join("configs")).unwrap();
        std::fs::create_dir_all(root.join("credentials")).unwrap();
        std::fs::write(
            root.join("sequence.json"),
            r#"{"accounts":{"2":{"email":"ana@example.com","uuid":"u2","organizationUuid":"o2","organizationName":"Ana"},
                            "5":{"email":"luis@example.com","uuid":"u5","organizationUuid":"o5"}}}"#,
        )
        .unwrap();
        std::fs::write(
            root.join("configs/.claude-config-2-ana@example.com.json"),
            r#"{"theme":"x","oauthAccount":{"accountUuid":"u2","emailAddress":"ana@example.com","displayName":"Ana"}}"#,
        )
        .unwrap();
        let creds = base64::engine::general_purpose::STANDARD
            .encode(r#"{"claudeAiOauth":{"accessToken":"a2","refreshToken":"r2","expiresAt":1}}"#);
        std::fs::write(root.join("credentials/.creds-2-ana@example.com.enc"), creds).unwrap();
        let data = home.join("data");
        let acc = Accounts::new(&home, &data);
        let mut inner = Inner {
            file: SavedFile::default(),
            timing: Timing::default(),
        };
        // La 5 es la activa: sin sesión guardada, pero apuntada.
        assert_eq!(acc.import_cswap(&mut inner, &home, Some("u5")), 2);
        let two = inner.file.accounts.iter().find(|a| a.number == 2).unwrap();
        assert_eq!(two.identity["displayName"], "Ana");
        assert!(!two.relogin);
        assert_eq!(acc.load_creds(2).unwrap()["refreshToken"], "r2");
        let five = inner.file.accounts.iter().find(|a| a.number == 5).unwrap();
        assert!(!five.relogin);
        assert!(acc.load_creds(5).is_none());
        // Una segunda vez no repite nada.
        assert_eq!(acc.import_cswap(&mut inner, &home, Some("u5")), 0);
        let _ = std::fs::remove_dir_all(&home);
    }
}
