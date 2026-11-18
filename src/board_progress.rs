//! El resumen del plan de las sesiones de este equipo en sus enlaces del tablero, y el cierre y
//! la limpieza de los enlaces de las sesiones que ya no están.
//!
//! Una tarea (`spawn`) hace una pasada (`pass`) cada `TICK` y cuando la llaman (`plan_changed`,
//! `session_deleted`). Una pasada solo lee el tablero si hay algo que mirar: un plan cambiado de
//! una sesión a la que ya le toca, un cierre pendiente, una copia o un registro que no son los
//! de la última vez, o la limpieza de cada `CLEAN_EVERY`. Entonces descifra la copia una vez
//! (fuera del hilo de tokio) y mira solo los enlaces de este equipo (`session.machine` es su id
//! en el hub ahora; los de un id viejo, de otro hub o de otra cuenta, ya no son suyos):
//!
//! - De una sesión que está en el registro (aunque esté parada), abierto: si su resumen
//!   (`title`, `done`, `total`, `active`, `blocked`, `failed`) o su nombre no son los del
//!   enlace, se escriben; como mucho una vez cada `GAP` por sesión, y todos sus enlaces a la vez.
//!   Lo que cambie mientras tanto se junta: se sube lo que haya al tocarle, nunca una cola.
//! - De una sesión que no está en el registro, abierto: se cierra (`closed`), con la hora en que
//!   se borró si fue aquí (`session_deleted`, con el último resumen de su plan) o la de ahora.
//! - Cerrado hace `KEEP_CLOSED` o más: se borra.
//! - Huérfano de un objetivo borrado (su objetivo es una lápida en la copia), abierto o cerrado
//!   y sea de quien sea la sesión: se borra. Quien borra un objetivo (`board_delete`, la web, la
//!   app) borra sus enlaces, pero uno borrado desde otro sitio, o una cascada a medias, los
//!   dejaría para siempre: no salen en el tablero y cuentan para el tope de objetos de la cuenta.
//!
//! Lo que no se toca nunca: los enlaces de otros equipos, los que no se pueden leer y los
//! huérfanos de un objetivo que no está sin más (sin lápida) o no se puede leer. Un objetivo que
//! falta puede volver (un hub que se quedó sin tablero y se está resubiendo, una copia de otra
//! cuenta, una clave que llega con el grupo): borrar o cerrar sus enlaces entonces los perdería
//! para siempre. Una lápida, en cambio, es un borrado de verdad.
//!
//! Sin conexión no se hace nada y no se pierde nada: lo pendiente es «lo que hay ahora frente a
//! lo que tiene el enlace», que se mira al volver. Con un 429 se espera `BUSY_WAIT`; con otro
//! error del hub, `HUB_WAIT`. Las subidas van antes que los cierres y los borrados, y de una vez
//! se piden como mucho `MAX_WRITES` escrituras.

