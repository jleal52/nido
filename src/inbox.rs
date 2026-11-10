//! La bandeja de mensajes entre sesiones (la herramienta `send` del MCP): una cola por sesión de
//! destino, en el nido de su equipo. La recoge el `nido mcp` de esa sesión (`GET
//! /api/v1/mcp/inbox`), que mete cada mensaje en Claude como evento del channel y luego dice que
//! lo metió (`ack`). Eso no basta: Claude a veces tira un evento (justo tras saludar), así que el
//! mensaje sigue en la cola hasta verlo en la transcripción de la sesión (`confirm`; el evento
//! lleva su id, `msg_id`). Si no aparece en un rato sin que Claude trabaje, se vuelve a dar (ver
//! `step`): al menos una vez, y nunca otra vez lo que ya se vio.
//!
//! Se guarda en `<estado>/inbox.json`: sobrevive a un reinicio de nido. Aquí también se sabe qué
//! sesiones tienen channel (su `nido mcp` está recogiendo la bandeja), solo en memoria.

use std::collections::{BTreeMap, HashMap};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio::sync::watch;

/// ¿Es del `nido mcp` que pide la bandeja de una sesión? Claude pasa su entorno a lo que
/// ejecuta: otro `claude` dentro de la sesión heredaría `NIDO_SESSION` y `NIDO_CHANNEL`, y su
/// `nido mcp` se llevaría los mensajes de esta.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Claim {
    /// Suya: recoge, confirma y la sesión tiene channel.
    Yes,
    /// Aún no se sabe cuál es el Claude de la sesión (arrancando, o es el `nido mcp` de antes de
    /// reiniciarla): ni mensajes ni channel, y que vuelva.
    Wait,
    /// De otro Claude: 409.
    No,
}

/// `sent`: el `nido mcp` mandó su pid; `ancestor`: el Claude que tiene encima
/// (`claude::claude_ancestor`); `known`: el Claude de la sesión (`claude::session_claude`);
/// `starting`: la sesión arranca (ver `CLAIM_STARTUP`).
pub fn claim(sent: bool, ancestor: Option<u32>, known: Option<u32>, starting: bool) -> Claim {
    match (sent, known) {
        // Un `nido mcp` que no lo manda: como siempre.
        (false, _) => Claim::Yes,
        (true, Some(k)) if ancestor == Some(k) => Claim::Yes,
        (true, Some(_)) => Claim::No,
        // Sin Claude en su árbol, aún: que vuelva.
        (true, None) if starting => Claim::Wait,
        // Ni pasado el arranque (con npm su proceso es `node`): como antes, sin proteger la
        // bandeja (ver `Inbox::unguarded`).
        (true, None) => Claim::Yes,
    }
}

/// Cuánto se espera, tras lanzar una sesión, a que aparezca su Claude antes de dar la bandeja a
/// quien la pida.
pub const CLAIM_STARTUP: Duration = Duration::from_secs(30);

/// ¿Arranca aún la sesión? `age`: cuánto hace que se lanzó su guardián (`None` si no lo hay:
/// parada, o entre parar y volver a lanzarla, y entonces su Claude no puede ser ninguno).
pub fn starting(age: Option<Duration>) -> bool {
    age.is_none_or(|a| a < CLAIM_STARTUP)
}

/// Por qué no tiene channel una sesión, según su pantalla al arrancar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// Claude no tiene el servidor `nido` («no MCP server configured»).
    NoServer,
    /// La organización de la cuenta de Claude no permite channels (`channelsEnabled`): Claude
    /// tira lo que le llegue por el channel.
    Blocked,
    /// Su Claude se fue sin haber enseñado su caja y la sesión sigue (p. ej. una shell de login
    /// que no le hace `exec`): nadie recogerá su bandeja.
    NoClaude,
}

impl Refusal {
    /// Para `sessions` (`channel_error`).
    pub fn code(self) -> &'static str {
        match self {
            Refusal::NoServer => "no nido MCP server",
            Refusal::Blocked => "blocked by org policy",
            Refusal::NoClaude => "claude not running",
        }
    }
}

/// Mientras vive, la bandeja de la sesión no da nada (ver `Inbox::hold`).
pub struct Hold {
    channels: Arc<Mutex<HashMap<String, Channel>>>,
    arrived: Arc<watch::Sender<()>>,
    id: String,
}

impl Hold {
    /// Lo que vio quien la retiene: no tiene channel (como `Inbox::refuse`).
    pub fn refuse(&self, why: Refusal) {
        if let Some(c) = self.channels.lock().expect("channels").get_mut(&self.id) {
            c.refused = Some(why);
        }
        self.arrived.send_replace(());
    }
}

impl Drop for Hold {
    fn drop(&mut self) {
        // Si entretanto se borró la sesión, no se vuelve a apuntar.
        if let Some(c) = self.channels.lock().expect("channels").get_mut(&self.id) {
            c.holding = c.holding.saturating_sub(1);
        }
        self.arrived.send_replace(());
    }
}

/// Lo más que se encola para una sesión.
pub const MAX_QUEUED: usize = 100;
/// Cuánto espera `GET /api/v1/mcp/inbox` a que llegue algo.
pub const POLL_WAIT: Duration = Duration::from_secs(25);
/// Una sesión tiene channel si su `nido mcp` recogió la bandeja hace menos de esto (o la está
/// recogiendo ahora).
pub const CHANNEL_FRESH: Duration = Duration::from_secs(60);

/// Un mensaje de otra sesión.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Msg {
    pub id: String,
    /// El nombre de la sesión que lo manda.
    pub from: String,
    /// Su dirección canónica (`<equipo>/<id>`): a donde se contesta.
    pub from_id: String,
    /// El nombre de su equipo.
    pub machine: String,
    pub text: String,
    /// Cuándo se mandó (ms).
    pub at: u64,
}

/// Dónde acababa la transcripción de una sesión: desde ahí se busca lo que entró después.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mark {
    /// La transcripción o, si su Claude aún no la había escrito, solo su nombre
    /// (`<session_id>.jsonl`, sin carpeta: aún no se sabe en cuál estará), con `offset` 0.
    pub path: PathBuf,
    pub offset: u64,
}

impl Mark {
    /// ¿Es de la transcripción `path`? La misma o, si al marcar aún no existía, la que tiene
    /// ese nombre (el id de la conversación).
    pub fn on(&self, path: &Path) -> bool {
        self.path == path
            || (self.path.parent() == Some(Path::new(""))
                && path.file_name() == self.path.file_name())
    }
}

/// Lo que se sabe de un mensaje ya dado a su `nido mcp` (con `Queued`; se guarda).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Out {
    /// El intento: 1 la primera vez que se da, uno más cada vez que se vuelve a dar (`Step::Again`).
    pub tries: u32,
    /// Dónde acababa la transcripción la primera vez que se dio (si la había).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<Mark>,
    /// Su `nido mcp` dijo que lo metió en Claude, en este intento.
    #[serde(default)]
    pub emitted: bool,
    /// Lo metió con su id (`msg_id`): un `nido mcp` de antes no lo pone, y entonces se reconoce
    /// por quién lo manda y su texto.
    #[serde(default)]
    pub tracked: bool,
    /// Tras `MAX_TRIES` intentos no apareció: no se da más (hasta volver a lanzar la sesión).
    #[serde(default)]
    pub failed: bool,
}

/// Un mensaje en la cola y, si ya se dio, cómo va (se guarda así, compatible con lo de antes).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Queued {
    #[serde(flatten)]
    msg: Msg,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    out: Option<Out>,
}

/// Lo que se ve de un evento del channel de nido en la transcripción.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Delivered {
    /// Con su id (`msg_id`, que pone este `nido mcp`).
    Id(String),
    /// Sin él (lo metió un `nido mcp` de antes): el `delivery_id` de Claude Code si llegó
    /// mientras trabajaba (un `queued_command`; se prueba como id), quién lo manda y el texto.
    Text {
        delivery_id: Option<String>,
        from_id: Option<String>,
        text: String,
    },
}

/// Cuánto se espera a ver en la transcripción un mensaje ya metido, sin contar el rato en que
/// Claude trabaja (puede no enseñar los eventos hasta acabar el turno).
pub const CONFIRM_WAIT: Duration = Duration::from_secs(30);
/// Sin escribir en su transcripción en este rato, Claude no está trabajando.
pub const TRANSCRIPT_QUIET: Duration = Duration::from_secs(5);
/// Lo mismo cuando nido no sabe en qué está Claude (sin estado): hace falta más rato.
pub const TRANSCRIPT_QUIET_UNKNOWN: Duration = Duration::from_secs(90);
/// Las veces que se da un mensaje antes de dejarlo (sigue en la cola, con un error de entrega).
pub const MAX_TRIES: u32 = 3;

