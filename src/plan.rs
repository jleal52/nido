//! El plan de fases y tareas que lleva el agente de una sesión (herramientas `plan_*` del MCP):
//! lo que manda, comprobado entero antes de guardarlo, con las horas que pone nido.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

pub const MAX_PHASES: usize = 50;
pub const MAX_TASKS: usize = 500;
pub const MAX_ID: usize = 64;
pub const MAX_NAME: usize = 200;
pub const MAX_NOTE: usize = 1000;
pub const MAX_BY: usize = 200;

/// Cuántos ids caben en el error de una tarea que no existe.
const MAX_IDS_IN_ERROR: usize = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Pending,
    InProgress,
    Done,
    Skipped,
    Blocked,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    pub name: String,
    pub status: Status,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by: Option<String>,
    /// ms desde 1970: al pasar a `in_progress`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started: Option<u64>,
    /// ms desde 1970: al pasar a `done`, `skipped` o `failed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Phase {
    pub id: String,
    pub name: String,
    pub tasks: Vec<Task>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plan {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub phases: Vec<Phase>,
    /// ms desde 1970: el último cambio.
    pub updated: u64,
}

/// Lo que manda el agente en `plan_set` (estados como texto: el error dice cuáles valen).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetArgs {
    #[serde(default)]
    pub title: Option<String>,
    pub phases: Vec<PhaseIn>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PhaseIn {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub tasks: Vec<TaskIn>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskIn {
    pub id: String,
    pub name: String,
    /// Sin él, `pending`.
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
    #[serde(default)]
    pub by: Option<String>,
}

/// `plan_task`: `note`/`by` solo si vienen; `""` los quita.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskArgs {
    pub id: String,
    pub status: String,
    #[serde(default)]
    pub note: Option<String>,
    #[serde(default)]
    pub by: Option<String>,
}

/// Cuánto va hecho: `done` cuenta también las saltadas; `active`, las que están en marcha.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
pub struct Progress {
    pub done: usize,
    pub total: usize,
    pub active: usize,
    pub blocked: usize,
    pub failed: usize,
}

impl Progress {
    /// `"3/12"`.
    pub fn text(&self) -> String {
        format!("{}/{}", self.done, self.total)
    }
}

/// Los estados como los escribe el agente (para el esquema de las herramientas del MCP).
pub const STATUSES: [&str; 6] = [
    "pending",
    "in_progress",
    "done",
    "skipped",
    "blocked",
    "failed",
];

pub fn parse_status(s: &str) -> Result<Status, String> {
    Ok(match s {
        "pending" => Status::Pending,
        "in_progress" => Status::InProgress,
        "done" => Status::Done,
        "skipped" => Status::Skipped,
        "blocked" => Status::Blocked,
        "failed" => Status::Failed,
        _ => {
            return Err(format!(
                "estado desconocido \"{s}\": vale pending, in_progress, done, skipped, blocked o failed"
            ));
        }
    })
}

fn check_id(id: &str) -> Result<(), String> {
    let ok = (1..=MAX_ID).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
    if ok {
        Ok(())
    } else {
        Err(format!(
            "id no válido \"{id}\": de 1 a 64 letras, dígitos, '.', '_' o '-'"
        ))
    }
}

/// Sin espacios a los lados y como mucho `max` caracteres (no bytes: no parte una letra).
fn clip(s: &str, max: usize) -> String {
    let s = s.trim();
    match s.char_indices().nth(max) {
        Some((at, _)) => s[..at].trim_end().to_string(),
        None => s.to_string(),
    }
}

/// [`clip`] para los textos que pueden faltar: vacío es que no hay.
fn clip_opt(s: Option<&str>, max: usize) -> Option<String> {
    Some(clip(s?, max)).filter(|s| !s.is_empty())
}

/// Las horas (empezada, acabada) de una tarea que pasa a `status`, a partir de cómo estaba.
fn stamp(status: Status, prev: Option<&Task>, now: u64) -> (Option<u64>, Option<u64>) {
    let started = prev.and_then(|p| p.started);
    let same = prev.is_some_and(|p| p.status == status);
    match status {
        Status::Pending => (None, None),
        Status::InProgress => (Some(started.filter(|_| same).unwrap_or(now)), None),
        Status::Blocked => (started, None),
        Status::Done | Status::Skipped | Status::Failed => {
            let finished = prev.and_then(|p| p.finished).filter(|_| same);
            (started, Some(finished.unwrap_or(now)))
        }
    }
}

impl Plan {
    /// El plan nuevo de un `plan_set`: todo comprobado antes de construirlo, con las horas de
    /// las tareas que ya estaban en `prev` (por id).
    pub fn set(prev: Option<&Plan>, a: SetArgs, now: u64) -> Result<Plan, String> {
        let n = a.phases.len();
        if n > MAX_PHASES {
            return Err(format!("demasiadas fases ({n}): como mucho {MAX_PHASES}"));
        }
        let n: usize = a.phases.iter().map(|f| f.tasks.len()).sum();
        if n > MAX_TASKS {
            return Err(format!("demasiadas tareas ({n}): como mucho {MAX_TASKS}"));
        }
        let mut seen = HashSet::new();
        let mut unique = |id: &str| {
            check_id(id)?;
            if seen.insert(id.to_string()) {
                Ok(())
            } else {
                Err(format!("id repetido \"{id}\""))
            }
        };
        let mut statuses = Vec::with_capacity(n);
        for f in &a.phases {
            unique(&f.id)?;
            if f.name.trim().is_empty() {
                return Err(format!("la fase \"{}\" no tiene nombre", f.id));
            }
            for t in &f.tasks {
                unique(&t.id)?;
                if t.name.trim().is_empty() {
                    return Err(format!("la tarea \"{}\" no tiene nombre", t.id));
                }
                statuses.push(match &t.status {
                    Some(s) => parse_status(s)?,
                    None => Status::Pending,
                });
            }
        }

        let mut statuses = statuses.into_iter();
        let phases = a
            .phases
            .into_iter()
            .map(|f| Phase {
                name: clip(&f.name, MAX_NAME),
                tasks: f
                    .tasks
                    .into_iter()
                    .zip(statuses.by_ref())
                    .map(|(t, status)| {
                        let (started, finished) =
                            stamp(status, prev.and_then(|p| p.task(&t.id)), now);
                        Task {
                            name: clip(&t.name, MAX_NAME),
                            status,
                            note: clip_opt(t.note.as_deref(), MAX_NOTE),
                            by: clip_opt(t.by.as_deref(), MAX_BY),
                            started,
                            finished,
                            id: t.id,
                        }
                    })
                    .collect(),
                id: f.id,
            })
            .collect();
        Ok(Plan {
            title: clip_opt(a.title.as_deref(), MAX_NAME),
            phases,
            updated: now,
        })
    }

