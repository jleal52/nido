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
//!
//! En Windows no se tocan los permisos (ver `sys::private`): los ficheros heredan los del perfil
//! del usuario, que ya son solo suyos. Por eso la carpeta de estado de nido tiene que seguir
//! dentro de ese perfil; fuera de él, los secretos tendrían los permisos de donde caigan.

use std::collections::HashMap;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::watch;

/// Cuánto espera una pregunta a que el usuario conteste.
pub const ASK_WAIT: Duration = Duration::from_secs(15 * 60);
/// Lo que vive, como mucho, un secreto sin recordar (ms).
pub const SESSION_KEEP_MS: u64 = 24 * 3600 * 1000;
/// Lo más que ocupa un secreto.
pub const MAX_VALUE: usize = 16 * 1024;
/// Lo que se espera, pasado el tiempo, a quien ya cogió la respuesta y la está guardando.
const SAVING_WAIT: Duration = Duration::from_secs(60);

const HOUR_MS: u64 = 3600 * 1000;
/// Lo que puede quedar un temporal (de una escritura que se cortó) antes de borrarlo.
pub(crate) const TMP_KEEP_MS: u64 = HOUR_MS;
/// Cómo acaba el nombre de un temporal de `write_private`. Con `~`, que no cabe en un nombre de
/// secreto: un temporal nunca se confunde con uno.
const TMP_END: &str = "~tmp";

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

/// Un secreto recordado, sin su valor (lo que guarda `equipo.json` y enseñan la web, la app y
/// `nido secrets`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Remembered {
    pub name: String,
    /// Hasta cuándo (ms); `None`: siempre.
    pub expires: Option<u64>,
    /// La sesión que lo pidió por última vez (su nombre, o los de las que lo pidieron a la vez).
    pub by: String,
    /// Cuándo (ms).
    pub at: u64,
}

/// Los secretos de este equipo, en `<state>/secrets`.
pub struct Store {
    dir: PathBuf,
    /// Leer, cambiar y escribir el índice (y sus ficheros), de uno en uno.
    lock: std::sync::Mutex<()>,
}

fn bad(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, what.to_owned())
}

fn check_name(name: &str) -> io::Result<()> {
    if valid_name(name) {
        Ok(())
    } else {
        Err(bad("nombre de secreto no válido"))
    }
}

/// Un id de sesión va en una ruta: solo si parece uno de nido.
fn check_session(id: &str) -> io::Result<()> {
    match crate::mcp::session_from(Some(id.to_owned())) {
        Some(_) => Ok(()),
        None => Err(bad("sesión no válida")),
    }
}

/// Escribe `data` en `path` solo para el usuario (0600), entero o nada: a un temporal al lado y
/// luego se renombra.
pub(crate) fn write_private(path: &Path, data: &[u8]) -> io::Result<()> {
    let name = path
        .file_name()
        .ok_or_else(|| bad("ruta sin nombre"))?
        .to_string_lossy();
    let tmp = path.with_file_name(format!(".{name}.{}{TMP_END}", crate::registry::new_id()));
    let res = write_new(&tmp, data).and_then(|()| std::fs::rename(&tmp, path));
    if res.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    res
}

/// Un temporal de `write_private` (que puede haber quedado de una escritura que se cortó).
pub(crate) fn is_tmp(name: &str) -> bool {
    name.starts_with('.') && name.ends_with(TMP_END)
}

/// Crea `path` (no puede existir) solo para el usuario y escribe `data`, ya en el disco.
fn write_new(path: &Path, data: &[u8]) -> io::Result<()> {
    let mut f =
        crate::sys::private(std::fs::OpenOptions::new().write(true).create_new(true)).open(path)?;
    f.write_all(data)?;
    f.sync_all()
}

/// Los ms desde 1970 de la última modificación de `p`.
fn mtime_ms(p: &Path) -> Option<u64> {
    let t = std::fs::metadata(p).ok()?.modified().ok()?;
    Some(t.duration_since(std::time::UNIX_EPOCH).ok()?.as_millis() as u64)
}

/// `p` tiene más de `keep` ms.
pub(crate) fn older_than(p: &Path, now: u64, keep: u64) -> bool {
    mtime_ms(p).is_some_and(|t| now.saturating_sub(t) > keep)
}

/// Fuera los temporales de `dir` de más de una hora (no los de una escritura en curso).
fn sweep_tmp(dir: &Path, now: u64) {
    for f in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        if is_tmp(&f.file_name().to_string_lossy()) && older_than(&f.path(), now, TMP_KEEP_MS) {
            let _ = std::fs::remove_file(f.path());
        }
    }
}