/// Qué hacer con un mensaje dado que aún no aparece en la transcripción.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// Seguir esperando.
    Wait,
    /// Darlo otra vez.
    Again,
    /// Ya se dio `MAX_TRIES` veces: no más.
    Failed,
}

/// Un mensaje dado (`out`) tras `waited` (sin contar lo que Claude trabajó) sin verse en la
/// transcripción: solo cuenta lo que su `nido mcp` dijo que metió.
pub fn step(out: &Out, waited: Duration, limit: Duration) -> Step {
    if !out.emitted || out.failed || waited < limit {
        Step::Wait
    } else if out.tries >= MAX_TRIES {
        Step::Failed
    } else {
        Step::Again
    }
}

/// No se pudo encolar: ya hay `MAX_QUEUED` esperando.
#[derive(Debug, PartialEq, Eq)]
pub struct Full;

/// Cómo recoge la bandeja el `nido mcp` de una sesión.
#[derive(Debug, Default)]
struct Channel {
    /// Recogidas en marcha ahora mismo.
    polling: usize,
    /// Cuándo acabó (o empezó) la última.
    last: Option<Instant>,
    /// Su pantalla dice que no tiene channel, y por qué (ver `Refusal`). Dura lo que dure ese
    /// arranque: lo quita `relaunched` (o `forget`), no una recogida.
    refused: Option<Refusal>,
    /// Arranques que se están mirando (ver `Inbox::hold`).
    holding: usize,
    /// Se paró a mano (ver `Inbox::stopped`). Lo quita `relaunched`.
    stopped: bool,
    /// Ya se dijo que su bandeja va sin proteger (`unguarded`). Lo quita `relaunched`.
    unguarded: bool,
}

impl Channel {
    /// No se le da nada a su `nido mcp` (ver `Inbox::held`).
    fn is_held(&self) -> bool {
        self.refused.is_some() || self.holding > 0 || self.stopped
    }
}

/// ¿Tiene channel? Si no se sabe que no, y está recogiendo o recogió hace poco.
pub fn live(polling: usize, last: Option<Instant>, refused: bool, now: Instant) -> bool {
    !refused
        && (polling > 0 || last.is_some_and(|t| now.saturating_duration_since(t) < CHANNEL_FRESH))
}

pub struct Inbox {
    path: PathBuf,
    queues: Mutex<HashMap<String, Vec<Queued>>>,
    /// Cuánto lleva cada mensaje metido sin verse, sin contar lo que trabajó Claude (por su id;
    /// solo en memoria: tras reiniciar `nido serve` se empieza a contar de nuevo).
    waits: Mutex<HashMap<String, Duration>>,
    /// Hasta dónde se ha mirado la transcripción de cada sesión.
    cursors: Mutex<HashMap<String, Mark>>,
    /// La transcripción ya encontrada de cada sesión, con la conversación de la que es (ver
    /// `transcript`): así no se busca en cada vuelta.
    transcripts: Mutex<HashMap<String, (String, PathBuf)>>,
    /// `CONFIRM_WAIT`, `TRANSCRIPT_QUIET` y `TRANSCRIPT_QUIET_UNKNOWN` en ms (las pruebas los
    /// acortan).
    confirm_ms: AtomicU64,
    quiet_ms: AtomicU64,
    quiet_unknown_ms: AtomicU64,
    /// Compartido con los `Hold`, que lo sueltan al acabar.
    channels: Arc<Mutex<HashMap<String, Channel>>>,
    /// Cambia con cada mensaje nuevo: despierta a las recogidas que esperan.
    /// Compartido con los `Hold`: al soltar, las recogidas que esperan vuelven a mirar.
    arrived: Arc<watch::Sender<()>>,
    /// `POLL_WAIT` en ms (las pruebas la acortan).
    wait_ms: AtomicU64,
    /// Cuándo se cargó (ver `uptime`).
    loaded_at: Mutex<Instant>,
}

impl Inbox {
    /// La de `path`; si no está o no se puede leer, vacía.
    pub fn load(path: PathBuf) -> Self {
        let queues: BTreeMap<String, Vec<Queued>> = match std::fs::read(&path) {
            Ok(b) => serde_json::from_slice(&b).unwrap_or_else(|e| {
                // Rota: se aparta (para mirarla) y se empieza vacía; si no, se pisaría al guardar.
                let bad = path.with_extension("json.bad");
                eprintln!(
                    "nido: la bandeja {} está rota ({e}); se aparta a {}",
                    path.display(),
                    bad.display()
                );
                if let Err(e) = std::fs::rename(&path, &bad) {
                    eprintln!("nido: no se pudo apartar la bandeja rota: {e}");
                }
                BTreeMap::new()
            }),
            Err(_) => BTreeMap::new(),
        };
        Self {
            path,
            queues: Mutex::new(queues.into_iter().collect()),
            waits: Mutex::default(),
            cursors: Mutex::default(),
            transcripts: Mutex::default(),
            confirm_ms: AtomicU64::new(CONFIRM_WAIT.as_millis() as u64),
            quiet_ms: AtomicU64::new(TRANSCRIPT_QUIET.as_millis() as u64),
            quiet_unknown_ms: AtomicU64::new(TRANSCRIPT_QUIET_UNKNOWN.as_millis() as u64),
            channels: Arc::default(),
            arrived: Arc::new(watch::Sender::new(())),
            wait_ms: AtomicU64::new(POLL_WAIT.as_millis() as u64),
            loaded_at: Mutex::new(Instant::now()),
        }
    }

    /// Cuánto hace que se cargó (que arrancó `nido serve`): hasta que pasa un rato, los `nido
    /// mcp` de las sesiones aún no han vuelto a recoger y no se sabe quién tiene channel.
    pub fn uptime(&self) -> Duration {
        self.loaded_at.lock().expect("bandeja").elapsed()
    }

    /// Para las pruebas: como si se hubiera cargado hace `d` más.
    pub fn backdate(&self, d: Duration) {
        let mut at = self.loaded_at.lock().expect("bandeja");
        if let Some(t) = at.checked_sub(d) {
            *at = t;
        }
    }

    /// Escribe en un temporal y lo renombra (solo para el usuario: son conversaciones).
    fn save(&self, queues: &HashMap<String, Vec<Queued>>) {
        let sorted: BTreeMap<&String, &Vec<Queued>> =
            queues.iter().filter(|(_, q)| !q.is_empty()).collect();
        let write = || -> std::io::Result<()> {
            if let Some(dir) = self.path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let tmp = self.path.with_extension("json.tmp");
            let body = serde_json::to_vec(&sorted)?;
            {
                let mut f = crate::sys::private(
                    std::fs::OpenOptions::new()
                        .write(true)
                        .create(true)
                        .truncate(true),
                )
                .open(&tmp)?;
                f.write_all(&body)?;
                f.sync_all()?;
            }
            std::fs::rename(&tmp, &self.path)
        };
        if let Err(e) = write() {
            eprintln!("nido: no se pudo guardar la bandeja: {e}");
        }
    }

    /// Encola `msg` para la sesión `to` y despierta a quien esté recogiendo.
    pub fn push(&self, to: &str, msg: Msg) -> Result<(), Full> {
        {
            let mut queues = self.queues.lock().expect("bandeja");
            let q = queues.entry(to.to_owned()).or_default();
            if q.len() >= MAX_QUEUED {
                return Err(Full);
            }
            q.push(Queued { msg, out: None });
            self.save(&queues);
        }
        self.arrived.send_replace(());
        Ok(())
    }

    /// Lo que hay en la cola de `id` (también lo ya dado que aún no se ha visto), del más antiguo
    /// al más nuevo.
    pub fn pending(&self, id: &str) -> Vec<Msg> {
        self.queues
            .lock()
            .expect("bandeja")
            .get(id)
            .map(|q| q.iter().map(|m| m.msg.clone()).collect())
            .unwrap_or_default()
    }

    /// Cómo va el mensaje `msg` de la cola de `id`, si sigue en ella.
    pub fn out(&self, id: &str, msg: &str) -> Option<Option<Out>> {
        self.queues
            .lock()
            .expect("bandeja")
            .get(id)?
            .iter()
            .find(|m| m.msg.id == msg)
            .map(|m| m.out.clone())
    }

    /// ¿Sigue en la cola de `id` el mensaje `msg`?
    pub fn has(&self, id: &str, msg: &str) -> bool {
        self.out(id, msg).is_some()
    }

    /// ¿Lo recogió ya la sesión? Si ya no está en la cola, o su `nido mcp` dijo que lo metió (lo
    /// que mira `send` con `wait`).
    pub fn taken(&self, id: &str, msg: &str) -> bool {
        match self.out(id, msg) {
            None => true,
            Some(out) => out.is_some_and(|o| o.emitted || o.failed),
        }
    }