    /// Un `plan_task`: cambia una tarea. Con error, el plan queda como estaba.
    pub fn update_task(&mut self, a: TaskArgs, now: u64) -> Result<(), String> {
        let Some(t) = self.task(&a.id) else {
            let mut ids: Vec<&str> = self
                .tasks()
                .take(MAX_IDS_IN_ERROR)
                .map(|t| t.id.as_str())
                .collect();
            if self.tasks().nth(MAX_IDS_IN_ERROR).is_some() {
                ids.push("…");
            }
            return Err(format!(
                "no hay ninguna tarea \"{}\"; las que hay: {}",
                a.id,
                ids.join(", ")
            ));
        };
        let status = parse_status(&a.status)?;
        let (started, finished) = stamp(status, Some(t), now);
        let t = self
            .phases
            .iter_mut()
            .flat_map(|f| &mut f.tasks)
            .find(|t| t.id == a.id)
            .expect("la tarea que se acaba de encontrar");
        t.status = status;
        t.started = started;
        t.finished = finished;
        if let Some(note) = a.note {
            t.note = clip_opt(Some(&note), MAX_NOTE);
        }
        if let Some(by) = a.by {
            t.by = clip_opt(Some(&by), MAX_BY);
        }
        self.updated = now;
        Ok(())
    }

    pub fn progress(&self) -> Progress {
        let mut p = Progress::default();
        for t in self.tasks() {
            p.total += 1;
            match t.status {
                Status::Done | Status::Skipped => p.done += 1,
                Status::InProgress => p.active += 1,
                Status::Blocked => p.blocked += 1,
                Status::Failed => p.failed += 1,
                Status::Pending => {}
            }
        }
        p
    }

    /// Las tareas de todas las fases, en orden.
    fn tasks(&self) -> impl Iterator<Item = &Task> {
        self.phases.iter().flat_map(|f| &f.tasks)
    }

    fn task(&self, id: &str) -> Option<&Task> {
        self.tasks().find(|t| t.id == id)
    }
}

/// Lo que se enseña en la lista de sesiones.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Summary {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub done: usize,
    pub total: usize,
    pub active: usize,
    pub blocked: usize,
    pub failed: usize,
    /// El nombre de la primera tarea `in_progress` (para el título del anillo en la web).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current: Option<String>,
    /// Sube con cada cambio. Empieza en la hora de arranque en ms, para que tras reiniciar
    /// `nido serve` no repita un valor que un cliente ya tenga (y deje de pedir el plan).
    pub rev: u64,
}

impl Summary {
    fn of(plan: &Plan, rev: u64) -> Summary {
        let p = plan.progress();
        Summary {
            title: plan.title.clone(),
            done: p.done,
            total: p.total,
            active: p.active,
            blocked: p.blocked,
            failed: p.failed,
            current: plan
                .tasks()
                .find(|t| t.status == Status::InProgress)
                .map(|t| t.name.clone()),
            rev,
        }
    }
}

/// Los planes de las sesiones de este equipo: en memoria y en `<state>/sessions/<id>/plan.json`.
///
/// Hay dos cerrojos. `inner`, lo que hay en memoria, se tiene solo un momento: `get` y `summary`
/// (que pide la lista de sesiones con el registro tomado) no esperan nunca a que se escriba en el
/// disco. El de cada sesión (`writers`) pone en fila sus cambios: con él se calcula el plan nuevo
/// (tomando `inner` un momento), se escribe en el disco sin `inner` y se publica (otra vez un
/// momento). Siempre en ese orden: primero el de la sesión, luego `inner`.
pub struct Plans {
    state: PathBuf,
    inner: Mutex<Inner>,
    /// El cerrojo de cada sesión que ha cambiado su plan (uno por sesión, que se quita al
    /// borrarla: son pocos).
    writers: Mutex<HashMap<String, Arc<Mutex<()>>>>,
}

struct Inner {
    /// El plan de cada sesión que tiene, con su `rev`.
    plans: HashMap<String, (Plan, u64)>,
    next_rev: u64,
    /// Las sesiones que ya se han mirado en el disco (tengan plan o no).
    loaded: HashSet<String>,
    /// Las que no se pudieron leer y ya se dijo (para no repetirlo en cada lista de sesiones).
    unreadable: HashSet<String>,
    /// Las sesiones borradas (`forget`): su plan ya no se lee ni se escribe, para que un cambio
    /// que llegue tarde no vuelva a crear su carpeta. No se quitan nunca: los ids son aleatorios
    /// (`registry::new_id`) y una sesión nueva no repite el de otra.
    gone: HashSet<String>,
}

impl Inner {
    fn bump(&mut self) -> u64 {
        let rev = self.next_rev;
        self.next_rev += 1;
        rev
    }
}

/// El error de cambiar el plan de una sesión borrada.
pub const GONE: &str = "la sesión ya no existe";

/// Que falte el fichero o su carpeta no es un error: esa sesión no tiene plan.
fn missing(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
    )
}

impl Plans {
    pub fn new(state_dir: PathBuf) -> Self {
        Plans {
            state: state_dir,
            inner: Mutex::new(Inner {
                plans: HashMap::new(),
                next_rev: crate::oauth::now_ms(),
                loaded: HashSet::new(),
                unreadable: HashSet::new(),
                gone: HashSet::new(),
            }),
            writers: Mutex::new(HashMap::new()),
        }
    }