impl Store {
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            lock: std::sync::Mutex::new(()),
        }
    }

    fn one(&self) -> std::sync::MutexGuard<'_, ()> {
        self.lock.lock().expect("secretos")
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn team_dir(&self) -> PathBuf {
        self.dir.join("equipo")
    }

    fn index(&self) -> PathBuf {
        self.dir.join("equipo.json")
    }

    fn sessions_dir(&self) -> PathBuf {
        self.dir.join("sesiones")
    }

    /// `dir` y las carpetas entre él y `secrets/`, creadas y solo para el usuario (0700).
    fn private_dirs(&self, dir: &Path) -> io::Result<()> {
        crate::sys::create_private_dir_all(dir)?;
        let mut d = dir;
        loop {
            crate::sys::private_dir(d)?;
            if d == self.dir {
                return Ok(());
            }
            match d.parent() {
                Some(p) if p.starts_with(&self.dir) => d = p,
                _ => return Ok(()),
            }
        }
    }

    fn load(&self) -> Vec<Remembered> {
        self.try_load().unwrap_or_default()
    }

    /// El índice; `None` si está pero no se puede leer (entonces no se borra nada por él).
    fn try_load(&self) -> Option<Vec<Remembered>> {
        match std::fs::read(self.index()) {
            Ok(b) => serde_json::from_slice(&b).ok(),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Some(Vec::new()),
            Err(_) => None,
        }
    }

    fn store(&self, list: &[Remembered]) -> io::Result<()> {
        self.private_dirs(&self.dir)?;
        let json = serde_json::to_vec_pretty(list).map_err(io::Error::other)?;
        write_private(&self.index(), &json)
    }

    /// La ruta del recordado `name`, si lo hay y no ha caducado.
    pub fn remembered(&self, name: &str, now: u64) -> Option<PathBuf> {
        let path = self.team_dir().join(name);
        let alive = valid_name(name)
            && self
                .load()
                .iter()
                .any(|r| r.name == name && r.expires.is_none_or(|e| e > now));
        (alive && path.is_file()).then_some(path)
    }

    /// Lo pidió otra vez `by` (se entrega sin diálogo, pero se apunta). Si no está, nada.
    pub fn touch(&self, name: &str, by: &str, now: u64) -> io::Result<()> {
        let _one = self.one();
        let mut list = self.load();
        let Some(r) = list.iter_mut().find(|r| r.name == name) else {
            return Ok(());
        };
        r.by = by.to_owned();
        r.at = now;
        self.store(&list)
    }

    /// Guarda un secreto para todo el equipo (lo sustituye si ya estaba).
    pub fn save_team(
        &self,
        name: &str,
        value: &[u8],
        remember: Remember,
        by: &str,
        now: u64,
    ) -> io::Result<PathBuf> {
        check_name(name)?;
        let _one = self.one();
        let dir = self.team_dir();
        self.private_dirs(&dir)?;
        let path = dir.join(name);
        write_private(&path, value)?;
        let mut list = self.load();
        list.retain(|r| r.name != name);
        list.push(Remembered {
            name: name.to_owned(),
            expires: remember.expires(now),
            by: by.to_owned(),
            at: now,
        });
        list.sort_by(|a, b| a.name.cmp(&b.name));
        self.store(&list)?;
        Ok(path)
    }

    /// Guarda un secreto solo para la sesión `session`.
    pub fn save_session(&self, session: &str, name: &str, value: &[u8]) -> io::Result<PathBuf> {
        check_name(name)?;
        check_session(session)?;
        let _one = self.one();
        let dir = self.sessions_dir().join(session);
        self.private_dirs(&dir)?;
        let path = dir.join(name);
        write_private(&path, value)?;
        Ok(path)
    }

    /// Lo que contestó el usuario a una pregunta de `sessions`: sin recordar, uno para cada una;
    /// recordado, uno para el equipo. Con `force` y sin recordar, el recordado de antes ya no
    /// valía: se olvida. Devuelve la ruta de cada sesión.
    #[allow(clippy::too_many_arguments)]
    pub fn save_answer(
        &self,
        name: &str,
        sessions: &[String],
        value: &[u8],
        remember: Remember,
        forced: bool,
        by: &str,
        now: u64,
    ) -> io::Result<HashMap<String, PathBuf>> {
        if remember == Remember::No {
            if forced {
                self.forget(name)?;
            }
            return sessions
                .iter()
                .map(|s| Ok((s.clone(), self.save_session(s, name, value)?)))
                .collect();
        }
        let path = self.save_team(name, value, remember, by, now)?;
        Ok(sessions.iter().map(|s| (s.clone(), path.clone())).collect())
    }

    /// Los recordados que valen, por nombre.
    pub fn list(&self, now: u64) -> Vec<Remembered> {
        let dir = self.team_dir();
        self.load()
            .into_iter()
            .filter(|r| r.expires.is_none_or(|e| e > now) && dir.join(&r.name).is_file())
            .collect()
    }

    /// Olvida un recordado. `false` si no estaba.
    pub fn forget(&self, name: &str) -> io::Result<bool> {
        check_name(name)?;
        let _one = self.one();
        let mut list = self.load();
        let before = list.len();
        list.retain(|r| r.name != name);
        let had = list.len() != before;
        if let Err(e) = std::fs::remove_file(self.team_dir().join(name))
            && e.kind() != io::ErrorKind::NotFound
        {
            return Err(e);
        }
        if had {
            self.store(&list)?;
        }
        Ok(had)
    }

    /// Los secretos sin recordar de una sesión, fuera (se cerró).
    pub fn forget_session(&self, session: &str) {
        if check_session(session).is_ok() {
            let _one = self.one();
            let _ = std::fs::remove_dir_all(self.sessions_dir().join(session));
        }
    }

    /// Fuera lo caducado: los recordados vencidos, las carpetas de sesiones que ya no existen
    /// (`live` dice cuáles sí) y los secretos sin recordar de más de 24 h. Y lo que dejó una
    /// escritura cortada: recordados que no están en el índice y temporales de más de una hora.
    pub fn prune(&self, now: u64, live: impl Fn(&str) -> bool) {
        let _one = self.one();
        if let Some(index) = self.try_load() {
            let (keep, gone): (Vec<_>, Vec<_>) = index
                .into_iter()
                .partition(|r| r.expires.is_none_or(|e| e > now));
            if !gone.is_empty() {
                let _ = self.store(&keep);
            }
            // Lo de `equipo/` que no está en el índice (los vencidos también). Con el cerrojo no
            // hay ninguna escritura en curso ahí.
            for f in std::fs::read_dir(self.team_dir())
                .into_iter()
                .flatten()
                .flatten()
            {
                let name = f.file_name().to_string_lossy().into_owned();
                if !keep.iter().any(|r| r.name == name) {
                    let _ = std::fs::remove_file(f.path());
                }
            }
        }
        sweep_tmp(&self.dir, now);
        let Ok(dirs) = std::fs::read_dir(self.sessions_dir()) else {
            return;
        };
        for d in dirs.flatten() {
            let id = d.file_name().to_string_lossy().into_owned();
            if !live(&id) {
                let _ = std::fs::remove_dir_all(d.path());
                continue;
            }
            for f in std::fs::read_dir(d.path()).into_iter().flatten().flatten() {
                if older_than(&f.path(), now, SESSION_KEEP_MS) {
                    let _ = std::fs::remove_file(f.path());
                }
            }
            sweep_tmp(&d.path(), now);
        }
    }
}