use std::collections::{HashMap, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::Arc;
use std::time::Duration;

use serde_json::Map;

use crate::board::{EditError, Object, PlanSummary};
use crate::group::Keyring;
use crate::hub::BoardError;
use crate::registry::now_ms;
use crate::server::AppState;

/// Lo menos que pasa entre dos subidas del resumen de una sesión (ms). El freno del hub es de
/// 3000 escrituras por hora y cuenta (decisión 6): una sesión en dos objetivos, como mucho 240.
pub const GAP: u64 = 30_000;
/// Cada cuánto se mira, como poco. Menos que `GAP`, para que la siguiente subida no espere
/// otra vuelta entera por unos milisegundos de diferencia entre los relojes.
const TICK: Duration = Duration::from_secs(10);
/// Cuánto se guarda un enlace cerrado antes de borrarlo (ms).
pub const KEEP_CLOSED: u64 = 30 * 24 * 3600 * 1000;
/// Cada cuánto se repasa el tablero aunque no haya cambiado nada, para los cerrados que
/// cumplen sus 30 días (ms).
const CLEAN_EVERY: u64 = 3600 * 1000;
/// Lo que se espera tras un 429 (ms).
const BUSY_WAIT: u64 = 60_000;
/// Lo que se espera tras otro error del hub (sin conexión a medias, un 503, sin clave): unas
/// vueltas, sin volver a leer ni a pedir en cada una (ms).
const HUB_WAIT: u64 = 30_000;
/// Lo más que se pide escribir al hub de una vez (peticiones de verdad: un `update` que choca
/// hace varias); lo demás, tras `BUSY_WAIT`. Una limpieza grande (un registro perdido cierra
/// todos los enlaces de este equipo) va así a unas 20 por minuto, como mucho unas 1200 por hora:
/// deja cupo del freno del hub para lo demás. Se cuentan todas las del nido mientras tanto, y
/// la última tarea puede pasarse en `MAX_TRIES - 1`.
const MAX_WRITES: usize = 20;

/// El estado de la tarea: está en `AppState`, pero la tarea la lanza `spawn_scanner`.
#[derive(Default)]
pub struct Progress {
    inner: std::sync::Mutex<Inner>,
    /// Que la tarea haga una pasada ya.
    wake: tokio::sync::Notify,
    /// Una pasada a la vez.
    one: tokio::sync::Mutex<()>,
    /// Cuántas veces se ha leído (descifrado) la copia.
    #[cfg(test)]
    reads: std::sync::atomic::AtomicUsize,
}

#[derive(Default)]
struct Inner {
    /// Las sesiones cuyo plan cambió y que quizá falta subir.
    dirty: HashSet<String>,
    /// Cuándo se subió por última vez lo de cada sesión (ms).
    last: HashMap<String, u64>,
    /// Las sesiones borradas aquí cuyos enlaces aún no se han cerrado.
    closing: HashMap<String, Closing>,
    /// Lo que vio el último repaso: la revisión de la copia y la huella del registro. `None`:
    /// hay que volver a repasar.
    seen: Option<(u64, u64)>,
    /// Las sesiones que tenían algún enlace abierto en el último repaso (vale mientras sea la
    /// revisión de `seen`): el plan de otra no hace leer la copia.
    linked: HashSet<String>,
    /// Cuándo toca el próximo repaso aunque no cambie nada (ms).
    next_clean: u64,
    /// Hasta cuándo no se escribe, tras un 429 (ms).
    busy_until: u64,
    /// Los enlaces que no se pudieron escribir por algo que no se arregla reintentando: su `v` y
    /// lo que se quería. Mientras no cambien, no se vuelve a probar (gastaría el freno del hub).
    stuck: HashMap<String, (i64, Job)>,
    /// El último error que se apuntó, para no repetirlo.
    said: Option<String>,
}

/// Una sesión borrada: cuándo y lo último que se sabe de su plan.
#[derive(Debug, Clone, PartialEq)]
struct Closing {
    at: u64,
    plan: LastPlan,
}

/// Lo último que se sabe del plan de una sesión borrada, para el cierre de sus enlaces.
#[derive(Debug, Clone, PartialEq)]
pub enum LastPlan {
    /// No se sabe: no se pudo leer, o la sesión no se borró aquí (no está en el registro, y ya).
    /// El resumen del enlace se deja como está.
    Unknown,
    /// No tenía plan: el resumen del enlace se quita.
    NoPlan,
    /// El resumen de su plan: va al enlace, si no es el que tiene.
    Summary(PlanSummary),
}

impl LastPlan {
    /// Lo que se pone en el enlace: `None` si se deja el que tiene; `Some(None)`, quitarlo.
    fn wanted(&self) -> Option<Option<&PlanSummary>> {
        match self {
            LastPlan::Unknown => None,
            LastPlan::NoPlan => Some(None),
            LastPlan::Summary(p) => Some(Some(p)),
        }
    }
}

/// Lo que hay que hacer con un enlace.
#[derive(Debug, Clone, PartialEq)]
enum Job {
    /// Poner el resumen del plan y el nombre de su sesión.
    Copy {
        session: String,
        plan: Option<PlanSummary>,
        name: String,
    },
    /// Cerrarlo.
    Close { session: String, closing: Closing },
    /// Borrarlo.
    Remove,
    /// Borrarlo, porque su objetivo (`objective`) está borrado.
    Orphan { objective: String },
}

impl Job {
    /// Si es lo mismo que `o`, sin mirar la hora de un cierre (la de ahora, en cada pasada).
    fn same(&self, o: &Job) -> bool {
        match (self, o) {
            (Job::Close { session: a, .. }, Job::Close { session: b, .. }) => a == b,
            _ => self == o,
        }
    }
}

#[derive(Debug)]
struct Task {
    link: String,
    v: i64,
    job: Job,
}

/// Un enlace de este equipo, legible y con su objetivo (o con su objetivo borrado: `orphan`).
#[derive(Debug)]
struct Mine {
    id: String,
    v: i64,
    session: String,
    name: String,
    plan: Option<PlanSummary>,
    closed: Option<u64>,
    /// El objetivo borrado (una lápida en la copia) del que cuelga, si es eso.
    orphan: Option<String>,
}

impl Mine {
    /// Abierto y de un objetivo que está: el que lleva el resumen del plan de su sesión.
    fn live(&self) -> bool {
        self.closed.is_none() && self.orphan.is_none()
    }
}

/// El resumen de un plan como va en el enlace.
pub fn link_plan(s: &crate::plan::Summary) -> PlanSummary {
    PlanSummary {
        title: s.title.clone(),
        done: s.done,
        total: s.total,
        active: s.active,
        blocked: s.blocked,
        failed: s.failed,
        extra: Map::new(),
    }
}

/// Si dos resúmenes dicen lo mismo (lo que no conocemos, `extra`, no cuenta).
fn same_plan(a: Option<&PlanSummary>, b: Option<&PlanSummary>) -> bool {
    let key = |p: &PlanSummary| {
        (
            p.title.clone(),
            p.done,
            p.total,
            p.active,
            p.blocked,
            p.failed,
        )
    };
    a.map(key) == b.map(key)
}

/// Pone `want` en `slot`, en el sitio: lo que no conocemos del resumen que había se queda.
fn put_plan(slot: &mut Option<PlanSummary>, want: Option<&PlanSummary>) {
    match (slot.as_mut(), want) {
        (_, None) => *slot = None,
        (Some(p), Some(w)) => {
            p.title = w.title.clone();
            p.done = w.done;
            p.total = w.total;
            p.active = w.active;
            p.blocked = w.blocked;
            p.failed = w.failed;
        }
        (None, Some(w)) => *slot = Some(w.clone()),
    }
}

/// Si a la sesión ya le toca subir (`last`: la última vez).
fn due(last: Option<&u64>, now: u64) -> bool {
    last.is_none_or(|&t| now < t || now - t >= GAP)
}

impl Progress {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().expect("resumen del tablero")
    }

    /// Ha cambiado el plan de la sesión `id`.
    pub fn plan_changed(&self, id: &str) {
        self.lock().dirty.insert(id.to_owned());
        self.wake.notify_one();
    }

    /// Se ha borrado la sesión `id` a las `at` (ms): sus enlaces se cierran, con `plan` (lo
    /// último que se sabe de su plan) si cambió.
    pub fn session_deleted(&self, id: &str, at: u64, plan: LastPlan) {
        let mut i = self.lock();
        i.dirty.remove(id);
        i.last.remove(id);
        i.closing.insert(id.to_owned(), Closing { at, plan });
        drop(i);
        self.wake.notify_one();
    }

    /// Si hay que repasar el tablero.
    fn wanted(&self, now: u64, seen: (u64, u64)) -> bool {
        let i = self.lock();
        if now < i.busy_until {
            return false;
        }
        !i.closing.is_empty()
            || i.seen != Some(seen)
            || now >= i.next_clean
            || i.dirty
                .iter()
                .any(|s| i.linked.contains(s) && due(i.last.get(s), now))
    }

    /// Fuera los cierres pendientes de hace `KEEP_CLOSED` o más. Sus enlaces, si quedan abiertos,
    /// se cierran igual por no estar la sesión en el registro, pero con la hora de ahora y sin el
    /// último resumen, y se borran `KEEP_CLOSED` después de ese cierre.
    fn expire(&self, now: u64) {
        self.lock()
            .closing
            .retain(|_, c| now.saturating_sub(c.at) < KEEP_CLOSED);
    }

    /// Qué hacer con cada enlace de este equipo (`mine`), con los resúmenes de ahora (`plans`:
    /// `Err` si el plan no se pudo leer; una sesión que falta es que no se miró, porque llegó al
    /// registro mientras se leía) y las sesiones del registro (`names`, por id, con su nombre en
    /// el tablero). Las sesiones a las que aún no les toca, o cuyo plan no se pudo leer o mirar,
    /// se quedan pendientes.
    fn decide(
        &self,
        mine: &[Mine],
        plans: &HashMap<String, Plan>,
        names: &HashMap<String, String>,
        now: u64,
    ) -> Vec<Task> {
        let mut i = self.lock();
        let mut tasks = vec![];
        let (mut later, mut unread) = (HashSet::new(), HashSet::new());
        for m in mine {
            let job = if let Some(objective) = &m.orphan {
                Job::Orphan {
                    objective: objective.clone(),
                }
            } else if let Some(t) = m.closed {
                if now.saturating_sub(t) < KEEP_CLOSED {
                    continue;
                }
                Job::Remove
            } else if let Some(name) = names.get(&m.session) {
                let plan = match plans.get(&m.session) {
                    Some(Ok(p)) => p,
                    Some(Err(())) => {
                        unread.insert(m.session.clone());
                        continue;
                    }
                    None => {
                        later.insert(m.session.clone());
                        continue;
                    }
                };
                if same_plan(m.plan.as_ref(), plan.as_ref()) && m.name == *name {
                    continue;
                }
                if !due(i.last.get(&m.session), now) {
                    later.insert(m.session.clone());
                    continue;
                }
                Job::Copy {
                    session: m.session.clone(),
                    plan: plan.clone(),
                    name: name.clone(),
                }
            } else {
                let closing = i.closing.get(&m.session).cloned().unwrap_or(Closing {
                    at: now,
                    plan: LastPlan::Unknown,
                });
                Job::Close {
                    session: m.session.clone(),
                    closing,
                }
            };
            if i.stuck
                .get(&m.id)
                .is_some_and(|(v, j)| *v == m.v && j.same(&job))
            {
                continue;
            }
            tasks.push(Task {
                link: m.id.clone(),
                v: m.v,
                job,
            });
        }
        // Un plan que no se pudo leer se vuelve a probar a su ritmo, no en cada vuelta.
        for s in &unread {
            i.last.insert(s.clone(), now);
        }
        i.dirty.extend(later.into_iter().chain(unread));
        // Lo de las sesiones que ya no están, fuera.
        i.last.retain(|s, _| names.contains_key(s));
        let ids: HashSet<&str> = mine.iter().map(|m| m.id.as_str()).collect();
        i.stuck.retain(|l, _| ids.contains(l.as_str()));
        tasks
    }

    /// Que no se escriba hasta `t` (ms), sin acortar una espera más larga que ya hubiera.
    fn wait_until(&self, t: u64) {
        let mut i = self.lock();
        i.busy_until = i.busy_until.max(t);
    }

    fn say(&self, what: String) {
        let mut i = self.lock();
        if i.said.as_deref() != Some(what.as_str()) {
            eprintln!("nido: tablero: {what}");
            i.said = Some(what);
        }
    }
}

/// El resumen de ahora del plan de una sesión (`None`: no tiene), o `Err` si no se pudo leer.
type Plan = Result<Option<PlanSummary>, ()>;