    /// La misma carpeta que `Paths::session_state`, para que `nido rm` se lo lleve.
    fn path(&self, id: &str) -> PathBuf {
        self.state.join("sessions").join(id).join("plan.json")
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().expect("planes")
    }

    /// El cerrojo de los cambios del plan de `id`.
    fn writer(&self, id: &str) -> Arc<Mutex<()>> {
        self.writers
            .lock()
            .expect("planes: cerrojos")
            .entry(id.to_string())
            .or_default()
            .clone()
    }

    /// Trae del disco el plan de `id` la primera vez que se pide (con `inner` tomado: es una vez
    /// por sesión, y así nadie escribe el fichero entre leerlo y apartarlo). Uno que no se
    /// entienda se aparta como `plan.json.bad`, para no tropezar con él cada vez; uno que no se
    /// pueda leer (permisos, un fallo del disco) se deja donde está y se vuelve a intentar la
    /// próxima vez: es el error.
    fn load(&self, inner: &mut Inner, id: &str) -> Result<(), String> {
        if inner.loaded.contains(id) || inner.gone.contains(id) {
            return Ok(());
        }
        let path = self.path(id);
        let bytes = match std::fs::read(&path) {
            Ok(b) => Some(b),
            Err(e) if missing(&e) => None,
            Err(e) => {
                if inner.unreadable.insert(id.to_string()) {
                    eprintln!("nido: el plan de {id} no se pudo leer ({e}): se volverá a intentar");
                }
                return Err(format!("no se pudo leer el plan: {e}"));
            }
        };
        inner.unreadable.remove(id);
        inner.loaded.insert(id.to_string());
        let Some(bytes) = bytes else {
            return Ok(());
        };
        match serde_json::from_slice::<Plan>(&bytes) {
            Ok(plan) => {
                let rev = inner.bump();
                inner.plans.insert(id.to_string(), (plan, rev));
            }
            Err(e) => match std::fs::rename(&path, path.with_extension("json.bad")) {
                Ok(()) => eprintln!(
                    "nido: el plan de {id} no se entiende ({e}): apartado como plan.json.bad"
                ),
                Err(re) => eprintln!(
                    "nido: el plan de {id} no se entiende ({e}) y no se pudo apartar ({re})"
                ),
            },
        }
        Ok(())
    }

    /// Escribe en un temporal y lo renombra: nunca queda un plan a medias.
    fn write(&self, id: &str, plan: &Plan) -> io::Result<()> {
        let path = self.path(id);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        let body = serde_json::to_vec_pretty(plan).map_err(io::Error::other)?;
        let written = (|| {
            let mut opts = std::fs::OpenOptions::new();
            opts.write(true).create(true).truncate(true);
            #[cfg(unix)]
            std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
            let mut f = opts.open(&tmp)?;
            f.write_all(&body)?;
            f.sync_all()?;
            std::fs::rename(&tmp, &path)
        })();
        if written.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        written
    }

    /// Cambia el plan de `id`: `f` hace el nuevo con el de ahora; se guarda y, solo si se pudo,
    /// se pone en memoria con un `rev` nuevo. Todo con el cerrojo de la sesión, y el disco sin
    /// `inner`.
    fn change(
        &self,
        id: &str,
        f: impl FnOnce(Option<&Plan>) -> Result<Plan, String>,
    ) -> Result<Progress, String> {
        let turn = self.writer(id);
        let _turn = turn.lock().unwrap_or_else(PoisonError::into_inner);
        let plan = {
            let mut inner = self.lock();
            if inner.gone.contains(id) {
                return Err(GONE.into());
            }
            self.load(&mut inner, id)?;
            f(inner.plans.get(id).map(|(p, _)| p))?
        };
        self.write(id, &plan)
            .map_err(|e| format!("no se pudo guardar el plan: {e}"))?;
        let progress = plan.progress();
        let mut inner = self.lock();
        let rev = inner.bump();
        inner.plans.insert(id.to_string(), (plan, rev));
        Ok(progress)
    }

    pub fn get(&self, id: &str) -> Option<Plan> {
        let mut inner = self.lock();
        let _ = self.load(&mut inner, id);
        inner.plans.get(id).map(|(p, _)| p.clone())
    }

    pub fn summary(&self, id: &str) -> Option<Summary> {
        let mut inner = self.lock();
        let _ = self.load(&mut inner, id);
        inner.plans.get(id).map(|(p, rev)| Summary::of(p, *rev))
    }

    /// `plan_set`: sustituye el plan entero (con las horas de las tareas que ya estaban).
    pub fn set(&self, id: &str, a: SetArgs, now: u64) -> Result<Progress, String> {
        self.change(id, |prev| Plan::set(prev, a, now))
    }

    /// `plan_task`: cambia una tarea.
    pub fn update_task(&self, id: &str, a: TaskArgs, now: u64) -> Result<Progress, String> {
        self.change(id, |prev| {
            let mut plan = prev.ok_or("esta sesión no tiene plan")?.clone();
            plan.update_task(a, now)?;
            Ok(plan)
        })
    }

    /// `plan_clear`: lo borra (que no hubiera no es error).
    pub fn clear(&self, id: &str) -> Result<(), String> {
        let turn = self.writer(id);
        let _turn = turn.lock().unwrap_or_else(PoisonError::into_inner);
        if self.lock().gone.contains(id) {
            return Err(GONE.into());
        }
        match std::fs::remove_file(self.path(id)) {
            Err(e) if !missing(&e) => return Err(format!("no se pudo borrar el plan: {e}")),
            _ => {}
        }
        let mut inner = self.lock();
        inner.plans.remove(id);
        inner.loaded.insert(id.to_string());
        Ok(())
    }