/// Por qué una sesión no tiene el secreto que pide `nido secrets run`.
#[derive(Debug, PartialEq)]
pub enum UseError {
    NoSession,
    BadName(String),
    Missing(String),
}

impl std::fmt::Display for UseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UseError::NoSession => write!(
                f,
                "falta NIDO_SESSION: `nido secrets run` es para las órdenes de una sesión de nido"
            ),
            UseError::BadName(n) => write!(
                f,
                "nombre de secreto no válido: {n:?} (letras, números, «.», «_» o «-»; hasta 64)"
            ),
            UseError::Missing(n) => write!(
                f,
                "no hay ningún secreto «{n}» para esta sesión: pídelo antes con la herramienta \
                 secret"
            ),
        }
    }
}

impl Store {
    /// El secreto `name` que puede usar la sesión `session` (`NIDO_SESSION`): el suyo sin
    /// recordar (si no tiene más de 24 h) y, si no, el recordado del equipo que no ha caducado.
    /// La edad del suyo se mira con el reloj de verdad; `now` solo cuenta para los recordados.
    pub fn usable(&self, session: Option<&str>, name: &str, now: u64) -> Result<PathBuf, UseError> {
        let session = session
            .and_then(|s| crate::mcp::session_from(Some(s.to_owned())))
            .ok_or(UseError::NoSession)?;
        if !valid_name(name) {
            return Err(UseError::BadName(name.to_owned()));
        }
        let own = self.sessions_dir().join(&session).join(name);
        let wall = crate::registry::now_ms();
        if own.is_file() && !older_than(&own, wall, SESSION_KEEP_MS) {
            return Ok(own);
        }
        self.remembered(name, now)
            .ok_or_else(|| UseError::Missing(name.to_owned()))
    }
}

/// Cómo acabó una pregunta.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// Contestada y guardada: la ruta de cada sesión que esperaba.
    Saved(HashMap<String, PathBuf>),
    /// Contestada, pero no se pudo guardar.
    Failed(String),
    /// El usuario no quiso.
    Rejected,
    /// Nadie contestó a tiempo.
    Expired,
    /// Ya no espera nadie (las sesiones se fueron).
    Closed,
}

/// Lo que ven la web y la app de una pregunta en curso (en la `SessionView` de cada sesión que
/// espera). Sin nada del valor.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AskView {
    pub id: String,
    pub name: String,
    pub reason: String,
    /// Cuándo se preguntó y hasta cuándo se espera (ms).
    pub since: u64,
    pub until: u64,
}

struct Ask {
    view: AskView,
    deadline: tokio::time::Instant,
    /// Una vez por cada espera: la misma sesión puede pedirlo otra vez mientras espera (p. ej. si
    /// Claude cancela la llamada y la repite).
    sessions: Vec<String>,
    /// Alguna de las sesiones lo pidió con `force` (el recordado ya no vale).
    forced: bool,
    done: watch::Sender<Option<Outcome>>,
}