/// Los enlaces de este equipo (`machine`) que se pueden leer y tienen su objetivo o lo tienen
/// borrado (una lápida en la copia: `orphan`), y los resúmenes de ahora de las sesiones de
/// `sessions` que tienen alguno abierto (de un objetivo que está). Descifra la copia entera y
/// puede leer planes del disco: fuera del hilo de tokio.
fn read_mine(
    st: &AppState,
    keys: &Keyring,
    machine: &str,
    sessions: &HashSet<String>,
) -> (Vec<Mine>, HashMap<String, Plan>) {
    let d = st.board.decrypted(keys);
    let objectives: HashSet<&str> = d
        .objects
        .iter()
        .filter(|e| matches!(e.object, Object::Objective(_)))
        .map(|e| e.id.as_str())
        .collect();
    let buried = |id: &str| st.board.item(id).is_some_and(|s| s.gone);
    let mine: Vec<Mine> = d
        .objects
        .iter()
        .filter_map(|e| match &e.object {
            Object::Link(l) if l.session.machine == machine => {
                let orphan = if objectives.contains(l.objective.as_str()) {
                    None
                } else if buried(&l.objective) {
                    Some(l.objective.clone())
                } else {
                    return None;
                };
                Some(Mine {
                    id: e.id.clone(),
                    v: e.v,
                    session: l.session.id.clone(),
                    name: l.session.name.clone(),
                    plan: l.plan.clone(),
                    closed: l.closed,
                    orphan,
                })
            }
            _ => None,
        })
        .collect();
    let mut plans = HashMap::new();
    for m in mine.iter().filter(|m| m.live()) {
        if plans.contains_key(&m.session) || !sessions.contains(&m.session) {
            continue;
        }
        let plan = st.plans.try_summary(&m.session).map_err(drop);
        plans.insert(m.session.clone(), plan.map(|s| s.as_ref().map(link_plan)));
    }
    (mine, plans)
}

/// Las sesiones del registro, por id, con su nombre en el tablero, y su huella.
async fn registry(st: &AppState, host: &str) -> (HashMap<String, String>, u64) {
    let mut list: Vec<(String, String)> = st
        .registry
        .lock()
        .await
        .sessions()
        .iter()
        .map(|s| (s.id.clone(), crate::board_tools::label(host, &s.name)))
        .collect();
    list.sort();
    let mut h = DefaultHasher::new();
    list.hash(&mut h);
    (list.into_iter().collect(), h.finish())
}

/// Lo que se escribe en el enlace: solo si sigue siendo de la sesión (tras un 409 se vuelve a
/// aplicar sobre el de ahora). El resumen y el nombre, en uno abierto; `copied` solo si cambia
/// el resumen. `changed` y `by` no: no es un cambio de nadie, es una copia (`copied` dice
/// cuándo).
fn copy(
    o: &mut Object,
    machine: &str,
    session: &str,
    plan: Option<&PlanSummary>,
    name: &str,
    now: u64,
) {
    if let Object::Link(l) = o
        && l.session.machine == machine
        && l.session.id == session
        && l.closed.is_none()
    {
        if !same_plan(l.plan.as_ref(), plan) {
            put_plan(&mut l.plan, plan);
            l.copied = now;
        }
        name.clone_into(&mut l.session.name);
    }
}

/// Cierra el enlace (si sigue abierto y es de la sesión), con el último resumen si se sabe. Esto
/// sí es un cambio: `changed` es ahora y `by`, la sesión que se cerró.
fn close(o: &mut Object, machine: &str, session: &str, c: &Closing, now: u64) {
    if let Object::Link(l) = o
        && l.session.machine == machine
        && l.session.id == session
        && l.closed.is_none()
    {
        l.closed = Some(c.at);
        if let Some(plan) = c.plan.wanted()
            && !same_plan(l.plan.as_ref(), plan)
        {
            put_plan(&mut l.plan, plan);
            l.copied = c.at;
        }
        l.changed = now;
        l.by = l.session.name.clone();
    }
}

/// Una pasada a la hora `now` (ms). Ver el principio del módulo.
pub async fn pass(st: &Arc<AppState>, now: u64) {
    let p = &st.progress;
    let _one = p.one.lock().await;
    p.expire(now);
    let machine = {
        let s = st.hub.status.borrow();
        if !s.connected {
            return;
        }
        s.id.clone()
    };
    let Some(machine) = machine else { return };
    let host = crate::e2e::Host::host_name(st.as_ref());
    let rev = *st.board.subscribe().borrow();
    let (before, fp) = registry(st, &host).await;
    if !p.wanted(now, (rev, fp)) {
        return;
    }
    let keys = st.group.board_keys();
    if keys.current().is_none() {
        // Sin clave no hay tablero que leer ni escribir (la crea quien escribe el primero).
        return;
    }
    // Lo que cambie desde aquí se queda para la próxima.
    let taken = std::mem::take(&mut p.lock().dirty);
    #[cfg(test)]
    p.reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let read = {
        let (st, keys, machine) = (st.clone(), keys.clone(), machine.clone());
        let sessions: HashSet<String> = before.into_keys().collect();
        tokio::task::spawn_blocking(move || read_mine(&st, &keys, &machine, &sessions)).await
    };
    let Ok((mine, plans)) = read else {
        p.lock().dirty.extend(taken);
        return;
    };
    // El registro, después de leer: una sesión que se borra entretanto ya no está (y no se le
    // sube un plan vacío), y una que se crea y se une entretanto ya está (y no se cierra).
    let (names, _) = registry(st, &host).await;
    let mut tasks = p.decide(&mine, &plans, &names, now);
    // Primero las subidas: no esperan tras una limpieza grande (orden estable).
    tasks.sort_by_key(|t| !matches!(t.job, Job::Copy { .. }));
    {
        let mut i = p.lock();
        i.seen = Some((rev, fp));
        i.next_clean = now + CLEAN_EVERY;
        // Las borradas que ya no tienen enlaces abiertos, hechas (o no tenían ninguno). Una que
        // se borra mientras tanto aún los tiene: se queda.
        let open: HashSet<String> = mine
            .iter()
            .filter(|m| m.live())
            .map(|m| m.session.clone())
            .collect();
        i.closing.retain(|s, _| open.contains(s));
        i.linked = open;
    }
    run(st, &keys, &machine, tasks, now).await;
}