    /// La sesión se borra: su plan sale de memoria y ya no se lee ni se escribe (ver `gone`).
    /// Espera a que acabe el cambio que se esté guardando, así que hay que llamarlo antes de
    /// borrar su carpeta: lo que se escribiera antes, se va con ella; lo de después, falla.
    pub fn forget(&self, id: &str) {
        let turn = self.writer(id);
        let _turn = turn.lock().unwrap_or_else(PoisonError::into_inner);
        let mut inner = self.lock();
        inner.gone.insert(id.to_string());
        inner.plans.remove(id);
        inner.loaded.remove(id);
        inner.unreadable.remove(id);
        drop(inner);
        self.writers.lock().expect("planes: cerrojos").remove(id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(id: &str, status: &str) -> TaskIn {
        TaskIn {
            id: id.into(),
            name: format!("tarea {id}"),
            status: Some(status.into()),
            note: None,
            by: None,
        }
    }

    fn phase(id: &str, tasks: Vec<TaskIn>) -> PhaseIn {
        PhaseIn {
            id: id.into(),
            name: format!("fase {id}"),
            tasks,
        }
    }

    fn args(phases: Vec<PhaseIn>) -> SetArgs {
        SetArgs {
            title: None,
            phases,
        }
    }

    fn task_args(id: &str, status: &str) -> TaskArgs {
        TaskArgs {
            id: id.into(),
            status: status.into(),
            note: None,
            by: None,
        }
    }

    fn find<'a>(p: &'a Plan, id: &str) -> &'a Task {
        p.phases
            .iter()
            .flat_map(|f| &f.tasks)
            .find(|t| t.id == id)
            .unwrap()
    }

    fn prev_task(status: Status, started: Option<u64>, finished: Option<u64>) -> Task {
        Task {
            id: "t".into(),
            name: "t".into(),
            status,
            note: None,
            by: None,
            started,
            finished,
        }
    }

    #[test]
    fn statuses_are_the_ones_parsed() {
        for s in STATUSES {
            let parsed = parse_status(s).unwrap();
            assert_eq!(serde_json::to_value(parsed).unwrap(), s);
        }
    }

    #[test]
    fn set_validates_everything() {
        let err = |a: SetArgs| Plan::set(None, a, 1).unwrap_err();

        let many = (0..51).map(|i| phase(&format!("f{i}"), vec![])).collect();
        assert_eq!(err(args(many)), "demasiadas fases (51): como mucho 50");

        let tasks: Vec<_> = (0..501)
            .map(|i| task(&format!("t{i}"), "pending"))
            .collect();
        let mut tasks = tasks.into_iter();
        let a: Vec<_> = tasks.by_ref().take(300).collect();
        let b: Vec<_> = tasks.collect();
        assert_eq!(
            err(args(vec![phase("f1", a), phase("f2", b)])),
            "demasiadas tareas (501): como mucho 500"
        );

        assert_eq!(
            err(args(vec![phase("f 1", vec![])])),
            "id no válido \"f 1\": de 1 a 64 letras, dígitos, '.', '_' o '-'"
        );
        assert_eq!(
            err(args(vec![phase("", vec![])])),
            "id no válido \"\": de 1 a 64 letras, dígitos, '.', '_' o '-'"
        );
        let long = "a".repeat(65);
        assert_eq!(
            err(args(vec![phase("f1", vec![task(&long, "done")])])),
            format!("id no válido \"{long}\": de 1 a 64 letras, dígitos, '.', '_' o '-'")
        );
        assert!(Plan::set(None, args(vec![phase(&"a".repeat(64), vec![])]), 1).is_ok());
        assert!(Plan::set(None, args(vec![phase("A-1.b_2", vec![])]), 1).is_ok());

        // Únicos entre fases y tareas juntas.
        assert_eq!(
            err(args(vec![phase("x", vec![task("x", "done")])])),
            "id repetido \"x\""
        );
        assert_eq!(
            err(args(vec![
                phase("f1", vec![task("t1", "done")]),
                phase("f2", vec![task("t1", "done")]),
            ])),
            "id repetido \"t1\""
        );
        assert_eq!(
            err(args(vec![phase("f1", vec![]), phase("f1", vec![])])),
            "id repetido \"f1\""
        );

        let mut f = phase("f1", vec![]);
        f.name = "   ".into();
        assert_eq!(err(args(vec![f])), "la fase \"f1\" no tiene nombre");
        let mut t = task("t1", "done");
        t.name = "".into();
        assert_eq!(
            err(args(vec![phase("f1", vec![t])])),
            "la tarea \"t1\" no tiene nombre"
        );

        assert_eq!(
            err(args(vec![phase("f1", vec![task("t1", "hecha")])])),
            "estado desconocido \"hecha\": vale pending, in_progress, done, skipped, blocked o failed"
        );

        // Sin estado, pending; título y textos vacíos, fuera.
        let mut t = task("t1", "x");
        t.status = None;
        t.note = Some("  ".into());
        t.by = Some("".into());
        let mut a = args(vec![phase("f1", vec![t])]);
        a.title = Some(" ".into());
        let p = Plan::set(None, a, 7).unwrap();
        assert_eq!(p.title, None);
        assert_eq!(p.updated, 7);
        let t = find(&p, "t1");
        assert_eq!(t.status, Status::Pending);
        assert_eq!((t.note.as_deref(), t.by.as_deref()), (None, None));
    }

    #[test]
    fn set_keeps_times_by_id() {
        let p1 = Plan::set(
            None,
            args(vec![phase("f1", vec![task("t1", "in_progress")])]),
            1000,
        )
        .unwrap();
        assert_eq!(find(&p1, "t1").started, Some(1000));

        // Mismo id y estado: conserva la hora, aunque cambie de fase.
        let p2 = Plan::set(
            Some(&p1),
            args(vec![
                phase("f0", vec![task("t0", "pending")]),
                phase("f1", vec![]),
                phase("f2", vec![task("t1", "in_progress")]),
            ]),
            2000,
        )
        .unwrap();
        assert_eq!(find(&p2, "t1").started, Some(1000));
        assert_eq!(find(&p2, "t1").finished, None);
        assert_eq!(p2.updated, 2000);

        let p3 = Plan::set(
            Some(&p2),
            args(vec![phase("f2", vec![task("t1", "done")])]),
            3000,
        )
        .unwrap();
        assert_eq!(find(&p3, "t1").started, Some(1000));
        assert_eq!(find(&p3, "t1").finished, Some(3000));

        let p4 = Plan::set(
            Some(&p3),
            args(vec![phase("f2", vec![task("t1", "done")])]),
            4000,
        )
        .unwrap();
        assert_eq!(find(&p4, "t1").finished, Some(3000));
    }