    /// Su `nido mcp` dice que metió en Claude los mensajes `ids` de la cola de `id` (los de otras
    /// colas no se tocan), con su id en el evento (`tracked`) o, uno de antes, sin él. Siguen en la
    /// cola hasta verlos en la transcripción (`confirm`). Devuelve cuántos eran de esta cola.
    ///
    /// Retenida (`held`), no apunta nada y devuelve 0, a propósito: mejor «al menos una vez» que
    /// «perdido». Lo que diga tarde, tras parar o volver a lanzar la sesión, no cuenta.
    pub fn ack(&self, id: &str, ids: &[String], tracked: bool) -> usize {
        if self.held(id) {
            return 0;
        }
        let mut queues = self.queues.lock().expect("bandeja");
        let Some(q) = queues.get_mut(id) else {
            return 0;
        };
        let mut acked = 0;
        let mut waits = self.waits.lock().expect("esperas");
        for m in q.iter_mut().filter(|m| ids.contains(&m.msg.id)) {
            let out = m.out.get_or_insert_with(Out::default);
            out.tries = out.tries.max(1);
            if !out.emitted {
                waits.insert(m.msg.id.clone(), Duration::ZERO);
            }
            out.emitted = true;
            out.tracked = tracked;
            acked += 1;
        }
        drop(waits);
        if acked > 0 {
            self.save(&queues);
        }
        acked
    }

    /// Lo que se ha visto en la transcripción de `id` (en `path`, mirada hasta `upto`; cada uno
    /// con dónde empieza su línea): fuera de su cola. Un evento con id quita ese mensaje; uno sin
    /// él (de un `nido mcp` de antes), el de su `delivery_id` si lo hay o, si no, el más antiguo ya
    /// dado sin id que lo mande el mismo y con el mismo texto, y solo si su línea es de después de
    /// darlo (en la misma transcripción y desde donde acababa entonces: si no, puede ser uno
    /// anterior igual). Uno por evento. Devuelve cuántos quitó.
    pub fn confirm(&self, id: &str, path: &Path, upto: u64, seen: &[(u64, Delivered)]) -> usize {
        self.cursors.lock().expect("cursores").insert(
            id.to_owned(),
            Mark {
                path: path.to_owned(),
                offset: upto,
            },
        );
        let mut queues = self.queues.lock().expect("bandeja");
        let Some(q) = queues.get_mut(id) else {
            return 0;
        };
        let before = q.len();
        let mut gone: Vec<String> = Vec::new();
        for (line, d) in seen {
            let at = match d {
                Delivered::Id(msg) => q.iter().position(|m| &m.msg.id == msg),
                Delivered::Text {
                    delivery_id,
                    from_id,
                    text,
                } => delivery_id
                    .as_ref()
                    .and_then(|d| q.iter().position(|m| &m.msg.id == d))
                    .or_else(|| {
                        let from_id = from_id.as_ref()?;
                        q.iter().position(|m| {
                            m.out.as_ref().is_some_and(|o| {
                                !o.tracked
                                    && o.from
                                        .as_ref()
                                        .is_some_and(|f| f.on(path) && *line >= f.offset)
                            }) && &m.msg.from_id == from_id
                                && &m.msg.text == text
                        })
                    }),
            };
            if let Some(i) = at {
                gone.push(q.remove(i).msg.id);
            }
        }
        let removed = before - q.len();
        if q.is_empty() {
            queues.remove(id);
        }
        if removed > 0 {
            self.save(&queues);
        }
        drop(queues);
        let mut waits = self.waits.lock().expect("esperas");
        for g in &gone {
            waits.remove(g);
        }
        removed
    }

