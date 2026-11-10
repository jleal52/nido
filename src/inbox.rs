//! La bandeja de mensajes entre sesiones (la herramienta `send` del MCP): una cola por sesión de
//! destino, en el nido de su equipo. La recoge el `nido mcp` de esa sesión (`GET
//! /api/v1/mcp/inbox`), que mete cada mensaje en Claude como evento del channel y luego lo
//! confirma. Lo no confirmado se vuelve a entregar: al menos una vez.
//!
//! Se guarda en `<estado>/inbox.json`: sobrevive a un reinicio de nido. Aquí también se sabe qué
//! sesiones tienen channel (su `nido mcp` está recogiendo la bandeja), solo en memoria.

use std::collections::{BTreeMap, HashMap};
use std::io::Write;
use std::path::PathBuf;
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
}

impl Refusal {
    /// Para `sessions` (`channel_error`).
    pub fn code(self) -> &'static str {
        match self {
            Refusal::NoServer => "no nido MCP server",
            Refusal::Blocked => "blocked by org policy",
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
    queues: Mutex<HashMap<String, Vec<Msg>>>,
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
        let queues: BTreeMap<String, Vec<Msg>> = match std::fs::read(&path) {
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
    fn save(&self, queues: &HashMap<String, Vec<Msg>>) {
        let sorted: BTreeMap<&String, &Vec<Msg>> =
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
            q.push(msg);
            self.save(&queues);
        }
        self.arrived.send_replace(());
        Ok(())
    }

    /// Lo que espera para `id`, del más antiguo al más nuevo.
    pub fn pending(&self, id: &str) -> Vec<Msg> {
        self.queues
            .lock()
            .expect("bandeja")
            .get(id)
            .cloned()
            .unwrap_or_default()
    }

    /// ¿Sigue en la cola de `id` el mensaje `msg`? (Si no, ya lo recogió.)
    pub fn has(&self, id: &str, msg: &str) -> bool {
        self.queues
            .lock()
            .expect("bandeja")
            .get(id)
            .is_some_and(|q| q.iter().any(|m| m.id == msg))
    }

    /// Quita de la cola de `id` los mensajes `ids` (los de otras colas no se tocan). Devuelve
    /// cuántos quitó.
    ///
    /// Retenida (`held`), no quita nada y devuelve 0, a propósito: mejor «al menos una vez» que
    /// «perdido». Una confirmación que llega tarde, tras parar o volver a lanzar la sesión, deja
    /// el mensaje en la cola y se entregará otra vez.
    pub fn ack(&self, id: &str, ids: &[String]) -> usize {
        // Retenida, no se le ha dado nada: lo que diga confirmar no es de este arranque.
        if self.held(id) {
            return 0;
        }
        let mut queues = self.queues.lock().expect("bandeja");
        let Some(q) = queues.get_mut(id) else {
            return 0;
        };
        let before = q.len();
        q.retain(|m| !ids.contains(&m.id));
        let removed = before - q.len();
        if q.is_empty() {
            queues.remove(id);
        }
        if removed > 0 {
            self.save(&queues);
        }
        removed
    }

    /// Una sesión que ya no está: fuera su cola y lo que se sabía de su channel.
    pub fn forget(&self, id: &str) {
        let mut queues = self.queues.lock().expect("bandeja");
        if queues.remove(id).is_some() {
            self.save(&queues);
        }
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
    pub fn relaunched(&self, id: &str) {
        if let Some(c) = self.channels.lock().expect("channels").get_mut(id) {
            c.refused = None;
            c.stopped = false;
            c.unguarded = false;
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
        let _polling = Polling::start(self, id);
        // Antes de mirar la cola: así no se pierde un mensaje que llegue entre medias.
        let mut rx = self.arrived.subscribe();
        let deadline = tokio::time::Instant::now() + self.poll_wait();
        loop {
            // Retenida (ver `held`), se sigue esperando: quien la suelte avisa.
            let msgs = if self.held(id) {
                Vec::new()
            } else {
                self.pending(id)
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
        assert_eq!(inbox.ack("hija", &["m3".into()]), 0);
        assert_eq!(ids(&inbox.pending("otra")), ["m3"]);
        assert_eq!(inbox.ack("nadie", &["m3".into()]), 0);
        // Sin confirmar, siguen ahí.
        assert_eq!(ids(&inbox.pending("hija")), ["m1", "m2"]);
        assert_eq!(inbox.ack("hija", &["m1".into(), "zz".into()]), 1);
        assert_eq!(ids(&inbox.pending("hija")), ["m2"]);
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
        inbox.ack("hija", &["m1".into()]);
        inbox.forget("otra");
        drop(inbox);
        assert!(!path.with_extension("json.tmp").exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        // Tras el «reinicio»: lo no confirmado se vuelve a entregar.
        let again = Inbox::load(path.clone());
        assert_eq!(again.pending("hija"), vec![msg("m2", "dos")]);
        assert!(again.pending("otra").is_empty());
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
        inbox.ack("hija", &["m0".into()]);
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
        inbox.ack("hija", &["m1".into()]);
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
        assert_eq!(inbox.ack("s", &["m1".into()]), 0);
        assert_eq!(ids(&inbox.pending("s")), ["m1"]);
        // Al soltarla, la que espera lo recoge ya.
        let b = poll(&inbox);
        tokio::time::sleep(Duration::from_millis(100)).await;
        let start = Instant::now();
        drop(hold);
        assert_eq!(ids(&b.await.unwrap()), ["m1"]);
        assert!(start.elapsed() < Duration::from_millis(1000));
        assert_eq!(inbox.ack("s", &["m1".into()]), 1);
        // Igual con un rechazo, que llega con una recogida en marcha.
        let c = poll(&inbox);
        tokio::time::sleep(Duration::from_millis(100)).await;
        inbox.refuse("s", Refusal::Blocked);
        inbox.push("s", msg("m2", "dos")).unwrap();
        assert!(c.await.unwrap().is_empty());
        inbox.relaunched("s");
        assert_eq!(inbox.ack("s", &["m2".into()]), 1);
        // Parada a mano: hasta volver a lanzarla, nada (su `nido mcp` se está muriendo).
        let d = poll(&inbox);
        tokio::time::sleep(Duration::from_millis(100)).await;
        inbox.stopped("s");
        inbox.push("s", msg("m3", "tres")).unwrap();
        assert!(d.await.unwrap().is_empty());
        assert!(!inbox.channel("s"));
        inbox.relaunched("s");
        assert_eq!(ids(&inbox.poll("s").await), ["m3"]);
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