    #[test]
    fn stamp_rules() {
        use Status::*;
        let now = 50;
        let none: Option<&Task> = None;

        // pending: sin horas, viniera de donde viniera.
        assert_eq!(stamp(Pending, none, now), (None, None));
        for s in [InProgress, Done, Skipped, Blocked, Failed] {
            let p = prev_task(s, Some(10), Some(20));
            assert_eq!(stamp(Pending, Some(&p), now), (None, None));
        }

        // in_progress: la de antes solo si ya estaba en marcha.
        assert_eq!(stamp(InProgress, none, now), (Some(50), None));
        let p = prev_task(InProgress, Some(10), None);
        assert_eq!(stamp(InProgress, Some(&p), now), (Some(10), None));
        let p = prev_task(InProgress, None, None);
        assert_eq!(stamp(InProgress, Some(&p), now), (Some(50), None));
        let p = prev_task(Blocked, Some(10), None);
        assert_eq!(stamp(InProgress, Some(&p), now), (Some(50), None));
        let p = prev_task(Done, Some(10), Some(20));
        assert_eq!(stamp(InProgress, Some(&p), now), (Some(50), None));

        // blocked: guarda la de empezar, quita la de acabar.
        assert_eq!(stamp(Blocked, none, now), (None, None));
        let p = prev_task(InProgress, Some(10), None);
        assert_eq!(stamp(Blocked, Some(&p), now), (Some(10), None));
        let p = prev_task(Failed, Some(10), Some(20));
        assert_eq!(stamp(Blocked, Some(&p), now), (Some(10), None));

        // done, skipped, failed.
        for s in [Done, Skipped, Failed] {
            assert_eq!(stamp(s, none, now), (None, Some(50)));
            let p = prev_task(InProgress, Some(10), None);
            assert_eq!(stamp(s, Some(&p), now), (Some(10), Some(50)));
            let p = prev_task(s, Some(10), Some(20));
            assert_eq!(stamp(s, Some(&p), now), (Some(10), Some(20)));
            let p = prev_task(s, Some(10), None);
            assert_eq!(stamp(s, Some(&p), now), (Some(10), Some(50)));
        }
        let p = prev_task(Done, Some(10), Some(20));
        assert_eq!(stamp(Failed, Some(&p), now), (Some(10), Some(50)));
        assert_eq!(stamp(Skipped, Some(&p), now), (Some(10), Some(50)));
    }

    #[test]
    fn update_task_changes_one() {
        let mut t1 = task("t1", "pending");
        t1.note = Some("una nota".into());
        t1.by = Some("WSL/a1b2c3d4".into());
        let mut p = Plan::set(
            None,
            args(vec![phase("f1", vec![t1, task("t2", "pending")])]),
            1000,
        )
        .unwrap();
        let before_t2 = find(&p, "t2").clone();

        // Sin note ni by: se quedan.
        p.update_task(task_args("t1", "in_progress"), 2000).unwrap();
        let t = find(&p, "t1");
        assert_eq!(t.status, Status::InProgress);
        assert_eq!(t.started, Some(2000));
        assert_eq!(t.note.as_deref(), Some("una nota"));
        assert_eq!(t.by.as_deref(), Some("WSL/a1b2c3d4"));
        assert_eq!(p.updated, 2000);
        assert_eq!(find(&p, "t2"), &before_t2);

        // "" las quita; otro texto las cambia.
        let mut a = task_args("t1", "done");
        a.note = Some("".into());
        a.by = Some(" subagente revisor ".into());
        p.update_task(a, 3000).unwrap();
        let t = find(&p, "t1");
        assert_eq!(t.status, Status::Done);
        assert_eq!((t.started, t.finished), (Some(2000), Some(3000)));
        assert_eq!(t.note, None);
        assert_eq!(t.by.as_deref(), Some("subagente revisor"));
        assert_eq!(p.updated, 3000);

        // Un estado malo no cambia nada.
        let before = p.clone();
        assert_eq!(
            p.update_task(task_args("t1", "listo"), 4000).unwrap_err(),
            "estado desconocido \"listo\": vale pending, in_progress, done, skipped, blocked o failed"
        );
        assert_eq!(p, before);
    }

    #[test]
    fn update_task_unknown_lists_ids() {
        let mut p = Plan::set(
            None,
            args(vec![phase(
                "f1",
                vec![task("t1", "pending"), task("t2", "done")],
            )]),
            1,
        )
        .unwrap();
        let before = p.clone();
        assert_eq!(
            p.update_task(task_args("t9", "done"), 2).unwrap_err(),
            "no hay ninguna tarea \"t9\"; las que hay: t1, t2"
        );
        // Una fase no es una tarea.
        assert_eq!(
            p.update_task(task_args("f1", "done"), 2).unwrap_err(),
            "no hay ninguna tarea \"f1\"; las que hay: t1, t2"
        );
        assert_eq!(p, before);

        let tasks = (1..=25)
            .map(|i| task(&format!("t{i}"), "pending"))
            .collect();
        let mut p = Plan::set(None, args(vec![phase("f1", tasks)]), 1).unwrap();
        let ids: Vec<_> = (1..=20).map(|i| format!("t{i}")).collect();
        assert_eq!(
            p.update_task(task_args("x", "done"), 2).unwrap_err(),
            format!(
                "no hay ninguna tarea \"x\"; las que hay: {}, …",
                ids.join(", ")
            )
        );
        let tasks = (1..=20)
            .map(|i| task(&format!("t{i}"), "pending"))
            .collect();
        let mut p = Plan::set(None, args(vec![phase("f1", tasks)]), 1).unwrap();
        assert_eq!(
            p.update_task(task_args("x", "done"), 2).unwrap_err(),
            format!(
                "no hay ninguna tarea \"x\"; las que hay: {}",
                ids.join(", ")
            )
        );
    }