/// Hace lo decidido, en orden. Un error que se puede arreglar esperando deja lo que falte para
/// la próxima pasada; con el hub caído o un 429, se para.
async fn run(st: &Arc<AppState>, keys: &Keyring, machine: &str, tasks: Vec<Task>, now: u64) {
    let p = &st.progress;
    let (mut again, mut closed, mut removed, mut orphans) = (false, 0, 0, 0);
    let start = st.hub.board_writes();
    for t in tasks {
        if st.hub.board_writes().saturating_sub(start) >= MAX_WRITES as u64 {
            p.wait_until(now + BUSY_WAIT);
            again = true;
            break;
        }
        let r = match &t.job {
            Job::Copy {
                session,
                plan,
                name,
            } => st
                .board
                .update(keys, &t.link, |o| {
                    copy(o, machine, session, plan.as_ref(), name, now)
                })
                .await
                .map(drop),
            Job::Close { session, closing } => st
                .board
                .update(keys, &t.link, |o| close(o, machine, session, closing, now))
                .await
                .map(drop),
            Job::Remove => {
                // Solo si sigue siendo el que se leyó y, tras cada choque, si sigue cerrado hace
                // 30 días: uno que se vuelve a abrir (`join`) entretanto no se borra.
                let same = st.board.item(&t.link).map(|s| s.v) == Some(t.v);
                let gone = |o: &Object| {
                    matches!(o, Object::Link(l) if l.session.machine == machine
                        && l.closed.is_some_and(|c| now.saturating_sub(c) >= KEEP_CLOSED))
                };
                let r = if same {
                    st.board.remove_when(keys, &t.link, gone).await
                } else {
                    Ok(false)
                };
                match r {
                    Ok(true) => Ok(()),
                    Ok(false) => {
                        again = true;
                        continue;
                    }
                    Err(e) => Err(e),
                }
            }
            Job::Orphan { objective } => {
                // Solo si sigue siendo el que se leyó, su objetivo sigue borrado y, tras cada
                // choque, si sigue siendo de este equipo y de ese objetivo: uno que otro pasa a
                // otro objetivo entretanto no se borra.
                let same = st.board.item(&t.link).map(|s| s.v) == Some(t.v);
                let buried = st.board.item(objective).is_some_and(|s| s.gone);
                let still = |o: &Object| {
                    matches!(o, Object::Link(l) if l.session.machine == machine
                        && l.objective == *objective)
                };
                let r = if same && buried {
                    st.board.remove_when(keys, &t.link, still).await
                } else {
                    Ok(false)
                };
                match r {
                    Ok(true) => Ok(()),
                    Ok(false) => {
                        again = true;
                        continue;
                    }
                    Err(e) => Err(e),
                }
            }
        };
        // Si hay que parar y si falta algo (que la próxima pasada vuelva a mirar).
        let (stop, pending) = match r {
            Ok(()) | Err(EditError::Missing) => {
                match &t.job {
                    Job::Copy { session, .. } => {
                        p.lock().last.insert(session.clone(), now);
                    }
                    Job::Close { .. } => closed += 1,
                    Job::Remove => removed += 1,
                    Job::Orphan { .. } => orphans += 1,
                }
                (false, false)
            }
            Err(EditError::Board(BoardError::Busy)) => {
                p.wait_until(now + BUSY_WAIT);
                (true, true)
            }
            Err(e @ (EditError::Board(BoardError::Hub(_)) | EditError::NoKey)) => {
                p.say(format!("no se pudieron poner al día los enlaces ({e})"));
                p.wait_until(now + HUB_WAIT);
                (true, true)
            }
            // Choca una y otra vez: en la próxima.
            Err(EditError::Board(BoardError::Conflict(_))) => (false, true),
            Err(e) => {
                p.say(format!(
                    "el enlace {} no se pudo poner al día ({e})",
                    t.link
                ));
                let mut i = p.lock();
                i.stuck.insert(t.link.clone(), (t.v, t.job.clone()));
                // No se va a cerrar: que no se repase en cada vuelta por él.
                if let Job::Close { session, .. } = &t.job {
                    i.closing.remove(session);
                }
                (false, false)
            }
        };
        again |= pending;
        if stop {
            break;
        }
    }
    {
        let mut i = p.lock();
        if again {
            i.seen = None;
        } else {
            i.said = None;
        }
    }
    if closed > 0 {
        eprintln!("nido: tablero: {closed} enlaces de sesiones que ya no están, cerrados");
    }
    if removed > 0 {
        eprintln!("nido: tablero: {removed} enlaces de sesiones cerradas hace 30 días, fuera");
    }
    if orphans > 0 {
        eprintln!("nido: tablero: {orphans} enlaces de objetivos borrados, fuera");
    }
}

/// La tarea: una pasada cada `TICK` y cada vez que la llaman.
pub fn spawn(st: Arc<AppState>) {
    spawn_with(st, TICK);
}

/// `spawn`, cada `every` (para las pruebas).
pub fn spawn_with(st: Arc<AppState>, every: Duration) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(every);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = tick.tick() => {}
                _ = st.progress.wake.notified() => {}
            }
            pass(&st, now_ms()).await;
        }
    });
}

#[cfg(test)]
impl Progress {
    /// Como tras reiniciar nido: sin nada apuntado.
    fn forget_all(&self) {
        *self.lock() = Inner::default();
    }

    /// Si ya se ha leído la copia alguna vez.
    fn has_read(&self) -> bool {
        self.lock().seen.is_some()
    }