/// Las sesiones, una vez cada una y en orden.
fn unique(sessions: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(sessions.len());
    for s in sessions {
        if !out.contains(&s) {
            out.push(s);
        }
    }
    out
}

/// Esa sesión ya espera otro secreto.
#[derive(Debug, PartialEq)]
pub struct Busy;

/// Las preguntas en curso de este equipo, por nombre.
#[derive(Default)]
pub struct Asks {
    map: std::sync::Mutex<HashMap<String, Ask>>,
    /// Cuánto se espera; `None`: `ASK_WAIT` (las pruebas lo acortan).
    wait: std::sync::Mutex<Option<Duration>>,
}

/// Lo que se lleva quien contesta: el nombre, a qué sesiones dar la ruta y cómo avisarlas.
pub struct Taken {
    pub name: String,
    pub sessions: Vec<String>,
    pub forced: bool,
    done: watch::Sender<Option<Outcome>>,
}

impl Taken {
    /// Avisa a las que esperan.
    pub fn finish(self, outcome: Outcome) {
        let _ = self.done.send(Some(outcome));
    }
}

/// Una sesión que espera la respuesta. Al soltarlo (también si su petición se cae), deja de
/// esperar.
pub struct Waiter<'a> {
    asks: &'a Asks,
    name: String,
    id: String,
    session: String,
    deadline: tokio::time::Instant,
    rx: watch::Receiver<Option<Outcome>>,
}

impl Asks {
    /// Para las pruebas: cuánto esperan las preguntas que se hagan desde ahora.
    pub fn set_wait(&self, d: Duration) {
        *self.wait.lock().expect("espera") = Some(d);
    }

    fn wait_for(&self) -> Duration {
        self.wait.lock().expect("espera").unwrap_or(ASK_WAIT)
    }

    /// La sesión `session` pide `name`: se une a la pregunta que haya con ese nombre o se crea
    /// una (`true`: es nueva, hay que avisar al usuario). Si esa sesión ya espera otro nombre,
    /// `Busy`; si espera este, otra espera más de la misma pregunta.
    pub fn join(
        &self,
        name: &str,
        reason: &str,
        session: &str,
        now: u64,
    ) -> Result<(Waiter<'_>, bool), Busy> {
        self.join_forced(name, reason, session, now, false)
    }

    /// Como [`Asks::join`]; con `forced`, el recordado que haya ya no vale.
    pub fn join_forced(
        &self,
        name: &str,
        reason: &str,
        session: &str,
        now: u64,
        forced: bool,
    ) -> Result<(Waiter<'_>, bool), Busy> {
        let wait = self.wait_for();
        let mut map = self.map.lock().expect("preguntas");
        if map
            .iter()
            .any(|(n, a)| n != name && a.sessions.iter().any(|s| s == session))
        {
            return Err(Busy);
        }
        let new = !map.contains_key(name);
        let ask = map.entry(name.to_owned()).or_insert_with(|| Ask {
            view: AskView {
                id: crate::registry::new_id(),
                name: name.to_owned(),
                reason: reason.to_owned(),
                since: now,
                until: now + wait.as_millis() as u64,
            },
            deadline: tokio::time::Instant::now() + wait,
            sessions: Vec::new(),
            forced: false,
            done: watch::channel(None).0,
        });
        ask.sessions.push(session.to_owned());
        ask.forced |= forced;
        let waiter = Waiter {
            asks: self,
            name: name.to_owned(),
            id: ask.view.id.clone(),
            session: session.to_owned(),
            deadline: ask.deadline,
            rx: ask.done.subscribe(),
        };
        Ok((waiter, new))
    }

    /// La pregunta que espera la sesión, si alguna.
    pub fn view_for(&self, session: &str) -> Option<AskView> {
        let map = self.map.lock().expect("preguntas");
        map.values()
            .find(|a| a.sessions.iter().any(|s| s == session))
            .map(|a| a.view.clone())
    }

    /// Para contestarla: la pregunta `id`, si sigue en curso y la espera `session`. Sale de la
    /// lista: la primera respuesta es la que vale.
    pub fn take(&self, id: &str, session: &str) -> Option<Taken> {
        let mut map = self.map.lock().expect("preguntas");
        let name = map
            .iter()
            .find(|(_, a)| a.view.id == id && a.sessions.iter().any(|s| s == session))
            .map(|(n, _)| n.clone())?;
        let a = map.remove(&name)?;
        Some(Taken {
            name,
            sessions: unique(a.sessions),
            forced: a.forced,
            done: a.done,
        })
    }

    /// Se acabó el tiempo de la pregunta `id`: fuera, y quien llama se lleva las sesiones que
    /// esperaban (para apuntar «sin contestar»). `None` si ya no estaba.
    fn expire(&self, name: &str, id: &str) -> Option<Vec<String>> {
        let mut map = self.map.lock().expect("preguntas");
        if map.get(name).is_none_or(|a| a.view.id != id) {
            return None;
        }
        let a = map.remove(name)?;
        let _ = a.done.send(Some(Outcome::Expired));
        Some(unique(a.sessions))
    }