    #[test]
    fn progress_counts() {
        let p = Plan::set(
            None,
            args(vec![
                phase(
                    "f1",
                    vec![
                        task("a", "pending"),
                        task("b", "in_progress"),
                        task("c", "done"),
                    ],
                ),
                phase("f2", vec![]),
                phase(
                    "f3",
                    vec![
                        task("d", "skipped"),
                        task("e", "blocked"),
                        task("f", "failed"),
                        task("g", "done"),
                    ],
                ),
            ]),
            1,
        )
        .unwrap();
        let pr = p.progress();
        assert_eq!(
            pr,
            Progress {
                done: 3,
                total: 7,
                active: 1,
                blocked: 1,
                failed: 1
            }
        );
        assert_eq!(pr.text(), "3/7");
        assert_eq!(Progress::default().text(), "0/0");
    }

    #[test]
    fn clips_long_text() {
        let long = "áéíóú".repeat(60); // 300 caracteres, 600 bytes
        let mut t = task("t1", "done");
        t.name = format!("  {long}");
        t.note = Some("ñ".repeat(1500));
        t.by = Some("b".repeat(300));
        let mut f = phase("f1", vec![t]);
        f.name = long.clone();
        let mut a = args(vec![f]);
        a.title = Some(format!("{long}  "));
        let p = Plan::set(None, a, 1).unwrap();

        let want: String = long.chars().take(MAX_NAME).collect();
        assert_eq!(want.chars().count(), 200);
        assert_eq!(p.title.as_deref(), Some(want.as_str()));
        assert_eq!(p.phases[0].name, want);
        let t = find(&p, "t1");
        assert_eq!(t.name, want);
        assert_eq!(t.note.as_ref().unwrap().chars().count(), MAX_NOTE);
        assert_eq!(t.by.as_ref().unwrap().chars().count(), MAX_BY);
        assert_eq!(clip("  hola  ", 10), "hola");
        assert_eq!(clip("ab cd", 3), "ab");

        let mut a = task_args("t1", "done");
        a.note = Some("é".repeat(2000));
        let mut p = p;
        p.update_task(a, 2).unwrap();
        assert_eq!(
            find(&p, "t1").note.as_ref().unwrap().chars().count(),
            MAX_NOTE
        );
    }

    #[test]
    fn status_round_trip() {
        assert_eq!(
            serde_json::to_string(&Status::InProgress).unwrap(),
            "\"in_progress\""
        );
        assert_eq!(
            serde_json::from_str::<Status>("\"in_progress\"").unwrap(),
            Status::InProgress
        );
        for (s, st) in [
            ("pending", Status::Pending),
            ("in_progress", Status::InProgress),
            ("done", Status::Done),
            ("skipped", Status::Skipped),
            ("blocked", Status::Blocked),
            ("failed", Status::Failed),
        ] {
            assert_eq!(parse_status(s).unwrap(), st);
            assert_eq!(serde_json::to_value(st).unwrap(), s);
        }

        // Y el plan entero, con sus horas y sin los campos vacíos.
        let p = Plan::set(
            None,
            args(vec![phase("f1", vec![task("t1", "in_progress")])]),
            5,
        )
        .unwrap();
        let v = serde_json::to_value(&p).unwrap();
        assert_eq!(
            v,
            serde_json::json!({
                "phases": [{"id": "f1", "name": "fase f1", "tasks": [
                    {"id": "t1", "name": "tarea t1", "status": "in_progress", "started": 5}
                ]}],
                "updated": 5
            })
        );
        assert_eq!(serde_json::from_value::<Plan>(v).unwrap(), p);

        // Lo que manda el agente: sin campos de más.
        assert!(
            serde_json::from_value::<TaskArgs>(
                serde_json::json!({"id": "t1", "status": "done", "otro": 1})
            )
            .is_err()
        );
        let a: SetArgs = serde_json::from_value(serde_json::json!({
            "phases": [{"id": "f1", "name": "F", "tasks": [{"id": "t1", "name": "T"}]}]
        }))
        .unwrap();
        assert_eq!(a.phases[0].tasks[0].status, None);
    }

    fn plan_of(tasks: &[(&str, &str)]) -> SetArgs {
        let mut a = args(vec![phase(
            "f1",
            tasks.iter().map(|(id, st)| task(id, st)).collect(),
        )]);
        a.title = Some("Migrar el login".into());
        a
    }

    fn plan_file(state: &std::path::Path, id: &str) -> std::path::PathBuf {
        state.join("sessions").join(id).join("plan.json")
    }

    #[test]
    fn plans_persist_across_instances() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        let plans = Plans::new(state.clone());
        assert_eq!(plans.get("s1"), None);
        assert_eq!(plans.summary("s1"), None);
        let pr = plans
            .set(
                "s1",
                plan_of(&[("t1", "in_progress"), ("t2", "pending")]),
                1000,
            )
            .unwrap();
        assert_eq!(pr.text(), "0/2");
        let pr = plans
            .update_task("s1", task_args("t2", "done"), 2000)
            .unwrap();
        assert_eq!(pr.text(), "1/2");
        let saved = plans.get("s1").unwrap();
        assert_eq!(saved.updated, 2000);

        let file = plan_file(&state, "s1");
        assert!(file.exists());
        assert!(!file.with_extension("json.tmp").exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }

        // Como tras reiniciar: la lista solo pide el resumen, y ya está.
        let again = Plans::new(state.clone());
        let s = again.summary("s1").unwrap();
        assert_eq!(s.title.as_deref(), Some("Migrar el login"));
        assert_eq!((s.done, s.total, s.active), (1, 2, 1));
        assert_eq!(again.get("s1").unwrap(), saved);