    /// Las sesiones con mensajes ya dados que aún no se han visto en su transcripción.
    pub fn outstanding(&self) -> Vec<String> {
        self.queues
            .lock()
            .expect("bandeja")
            .iter()
            .filter(|(_, q)| q.iter().any(|m| m.out.as_ref().is_some_and(|o| !o.failed)))
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// Desde dónde mirar la transcripción de `id` (en `path`) para ver lo ya dado: hasta donde se
    /// miró la otra vez o, si no, desde donde acababa al darlo (de cero si era otra o no la había).
    /// `None`: no hay nada dado que buscar.
    ///
    /// Si cambia la transcripción (`/clear`, otra conversación), se empieza a mirar la nueva y no
    /// se vuelve a la de antes: lo que entró en ella justo antes del cambio y aún no se había
    /// mirado se da otra vez (una). Se acepta: mejor «al menos una vez» que perderlo.
    pub fn scan_from(&self, id: &str, path: &Path) -> Option<u64> {
        let queues = self.queues.lock().expect("bandeja");
        let outs: Vec<&Out> = queues
            .get(id)?
            .iter()
            .filter_map(|m| m.out.as_ref())
            .collect();
        if outs.is_empty() {
            return None;
        }
        let earliest = outs
            .iter()
            .map(|o| match &o.from {
                Some(f) if f.on(path) => f.offset,
                _ => 0,
            })
            .min()
            .unwrap_or(0);
        let cursor = self
            .cursors
            .lock()
            .expect("cursores")
            .get(id)
            .filter(|c| c.path == path)
            .map(|c| c.offset);
        Some(cursor.unwrap_or(earliest))
    }

    /// Ha pasado `elapsed` para los mensajes metidos en `id`, y contaba (`quiet`: Claude no
    /// trabajaba). Los que toca se vuelven a dar (y despiertan a quien recoge) o se dejan (ver
    /// `step`). Devuelve cuántos se vuelven a dar y cuántos se dejan.
    pub fn tick(&self, id: &str, quiet: bool, elapsed: Duration) -> (usize, usize) {
        let limit = Duration::from_millis(self.confirm_ms.load(Ordering::Relaxed));
        let (mut again, mut failed) = (0, 0);
        {
            let mut queues = self.queues.lock().expect("bandeja");
            let Some(q) = queues.get_mut(id) else {
                return (0, 0);
            };
            let mut waits = self.waits.lock().expect("esperas");
            for m in q.iter_mut() {
                let Some(out) = m.out.as_mut() else { continue };
                if !out.emitted || out.failed {
                    continue;
                }
                let w = waits.entry(m.msg.id.clone()).or_default();
                if quiet {
                    *w += elapsed;
                }
                match step(out, *w, limit) {
                    Step::Wait => {}
                    Step::Again => {
                        out.emitted = false;
                        out.tries += 1;
                        *w = Duration::ZERO;
                        again += 1;
                    }
                    Step::Failed => {
                        out.failed = true;
                        failed += 1;
                        eprintln!(
                            "nido: el mensaje {} para la sesión {id} no aparece en su \
                             conversación tras {MAX_TRIES} intentos; sigue en la cola",
                            m.msg.id
                        );
                    }
                }
            }
            drop(waits);
            if again + failed > 0 {
                self.save(&queues);
            }
        }
        if again > 0 {
            self.arrived.send_replace(());
        }
        (again, failed)
    }

    /// Cuántos mensajes de la cola de `id` se dejaron sin entregar (ver `Step::Failed`).
    pub fn undelivered(&self, id: &str) -> usize {
        self.queues.lock().expect("bandeja").get(id).map_or(0, |q| {
            q.iter()
                .filter(|m| m.out.as_ref().is_some_and(|o| o.failed))
                .count()
        })
    }

    /// Para las pruebas: cuánto se espera a ver un mensaje y cuánto sin escribir es no trabajar
    /// (sabiendo el estado de Claude y sin saberlo).
    pub fn set_confirm_wait(&self, wait: Duration, quiet: Duration, quiet_unknown: Duration) {
        self.confirm_ms
            .store(wait.as_millis() as u64, Ordering::Relaxed);
        self.quiet_ms
            .store(quiet.as_millis() as u64, Ordering::Relaxed);
        self.quiet_unknown_ms
            .store(quiet_unknown.as_millis() as u64, Ordering::Relaxed);
    }

    /// Sin escribir en su transcripción en este rato, Claude no está trabajando: sabiendo su
    /// estado, y sin saberlo (`TRANSCRIPT_QUIET_UNKNOWN`).
    pub fn transcript_quiet(&self) -> (Duration, Duration) {
        (
            Duration::from_millis(self.quiet_ms.load(Ordering::Relaxed)),
            Duration::from_millis(self.quiet_unknown_ms.load(Ordering::Relaxed)),
        )
    }

    /// La transcripción ya encontrada de `id`, si es de la conversación `conversation` (si no, ya
    /// no vale: se olvida).
    pub fn transcript(&self, id: &str, conversation: &str) -> Option<PathBuf> {
        let mut t = self.transcripts.lock().expect("transcripciones");
        match t.get(id) {
            Some((c, p)) if c == conversation => Some(p.clone()),
            Some(_) => {
                t.remove(id);
                None
            }
            None => None,
        }
    }

    /// Apunta la transcripción encontrada de `id` (de la conversación `conversation`).
    pub fn set_transcript(&self, id: &str, conversation: &str, path: PathBuf) {
        self.transcripts
            .lock()
            .expect("transcripciones")
            .insert(id.to_owned(), (conversation.to_owned(), path));
    }

    /// La de `id` ya no vale (no se pudo leer): se buscará otra vez.
    pub fn forget_transcript(&self, id: &str) {
        self.transcripts.lock().expect("transcripciones").remove(id);
    }

    /// Una sesión que ya no está: fuera su cola y lo que se sabía de su channel.
    pub fn forget(&self, id: &str) {
        let mut queues = self.queues.lock().expect("bandeja");
        if let Some(q) = queues.remove(id) {
            self.save(&queues);
            let mut waits = self.waits.lock().expect("esperas");
            for m in q {
                waits.remove(&m.msg.id);
            }
        }
        self.cursors.lock().expect("cursores").remove(id);
        self.forget_transcript(id);
        drop(queues);
        self.channels.lock().expect("channels").remove(id);
    }

    /// ¿Tiene channel la sesión `id`? (Ver `live`.)
    pub fn channel(&self, id: &str) -> bool {
        self.channels
            .lock()
            .expect("channels")
            .get(id)
            .is_some_and(|c| live(c.polling, c.last, c.is_held(), Instant::now()))
    }

    /// La sesión `id` no tiene channel, según su pantalla (hasta que se vuelva a lanzar).
    pub fn refuse(&self, id: &str, why: Refusal) {
        self.channels
            .lock()
            .expect("channels")
            .entry(id.to_owned())
            .or_default()
            .refused = Some(why);
        self.arrived.send_replace(());
    }

    /// Se para la sesión `id` (a mano): hasta que se vuelva a lanzar no se le da nada, que su
    /// `nido mcp` se está muriendo con su Claude.
    pub fn stopped(&self, id: &str) {
        self.channels
            .lock()
            .expect("channels")
            .entry(id.to_owned())
            .or_default()
            .stopped = true;
        self.arrived.send_replace(());
    }

    /// Por qué no tiene channel la sesión `id`, si su pantalla lo dijo en este arranque.
    pub fn refusal(&self, id: &str) -> Option<Refusal> {
        self.channels
            .lock()
            .expect("channels")
            .get(id)
            .and_then(|c| c.refused)
    }

    /// ¿Se le retiene la bandeja a su `nido mcp`? Sin channel (Claude tiraría los mensajes),
    /// mientras se mira su arranque (`hold`) o parada a mano (`stopped`). Mientras, tampoco
    /// cuenta como con channel ni confirma nada.
    pub fn held(&self, id: &str) -> bool {
        self.channels
            .lock()
            .expect("channels")
            .get(id)
            .is_some_and(Channel::is_held)
    }

    /// Mientras se mira el arranque de `id` y aún no se sabe si Claude acepta el channel: su
    /// `nido mcp` puede estar ya recogiendo, y lo que se le diera antes de ver un «blocked by
    /// org policy» se perdería. Hasta que se suelte (`drop`).
    pub fn hold(&self, id: &str) -> Hold {
        self.channels
            .lock()
            .expect("channels")
            .entry(id.to_owned())
            .or_default()
            .holding += 1;
        self.arrived.send_replace(());
        Hold {
            channels: self.channels.clone(),
            arrived: self.arrived.clone(),
            id: id.to_owned(),
        }
    }

    /// Su bandeja va sin proteger (no se encuentra su Claude, ver `claim`): `true` la primera
    /// vez en este arranque, para decirlo una vez.
    pub fn unguarded(&self, id: &str) -> bool {
        let mut channels = self.channels.lock().expect("channels");
        let c = channels.entry(id.to_owned()).or_default();
        !std::mem::replace(&mut c.unguarded, true)
    }

    /// La sesión `id` se acaba de lanzar otra vez: lo que se sabía de su channel era del
    /// arranque anterior.
    ///
    /// Lo dado al arranque de antes que no se ha visto en su transcripción se da otra vez al
    /// nuevo, desde el primer intento (antes de darlo se mira la transcripción: ver `scan_from`).
    pub fn relaunched(&self, id: &str) {
        self.forget_transcript(id);
        if let Some(c) = self.channels.lock().expect("channels").get_mut(id) {
            c.refused = None;
            c.stopped = false;
            c.unguarded = false;
        }
        let mut queues = self.queues.lock().expect("bandeja");
        let mut changed = false;
        if let Some(q) = queues.get_mut(id) {
            let mut waits = self.waits.lock().expect("esperas");
            for m in q.iter_mut() {
                if let Some(o) = m.out.as_mut() {
                    o.emitted = false;
                    o.failed = false;
                    o.tries = 0;
                    waits.remove(&m.msg.id);
                    changed = true;
                }
            }
        }
        if changed {
            self.save(&queues);
        }
    }

    /// Cuánto espera una recogida.
    pub fn poll_wait(&self) -> Duration {
        Duration::from_millis(self.wait_ms.load(Ordering::Relaxed))
    }

    /// Para las pruebas: recogidas más cortas.
    pub fn set_poll_wait(&self, d: Duration) {
        self.wait_ms.store(d.as_millis() as u64, Ordering::Relaxed);
    }

    /// La recogida del `nido mcp` de `id`: lo que espera, en cuanto haya algo, o nada si pasa
    /// `poll_wait`. Mientras dura (y un rato después), la sesión tiene channel.
    pub async fn poll(&self, id: &str) -> Vec<Msg> {
        self.poll_from(id, None)
            .await
            .into_iter()
            .map(|(m, _)| m)
            .collect()
    }

    /// `poll`, con la transcripción de la sesión (`transcript`; si aún no existe, su nombre: ver
    /// `Mark`): lo que se da por primera vez se buscará desde donde acaba al darlo. Da lo que no
    /// se ha metido aún (ni se dejó), cada uno con su intento.
    pub async fn poll_from(&self, id: &str, transcript: Option<PathBuf>) -> Vec<(Msg, u32)> {
        let _polling = Polling::start(self, id);
        // Antes de mirar la cola: así no se pierde un mensaje que llegue entre medias.
        let mut rx = self.arrived.subscribe();
        let deadline = tokio::time::Instant::now() + self.poll_wait();
        loop {
            // Retenida (ver `held`), se sigue esperando: quien la suelte avisa.
            let msgs = if self.held(id) {
                Vec::new()
            } else {
                // Dónde acaba ahora, al darlo (no al empezar a esperar).
                // Solo con su nombre (aún no existe): desde el principio, sin mirar nada.
                let from = match &transcript {
                    Some(path) if path.parent() == Some(Path::new("")) => Some(Mark {
                        path: path.clone(),
                        offset: 0,
                    }),
                    Some(path) => Some(Mark {
                        path: path.clone(),
                        offset: tokio::fs::metadata(path).await.map_or(0, |m| m.len()),
                    }),
                    None => None,
                };
                self.offer(id, from.as_ref())
            };
            if !msgs.is_empty() {
                return msgs;
            }
            match tokio::time::timeout_at(deadline, rx.changed()).await {
                Ok(Ok(())) => {}
                _ => return Vec::new(),
            }
        }
    }
}

impl Inbox {
    /// Lo que se le puede dar ahora al `nido mcp` de `id`, apuntado como dado.
    fn offer(&self, id: &str, from: Option<&Mark>) -> Vec<(Msg, u32)> {
        let mut queues = self.queues.lock().expect("bandeja");
        let Some(q) = queues.get_mut(id) else {
            return Vec::new();
        };
        let mut changed = false;
        let mut out = Vec::new();
        for m in q.iter_mut() {
            let o = m.out.get_or_insert_with(|| {
                changed = true;
                Out::default()
            });
            if o.emitted || o.failed {
                continue;
            }
            if o.tries == 0 {
                o.tries = 1;
                changed = true;
            }
            if o.from.is_none() && from.is_some() {
                o.from = from.cloned();
                changed = true;
            }
            out.push((m.msg.clone(), o.tries));
        }
        if changed {
            self.save(&queues);
        }
        out
    }
}

/// Los eventos del channel de nido que hay en la transcripción `path` desde `from` (desde la
/// primera línea entera; si el fichero es más corto, se rehízo: desde el principio), y hasta
/// dónde se miró (la última línea entera). Solo se leen las líneas con `"channel"` (con comillas,
/// el valor de `origin.kind`): un turno de
/// Claude con el evento (`origin.kind == "channel"`), o el mismo metido mientras trabajaba (un
/// `queued_command` con ese origen).
pub fn channel_deliveries(path: &Path, from: u64) -> std::io::Result<(Vec<(u64, Delivered)>, u64)> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path)?;
    let len = f.metadata()?.len();
    let from = if from > len { 0 } else { from };
    let start = from.saturating_sub(1);
    f.seek(SeekFrom::Start(start))?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf)?;
    // Lo de antes de `from` es media línea (o el salto de la anterior).
    let skip = if from == 0 {
        0
    } else {
        match buf.iter().position(|&b| b == b'\n') {
            Some(i) => i + 1,
            None => return Ok((Vec::new(), from)),
        }
    };
    let complete = buf.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
    if complete <= skip {
        return Ok((Vec::new(), start + skip as u64));
    }
    let upto = start + complete as u64;
    let mut found = Vec::new();
    let mut at = start + skip as u64;
    for line in buf[skip..complete - 1].split(|&b| b == b'\n') {
        if line.windows(9).any(|w| w == b"\"channel\"")
            && let Some(d) = std::str::from_utf8(line).ok().and_then(delivered)
        {
            found.push((at, d));
        }
        at += line.len() as u64 + 1;
    }
    Ok((found, upto))
}