    fn reads(&self) -> usize {
        self.reads.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn closing_len(&self) -> usize {
        self.lock().closing.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::board;
    use crate::board_tools::tests::{World, session, state, world};
    use crate::registry::Session;
    use serde_json::{Value, json};

    /// Una hora cualquiera (ms).
    const T0: u64 = 1_800_000_000_000;
    const DAY: u64 = 24 * 3600 * 1000;

    fn limit() -> crate::watchdog::Limit {
        crate::watchdog::limit(Duration::from_secs(60))
    }

    /// Pone a la sesión `id` un plan de una fase con tareas en estos estados.
    fn set_plan(w: &World, id: &str, title: &str, states: &[&str]) {
        let tasks: Vec<Value> = states
            .iter()
            .enumerate()
            .map(|(n, s)| json!({"id": format!("t{n}"), "name": format!("tarea {n}"), "status": s}))
            .collect();
        let a = serde_json::from_value(
            json!({"title": title, "phases": [{"id": "f1", "name": "Uno", "tasks": tasks}]}),
        )
        .unwrap();
        w.st.plans.set(id, a, 1).unwrap();
        crate::server::plan_changed(&w.st, id);
    }

    /// Un proyecto «P» con un objetivo «O»: el id del objetivo.
    async fn goal(w: &World) -> String {
        w.call("project", json!({"name": "P"})).await.unwrap();
        let r = w
            .call("objective", json!({"project": "P", "name": "O"}))
            .await
            .unwrap();
        r["id"].as_str().unwrap().to_owned()
    }

    /// `s` se une a «O»: el id del enlace.
    async fn join(w: &World, s: &Session) -> String {
        let r = w
            .call_as(s, "join", json!({"objective": "O"}))
            .await
            .unwrap();
        r["link"].as_str().unwrap().to_owned()
    }

    /// Otro cliente escribe un enlace al objetivo `o` de la sesión `session` del equipo
    /// `machine`, con lo de `more` encima.
    fn put_link(w: &World, id: &str, o: &str, machine: &str, session: &str, more: Value) {
        let mut v = json!({"t": "link", "objective": o,
            "session": {"machine": machine, "id": session, "name": "Otro/vieja"},
            "plan": null, "copied": 0, "closed": null, "created": 1, "changed": 1, "by": "web"});
        for (k, x) in more.as_object().unwrap() {
            v[k] = x.clone();
        }
        w.put(id, v);
    }

    fn summary(title: &str, done: u64, total: u64, active: u64) -> Value {
        json!({"title": title, "done": done, "total": total, "active": active, "blocked": 0,
            "failed": 0})
    }

    fn items(w: &World) -> std::collections::BTreeMap<String, crate::hub::BoardItem> {
        w.board.lock().unwrap().items.clone()
    }

    /// El primer cambio sube enseguida; los que llegan en los 30 s siguientes se juntan y sube
    /// solo el último, al cumplirse. Sin cambios, nada.
    #[tokio::test]
    async fn two_changes_in_30_s_upload_one() {
        let _limit = limit();
        let w = world().await;
        goal(&w).await;
        let l = join(&w, &w.s).await;
        set_plan(&w, &w.s.id, "Migrar", &["done", "in_progress", "pending"]);
        let n = w.writes();
        pass(&w.st, T0).await;
        assert_eq!(w.writes(), n + 1);
        let v = w.stored(&l).unwrap();
        assert_eq!(v["plan"], summary("Migrar", 1, 3, 1));
        assert_eq!(v["copied"], T0);
        assert_eq!(
            (&v["changed"], &v["by"]),
            (&w.stored(&l).unwrap()["created"], &json!(w.by(&w.s)))
        );

        set_plan(&w, &w.s.id, "Migrar", &["done", "done", "pending"]);
        pass(&w.st, T0 + 10_000).await;
        set_plan(&w, &w.s.id, "Migrar", &["done", "done", "in_progress"]);
        pass(&w.st, T0 + 29_999).await;
        assert_eq!(w.writes(), n + 1, "aún no le toca");
        pass(&w.st, T0 + GAP).await;
        assert_eq!(w.writes(), n + 2);
        let v = w.stored(&l).unwrap();
        assert_eq!(v["plan"], summary("Migrar", 2, 3, 1));
        assert_eq!(v["copied"], T0 + GAP);

        // Sin cambios, ni con la copia nueva ni pasado el tiempo.
        pass(&w.st, T0 + 2 * GAP).await;
        pass(&w.st, T0 + DAY).await;
        assert_eq!(w.writes(), n + 2);
    }

    /// Un cambio del plan que no toca el resumen (una nota) no sube.
    #[tokio::test]
    async fn a_change_outside_the_summary_does_not_upload() {
        let _limit = limit();
        let w = world().await;
        goal(&w).await;
        join(&w, &w.s).await;
        set_plan(&w, &w.s.id, "Migrar", &["done", "in_progress"]);
        pass(&w.st, T0).await;
        let n = w.writes();
        let t =
            serde_json::from_value(json!({"id": "t1", "status": "in_progress", "note": "va bien"}))
                .unwrap();
        w.st.plans.update_task(&w.s.id, t, 2).unwrap();
        crate::server::plan_changed(&w.st, &w.s.id);
        pass(&w.st, T0 + 2 * GAP).await;
        assert_eq!(w.writes(), n);
    }

    /// Una sesión sin enlaces no sube nada, aunque cambie su plan.
    #[tokio::test]
    async fn a_session_without_links_uploads_nothing() {
        let _limit = limit();
        let w = world().await;
        let o = goal(&w).await;
        // El enlace de otra sesión de aquí no es suyo.
        let other =
            w.st.registry
                .lock()
                .await
                .add(session(w.tmp.path(), "otra"))
                .unwrap();
        let l = join(&w, &other).await;
        set_plan(&w, &w.s.id, "Migrar", &["done"]);
        let n = w.writes();
        pass(&w.st, T0).await;
        pass(&w.st, T0 + GAP).await;
        assert_eq!(w.writes(), n);
        assert_eq!(w.stored(&l).unwrap()["objective"], o.as_str());
        assert_eq!(w.stored(&l).unwrap()["plan"], Value::Null);
    }

    /// Tras reiniciar (sin nada apuntado), lo que ya está en el enlace no se vuelve a subir.
    #[tokio::test]
    async fn what_the_link_already_has_is_not_uploaded_again() {
        let _limit = limit();
        let w = world().await;
        goal(&w).await;
        join(&w, &w.s).await;
        set_plan(&w, &w.s.id, "Migrar", &["done", "pending"]);
        pass(&w.st, T0).await;
        let n = w.writes();
        w.st.progress.forget_all();
        crate::server::plan_changed(&w.st, &w.s.id);
        pass(&w.st, T0 + 1).await;
        assert_eq!(w.writes(), n);
    }

    /// Al borrar la sesión, sus enlaces se cierran con la hora del borrado y el último resumen
    /// de su plan (aunque aún no tocara subirlo).
    #[tokio::test]
    async fn deleting_the_session_closes_its_links() {
        let _limit = limit();
        let w = world().await;
        let o = goal(&w).await;
        w.call("objective", json!({"project": "P", "name": "O2"}))
            .await
            .unwrap();
        let l1 = join(&w, &w.s).await;
        let l2 = w.call("join", json!({"objective": "O2"})).await.unwrap()["link"]
            .as_str()
            .unwrap()
            .to_owned();
        set_plan(&w, &w.s.id, "Migrar", &["done", "pending"]);
        // Con el reloj de verdad: el cierre lleva la hora del borrado.
        let t0 = now_ms();
        pass(&w.st, t0).await;
        // Los dos enlaces, a la vez.
        for l in [&l1, &l2] {
            assert_eq!(w.stored(l).unwrap()["plan"], summary("Migrar", 1, 2, 0));
        }
        set_plan(&w, &w.s.id, "Migrar", &["done", "done"]);
        let before = now_ms();
        crate::server::delete_session(&w.st, &w.s.id).await.unwrap();
        let after = now_ms();
        pass(&w.st, t0 + 1000).await;
        for l in [&l1, &l2] {
            let v = w.stored(l).unwrap();
            let closed = v["closed"].as_u64().expect("cerrado");
            assert!((before..=after).contains(&closed), "{v}");
            assert_eq!(v["plan"], summary("Migrar", 2, 2, 0));
            assert_eq!(v["copied"], closed);
            assert_eq!(v["changed"], t0 + 1000);
            assert_eq!(v["session"]["name"], w.by(&w.s));
            assert_eq!(v["by"], w.by(&w.s));
        }
        assert_eq!(w.stored(&l1).unwrap()["objective"], o.as_str());
        // Hecho: nada más (hasta los 30 días).
        let n = w.writes();
        pass(&w.st, t0 + 2000).await;
        pass(&w.st, t0 + DAY).await;
        assert_eq!(w.writes(), n);
    }

    /// Un enlace cerrado de este equipo se borra a los 30 días; antes no.
    #[tokio::test]
    async fn after_30_days_a_closed_link_goes() {
        let _limit = limit();
        let w = world().await;
        let o = goal(&w).await;
        let l = "1".repeat(32);
        put_link(&w, &l, &o, "este", "dead0000", json!({"closed": T0}));
        // Uno de una sesión que sigue (`join` lo vuelve a abrir): también.
        let mine = "2".repeat(32);
        put_link(&w, &mine, &o, "este", &w.s.id, json!({"closed": T0}));
        w.st.board.sync().await.unwrap();
        let n = w.writes();
        pass(&w.st, T0 + KEEP_CLOSED - CLEAN_EVERY).await;
        assert_eq!(w.writes(), n);
        assert!(w.stored(&l).is_some());
        pass(&w.st, T0 + KEEP_CLOSED).await;
        assert!(w.stored(&l).is_none(), "borrado");
        assert!(w.stored(&mine).is_none(), "borrado");
        assert!(items(&w)[&l].gone);
    }

    /// Una sesión que ya no está en el registro: primero se cierra (ahora) y se borra a los 30
    /// días. Una parada sigue en el registro: no se toca.
    #[tokio::test]
    async fn links_of_sessions_that_are_gone_are_closed_then_removed() {
        let _limit = limit();
        let w = world().await;
        let o = goal(&w).await;
        let mut stopped = session(w.tmp.path(), "parada");
        stopped.stopped = 5;
        let stopped = w.st.registry.lock().await.add(stopped).unwrap();
        let ls = join(&w, &stopped).await;
        let gone = "1".repeat(32);
        put_link(&w, &gone, &o, "este", "dead0000", json!({}));
        w.st.board.sync().await.unwrap();
        pass(&w.st, T0).await;
        let v = w.stored(&gone).unwrap();
        assert_eq!(v["closed"], T0);
        assert_eq!(v["plan"], Value::Null, "sin plan que copiar");
        assert_eq!(v["by"], "Otro/vieja");
        assert_eq!(w.stored(&ls).unwrap()["closed"], Value::Null);

        pass(&w.st, T0 + KEEP_CLOSED - CLEAN_EVERY).await;
        assert!(w.stored(&gone).is_some());
        pass(&w.st, T0 + KEEP_CLOSED).await;
        assert!(w.stored(&gone).is_none());
        assert_eq!(w.stored(&ls).unwrap()["closed"], Value::Null);
    }

    /// Los enlaces de otros equipos, o de este con otro id en el hub (otro hub, otra cuenta), no
    /// se tocan: ni el resumen, ni el cierre, ni el borrado.
    #[tokio::test]
    async fn links_of_other_machines_are_not_touched() {
        let _limit = limit();
        let w = world().await;
        let o = goal(&w).await;
        put_link(&w, &"1".repeat(32), &o, "mac", &w.s.id, json!({}));
        put_link(&w, &"2".repeat(32), &o, "viejo", "dead0000", json!({}));
        put_link(
            &w,
            &"3".repeat(32),
            &o,
            "mac",
            "dead0000",
            json!({"closed": 1}),
        );
        set_plan(&w, &w.s.id, "Migrar", &["done"]);
        w.st.board.sync().await.unwrap();
        let (before, n) = (items(&w), w.writes());
        pass(&w.st, T0).await;
        pass(&w.st, T0 + KEEP_CLOSED).await;
        assert_eq!(w.writes(), n);
        assert_eq!(items(&w), before);
    }

    /// Los enlaces de este equipo cuyo objetivo se borró de verdad (es una lápida en la copia) se
    /// borran, abiertos o cerrados y sea de quien sea la sesión. Los de otro equipo, no; ni los de
    /// un objetivo que no está sin más (puede volver).
    #[tokio::test]
    async fn links_of_a_deleted_objective_are_removed() {
        let _limit = limit();
        let w = world().await;
        let o = goal(&w).await;
        let mine = join(&w, &w.s).await;
        let (closed, gone, theirs) = ("1".repeat(32), "2".repeat(32), "3".repeat(32));
        put_link(&w, &closed, &o, "este", &w.s.id, json!({"closed": T0}));
        put_link(&w, &gone, &o, "este", "dead0000", json!({}));
        put_link(&w, &theirs, &o, "mac", "dead0000", json!({}));
        // Uno de un objetivo que no está (sin lápida): se queda.
        let nowhere = "4".repeat(32);
        put_link(&w, &nowhere, &"9".repeat(32), "este", &w.s.id, json!({}));
        set_plan(&w, &w.s.id, "Migrar", &["done"]);
        // Otro (la web, la app, una cascada a medias) borra el objetivo y no sus enlaces.
        w.board.lock().unwrap().write(&o, None);
        w.st.board.sync().await.unwrap();
        let n = w.writes();
        pass(&w.st, T0).await;
        for l in [&mine, &closed, &gone] {
            assert!(items(&w)[l].gone, "{l}");
        }
        // Solo los borrados: ni el resumen ni el cierre se escriben antes.
        assert_eq!(w.writes(), n + 3);
        assert!(w.stored(&theirs).is_some());
        assert!(w.stored(&nowhere).is_some());
        let (before, n) = (items(&w), w.writes());
        pass(&w.st, T0 + GAP).await;
        pass(&w.st, T0 + KEEP_CLOSED).await;
        assert_eq!(w.writes(), n);
        assert_eq!(items(&w), before);
    }

    /// Si el enlace ya no estaba (otro lo borró antes, o el hub no lo tiene), el borrado choca
    /// (409) una vez y cuenta como hecho: no se vuelve a pedir en cada pasada.
    #[tokio::test]
    async fn an_orphan_already_gone_is_not_retried() {
        let _limit = limit();
        let w = world().await;
        let o = goal(&w).await;
        let (deleted, lost) = ("1".repeat(32), "2".repeat(32));
        put_link(&w, &deleted, &o, "este", &w.s.id, json!({}));
        put_link(&w, &lost, &o, "este", "dead0000", json!({}));
        w.board.lock().unwrap().write(&o, None);
        w.st.board.sync().await.unwrap();
        // El hub ya no los tiene como los tiene la copia: uno lo borró otro, y el otro no está.
        w.board.lock().unwrap().write(&deleted, None);
        w.board.lock().unwrap().items.remove(&lost);
        let n = w.writes();
        pass(&w.st, T0).await;
        assert_eq!(w.writes(), n + 2, "un intento cada uno");
        assert!(w.st.board.item(&deleted).unwrap().gone);
        assert!(w.st.board.item(&lost).is_none());
        pass(&w.st, T0 + GAP).await;
        pass(&w.st, T0 + 2 * GAP).await;
        pass(&w.st, T0 + KEEP_CLOSED).await;
        assert_eq!(w.writes(), n + 2);
    }

    /// Los huérfanos (su objetivo no está o no se puede leer) y los que no se pueden leer no se
    /// tocan.
    #[tokio::test]
    async fn orphans_and_unreadable_links_are_not_touched() {
        let _limit = limit();
        let w = world().await;
        let o = goal(&w).await;
        let nowhere = "9".repeat(32);
        put_link(&w, &"1".repeat(32), &nowhere, "este", &w.s.id, json!({}));
        put_link(&w, &"2".repeat(32), &nowhere, "este", "dead0000", json!({}));
        put_link(
            &w,
            &"3".repeat(32),
            &nowhere,
            "este",
            "dead0000",
            json!({"closed": 1}),
        );
        // Un objetivo con una clave que no está en el llavero, y su enlace.
        let other = [9u8; 32];
        let hidden = "8".repeat(32);
        let blob = board::seal(
            &other,
            &hidden,
            br#"{"t":"objective","project":"x","name":"O3","state":"doing","created":1,"changed":1}"#,
        )
        .unwrap();
        w.board
            .lock()
            .unwrap()
            .write(&hidden, Some((&board::kid(&other), &blob)));
        put_link(&w, &"4".repeat(32), &hidden, "este", &w.s.id, json!({}));
        // Un enlace con esa clave, y uno que no es un enlace (sin `session`).
        let text = json!({"t": "link", "objective": o,
            "session": {"machine": "este", "id": w.s.id, "name": "x"}, "created": 1,
            "changed": 1});
        let id = "5".repeat(32);
        let blob = board::seal(&other, &id, text.to_string().as_bytes()).unwrap();
        w.board
            .lock()
            .unwrap()
            .write(&id, Some((&board::kid(&other), &blob)));
        w.put(
            &"6".repeat(32),
            json!({"t": "link", "objective": o, "created": 1, "changed": 1}),
        );
        set_plan(&w, &w.s.id, "Migrar", &["done"]);
        w.st.board.sync().await.unwrap();
        let (before, n) = (items(&w), w.writes());
        pass(&w.st, T0).await;
        pass(&w.st, T0 + KEEP_CLOSED).await;
        assert_eq!(w.writes(), n);
        assert_eq!(items(&w), before);
    }

    /// Se escribe en el enlace leído: lo que no conocemos (del enlace, de su sesión y de su
    /// resumen) se queda, y `changed` y `by` no cambian.
    #[tokio::test]
    async fn what_we_do_not_know_is_kept() {
        let _limit = limit();
        let w = world().await;
        let o = goal(&w).await;
        let l = "1".repeat(32);
        put_link(
            &w,
            &l,
            &o,
            "este",
            &w.s.id,
            json!({"futuro": {"a": 1},
                "session": {"machine": "este", "id": w.s.id, "name": "viejo", "apodo": "x"},
                "plan": {"title": "a", "done": 0, "total": 1, "active": 0, "blocked": 0,
                    "failed": 0, "color": "rojo"},
                "copied": 5, "changed": 2}),
        );
        w.st.board.sync().await.unwrap();
        set_plan(&w, &w.s.id, "Migrar", &["done", "in_progress"]);
        pass(&w.st, T0).await;
        let v = w.stored(&l).unwrap();
        assert_eq!(v["futuro"], json!({"a": 1}));
        assert_eq!(
            v["session"],
            json!({"machine": "este", "id": w.s.id, "name": w.by(&w.s), "apodo": "x"})
        );
        let mut want = summary("Migrar", 1, 2, 1);
        want["color"] = "rojo".into();
        assert_eq!(v["plan"], want);
        assert_eq!(
            (&v["copied"], &v["changed"], &v["by"]),
            (&json!(T0), &json!(2), &json!("web"))
        );
    }

    /// Si la sesión cambia de nombre, sus enlaces también (sin tocar `copied`: el resumen es el
    /// mismo).
    #[tokio::test]
    async fn a_renamed_session_renames_its_links() {
        let _limit = limit();
        let w = world().await;
        goal(&w).await;
        let l = join(&w, &w.s).await;
        let n = w.writes();
        pass(&w.st, T0).await;
        assert_eq!(w.writes(), n, "sin plan y con su nombre: nada");
        let renamed =
            w.st.registry
                .lock()
                .await
                .update(&w.s.id, |s| s.name = "nueva".into())
                .unwrap();
        pass(&w.st, T0 + 1000).await;
        let v = w.stored(&l).unwrap();
        assert_eq!(v["session"]["name"], w.by(&renamed));
        assert_eq!((&v["plan"], &v["copied"]), (&Value::Null, &json!(0)));
        assert_eq!(w.writes(), n + 1);
    }

    /// Sin conexión no se sube nada; al volver, solo lo último.
    #[tokio::test]
    async fn offline_only_the_last_one_goes_up_on_return() {
        let _limit = limit();
        let w = world().await;
        goal(&w).await;
        let l = join(&w, &w.s).await;
        set_plan(&w, &w.s.id, "Migrar", &["pending", "pending"]);
        pass(&w.st, T0).await;
        let mut status = w.st.hub.status.subscribe();
        w.rec.tell(crate::hub::fake::Tell::Hangup);
        tokio::time::timeout(Duration::from_secs(10), status.wait_for(|s| !s.connected))
            .await
            .unwrap()
            .unwrap();
        set_plan(&w, &w.s.id, "Migrar", &["done", "pending"]);
        pass(&w.st, T0 + GAP).await;
        set_plan(&w, &w.s.id, "Migrar", &["done", "done"]);
        pass(&w.st, T0 + 2 * GAP).await;
        let n = w.writes();
        tokio::time::timeout(Duration::from_secs(10), status.wait_for(|s| s.connected))
            .await
            .unwrap()
            .unwrap();
        pass(&w.st, T0 + 3 * GAP).await;
        assert_eq!(w.writes(), n + 1);
        assert_eq!(w.stored(&l).unwrap()["plan"], summary("Migrar", 2, 2, 0));
    }

    /// Borrada sin conexión: el cierre queda pendiente, con la hora del borrado, y se hace al
    /// volver.
    #[tokio::test]
    async fn a_session_deleted_offline_is_closed_on_return() {
        let _limit = limit();
        let w = world().await;
        goal(&w).await;
        let l = join(&w, &w.s).await;
        let mut status = w.st.hub.status.subscribe();
        w.rec.tell(crate::hub::fake::Tell::Hangup);
        tokio::time::timeout(Duration::from_secs(10), status.wait_for(|s| !s.connected))
            .await
            .unwrap()
            .unwrap();
        let before = now_ms();
        crate::server::delete_session(&w.st, &w.s.id).await.unwrap();
        let after = now_ms();
        let n = w.writes();
        pass(&w.st, after + 1000).await;
        assert_eq!(w.writes(), n);
        tokio::time::timeout(Duration::from_secs(10), status.wait_for(|s| s.connected))
            .await
            .unwrap()
            .unwrap();
        pass(&w.st, after + 5 * GAP).await;
        let closed = w.stored(&l).unwrap()["closed"].as_u64().expect("cerrado");
        assert!((before..=after).contains(&closed));
    }

    /// Muchos de golpe: `MAX_WRITES` y luego se espera.
    #[tokio::test]
    async fn a_big_cleanup_goes_in_batches() {
        let _limit = limit();
        let w = world().await;
        let o = goal(&w).await;
        for n in 0..MAX_WRITES + 5 {
            put_link(&w, &format!("{n:032x}"), &o, "este", "dead0000", json!({}));
        }
        w.st.board.sync().await.unwrap();
        let n = w.writes();
        pass(&w.st, T0).await;
        assert_eq!(w.writes(), n + MAX_WRITES);
        pass(&w.st, T0 + GAP).await;
        assert_eq!(w.writes(), n + MAX_WRITES, "esperando");
        pass(&w.st, T0 + BUSY_WAIT).await;
        assert_eq!(w.writes(), n + MAX_WRITES + 5);
        let open = w
            .live("link")
            .into_iter()
            .filter(|(_, v)| v["closed"].is_null())
            .count();
        assert_eq!(open, 0);
    }

    /// Con un 429 se espera antes de volver a escribir.
    #[tokio::test]
    async fn on_429_it_waits() {
        let _limit = limit();
        let w = world().await;
        goal(&w).await;
        let l = join(&w, &w.s).await;
        set_plan(&w, &w.s.id, "Migrar", &["done"]);
        w.board.lock().unwrap().busy = 1;
        let n = w.writes();
        pass(&w.st, T0).await;
        assert_eq!(w.writes(), n + 1);
        assert_eq!(w.stored(&l).unwrap()["plan"], Value::Null);
        pass(&w.st, T0 + GAP).await;
        assert_eq!(w.writes(), n + 1, "esperando");
        pass(&w.st, T0 + BUSY_WAIT).await;
        assert_eq!(w.writes(), n + 2);
        assert_eq!(w.stored(&l).unwrap()["plan"], summary("Migrar", 1, 1, 0));
    }

    /// La tarea: un cambio del plan la despierta.
    #[tokio::test]
    async fn the_task_wakes_up_with_a_change() {
        let _limit = limit();
        let w = world().await;
        goal(&w).await;
        let l = join(&w, &w.s).await;
        spawn_with(w.st.clone(), Duration::from_secs(3600));
        // El primer tic, ya: que acabe antes del cambio (si no, no se prueba el despertar).
        let first = async {
            while !w.st.progress.has_read() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        };
        tokio::time::timeout(Duration::from_secs(10), first)
            .await
            .expect("no hubo primer tic");
        set_plan(&w, &w.s.id, "Migrar", &["done"]);
        let wait = async {
            while w.stored(&l).unwrap()["plan"].is_null() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        };
        tokio::time::timeout(Duration::from_secs(10), wait)
            .await
            .expect("no subió el resumen");
    }

    /// El blob de `v` para `id`, con la clave actual: `(kid, blob)`.
    fn sealed(w: &World, id: &str, v: Value) -> (String, String) {
        let (kid, raw) = w.key();
        (
            kid,
            board::seal(&raw, id, v.to_string().as_bytes()).unwrap(),
        )
    }

    /// Un enlace que otro vuelve a abrir justo antes de borrarlo (un 409) no se borra.
    #[tokio::test]
    async fn a_link_reopened_meanwhile_is_not_removed() {
        let _limit = limit();
        let w = world().await;
        let o = goal(&w).await;
        let l = "1".repeat(32);
        put_link(&w, &l, &o, "este", "dead0000", json!({"closed": T0}));
        w.st.board.sync().await.unwrap();
        let reopened = json!({"t": "link", "objective": o,
            "session": {"machine": "este", "id": "dead0000", "name": "Otro/vieja"},
            "plan": null, "copied": 0, "closed": null, "created": 1, "changed": 3, "by": "web"});
        let other = sealed(&w, &l, reopened);
        w.board.lock().unwrap().clash(&l, Some(other));
        pass(&w.st, T0 + KEEP_CLOSED).await;
        let v = w.stored(&l).expect("no se borra");
        assert_eq!(v["closed"], Value::Null);
    }

    /// Un 409 que cierra el enlace o lo pasa a otra sesión o a otro equipo: ni el resumen ni el
    /// cierre lo reabren ni lo pisan.
    #[tokio::test]
    async fn a_clash_that_closes_or_moves_the_link_is_respected() {
        let _limit = limit();
        let w = world().await;
        let o = goal(&w).await;
        let l1 = join(&w, &w.s).await;
        let (l2, l3) = ("2".repeat(32), "3".repeat(32));
        put_link(&w, &l2, &o, "este", &w.s.id, json!({}));
        put_link(&w, &l3, &o, "este", "dead0000", json!({}));
        w.st.board.sync().await.unwrap();
        let link = |session: &str, machine: &str, closed: Value| {
            json!({"t": "link", "objective": o,
                "session": {"machine": machine, "id": session, "name": "Otro/vieja"},
                "plan": null, "copied": 0, "closed": closed, "created": 1, "changed": 3,
                "by": "web"})
        };
        let c1 = sealed(&w, &l1, link(&w.s.id, "este", json!(5)));
        let c2 = sealed(&w, &l2, link("otra0000", "este", Value::Null));
        let c3 = sealed(&w, &l3, link("dead0000", "mac", Value::Null));
        {
            let mut b = w.board.lock().unwrap();
            b.clash(&l1, Some(c1));
            b.clash(&l2, Some(c2));
            b.clash(&l3, Some(c3));
        }
        set_plan(&w, &w.s.id, "Migrar", &["done"]);
        let n = w.writes();
        pass(&w.st, T0).await;
        // Una escritura cada uno, la que choca: tras el 409 no queda nada que cambiar, y no se
        // escribe otra vez lo mismo.
        assert_eq!(w.writes(), n + 3);
        let v1 = w.stored(&l1).unwrap();
        assert_eq!((&v1["closed"], &v1["plan"]), (&json!(5), &Value::Null));
        let v2 = w.stored(&l2).unwrap();
        assert_eq!(
            (&v2["session"]["id"], &v2["plan"]),
            (&json!("otra0000"), &Value::Null)
        );
        let v3 = w.stored(&l3).unwrap();
        assert_eq!(
            (&v3["session"]["machine"], &v3["closed"]),
            (&json!("mac"), &Value::Null)
        );
    }

    /// Con el hub caído (un 503) también se espera un rato, sin volver a leer ni pedir en cada
    /// tic.
    #[tokio::test]
    async fn on_a_hub_error_it_waits_a_little() {
        let _limit = limit();
        let w = world().await;
        goal(&w).await;
        let l = join(&w, &w.s).await;
        set_plan(&w, &w.s.id, "Migrar", &["done"]);
        w.board.lock().unwrap().down = 1;
        let n = w.writes();
        pass(&w.st, T0).await;
        assert_eq!(w.writes(), n + 1);
        let reads = w.st.progress.reads();
        crate::server::plan_changed(&w.st, &w.s.id);
        pass(&w.st, T0 + 10_000).await;
        assert_eq!(
            (w.writes(), w.st.progress.reads()),
            (n + 1, reads),
            "esperando"
        );
        pass(&w.st, T0 + 30_000).await;
        assert_eq!(w.writes(), n + 2);
        assert_eq!(w.stored(&l).unwrap()["plan"], summary("Migrar", 1, 1, 0));
    }

    /// Las subidas del resumen van antes que una limpieza grande.
    #[tokio::test]
    async fn uploads_go_before_a_big_cleanup() {
        let _limit = limit();
        let w = world().await;
        let o = goal(&w).await;
        for n in 0..MAX_WRITES + 5 {
            put_link(&w, &format!("{n:032x}"), &o, "este", "dead0000", json!({}));
        }
        // El último de todos por id.
        let mine = "f".repeat(32);
        put_link(&w, &mine, &o, "este", &w.s.id, json!({}));
        w.st.board.sync().await.unwrap();
        set_plan(&w, &w.s.id, "Migrar", &["done"]);
        pass(&w.st, T0).await;
        assert_eq!(w.stored(&mine).unwrap()["plan"], summary("Migrar", 1, 1, 0));
    }

    /// El tope cuenta las peticiones de verdad: un 409 hace dos.
    #[tokio::test]
    async fn the_cap_counts_real_requests() {
        let _limit = limit();
        let w = world().await;
        let o = goal(&w).await;
        let ids: Vec<String> = (0..MAX_WRITES).map(|n| format!("{n:032x}")).collect();
        for id in &ids {
            put_link(&w, id, &o, "este", "dead0000", json!({}));
        }
        w.st.board.sync().await.unwrap();
        for id in &ids[..5] {
            let again = sealed(
                &w,
                id,
                json!({"t": "link", "objective": o,
                    "session": {"machine": "este", "id": "dead0000", "name": "Otro/vieja"},
                    "created": 1, "changed": 2}),
            );
            w.board.lock().unwrap().clash(id, Some(again));
        }
        let n = w.writes();
        pass(&w.st, T0).await;
        assert_eq!(w.writes(), n + MAX_WRITES);
    }

    /// Una sesión sin enlaces no hace leer la copia por cambiar su plan.
    #[tokio::test]
    async fn a_session_without_links_does_not_make_it_read() {
        let _limit = limit();
        let w = world().await;
        goal(&w).await;
        set_plan(&w, &w.s.id, "Migrar", &["pending"]);
        pass(&w.st, T0).await;
        let reads = w.st.progress.reads();
        set_plan(&w, &w.s.id, "Migrar", &["done"]);
        pass(&w.st, T0 + GAP).await;
        set_plan(&w, &w.s.id, "Migrar", &["done"]);
        pass(&w.st, T0 + 2 * GAP).await;
        assert_eq!(w.st.progress.reads(), reads);
        // Si se une, sí (la copia cambia).
        let l = join(&w, &w.s).await;
        pass(&w.st, T0 + 3 * GAP).await;
        assert_eq!(w.stored(&l).unwrap()["plan"], summary("Migrar", 1, 1, 0));
    }

    /// Lo que no se arregla reintentando (un 413) se prueba una vez, y otra cuando cambia lo
    /// que se quiere escribir: con el `code` del hub y sin él (un hub de antes).
    #[tokio::test]
    async fn what_cannot_be_written_is_not_retried_until_it_changes() {
        for uncoded in [false, true] {
            what_cannot_be_written(uncoded).await;
        }
    }

    async fn what_cannot_be_written(uncoded: bool) {
        let _limit = limit();
        let w = world().await;
        goal(&w).await;
        let l = join(&w, &w.s).await;
        w.board.lock().unwrap().too_big.insert(l.clone());
        w.board.lock().unwrap().uncoded = uncoded;
        set_plan(&w, &w.s.id, "Migrar", &["pending"]);
        let n = w.writes();
        pass(&w.st, T0).await;
        assert_eq!(w.writes(), n + 1);
        for k in 1..4 {
            set_plan(&w, &w.s.id, "Migrar", &["pending"]);
            pass(&w.st, T0 + k * GAP).await;
        }
        assert_eq!(w.writes(), n + 1);
        set_plan(&w, &w.s.id, "Migrar", &["done"]);
        pass(&w.st, T0 + 5 * GAP).await;
        assert_eq!(w.writes(), n + 2);
    }

    /// Un plan que no se puede leer del disco no borra el resumen del enlace.
    #[tokio::test]
    async fn an_unreadable_plan_keeps_the_summary() {
        let _limit = limit();
        let w = world().await;
        let o = goal(&w).await;
        let l = "1".repeat(32);
        put_link(
            &w,
            &l,
            &o,
            "este",
            &w.s.id,
            json!({"plan": summary("a", 1, 2, 0)}),
        );
        w.st.board.sync().await.unwrap();
        // Un directorio donde va el fichero: leerlo falla (y no es «no hay»).
        let dir =
            w.st.paths
                .state
                .join("sessions")
                .join(&w.s.id)
                .join("plan.json");
        std::fs::create_dir_all(&dir).unwrap();
        crate::server::plan_changed(&w.st, &w.s.id);
        let n = w.writes();
        pass(&w.st, T0).await;
        assert_eq!(w.writes(), n);
        assert_eq!(w.stored(&l).unwrap()["plan"], summary("a", 1, 2, 0));
    }

    /// Los cierres pendientes no se guardan para siempre: tampoco sin hub.
    #[tokio::test]
    async fn pending_closes_expire_even_without_a_hub() {
        let _limit = limit();
        let tmp = tempfile::tempdir().unwrap();
        let st = state(tmp.path());
        st.progress
            .session_deleted("dead0000", T0, LastPlan::Unknown);
        pass(&st, T0 + KEEP_CLOSED - 1).await;
        assert_eq!(st.progress.closing_len(), 1);
        pass(&st, T0 + KEEP_CLOSED).await;
        assert_eq!(st.progress.closing_len(), 0);
    }

    /// Si la copia cambia antes de su turno (otro lo escribió, un `join` de aquí lo reabrió),
    /// el borrado no se hace, aunque el enlace siga cerrado hace 30 días: se vuelve a mirar.
    #[tokio::test]
    async fn a_link_changed_before_its_turn_is_not_removed() {
        let _limit = limit();
        let w = world().await;
        let o = goal(&w).await;
        let l = "1".repeat(32);
        put_link(&w, &l, &o, "este", "dead0000", json!({"closed": T0}));
        w.st.board.sync().await.unwrap();
        let v = w.st.board.item(&l).unwrap().v;
        put_link(
            &w,
            &l,
            &o,
            "este",
            "dead0000",
            json!({"closed": T0, "changed": 7}),
        );
        w.st.board.sync().await.unwrap();
        assert!(w.st.board.item(&l).unwrap().v > v);
        pass(&w.st, T0).await;
        assert!(w.st.progress.has_read());
        let n = w.writes();
        let task = Task {
            link: l.clone(),
            v,
            job: Job::Remove,
        };
        let keys = w.st.group.board_keys();
        run(&w.st, &keys, "este", vec![task], T0 + KEEP_CLOSED).await;
        assert_eq!(w.writes(), n);
        assert_eq!(w.stored(&l).unwrap()["changed"], 7);
        assert!(!w.st.progress.has_read(), "se vuelve a mirar");
    }
}