        // Y plan_task sigue, con las horas de antes.
        let again = Plans::new(state.clone());
        again
            .update_task("s1", task_args("t1", "done"), 3000)
            .unwrap();
        let t = again.get("s1").unwrap();
        assert_eq!(find(&t, "t1").started, Some(1000));
        assert_eq!(find(&t, "t1").finished, Some(3000));
        // Y plan_set también conserva las horas del disco.
        let again = Plans::new(state.clone());
        again
            .set("s1", plan_of(&[("t1", "done"), ("t2", "done")]), 4000)
            .unwrap();
        let t = again.get("s1").unwrap();
        assert_eq!(find(&t, "t1").finished, Some(3000));
        assert_eq!(find(&t, "t2").finished, Some(2000));

        // Sin plan, plan_task es error; otra sesión no se ve.
        assert_eq!(
            again
                .update_task("s2", task_args("t1", "done"), 1)
                .unwrap_err(),
            "esta sesión no tiene plan"
        );
        assert_eq!(again.get("s2"), None);
        assert!(!state.join("sessions/s2").exists());
    }

    #[test]
    fn bad_file_is_set_aside() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().to_path_buf();
        let file = plan_file(&state, "s1");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "{ esto no es un plan").unwrap();

        let plans = Plans::new(state.clone());
        assert_eq!(plans.summary("s1"), None);
        assert!(!file.exists());
        assert_eq!(
            std::fs::read_to_string(file.with_extension("json.bad")).unwrap(),
            "{ esto no es un plan"
        );
        assert_eq!(plans.get("s1"), None);

        // Se puede empezar otro.
        plans.set("s1", plan_of(&[("t1", "pending")]), 1).unwrap();
        assert!(file.exists());
        assert_eq!(Plans::new(state).summary("s1").unwrap().total, 1);
    }

    #[test]
    fn rev_goes_up() {
        let start = crate::oauth::now_ms();
        let dir = tempfile::tempdir().unwrap();
        let plans = Plans::new(dir.path().to_path_buf());
        plans.set("s1", plan_of(&[("t1", "pending")]), 1).unwrap();
        let r1 = plans.summary("s1").unwrap().rev;
        assert!(r1 >= start, "{r1} < {start}");
        assert_eq!(plans.summary("s1").unwrap().rev, r1, "leer no lo sube");

        plans.update_task("s1", task_args("t1", "done"), 2).unwrap();
        let r2 = plans.summary("s1").unwrap().rev;
        assert!(r2 > r1);
        plans.set("s2", plan_of(&[("t1", "pending")]), 3).unwrap();
        let r3 = plans.summary("s2").unwrap().rev;
        assert!(r3 > r2);
        plans.set("s1", plan_of(&[("t1", "done")]), 4).unwrap();
        assert!(plans.summary("s1").unwrap().rev > r3);

        // Un error no lo sube.
        let r = plans.summary("s1").unwrap().rev;
        plans
            .update_task("s1", task_args("x", "done"), 5)
            .unwrap_err();
        plans.set("s1", plan_of(&[("t1", "mal")]), 5).unwrap_err();
        assert_eq!(plans.summary("s1").unwrap().rev, r);

        // Al cargar del disco, uno nuevo y no menor que el arranque.
        let start = crate::oauth::now_ms();
        let again = Plans::new(dir.path().to_path_buf());
        assert!(again.summary("s1").unwrap().rev >= start);
    }

    #[test]
    fn summary_current() {
        let dir = tempfile::tempdir().unwrap();
        let plans = Plans::new(dir.path().to_path_buf());
        let mut a = args(vec![
            phase("f1", vec![task("t1", "done"), task("t2", "pending")]),
            phase(
                "f2",
                vec![task("t3", "in_progress"), task("t4", "in_progress")],
            ),
        ]);
        a.phases[1].tasks[0].name = "Escribir los tests".into();
        plans.set("s1", a, 1).unwrap();
        let s = plans.summary("s1").unwrap();
        assert_eq!(s.current.as_deref(), Some("Escribir los tests"));
        assert_eq!(s.title, None);
        assert_eq!(
            (s.done, s.total, s.active, s.blocked, s.failed),
            (1, 4, 2, 0, 0)
        );
        let v = serde_json::to_value(&s).unwrap();
        assert!(v.get("title").is_none());
        assert_eq!(v["current"], "Escribir los tests");

        plans
            .update_task("s1", task_args("t3", "blocked"), 2)
            .unwrap();
        plans
            .update_task("s1", task_args("t4", "failed"), 2)
            .unwrap();
        let s = plans.summary("s1").unwrap();
        assert_eq!(s.current, None);
        assert_eq!((s.active, s.blocked, s.failed), (0, 1, 1));
        assert!(serde_json::to_value(&s).unwrap().get("current").is_none());
    }

    #[test]
    fn clear_removes_file() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().to_path_buf();
        let plans = Plans::new(state.clone());
        // Que no haya plan no es error.
        plans.clear("s1").unwrap();
        plans.set("s1", plan_of(&[("t1", "pending")]), 1).unwrap();
        let file = plan_file(&state, "s1");
        assert!(file.exists());
        plans.clear("s1").unwrap();
        assert!(!file.exists());
        assert_eq!(plans.get("s1"), None);
        assert_eq!(plans.summary("s1"), None);
        assert_eq!(Plans::new(state.clone()).summary("s1"), None);

        // forget lo quita de memoria; el fichero sigue (lo borra quien borra la sesión).
        plans.set("s1", plan_of(&[("t1", "pending")]), 2).unwrap();
        plans.forget("s1");
        assert!(file.exists());
        std::fs::remove_dir_all(state.join("sessions/s1")).unwrap();
        assert_eq!(plans.get("s1"), None);
    }

    /// Un plan que no se puede leer (aquí, una carpeta en su sitio) no es uno estropeado: no se
    /// aparta, se vuelve a intentar la siguiente vez, y mientras tanto no se escribe encima.
    #[test]
    fn unreadable_file_is_retried() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().to_path_buf();
        let file = plan_file(&state, "s1");
        std::fs::create_dir_all(&file).unwrap();

        let plans = Plans::new(state.clone());
        assert_eq!(plans.summary("s1"), None);
        assert_eq!(plans.get("s1"), None);
        assert!(file.is_dir());
        assert!(!file.with_extension("json.bad").exists());
        let e = plans
            .set("s1", plan_of(&[("t1", "pending")]), 1)
            .unwrap_err();
        assert!(e.starts_with("no se pudo leer el plan: "), "{e}");
        let e = plans
            .update_task("s1", task_args("t1", "done"), 1)
            .unwrap_err();
        assert!(e.starts_with("no se pudo leer el plan: "), "{e}");
        assert!(file.is_dir());

        // Cuando se puede leer, se lee.
        let other = dir.path().join("otro");
        Plans::new(other.clone())
            .set("s1", plan_of(&[("t1", "done")]), 1)
            .unwrap();
        std::fs::remove_dir(&file).unwrap();
        std::fs::copy(plan_file(&other, "s1"), &file).unwrap();
        assert_eq!(plans.summary("s1").unwrap().done, 1);
        plans
            .update_task("s1", task_args("t1", "pending"), 2)
            .unwrap();
        assert_eq!(Plans::new(state).summary("s1").unwrap().done, 0);
    }

    /// `f` en otro hilo: si acaba en `secs`, lo que devuelve.
    fn within<T: Send + 'static>(secs: u64, f: impl FnOnce() -> T + Send + 'static) -> Option<T> {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(f());
        });
        rx.recv_timeout(std::time::Duration::from_secs(secs)).ok()
    }

    /// Mientras se escribe el plan de una sesión, leerlo (y escribir el de otra) no espera; otro
    /// cambio de la misma espera su turno.
    #[test]
    fn readers_do_not_wait_for_writes() {
        let dir = tempfile::tempdir().unwrap();
        let plans = std::sync::Arc::new(Plans::new(dir.path().to_path_buf()));
        plans.set("s1", plan_of(&[("t1", "pending")]), 1).unwrap();
        let w = plans.writer("s1");
        let writing = w.lock().unwrap();

        let p = plans.clone();
        let read = within(5, move || {
            (
                p.summary("s1").map(|s| s.done),
                p.get("s1").is_some(),
                p.set("s2", plan_of(&[("t1", "done")]), 2).is_ok(),
            )
        });
        assert_eq!(read, Some((Some(0), true, true)));

        let p = plans.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let second = std::thread::spawn(move || {
            let r = p.update_task("s1", task_args("t1", "done"), 3);
            let _ = tx.send(());
            r
        });
        assert!(
            rx.recv_timeout(std::time::Duration::from_millis(200))
                .is_err(),
            "no esperó su turno"
        );
        assert_eq!(plans.summary("s1").unwrap().done, 0);
        drop(writing);
        second.join().unwrap().unwrap();
        assert_eq!(plans.summary("s1").unwrap().done, 1);
    }

    /// Una sesión borrada no vuelve a tener plan, aunque llegue tarde un cambio: `forget` espera
    /// al que esté escribiendo y los de después fallan, sin crear nada en el disco.
    #[test]
    fn a_forgotten_session_is_not_written() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().to_path_buf();
        let plans = std::sync::Arc::new(Plans::new(state.clone()));
        plans.set("s1", plan_of(&[("t1", "pending")]), 1).unwrap();

        // Uno a medias: forget espera a que acabe.
        let w = plans.writer("s1");
        let writing = w.lock().unwrap();
        let p = plans.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let forgetting = std::thread::spawn(move || {
            p.forget("s1");
            let _ = tx.send(());
        });
        assert!(
            rx.recv_timeout(std::time::Duration::from_millis(200))
                .is_err(),
            "forget no esperó"
        );
        drop(writing);
        forgetting.join().unwrap();

        // Se borra la carpeta (como `delete_session`): ya no se escribe ni se lee.
        std::fs::remove_dir_all(state.join("sessions/s1")).unwrap();
        let gone = "la sesión ya no existe";
        assert_eq!(
            plans.set("s1", plan_of(&[("t1", "done")]), 2).unwrap_err(),
            gone
        );
        assert_eq!(
            plans
                .update_task("s1", task_args("t1", "done"), 2)
                .unwrap_err(),
            gone
        );
        assert_eq!(plans.clear("s1").unwrap_err(), gone);
        assert!(!state.join("sessions/s1").exists());
        // Ni aunque alguien deje el fichero.
        Plans::new(state.clone())
            .set("s1", plan_of(&[("t1", "done")]), 3)
            .unwrap();
        assert_eq!(plans.summary("s1"), None);
        assert_eq!(plans.get("s1"), None);
        // Las demás, como siempre.
        plans.set("s2", plan_of(&[("t1", "done")]), 4).unwrap();
        assert_eq!(plans.summary("s2").unwrap().done, 1);
    }

    #[test]
    fn write_failure_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        let plans = Plans::new(state.clone());
        plans
            .set(
                "s1",
                plan_of(&[("t1", "in_progress"), ("t2", "pending")]),
                1,
            )
            .unwrap();
        let before = plans.get("s1").unwrap();
        let rev = plans.summary("s1").unwrap().rev;

        // La carpeta de estado, convertida en fichero.
        std::fs::remove_dir_all(&state).unwrap();
        std::fs::write(&state, "no soy una carpeta").unwrap();

        let e = plans.set("s1", plan_of(&[("t1", "done")]), 2).unwrap_err();
        assert!(e.starts_with("no se pudo guardar el plan: "), "{e}");
        let e = plans
            .update_task("s1", task_args("t2", "done"), 3)
            .unwrap_err();
        assert!(e.starts_with("no se pudo guardar el plan: "), "{e}");
        assert_eq!(plans.get("s1").unwrap(), before);
        assert_eq!(plans.summary("s1").unwrap().rev, rev);

        // Una sesión nueva tampoco se guarda, y no deja nada en memoria.
        let e = plans.set("s2", plan_of(&[("t1", "done")]), 2).unwrap_err();
        assert!(e.starts_with("no se pudo guardar el plan: "), "{e}");
        assert_eq!(plans.get("s2"), None);
        assert_eq!(
            std::fs::read_to_string(&state).unwrap(),
            "no soy una carpeta"
        );
    }
}