/// Una línea de la transcripción, si es un evento del channel de nido entregado.
fn delivered(line: &str) -> Option<Delivered> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    let channel = |o: &serde_json::Value| {
        o.pointer("/origin/kind").and_then(|k| k.as_str()) == Some("channel")
    };
    let text = match v.get("type").and_then(|t| t.as_str())? {
        "user" if channel(&v) => match v.pointer("/message/content")? {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Array(parts) => parts
                .iter()
                .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join(""),
            _ => return None,
        },
        "attachment" => {
            let a = v.get("attachment")?;
            if a.get("type").and_then(|t| t.as_str()) != Some("queued_command") || !channel(a) {
                return None;
            }
            let d = channel_tag(a.get("prompt")?.as_str()?)?;
            let delivery_id = a.get("delivery_id").and_then(|d| d.as_str());
            return Some(match (d, delivery_id) {
                (Delivered::Text { from_id, text, .. }, Some(id)) => Delivered::Text {
                    delivery_id: Some(id.to_owned()),
                    from_id,
                    text,
                },
                (d, _) => d,
            });
        }
        _ => return None,
    };
    channel_tag(&text)
}

/// `<channel source="nido" … msg_id="…">\ntexto\n</channel>`: su id o, sin él, quién lo manda (si
/// se sabe) y el texto. No se mira `source` (es el nombre con que Claude tiene el servidor): los ids son
/// únicos, y sin id se compara con lo dado (quién y qué).
fn channel_tag(text: &str) -> Option<Delivered> {
    let rest = text.trim_start().strip_prefix("<channel")?;
    let close = rest.find('>')?;
    let attrs = attributes(&rest[..close]);
    let get = |k: &str| attrs.iter().find(|(a, _)| a == k).map(|(_, v)| v.clone());
    if let Some(id) = get("msg_id") {
        return Some(Delivered::Id(id));
    }
    let body = &rest[close + 1..];
    let body = body.strip_prefix('\n').unwrap_or(body);
    let body = body.trim_end();
    let body = body.strip_suffix("</channel>")?;
    let body = body.strip_suffix('\n').unwrap_or(body);
    Some(Delivered::Text {
        delivery_id: None,
        from_id: get("from_id"),
        text: body.to_owned(),
    })
}

/// `a="1" b="dos"` → [(a, 1), (b, dos)] (con `&quot;`, `&lt;`, `&gt;` y `&amp;` deshechos).
fn attributes(s: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut rest = s;
    while let Some(eq) = rest.find("=\"") {
        let key = rest[..eq].trim().to_owned();
        let after = &rest[eq + 2..];
        let Some(end) = after.find('"') else { break };
        let value = after[..end]
            .replace("&quot;", "\"")
            .replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&amp;", "&");
        out.push((key, value));
        rest = &after[end + 1..];
    }
    out
}

/// Una recogida en marcha: la sesión tiene channel hasta un rato después de soltarla.
struct Polling<'a> {
    inbox: &'a Inbox,
    id: String,
}

impl<'a> Polling<'a> {
    fn start(inbox: &'a Inbox, id: &str) -> Self {
        let mut channels = inbox.channels.lock().expect("channels");
        let c = channels.entry(id.to_owned()).or_default();
        c.polling += 1;
        c.last = Some(Instant::now());
        Self {
            inbox,
            id: id.to_owned(),
        }
    }
}

