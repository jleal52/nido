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
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio::sync::watch;

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
    /// Su pantalla dice que no tiene channel (lo pondrá la tarea 10, al ver «no MCP server
    /// configured» al arrancar). La quita la siguiente recogida.
    refused: bool,
}

/// ¿Tiene channel? Si no se sabe que no, y está recogiendo o recogió hace poco.
pub fn live(polling: usize, last: Option<Instant>, refused: bool, now: Instant) -> bool {
    !refused
        && (polling > 0 || last.is_some_and(|t| now.saturating_duration_since(t) < CHANNEL_FRESH))
}

pub struct Inbox {
    path: PathBuf,
    queues: Mutex<HashMap<String, Vec<Msg>>>,
    channels: Mutex<HashMap<String, Channel>>,
    /// Cambia con cada mensaje nuevo: despierta a las recogidas que esperan.
    arrived: watch::Sender<()>,
    /// `POLL_WAIT` en ms (las pruebas la acortan).
    wait_ms: AtomicU64,
}

impl Inbox {
    /// La de `path`; si no está o no se puede leer, vacía.
    pub fn load(path: PathBuf) -> Self {
        let queues: BTreeMap<String, Vec<Msg>> = match std::fs::read(&path) {
            Ok(b) => serde_json::from_slice(&b).unwrap_or_else(|e| {
                eprintln!("nido: no se pudo leer la bandeja {}: {e}", path.display());
                BTreeMap::new()
            }),
            Err(_) => BTreeMap::new(),
        };
        Self {
            path,
            queues: Mutex::new(queues.into_iter().collect()),
            channels: Mutex::default(),
            arrived: watch::Sender::new(()),
            wait_ms: AtomicU64::new(POLL_WAIT.as_millis() as u64),
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
    pub fn ack(&self, id: &str, ids: &[String]) -> usize {
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
            .is_some_and(|c| live(c.polling, c.last, c.refused, Instant::now()))
    }

    /// La sesión `id` no tiene channel, según su pantalla. El gancho de la tarea 10: aún no lo
    /// llama nadie.
    pub fn refuse(&self, id: &str) {
        self.channels
            .lock()
            .expect("channels")
            .entry(id.to_owned())
            .or_default()
            .refused = true;
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
            let msgs = self.pending(id);
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
        c.refused = false;
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
        // Un fichero roto: vacía, sin fallar.
        std::fs::write(&path, "{roto").unwrap();
        assert!(Inbox::load(path).pending("hija").is_empty());
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
        // La pantalla dice que no tiene; la siguiente recogida lo quita. Borrada, nada.
        inbox.refuse("hija");
        assert!(!inbox.channel("hija"));
        inbox.poll("hija").await;
        assert!(inbox.channel("hija"));
        inbox.forget("hija");
        assert!(!inbox.channel("hija"));
    }
}
