//! El plan de fases y tareas que lleva el agente de una sesión (herramientas `plan_*` del MCP):
//! lo que manda, comprobado entero antes de guardarlo, con las horas que pone nido.

use serde::{Deserialize, Serialize};
use std::collections::HashSet;

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
}