    /// La sesión deja de esperar (`all`: todas sus esperas; si no, una); si no queda nadie, la
    /// pregunta se cierra.
    fn leave(&self, name: &str, id: &str, session: &str, all: bool) -> bool {
        let mut map = self.map.lock().expect("preguntas");
        let Some(a) = map.get_mut(name).filter(|a| a.view.id == id) else {
            return false;
        };
        let before = a.sessions.len();
        if all {
            a.sessions.retain(|s| s != session);
        } else if let Some(i) = a.sessions.iter().position(|s| s == session) {
            a.sessions.remove(i);
        }
        let left = a.sessions.len() != before;
        if a.sessions.is_empty()
            && let Some(a) = map.remove(name)
        {
            let _ = a.done.send(Some(Outcome::Closed));
        }
        left
    }

    /// La sesión se cerró o ya no está viva: deja de esperar lo que esperase.
    pub fn cancel_session(&self, session: &str) -> bool {
        let found = {
            let map = self.map.lock().expect("preguntas");
            map.iter()
                .find(|(_, a)| a.sessions.iter().any(|s| s == session))
                .map(|(n, a)| (n.clone(), a.view.id.clone()))
        };
        found.is_some_and(|(name, id)| self.leave(&name, &id, session, true))
    }
}

impl Waiter<'_> {
    /// Espera a que acabe. Si se acaba el tiempo y es este quien la quita, también devuelve las
    /// sesiones que esperaban (las demás esperas devuelven una lista vacía).
    pub async fn wait(&mut self) -> (Outcome, Vec<String>) {
        let rx = &mut self.rx;
        let got = tokio::time::timeout_at(self.deadline, async {
            rx.wait_for(Option::is_some).await.map(|o| o.clone())
        })
        .await;
        match got {
            Ok(Ok(Some(o))) => (o, Vec::new()),
            // Quien contestaba se fue sin decir nada.
            Ok(_) => (Outcome::Closed, Vec::new()),
            Err(_) => match self.asks.expire(&self.name, &self.id) {
                Some(sessions) => (Outcome::Expired, sessions),
                // Ya no estaba: o la quitó otra espera al acabarse el tiempo (y ya lo dijo), o
                // alguien la contestó y aún la guarda. Se le espera un poco; si no acaba, nada.
                None => {
                    let rx = &mut self.rx;
                    let got = tokio::time::timeout(SAVING_WAIT, async {
                        rx.wait_for(Option::is_some).await.map(|o| o.clone())
                    })
                    .await;
                    match got {
                        Ok(Ok(Some(o))) => (o, Vec::new()),
                        _ => (Outcome::Closed, Vec::new()),
                    }
                }
            },
        }
    }
}