impl Drop for Polling<'_> {
    fn drop(&mut self) {
        // Si entretanto se borró la sesión, no se vuelve a apuntar.
        if let Some(c) = self
            .inbox
            .channels
            .lock()
            .expect("channels")
            .get_mut(&self.id)
        {
            c.polling = c.polling.saturating_sub(1);
            c.last = Some(Instant::now());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(id: &str, text: &str) -> Msg {
        Msg {
            id: id.into(),
            from: "padre".into(),
            from_id: "k123/padre1".into(),
            machine: "Portatil".into(),
            text: text.into(),
            at: 1,
        }
    }

    fn ids(v: &[Msg]) -> Vec<&str> {
        v.iter().map(|m| m.id.as_str()).collect()
    }

    #[test]
    fn queue_and_ack_only_the_own_queue() {
        let dir = tempfile::tempdir().unwrap();
        let inbox = Inbox::load(dir.path().join("inbox.json"));
        inbox.push("hija", msg("m1", "uno")).unwrap();
        inbox.push("hija", msg("m2", "dos")).unwrap();
        inbox.push("otra", msg("m3", "tres")).unwrap();
        assert_eq!(ids(&inbox.pending("hija")), ["m1", "m2"], "en orden");
        assert!(inbox.has("hija", "m1") && !inbox.has("hija", "m3"));
        // Los de otra cola no se tocan.
        assert_eq!(inbox.ack("hija", &["m3".into()], true), 0);
        assert_eq!(inbox.out("otra", "m3"), Some(None));
        assert_eq!(inbox.ack("nadie", &["m3".into()], true), 0);
        // Metido (dice su `nido mcp`), sigue en la cola hasta verlo en la transcripción.
        assert_eq!(inbox.ack("hija", &["m1".into(), "zz".into()], true), 1);
        assert_eq!(ids(&inbox.pending("hija")), ["m1", "m2"]);
        assert!(inbox.taken("hija", "m1") && !inbox.taken("hija", "m2"));
        let path = Path::new("/t.jsonl");
        assert_eq!(
            inbox.confirm("hija", path, 0, &[(0, Delivered::Id("m1".into()))]),
            1
        );
        assert_eq!(ids(&inbox.pending("hija")), ["m2"]);
        assert!(inbox.taken("hija", "m1"), "ya no está");
        inbox.forget("otra");
        assert!(inbox.pending("otra").is_empty());
    }

    #[test]
    fn survives_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state/inbox.json");
        let inbox = Inbox::load(path.clone());
        inbox.push("hija", msg("m1", "uno")).unwrap();
        inbox.push("hija", msg("m2", "dos")).unwrap();
        inbox.push("otra", msg("m3", "tres")).unwrap();
        // Dado y metido, y luego visto: fuera. Dado y metido sin verse: sigue, y así se guarda.
        let from = Mark {
            path: "/t.jsonl".into(),
            offset: 7,
        };
        assert_eq!(inbox.offer("hija", Some(&from)).len(), 2);
        inbox.ack("hija", &["m1".into(), "m2".into()], true);
        inbox.confirm(
            "hija",
            Path::new("/t.jsonl"),
            9,
            &[(0, Delivered::Id("m1".into()))],
        );
        inbox.forget("otra");
        drop(inbox);
        assert!(!path.with_extension("json.tmp").exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        // Tras el «reinicio»: lo no visto sigue, con dónde buscarlo.
        let again = Inbox::load(path.clone());
        assert_eq!(again.pending("hija"), vec![msg("m2", "dos")]);
        assert_eq!(
            again.out("hija", "m2"),
            Some(Some(Out {
                tries: 1,
                from: Some(from),
                emitted: true,
                tracked: true,
                failed: false,
            }))
        );
        assert_eq!(again.scan_from("hija", Path::new("/t.jsonl")), Some(7));
        assert!(again.pending("otra").is_empty());
        // Una bandeja de antes (sin cómo va cada uno) se lee igual.
        std::fs::write(
            &path,
            r#"{"vieja":[{"id":"v1","from":"a","from_id":"k/a","machine":"M","text":"t","at":1}]}"#,
        )
        .unwrap();
        let old = Inbox::load(path);
        assert_eq!(ids(&old.pending("vieja")), ["v1"]);
        assert_eq!(old.out("vieja", "v1"), Some(None));
    }

    #[test]
    fn a_broken_file_is_set_aside() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("inbox.json");
        std::fs::write(&path, "{roto").unwrap();
        let inbox = Inbox::load(path.clone());
        assert!(inbox.pending("hija").is_empty(), "vacía, sin fallar");
        assert!(!path.exists());
        let bad = dir.path().join("inbox.json.bad");
        assert_eq!(std::fs::read_to_string(&bad).unwrap(), "{roto");
        // Lo nuevo no la pisa.
        inbox.push("hija", msg("m1", "uno")).unwrap();
        assert_eq!(std::fs::read_to_string(&bad).unwrap(), "{roto");
        assert_eq!(Inbox::load(path).pending("hija"), vec![msg("m1", "uno")]);
    }

    #[test]
    fn uptime_since_loaded() {
        let dir = tempfile::tempdir().unwrap();
        let inbox = Inbox::load(dir.path().join("inbox.json"));
        assert!(inbox.uptime() < Duration::from_secs(5));
        inbox.backdate(Duration::from_secs(300));
        assert!(inbox.uptime() >= Duration::from_secs(300));
    }

    #[test]
    fn at_most_100_per_session() {
        let dir = tempfile::tempdir().unwrap();
        let inbox = Inbox::load(dir.path().join("inbox.json"));
        for i in 0..MAX_QUEUED {
            inbox.push("hija", msg(&format!("m{i}"), "x")).unwrap();
        }
        assert_eq!(inbox.push("hija", msg("otro", "x")), Err(Full));
        assert_eq!(inbox.pending("hija").len(), MAX_QUEUED);
        // Las demás sesiones, aparte.
        inbox.push("otra", msg("m", "x")).unwrap();
        inbox.ack("hija", &["m0".into()], true);
        inbox.confirm(
            "hija",
            Path::new("/t"),
            0,
            &[(0, Delivered::Id("m0".into()))],
        );
        inbox.push("hija", msg("otro", "x")).unwrap();
    }

    #[test]
    fn channel_liveness_rule() {
        let now = Instant::now() + Duration::from_secs(3600);
        assert!(!live(0, None, false, now), "nunca recogió");
        assert!(live(1, None, false, now), "recogiendo ahora");
        assert!(live(0, Some(now - Duration::from_secs(59)), false, now));
        assert!(!live(0, Some(now - Duration::from_secs(60)), false, now));
        assert!(live(2, Some(now - Duration::from_secs(600)), false, now));
        // Su pantalla dice que no tiene channel.
        assert!(!live(1, Some(now), true, now));
    }

    #[tokio::test]
    async fn poll_wakes_on_a_message_and_marks_the_channel() {
        let _limit = crate::watchdog::limit(std::time::Duration::from_secs(60));
        let dir = tempfile::tempdir().unwrap();
        let inbox = std::sync::Arc::new(Inbox::load(dir.path().join("inbox.json")));
        inbox.set_poll_wait(Duration::from_secs(10));
        assert!(!inbox.channel("hija"));
        let poller = {
            let inbox = inbox.clone();
            tokio::spawn(async move { inbox.poll("hija").await })
        };
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(inbox.channel("hija"), "recogiendo");
        let start = Instant::now();
        inbox.push("otra", msg("m0", "para otra")).unwrap();
        inbox.push("hija", msg("m1", "uno")).unwrap();
        let got = poller.await.unwrap();
        assert!(start.elapsed() < Duration::from_secs(2));
        assert_eq!(ids(&got), ["m1"]);
        assert!(inbox.channel("hija"), "acaba de recoger");
        // Con algo en la cola, vuelve enseguida; sin nada, al acabarse la espera.
        assert_eq!(ids(&inbox.poll("hija").await), ["m1"]);
        inbox.ack("hija", &["m1".into()], true);
        inbox.set_poll_wait(Duration::from_millis(200));
        let start = Instant::now();
        assert!(inbox.poll("hija").await.is_empty());
        assert!(start.elapsed() >= Duration::from_millis(200));
        // La pantalla dice que no tiene: así hasta que se vuelva a lanzar, aunque recoja.
        assert_eq!(inbox.refusal("hija"), None);
        inbox.refuse("hija", Refusal::Blocked);
        assert!(!inbox.channel("hija"));
        assert_eq!(inbox.refusal("hija"), Some(Refusal::Blocked));
        assert!(inbox.held("hija"), "sin channel, no se le da nada");
        inbox.poll("hija").await;
        assert!(!inbox.channel("hija"), "la recogida no quita el rechazo");
        inbox.relaunched("hija");
        assert_eq!(inbox.refusal("hija"), None);
        assert!(!inbox.held("hija"));
        inbox.poll("hija").await;
        assert!(inbox.channel("hija"));
        // Borrada, nada (y el rechazo tampoco queda para otra con el mismo id).
        inbox.refuse("hija", Refusal::NoServer);
        inbox.forget("hija");
        assert_eq!(inbox.refusal("hija"), None);
        assert!(!inbox.channel("hija"));
        inbox.poll("hija").await;
        assert!(inbox.channel("hija"));
    }

    #[tokio::test]
    async fn a_poll_in_flight_respects_the_hold() {
        let _limit = crate::watchdog::limit(std::time::Duration::from_secs(60));
        let dir = tempfile::tempdir().unwrap();
        let inbox = std::sync::Arc::new(Inbox::load(dir.path().join("inbox.json")));
        inbox.set_poll_wait(Duration::from_millis(1500));
        let poll = |inbox: &std::sync::Arc<Inbox>| {
            let inbox = inbox.clone();
            tokio::spawn(async move { inbox.poll("s").await })
        };
        // Una recogida que empezó antes de retener la bandeja: lo que llegue no se lo lleva, y
        // la sesión no cuenta como con channel.
        let a = poll(&inbox);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(inbox.channel("s"));
        let hold = inbox.hold("s");
        assert!(!inbox.channel("s"), "retenida, no tiene channel");
        inbox.push("s", msg("m1", "uno")).unwrap();
        assert!(a.await.unwrap().is_empty(), "se lo llevó retenida");
        // Ni confirmar.
        assert_eq!(inbox.ack("s", &["m1".into()], true), 0);
        assert_eq!(ids(&inbox.pending("s")), ["m1"]);
        // Al soltarla, la que espera lo recoge ya.
        let b = poll(&inbox);
        tokio::time::sleep(Duration::from_millis(100)).await;
        let start = Instant::now();
        drop(hold);
        assert_eq!(ids(&b.await.unwrap()), ["m1"]);
        assert!(start.elapsed() < Duration::from_millis(1000));
        assert_eq!(inbox.ack("s", &["m1".into()], true), 1);
        // Igual con un rechazo, que llega con una recogida en marcha.
        let c = poll(&inbox);
        tokio::time::sleep(Duration::from_millis(100)).await;
        inbox.refuse("s", Refusal::Blocked);
        inbox.push("s", msg("m2", "dos")).unwrap();
        assert!(c.await.unwrap().is_empty());
        inbox.relaunched("s");
        // El arranque nuevo recibe también lo metido al de antes sin verse (m1).
        assert_eq!(inbox.ack("s", &["m1".into(), "m2".into()], true), 2);
        // Parada a mano: hasta volver a lanzarla, nada (su `nido mcp` se está muriendo).
        let d = poll(&inbox);
        tokio::time::sleep(Duration::from_millis(100)).await;
        inbox.stopped("s");
        inbox.push("s", msg("m3", "tres")).unwrap();
        assert!(d.await.unwrap().is_empty());
        assert!(!inbox.channel("s"));
        inbox.relaunched("s");
        // Lo metido al arranque de antes sin verse en su transcripción, otra vez al nuevo.
        assert_eq!(ids(&inbox.poll("s").await), ["m1", "m2", "m3"]);
    }

    #[test]
    fn what_to_do_with_a_message_not_seen_yet() {
        let limit = CONFIRM_WAIT;
        let out = |tries, emitted| Out {
            tries,
            emitted,
            ..Out::default()
        };
        // Dado pero sin que su `nido mcp` diga que lo metió: no cuenta.
        assert_eq!(step(&out(1, false), limit * 10, limit), Step::Wait);
        assert_eq!(step(&out(1, true), limit / 2, limit), Step::Wait);
        assert_eq!(step(&out(1, true), limit, limit), Step::Again);
        assert_eq!(step(&out(MAX_TRIES - 1, true), limit, limit), Step::Again);
        assert_eq!(step(&out(MAX_TRIES, true), limit, limit), Step::Failed);
        let failed = Out {
            failed: true,
            ..out(MAX_TRIES, true)
        };
        assert_eq!(step(&failed, limit * 10, limit), Step::Wait);
    }

    #[tokio::test]
    async fn a_message_is_given_again_until_seen_or_given_up() {
        let _limit = crate::watchdog::limit(std::time::Duration::from_secs(60));
        let dir = tempfile::tempdir().unwrap();
        let inbox = Inbox::load(dir.path().join("inbox.json"));
        inbox.set_poll_wait(Duration::from_millis(50));
        let t = &dir.path().join("t.jsonl");
        let grow = |len: usize| std::fs::write(t, "x".repeat(len)).unwrap();
        let tries = |v: Vec<(Msg, u32)>| -> Vec<(String, u32)> {
            v.into_iter().map(|(m, n)| (m.id, n)).collect()
        };
        inbox.push("s", msg("m1", "uno")).unwrap();
        assert_eq!(inbox.outstanding(), Vec::<String>::new());
        assert_eq!(inbox.scan_from("s", t), None, "nada dado: nada que buscar");
        // Dado: primer intento, buscado desde donde acababa la transcripción al darlo.
        grow(40);
        assert_eq!(
            tries(inbox.poll_from("s", Some(t.clone())).await),
            [("m1".into(), 1)]
        );
        assert_eq!(inbox.outstanding(), ["s"]);
        assert_eq!(inbox.scan_from("s", t), Some(40));
        assert_eq!(inbox.scan_from("s", Path::new("/otra.jsonl")), Some(0));
        // Sin que diga que lo metió, sale igual en la siguiente (y no cuenta el tiempo).
        assert_eq!(inbox.tick("s", true, CONFIRM_WAIT * 2), (0, 0));
        grow(50);
        assert_eq!(
            tries(inbox.poll_from("s", Some(t.clone())).await),
            [("m1".into(), 1)]
        );
        assert_eq!(inbox.scan_from("s", t), Some(40), "desde la primera vez");
        // Metido: ya no sale, ni con Claude trabajando cuenta el tiempo.
        assert_eq!(inbox.ack("s", &["m1".into()], true), 1);
        assert!(inbox.poll_from("s", None).await.is_empty());
        assert_eq!(inbox.tick("s", false, CONFIRM_WAIT * 2), (0, 0));
        assert_eq!(inbox.tick("s", true, CONFIRM_WAIT / 2), (0, 0));
        // Visto lo de otro (y hasta dónde se miró): sigue.
        assert_eq!(
            inbox.confirm("s", t, 60, &[(0, Delivered::Id("otro".into()))]),
            0
        );
        assert_eq!(inbox.scan_from("s", t), Some(60));
        // Sin verse tras el plazo: otra vez, segundo intento.
        assert_eq!(inbox.tick("s", true, CONFIRM_WAIT / 2), (1, 0));
        assert!(!inbox.taken("s", "m1"));
        assert_eq!(tries(inbox.poll_from("s", None).await), [("m1".into(), 2)]);
        inbox.ack("s", &["m1".into()], true);
        assert_eq!(inbox.tick("s", true, CONFIRM_WAIT), (1, 0));
        assert_eq!(tries(inbox.poll_from("s", None).await), [("m1".into(), 3)]);
        inbox.ack("s", &["m1".into()], true);
        assert_eq!(inbox.undelivered("s"), 0);
        // El último: se deja, sigue en la cola y ya no se da ni se busca.
        assert_eq!(inbox.tick("s", true, CONFIRM_WAIT), (0, 1));
        assert_eq!(inbox.undelivered("s"), 1);
        assert_eq!(ids(&inbox.pending("s")), ["m1"]);
        assert!(inbox.poll_from("s", None).await.is_empty());
        assert_eq!(inbox.outstanding(), Vec::<String>::new());
        assert_eq!(inbox.tick("s", true, CONFIRM_WAIT * 9), (0, 0), "una vez");
        // Al volver a lanzarla, otra vez desde el primero.
        inbox.relaunched("s");
        assert_eq!(inbox.undelivered("s"), 0);
        assert_eq!(tries(inbox.poll_from("s", None).await), [("m1".into(), 1)]);
        inbox.ack("s", &["m1".into()], true);
        // Visto: fuera, y ya no se busca ni se vuelve a dar.
        assert_eq!(
            inbox.confirm("s", t, 90, &[(0, Delivered::Id("m1".into()))]),
            1
        );
        assert!(inbox.pending("s").is_empty());
        assert_eq!(inbox.outstanding(), Vec::<String>::new());
        assert_eq!(inbox.tick("s", true, CONFIRM_WAIT * 9), (0, 0));
        assert!(inbox.poll_from("s", None).await.is_empty());
    }

    #[test]
    fn without_an_id_it_is_recognised_by_sender_and_text() {
        let dir = tempfile::tempdir().unwrap();
        let inbox = Inbox::load(dir.path().join("inbox.json"));
        let t = Path::new("/c/7a0ae70e.jsonl");
        for (id, text) in [("a", "hola"), ("b", "hola"), ("c", "otro")] {
            inbox.push("s", msg(id, text)).unwrap();
        }
        inbox.push("s", msg("d", "hola")).unwrap();
        // Un `nido mcp` de antes (sin `msg_id`) metió a, b y c cuando la transcripción acababa
        // en 100; d aún no se ha dado.
        let from = Mark {
            path: t.to_owned(),
            offset: 100,
        };
        inbox.offer("s", Some(&from));
        inbox.ack("s", &["a".into(), "b".into(), "c".into()], false);
        inbox.ack("s", &["d".into()], true);
        let text = |at: u64, from_id: &str, text: &str| {
            (
                at,
                Delivered::Text {
                    delivery_id: None,
                    from_id: Some(from_id.into()),
                    text: text.into(),
                },
            )
        };
        // De antes de darlo (otro igual que entró antes), o en otra transcripción: no.
        assert_eq!(
            inbox.confirm("s", t, 0, &[text(40, "k123/padre1", "hola")]),
            0
        );
        let other = Path::new("/c/otra.jsonl");
        assert_eq!(
            inbox.confirm("s", other, 0, &[text(400, "k123/padre1", "hola")]),
            0
        );
        // Un evento es un mensaje: el más antiguo de ese que lo manda y con ese texto.
        assert_eq!(
            inbox.confirm("s", t, 0, &[text(100, "k123/padre1", "hola")]),
            1
        );
        assert_eq!(ids(&inbox.pending("s")), ["b", "c", "d"]);
        // De otro, o con otro texto: no.
        assert_eq!(inbox.confirm("s", t, 0, &[text(200, "k9/otra", "hola")]), 0);
        assert_eq!(
            inbox.confirm("s", t, 0, &[text(200, "k123/padre1", "hola!")]),
            0
        );
        // Dos eventos iguales, dos mensajes; el de id (d) no se toma por el texto.
        let two = [
            text(300, "k123/padre1", "hola"),
            text(400, "k123/padre1", "hola"),
        ];
        assert_eq!(inbox.confirm("s", t, 0, &two), 1);
        assert_eq!(ids(&inbox.pending("s")), ["c", "d"]);
        // Con el `delivery_id` de Claude Code: ese, aunque el texto no cuadre (y aunque tenga id).
        let queued = Delivered::Text {
            delivery_id: Some("d".into()),
            from_id: Some("k123/padre1".into()),
            text: "otro".into(),
        };
        assert_eq!(inbox.confirm("s", t, 0, &[(500, queued)]), 1);
        assert_eq!(ids(&inbox.pending("s")), ["c"]);
        // Sin `from_id` (ni `delivery_id` que cuadre): no hay con qué compararlo.
        let anon = Delivered::Text {
            delivery_id: Some("zz".into()),
            from_id: None,
            text: "otro".into(),
        };
        assert_eq!(inbox.confirm("s", t, 0, &[(600, anon)]), 0);
        // Sin dónde acababa al darlo: por el texto no se confirma.
        inbox.push("s", msg("e", "tarde")).unwrap();
        inbox.offer("s", None);
        inbox.ack("s", &["e".into()], false);
        assert_eq!(
            inbox.confirm("s", t, 0, &[text(900, "k123/padre1", "tarde")]),
            0
        );
        // Dado antes de que su Claude escribiera la transcripción (solo su nombre): toda cuenta,
        // pero solo la de ese nombre.
        inbox.push("s", msg("f", "nueva")).unwrap();
        let unwritten = Mark {
            path: "7a0ae70e.jsonl".into(),
            offset: 0,
        };
        inbox.offer("s", Some(&unwritten));
        inbox.ack("s", &["f".into()], false);
        assert!(unwritten.on(t) && !unwritten.on(other));
        assert_eq!(inbox.scan_from("s", other), Some(0));
        assert_eq!(
            inbox.confirm("s", other, 0, &[text(0, "k123/padre1", "nueva")]),
            0
        );
        assert_eq!(
            inbox.confirm("s", t, 0, &[text(0, "k123/padre1", "nueva")]),
            1
        );
        assert_eq!(ids(&inbox.pending("s")), ["c", "e"]);
    }

    /// Dos líneas de verdad de una transcripción de Claude Code 2.1.322 (con la ruta y el `msg_id`
    /// de la primera cambiados):
    /// un evento del channel con Claude esperando, y otro que llegó mientras trabajaba.
    const DELIVERED_IDLE: &str = r#"{"parentUuid":"bd8fc7db-f0ee-42dc-b7c5-de6896853ea0","isSidechain":false,"promptId":"44fee0fa-00cb-4eda-871d-a84d3d354e70","type":"user","message":{"role":"user","content":"<channel source=\"nido\" from=\"padre\" from_id=\"k123/padre1\" machine=\"Portatil\" msg_id=\"0b1f8a52-3c3e-4d7a-9a41-6f2d8e5b7c10\">\nHola, ¿cuánto es 2+2?\n</channel>"},"isMeta":true,"uuid":"a7cfc6c0-6d8e-4e40-baf6-50b3d0ebd382","timestamp":"2026-11-10T09:58:06.693Z","permissionMode":"auto","origin":{"kind":"channel","server":"nido"},"promptSource":"system","queueSkipSlashCommands":true,"turnOrigin":"peer","turnPosition":{"promptIndex":0,"turnIndex":1},"queueSkipAttachments":true,"userType":"external","entrypoint":"cli","cwd":"/home/ana/proyecto","sessionId":"7a0ae70e-d2c0-4be6-9bbb-92c3be2eb706","version":"2.1.322","gitBranch":"HEAD"}"#;
    const DELIVERED_BUSY: &str = r#"{"parentUuid":"fd471423-b95c-434b-9e53-91cab74e1a5e","isSidechain":false,"attachment":{"type":"queued_command","prompt":"<channel source=\"nido\" from=\"padre\" from_id=\"k123/padre1\" machine=\"Portatil\">\nMensaje enviado mientras trabajas: di la palabra GIRASOL.\n</channel>","source_uuid":"e7446d97-604a-49b7-865c-3075ffd969bd","delivery_id":"5e5c4756-9e96-4225-9d4c-ee4453697737","commandMode":"prompt","origin":{"kind":"channel","server":"nido"},"timestamp":"2026-11-10T09:58:37.471Z","isMeta":true},"type":"attachment","uuid":"b9aa7fd6-dcd1-418d-bbea-3b523113a334","timestamp":"2026-11-10T09:58:37.471Z","rendered":[{"content":"<system-reminder>\nA message arrived from nido while you were working:\n<channel source=\"nido\" from=\"padre\">\nMensaje enviado mientras trabajas: di la palabra GIRASOL.\n</channel>\n</system-reminder>"}],"renderedRole":"system","session_id":"7a0ae70e-d2c0-4be6-9bbb-92c3be2eb706","userType":"external","entrypoint":"cli","cwd":"/home/ana/proyecto","sessionId":"7a0ae70e-d2c0-4be6-9bbb-92c3be2eb706","version":"2.1.322","gitBranch":"HEAD"}"#;

    #[test]
    fn deliveries_are_read_from_the_transcript_tail() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        // Un turno tuyo que cita un evento (no es del channel) y uno de Claude.
        let quoted = r#"{"type":"user","message":{"role":"user","content":"<channel source=\"nido\" msg_id=\"falso\">\nx\n</channel>"}}"#;
        let reply =
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"channel"}]}}"#;
        let head = format!("{quoted}\n{reply}\n");
        std::fs::write(&path, format!("{head}{DELIVERED_IDLE}\n{DELIVERED_BUSY}\n")).unwrap();
        let len = std::fs::metadata(&path).unwrap().len();
        let (found, upto) = channel_deliveries(&path, 0).unwrap();
        // Cada uno con dónde empieza su línea; el de la cola, con el `delivery_id` de Claude Code.
        let idle_at = head.len() as u64;
        let busy_at = idle_at + DELIVERED_IDLE.len() as u64 + 1;
        assert_eq!(
            found,
            [
                (
                    idle_at,
                    Delivered::Id("0b1f8a52-3c3e-4d7a-9a41-6f2d8e5b7c10".into())
                ),
                (
                    busy_at,
                    Delivered::Text {
                        delivery_id: Some("5e5c4756-9e96-4225-9d4c-ee4453697737".into()),
                        from_id: Some("k123/padre1".into()),
                        text: "Mensaje enviado mientras trabajas: di la palabra GIRASOL.".into()
                    }
                ),
            ]
        );
        assert_eq!(upto, len);
        // Desde lo ya mirado: nada; desde a media línea, solo lo de después.
        assert_eq!(channel_deliveries(&path, len).unwrap(), (vec![], len));
        let mid = head.len() as u64 + 10;
        let (found, _) = channel_deliveries(&path, mid).unwrap();
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].0, busy_at);
        assert!(matches!(found[0].1, Delivered::Text { .. }));
        // Una línea a medio escribir no cuenta (ni se salta): hasta la última entera.
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        std::io::Write::write_all(&mut f, &DELIVERED_IDLE.as_bytes()[..100]).unwrap();
        assert_eq!(channel_deliveries(&path, len).unwrap(), (vec![], len));
        std::io::Write::write_all(&mut f, &DELIVERED_IDLE.as_bytes()[100..]).unwrap();
        std::io::Write::write_all(&mut f, b"\n").unwrap();
        let (found, upto) = channel_deliveries(&path, len).unwrap();
        assert_eq!(found.len(), 1);
        assert!(upto > len);
        // Más corto que lo mirado (se rehízo): desde el principio.
        std::fs::write(&path, format!("{DELIVERED_IDLE}\n")).unwrap();
        let (found, _) = channel_deliveries(&path, upto).unwrap();
        assert_eq!(found.len(), 1);
    }

    #[test]
    fn the_inbox_waits_while_the_startup_is_watched() {
        // Mientras se mira el arranque (y no se sabe aún si Claude va a tirar los mensajes), no
        // se da nada; con dos mirando, hasta que acaban los dos.
        let inbox = Inbox::load(std::env::temp_dir().join("nido-no-existe/inbox.json"));
        assert!(!inbox.held("s"));
        let a = inbox.hold("s");
        let b = inbox.hold("s");
        assert!(inbox.held("s"));
        assert!(!inbox.held("t"));
        drop(a);
        assert!(inbox.held("s"));
        drop(b);
        assert!(!inbox.held("s"));
        // Borrada mientras se mira: al acabar no vuelve a apuntarse.
        let c = inbox.hold("s");
        inbox.forget("s");
        drop(c);
        assert!(!inbox.held("s"));
        assert!(inbox.channels.lock().unwrap().get("s").is_none());
        // Quien la retiene puede decir que no tiene channel (lo vio en su pantalla).
        let d = inbox.hold("s");
        d.refuse(Refusal::NoServer);
        drop(d);
        assert_eq!(inbox.refusal("s"), Some(Refusal::NoServer));
        assert!(inbox.held("s"));
    }

    #[test]
    fn the_inbox_belongs_to_the_sessions_own_claude() {
        for starting in [true, false] {
            // Sin pid (un `nido mcp` que no lo manda): como siempre.
            assert_eq!(claim(false, None, None, starting), Claim::Yes);
            assert_eq!(claim(false, None, Some(30), starting), Claim::Yes);
            // Su Claude es el de la sesión (el que encuentra `session_claude`): suya.
            assert_eq!(claim(true, Some(30), Some(30), starting), Claim::Yes);
            // Otro Claude de dentro, o ninguno: no.
            assert_eq!(claim(true, Some(40), Some(30), starting), Claim::No);
            assert_eq!(claim(true, None, Some(30), starting), Claim::No);
        }
        // Arrancando, sin Claude aún en su árbol (o el `nido mcp` del Claude de antes, que aún
        // no ha muerto): que espere, sin mensajes y sin marcar el channel.
        assert_eq!(claim(true, Some(30), None, true), Claim::Wait);
        assert_eq!(claim(true, None, None, true), Claim::Wait);
        // Pasado el arranque y sin Claude que encontrar (instalado con npm, es `node`): como
        // antes, sin proteger la bandeja.
        assert_eq!(claim(true, None, None, false), Claim::Yes);
        assert_eq!(claim(true, Some(30), None, false), Claim::Yes);
    }

    #[test]
    fn the_session_is_starting_for_a_while() {
        // Sin guardián (parada, o entre parar y volver a lanzarla): aún no hay de quién.
        assert!(starting(None));
        assert!(starting(Some(Duration::ZERO)));
        assert!(starting(Some(Duration::from_secs(29))));
        assert!(!starting(Some(CLAIM_STARTUP)));
        assert!(!starting(Some(Duration::from_secs(3600))));
    }

    #[test]
    fn says_once_per_launch_that_the_inbox_is_unguarded() {
        let inbox = Inbox::load(std::env::temp_dir().join("nido-no-existe/inbox.json"));
        assert!(inbox.unguarded("s"));
        assert!(!inbox.unguarded("s"));
        assert!(inbox.unguarded("t"));
        inbox.relaunched("s");
        assert!(inbox.unguarded("s"));
        assert!(!inbox.unguarded("s"));
    }
}