impl Drop for Waiter<'_> {
    fn drop(&mut self) {
        self.asks.leave(&self.name, &self.id, &self.session, false);
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

    fn store() -> (tempfile::TempDir, Store) {
        let tmp = tempfile::tempdir().unwrap();
        let s = Store::new(tmp.path().join("secrets"));
        (tmp, s)
    }

    #[test]
    fn a_session_uses_its_own_secret_first_then_the_machines() {
        let (_tmp, s) = store();
        assert_eq!(s.usable(None, "tok", 1), Err(UseError::NoSession));
        assert_eq!(s.usable(Some("../x"), "tok", 1), Err(UseError::NoSession));
        assert_eq!(
            s.usable(Some("aaaa1111"), "a/b", 1),
            Err(UseError::BadName("a/b".into()))
        );
        assert_eq!(
            s.usable(Some("aaaa1111"), "tok", 1),
            Err(UseError::Missing("tok".into()))
        );
        let team = s
            .save_team("tok", b"equipo", Remember::TwoHours, "api", 1)
            .unwrap();
        assert_eq!(s.usable(Some("aaaa1111"), "tok", 1), Ok(team.clone()));
        let own = s.save_session("aaaa1111", "tok", b"suyo").unwrap();
        assert_eq!(s.usable(Some("aaaa1111"), "tok", 1), Ok(own.clone()));
        assert_eq!(s.usable(Some("bbbb2222"), "tok", 1), Ok(team.clone()));
        // El recordado caducado ya no vale.
        let later = 1 + 2 * 3600 * 1000;
        assert_eq!(
            s.usable(Some("bbbb2222"), "tok", later),
            Err(UseError::Missing("tok".into()))
        );
        // El propio de más de 24 h tampoco: el del equipo.
        std::fs::File::options()
            .write(true)
            .open(&own)
            .unwrap()
            .set_modified(std::time::SystemTime::now() - Duration::from_secs(25 * 3600))
            .unwrap();
        assert_eq!(s.usable(Some("aaaa1111"), "tok", 1), Ok(team));
        assert_eq!(
            UseError::Missing("tok".into()).to_string(),
            "no hay ningún secreto «tok» para esta sesión: pídelo antes con la herramienta secret"
        );
        assert!(UseError::NoSession.to_string().contains("NIDO_SESSION"));
    }

    #[test]
    fn session_secret_is_private_and_only_for_it() {
        let (_tmp, s) = store();
        let p = s.save_session("abc12345", "tok", b"s3cr3t").unwrap();
        assert_eq!(p, s.dir().join("sesiones/abc12345/tok"));
        assert_eq!(std::fs::read(&p).unwrap(), b"s3cr3t");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&p), 0o600);
            assert_eq!(mode(s.dir()), 0o700);
            assert_eq!(mode(&s.dir().join("sesiones")), 0o700);
            assert_eq!(mode(&s.dir().join("sesiones/abc12345")), 0o700);
        }
        // No es de todo el equipo.
        assert_eq!(s.remembered("tok", 0), None);
        assert!(s.save_session("../x", "tok", b"v").is_err());
        assert!(s.save_session("abc12345", "../x", b"v").is_err());
        s.forget_session("abc12345");
        assert!(!p.exists());
    }

    #[test]
    fn remembered_secrets_expire_and_keep_who_asked() {
        let (_tmp, s) = store();
        let p = s
            .save_team("gh", b"uno", Remember::TwoHours, "api", 1_000)
            .unwrap();
        assert_eq!(p, s.dir().join("equipo/gh"));
        assert_eq!(s.remembered("gh", 1_000), Some(p.clone()));
        let list = s.list(1_000);
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].by, "api");
        assert_eq!(list[0].expires, Some(1_000 + 2 * 3600 * 1000));
        // El índice no lleva el valor.
        let index = std::fs::read_to_string(s.dir().join("equipo.json")).unwrap();
        assert!(!index.contains("uno"), "{index}");
        // Otra sesión lo pide: se apunta.
        s.touch("gh", "web", 2_000).unwrap();
        assert_eq!(s.list(2_000)[0].by, "web");
        assert_eq!(s.list(2_000)[0].at, 2_000);
        // Caducado: ya no vale, y `prune` lo borra.
        let later = 1_000 + 2 * 3600 * 1000;
        assert_eq!(s.remembered("gh", later), None);
        assert!(s.list(later).is_empty());
        s.prune(later, |_| true);
        assert!(!p.exists());
        // Siempre: no caduca; sustituirlo cambia el valor y la caducidad.
        s.save_team("gh", b"dos", Remember::Always, "api", 3_000)
            .unwrap();
        s.save_team("gh", b"tres", Remember::SevenDays, "api", 4_000)
            .unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"tres");
        assert_eq!(s.list(4_000).len(), 1);
        assert!(s.forget("gh").unwrap());
        assert!(!s.forget("gh").unwrap());
        assert!(!p.exists() && s.list(4_000).is_empty());
    }

    #[test]
    fn prune_drops_old_and_orphan_session_secrets() {
        let (_tmp, s) = store();
        let viejo = s.save_session("aaaa1111", "a", b"1").unwrap();
        let nuevo = s.save_session("bbbb2222", "b", b"2").unwrap();
        let huerfano = s.save_session("cccc3333", "c", b"3").unwrap();
        let now = std::time::SystemTime::now();
        std::fs::File::options()
            .write(true)
            .open(&viejo)
            .unwrap()
            .set_modified(now - Duration::from_secs(25 * 3600))
            .unwrap();
        let now_ms = now
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        s.prune(now_ms, |id| id != "cccc3333");
        assert!(!viejo.exists());
        assert!(nuevo.exists());
        assert!(!huerfano.exists() && !huerfano.parent().unwrap().exists());
    }

    #[test]
    fn answer_goes_to_each_session_or_to_the_machine() {
        let (_tmp, s) = store();
        let sessions = vec!["aaaa1111".to_owned(), "bbbb2222".to_owned()];
        let paths = s
            .save_answer("tok", &sessions, b"v", Remember::No, false, "api, web", 1)
            .unwrap();
        assert_eq!(paths["aaaa1111"], s.dir().join("sesiones/aaaa1111/tok"));
        assert_eq!(paths["bbbb2222"], s.dir().join("sesiones/bbbb2222/tok"));
        let paths = s
            .save_answer(
                "tok",
                &sessions,
                b"v",
                Remember::Always,
                false,
                "api, web",
                1,
            )
            .unwrap();
        assert_eq!(paths["aaaa1111"], s.dir().join("equipo/tok"));
        assert_eq!(paths["bbbb2222"], s.dir().join("equipo/tok"));
        // Con `force` y sin recordar: el recordado ya no valía, fuera.
        s.save_answer("tok", &sessions[..1], b"w", Remember::No, true, "api", 2)
            .unwrap();
        assert_eq!(s.remembered("tok", 2), None);
    }

    #[tokio::test]
    async fn two_sessions_share_one_ask_and_get_their_paths() {
        let asks = Asks::default();
        let (mut a, new_a) = asks.join("tok", "para subir", "aaaa1111", 5).unwrap();
        let (mut b, new_b) = asks.join("tok", "otro motivo", "bbbb2222", 6).unwrap();
        assert!(new_a && !new_b);
        let va = asks.view_for("aaaa1111").unwrap();
        assert_eq!(va, asks.view_for("bbbb2222").unwrap());
        assert_eq!(
            (va.name.as_str(), va.reason.as_str(), va.since),
            ("tok", "para subir", 5)
        );
        assert_eq!(va.until, 5 + ASK_WAIT.as_millis() as u64);
        // Una sesión, una pregunta.
        assert!(matches!(asks.join("otro", "x", "aaaa1111", 7), Err(Busy)));
        // Contesta quien la ve en una de sus sesiones.
        assert!(asks.take(&va.id, "cccc3333").is_none());
        let taken = asks.take(&va.id, "bbbb2222").unwrap();
        assert_eq!(taken.name, "tok");
        assert_eq!(taken.sessions, ["aaaa1111", "bbbb2222"]);
        assert!(asks.view_for("aaaa1111").is_none());
        let paths: HashMap<String, PathBuf> = [
            ("aaaa1111".to_owned(), PathBuf::from("/s/a")),
            ("bbbb2222".to_owned(), PathBuf::from("/s/b")),
        ]
        .into();
        taken.finish(Outcome::Saved(paths.clone()));
        assert_eq!(a.wait().await, (Outcome::Saved(paths.clone()), vec![]));
        assert_eq!(b.wait().await, (Outcome::Saved(paths), vec![]));
        // Ya contestada: otra vez, nada.
        assert!(asks.take(&va.id, "aaaa1111").is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn an_unanswered_ask_expires_once_for_all() {
        let asks = Asks::default();
        let start = tokio::time::Instant::now();
        let (mut a, _) = asks.join("tok", "x", "aaaa1111", 0).unwrap();
        let (mut b, _) = asks.join("tok", "x", "bbbb2222", 0).unwrap();
        let (ra, rb) = tokio::join!(a.wait(), b.wait());
        assert!(start.elapsed() >= ASK_WAIT, "{:?}", start.elapsed());
        assert_eq!(ra.0, Outcome::Expired);
        assert_eq!(rb.0, Outcome::Expired);
        // Solo uno de los dos se lleva las sesiones (para apuntar «sin contestar» una vez).
        let mut noted = [ra.1, rb.1].concat();
        noted.sort();
        assert_eq!(noted, ["aaaa1111", "bbbb2222"]);
        assert!(asks.view_for("aaaa1111").is_none());
    }

    #[tokio::test]
    async fn leaving_closes_it_when_nobody_waits() {
        let asks = Asks::default();
        let (a, _) = asks.join("tok", "x", "aaaa1111", 0).unwrap();
        let (mut b, _) = asks.join("tok", "x", "bbbb2222", 0).unwrap();
        drop(a); // su petición se cayó
        assert!(asks.view_for("aaaa1111").is_none());
        assert!(asks.view_for("bbbb2222").is_some());
        assert!(asks.cancel_session("bbbb2222"));
        assert_eq!(b.wait().await.0, Outcome::Closed);
        assert!(!asks.cancel_session("bbbb2222"));
        // Ya libre: puede preguntar otra cosa.
        assert!(asks.join("otro", "x", "bbbb2222", 0).is_ok());
    }

    #[tokio::test]
    async fn rejected_and_forced() {
        let asks = Asks::default();
        let (mut a, _) = asks.join_forced("tok", "x", "aaaa1111", 0, false).unwrap();
        let (_b, _) = asks.join_forced("tok", "x", "bbbb2222", 0, true).unwrap();
        let id = asks.view_for("aaaa1111").unwrap().id;
        let taken = asks.take(&id, "aaaa1111").unwrap();
        assert!(taken.forced);
        taken.finish(Outcome::Rejected);
        assert_eq!(a.wait().await.0, Outcome::Rejected);
    }

    #[test]
    fn touch_and_save_team_at_once_lose_nothing() {
        let (_tmp, s) = store();
        s.save_team("gh", b"v", Remember::Always, "api", 1).unwrap();
        std::thread::scope(|t| {
            t.spawn(|| {
                for i in 0..40 {
                    s.touch("gh", "web", 10 + i).unwrap();
                }
            });
            t.spawn(|| {
                for i in 0..40 {
                    s.save_team(&format!("n{i}"), b"v", Remember::Always, "api", 2)
                        .unwrap();
                }
            });
            t.spawn(|| {
                for _ in 0..40 {
                    s.prune(3, |_| true);
                }
            });
        });
        let list = s.list(100);
        assert_eq!(list.len(), 41, "{list:?}");
        assert_eq!(list.iter().find(|r| r.name == "gh").unwrap().by, "web");
        for i in 0..40 {
            assert!(s.remembered(&format!("n{i}"), 100).is_some(), "n{i}");
        }
    }

    #[test]
    fn touching_an_unknown_name_writes_nothing() {
        let (_tmp, s) = store();
        s.touch("nada", "api", 1).unwrap();
        assert!(!s.dir().join("equipo.json").exists());
        assert!(s.list(1).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn write_private_replaces_a_planted_link_instead_of_following_it() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let victim = tmp.path().join("victima");
        std::fs::write(&victim, b"original").unwrap();
        let target = tmp.path().join("secreto");
        std::os::unix::fs::symlink(&victim, &target).unwrap();
        write_private(&target, b"nuevo").unwrap();
        let meta = std::fs::symlink_metadata(&target).unwrap();
        assert!(meta.file_type().is_file());
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::read(&target).unwrap(), b"nuevo");
        assert_eq!(std::fs::read(&victim).unwrap(), b"original");
    }

    #[test]
    fn prune_sweeps_orphans_and_old_temporaries() {
        let (_tmp, s) = store();
        let kept = s.save_team("gh", b"v", Remember::Always, "api", 1).unwrap();
        let session = s.save_session("aaaa1111", "tok", b"v").unwrap();
        let team = s.dir().join("equipo");
        let sdir = s.dir().join("sesiones/aaaa1111");
        // Un recordado sin su línea en el índice (un corte a medias) y temporales de una caída.
        let orphan = team.join("huerfano");
        std::fs::write(&orphan, b"x").unwrap();
        let team_tmp = team.join(".gh.0a1b2c3d~tmp");
        let index_tmp = s.dir().join(".equipo.json.0a1b2c3d~tmp");
        let old_tmp = sdir.join(".tok.0a1b2c3d~tmp");
        let new_tmp = sdir.join(".tok.4e5f6a7b~tmp");
        for p in [&team_tmp, &index_tmp, &old_tmp, &new_tmp] {
            std::fs::write(p, b"x").unwrap();
        }
        let now = std::time::SystemTime::now();
        let two_hours_ago = now - Duration::from_secs(2 * 3600);
        for p in [&index_tmp, &old_tmp, &session] {
            std::fs::File::options()
                .write(true)
                .open(p)
                .unwrap()
                .set_modified(two_hours_ago)
                .unwrap();
        }
        let now_ms = now
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        s.prune(now_ms, |_| true);
        assert!(kept.exists() && session.exists() && new_tmp.exists());
        for gone in [&orphan, &team_tmp, &index_tmp, &old_tmp] {
            assert!(!gone.exists(), "{}", gone.display());
        }
    }

    #[tokio::test(start_paused = true)]
    async fn an_answer_being_saved_is_not_reported_as_expired() {
        let asks = Asks::default();
        let (mut a, _) = asks.join("tok", "x", "aaaa1111", 0).unwrap();
        let id = asks.view_for("aaaa1111").unwrap().id;
        // El usuario contesta justo antes de que se acabe el tiempo, y guardarlo tarda.
        let taken = asks.take(&id, "aaaa1111").unwrap();
        let paths: HashMap<String, PathBuf> =
            [("aaaa1111".to_owned(), PathBuf::from("/s/a"))].into();
        let saved = paths.clone();
        let (got, ()) = tokio::join!(a.wait(), async move {
            tokio::time::sleep(ASK_WAIT + Duration::from_secs(5)).await;
            taken.finish(Outcome::Saved(saved));
        });
        assert_eq!(got, (Outcome::Saved(paths), vec![]));
        // Si quien la cogió no acaba nunca: cerrada, no caducada.
        let (mut b, _) = asks.join("otro", "x", "bbbb2222", 0).unwrap();
        let id = asks.view_for("bbbb2222").unwrap().id;
        let stuck = asks.take(&id, "bbbb2222").unwrap();
        assert_eq!(b.wait().await, (Outcome::Closed, vec![]));
        drop(stuck);
    }

    #[tokio::test]
    async fn the_same_session_asking_again_joins_its_ask() {
        let asks = Asks::default();
        let (first, new) = asks.join("tok", "x", "aaaa1111", 0).unwrap();
        assert!(new);
        let (mut again, new) = asks.join_forced("tok", "x", "aaaa1111", 1, true).unwrap();
        assert!(!new);
        // Otro nombre, no.
        assert!(matches!(asks.join("otro", "x", "aaaa1111", 2), Err(Busy)));
        // La primera petición se cae: la segunda sigue esperando.
        drop(first);
        let id = asks.view_for("aaaa1111").unwrap().id;
        let taken = asks.take(&id, "aaaa1111").unwrap();
        assert_eq!(taken.sessions, ["aaaa1111"]);
        assert!(taken.forced);
        taken.finish(Outcome::Rejected);
        assert_eq!(again.wait().await.0, Outcome::Rejected);
        // Cerrar la sesión la quita aunque esperase dos veces.
        let (_x, _) = asks.join("tok", "x", "bbbb2222", 0).unwrap();
        let (_y, _) = asks.join("tok", "x", "bbbb2222", 0).unwrap();
        assert!(asks.cancel_session("bbbb2222"));
        assert!(asks.view_for("bbbb2222").is_none());
    }
}
