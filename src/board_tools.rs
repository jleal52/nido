//! Las herramientas MCP del tablero de la cuenta: `board`, `project`, `objective`, `join` y
//! `leave`. Lo que ve y cambia una sesión de Claude de los proyectos, los objetivos y los enlaces
//! (sesión ↔ objetivo).
//!
//! Solo desde una sesión de este equipo: no van por el canal «peer», porque cada nido tiene su
//! copia del mismo tablero. Todo pasa por `BoardCache`: antes de nada se sincroniza con el hub,
//! se lee con `decrypted` y se escribe con `create`, `update` y `remove`, que cifran con la clave
//! actual y reintentan ante un 409. Al editar se cambia el objeto leído, en el sitio, así se
//! conserva lo que no conocemos (`extra`, estados nuevos).
//!
//! Lo que no se puede leer nunca se reescribe ni se borra, y de ello solo sale su id y por qué,
//! nunca nada descifrado. Los mensajes se escriben en español y salen traducidos (`i18n`): los
//! errores con `tr`, en la frontera; en el texto de `board`, solo las etiquetas fijas, nunca lo que
//! escribió el usuario.
//!
//! Los objetos se buscan por su id (o sus primeros 8 hex o más, con `#` o sin él) o por su nombre
//! exacto, sin mirar mayúsculas. Un objetivo, dentro de su proyecto si se da; si no, entre todos.

use std::collections::HashMap;
use std::sync::Arc;

use axum::http::StatusCode;
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::board::{
    BlobError, Decrypted, EditError, Entry, Link, LinkSession, MAX_PLAIN, Object, Objective,
    ObjectiveState, PlanSummary, Project, ProjectState, Unreadable, blob_error_kind,
};
use crate::group::Keyring;
use crate::hub::BoardError;
use crate::i18n::{self, Lang};
use crate::mcp_api::{Fail, MAX_READ, Machine, Reply, args_of, fail};
use crate::registry::{Session, now_ms};
use crate::server::{self, AppState};

/// Lo más largo de un nombre (caracteres): sale en cada línea de `board`.
pub const MAX_NAME: usize = 120;
/// Lo más que se enseña de una nota y de una descripción en `board` (caracteres).
const SHOWN_NOTE: usize = 200;
const SHOWN_DESC: usize = 300;
/// Cuántos de los que no se pueden leer se listan en `board` (de los demás, cuántos son).
const SHOWN_UNREADABLE: usize = 20;
/// Cuántos nombres se dan cuando no se encuentra uno.
const SHOWN_NAMES: usize = 10;

const NO_HUB: &str = "este equipo no está conectado a ningún hub: el tablero es de la cuenta del \
                      hub (nido hub set …)";
const NO_HUB_ID: &str = "este equipo aún no tiene id en el hub: prueba en un rato";

// ---------- Quién llama ----------

/// La sesión que llama, como se apunta en el tablero.
#[derive(Debug, Clone, PartialEq)]
pub struct Who {
    /// `"<equipo>/<sesión>"`, con los nombres (`"WSL/nido-hub"`): lo que va en `by` y en el nombre
    /// del enlace, como dice el contrato. Es lo que enseñan la web y la app; la sesión de verdad
    /// va en el enlace, por ids (`machine` e `id`).
    pub by: String,
    /// El id de este equipo en el hub; sin él no hay tablero.
    pub machine: Option<String>,
    /// El id de la sesión en este nido.
    pub id: String,
}

impl Who {
    pub fn of(st: &AppState, s: &Session) -> Self {
        Self {
            by: format!("{}/{}", Machine::of(st).name, s.name),
            machine: st.hub.status.borrow().id.clone(),
            id: s.id.clone(),
        }
    }
}

/// Los enlaces de la sesión `id` del equipo `machine` (abiertos y cerrados). La Tarea 12 sube
/// con ellos el resumen del plan de la sesión.
pub fn session_links<'a>(
    d: &'a Decrypted,
    machine: &'a str,
    id: &'a str,
) -> impl Iterator<Item = (&'a Entry, &'a Link)> + 'a {
    links(d).filter(move |(_, l)| l.session.machine == machine && l.session.id == id)
}

// ---------- Leer el tablero ----------

fn projects(d: &Decrypted) -> impl Iterator<Item = (&Entry, &Project)> {
    d.objects.iter().filter_map(|e| match &e.object {
        Object::Project(p) => Some((e, p)),
        _ => None,
    })
}

fn objectives(d: &Decrypted) -> impl Iterator<Item = (&Entry, &Objective)> {
    d.objects.iter().filter_map(|e| match &e.object {
        Object::Objective(o) => Some((e, o)),
        _ => None,
    })
}

fn links(d: &Decrypted) -> impl Iterator<Item = (&Entry, &Link)> {
    d.objects.iter().filter_map(|e| match &e.object {
        Object::Link(l) => Some((e, l)),
        _ => None,
    })
}

fn project_named<'a>(d: &'a Decrypted, id: &str) -> Option<&'a Project> {
    projects(d).find(|(e, _)| e.id == id).map(|(_, p)| p)
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Kind {
    Project,
    Objective,
}

impl Kind {
    fn of(o: &Object) -> Option<Kind> {
        match o {
            Object::Project(_) => Some(Kind::Project),
            Object::Objective(_) => Some(Kind::Objective),
            _ => None,
        }
    }

    fn one(self) -> &'static str {
        match self {
            Kind::Project => "proyecto",
            Kind::Objective => "objetivo",
        }
    }

    fn many(self) -> &'static str {
        match self {
            Kind::Project => "proyectos",
            Kind::Objective => "objetivos",
        }
    }
}

/// El nombre de un proyecto o de un objetivo.
fn name_of(o: &Object) -> &str {
    match o {
        Object::Project(p) => &p.name,
        Object::Objective(o) => &o.name,
        _ => "",
    }
}

/// Para comparar nombres: sin espacios a los lados y sin mayúsculas (también las acentuadas).
fn norm(s: &str) -> String {
    s.trim().to_lowercase()
}

/// Cómo se enseña un id: `#` y sus primeros 8 hex.
fn short(id: &str) -> String {
    format!("#{}", id.get(..8).unwrap_or(id))
}

/// Lo dado, si puede ser un id o su principio: de 8 a 32 hex (con `#` delante o sin él).
fn as_id(q: &str) -> Option<String> {
    let q = q.trim().trim_start_matches('#').to_ascii_lowercase();
    (q.len() >= 8 && q.len() <= 32 && q.bytes().all(|b| b.is_ascii_hexdigit())).then_some(q)
}

/// El error de algo que no se puede leer: su id y por qué; nada más.
fn unreadable_fail(id: &str, why: Unreadable) -> Fail {
    fail(
        StatusCode::CONFLICT,
        format!("el objeto {id} no se puede leer ({why}): no se cambia ni se borra"),
    )
}

/// Busca un proyecto o un objetivo por su id (o su principio) o por su nombre. Un objetivo por
/// nombre, solo entre los de proyectos que se pueden leer, y dentro de `within` si se da. Lo que
/// no se puede leer da error (`unreadable_fail`); varios, error con los candidatos.
fn find<'a>(
    d: &'a Decrypted,
    q: &str,
    kind: Kind,
    within: Option<&str>,
) -> Result<&'a Entry, Fail> {
    if let Some(id) = as_id(q) {
        let hits: Vec<&Entry> = d.objects.iter().filter(|e| e.id.starts_with(&id)).collect();
        let lost: Vec<&(String, Unreadable)> = d
            .unreadable
            .iter()
            .filter(|(i, _)| i.starts_with(&id))
            .collect();
        let mine: Vec<&Entry> = hits
            .iter()
            .copied()
            .filter(|e| Kind::of(&e.object) == Some(kind))
            .collect();
        match (&mine[..], &lost[..]) {
            ([e], []) => {
                if let (Some(p), Object::Objective(o)) = (within, &e.object)
                    && o.project != p
                {
                    return Err(fail(
                        StatusCode::BAD_REQUEST,
                        format!("{} no es de ese proyecto", short(&e.id)),
                    ));
                }
                return Ok(*e);
            }
            ([], [(i, why)]) => return Err(unreadable_fail(i, *why)),
            ([], []) if !hits.is_empty() => {
                return Err(fail(
                    StatusCode::BAD_REQUEST,
                    format!("{} no es un {}", short(&hits[0].id), kind.one()),
                ));
            }
            ([], []) => {}
            _ => {
                let all: Vec<&str> = mine
                    .iter()
                    .map(|e| e.id.as_str())
                    .chain(lost.iter().map(|(i, _)| i.as_str()))
                    .collect();
                return Err(fail(
                    StatusCode::BAD_REQUEST,
                    format!(
                        "el id {id} es el principio de varios: {}; da más cifras",
                        all.join(", ")
                    ),
                ));
            }
        }
    }
    let q = q.trim();
    let wanted = norm(q);
    let pool: Vec<&Entry> = match kind {
        Kind::Project => projects(d).map(|(e, _)| e).collect(),
        Kind::Objective => objectives(d)
            .filter(|(_, o)| project_named(d, &o.project).is_some())
            .filter(|(_, o)| within.is_none_or(|p| o.project == p))
            .map(|(e, _)| e)
            .collect(),
    };
    let hits: Vec<&Entry> = pool
        .iter()
        .copied()
        .filter(|e| norm(name_of(&e.object)) == wanted)
        .collect();
    match &hits[..] {
        [e] => Ok(*e),
        [] => {
            let names: Vec<String> = pool
                .iter()
                .take(SHOWN_NAMES)
                .map(|e| format!("«{}»", oneline(name_of(&e.object), MAX_NAME)))
                .collect();
            let place = within
                .and_then(|p| project_named(d, p))
                .map(|p| format!(" en «{}»", oneline(&p.name, MAX_NAME)))
                .unwrap_or_default();
            let msg = if names.is_empty() {
                format!("no hay ningún {} «{q}»{place}", kind.one())
            } else {
                let more = if pool.len() > SHOWN_NAMES {
                    ", …"
                } else {
                    ""
                };
                format!(
                    "no hay ningún {} «{q}»{place}; los que hay: {}{more}",
                    kind.one(),
                    names.join(", ")
                )
            };
            Err(fail(StatusCode::NOT_FOUND, msg))
        }
        many => {
            let list: Vec<String> = many
                .iter()
                .map(|e| {
                    let name = oneline(name_of(&e.object), MAX_NAME);
                    match &e.object {
                        // Con su proyecto delante, como una ruta.
                        Object::Objective(o) => {
                            let p = project_named(d, &o.project).map_or("", |p| p.name.as_str());
                            format!("«{}» / «{name}» {}", oneline(p, MAX_NAME), e.id)
                        }
                        _ => format!("«{name}» {}", e.id),
                    }
                })
                .collect();
            Err(fail(
                StatusCode::BAD_REQUEST,
                format!(
                    "hay {} {} «{q}»: {}; usa el id",
                    many.len(),
                    kind.many(),
                    list.join(", ")
                ),
            ))
        }
    }
}

/// ¿Hay ya otro (que no es `but`) con ese nombre? En proyectos, entre todos; en objetivos,
/// dentro de su proyecto.
fn taken<'a>(
    d: &'a Decrypted,
    name: &str,
    kind: Kind,
    within: Option<&str>,
    but: Option<&str>,
) -> Option<&'a Entry> {
    let wanted = norm(name);
    let same = |e: &Entry| Some(e.id.as_str()) != but && norm(name_of(&e.object)) == wanted;
    match kind {
        Kind::Project => projects(d).map(|(e, _)| e).find(|e| same(e)),
        Kind::Objective => objectives(d)
            .filter(|(_, o)| within.is_none_or(|p| o.project == p))
            .map(|(e, _)| e)
            .find(|e| same(e)),
    }
}

fn name_taken(e: &Entry, kind: Kind) -> Fail {
    fail(
        StatusCode::CONFLICT,
        format!(
            "ya hay un {} «{}» ({}): usa otro nombre",
            kind.one(),
            oneline(name_of(&e.object), MAX_NAME),
            e.id
        ),
    )
}

// ---------- Los errores ----------

/// Lo que dijo el hub, para el modelo: qué pasó y qué hacer.
fn board_fail(e: BoardError) -> Fail {
    match e {
        BoardError::Conflict(_) => fail(
            StatusCode::CONFLICT,
            "el objeto cambió a la vez en otro sitio varias veces seguidas: prueba otra vez",
        ),
        BoardError::Full => fail(
            StatusCode::PAYLOAD_TOO_LARGE,
            "el tablero de la cuenta está lleno: borra objetivos o proyectos que ya no hagan falta",
        ),
        BoardError::TooBig => fail(
            StatusCode::PAYLOAD_TOO_LARGE,
            "el objeto es demasiado grande para el hub: acorta la descripción o la nota",
        ),
        BoardError::Busy => fail(
            StatusCode::TOO_MANY_REQUESTS,
            "demasiadas escrituras en el tablero: prueba en un rato",
        ),
        e @ BoardError::Bad(_) => fail(StatusCode::BAD_GATEWAY, e.to_string()),
        BoardError::Hub(e) => fail(StatusCode::SERVICE_UNAVAILABLE, e),
    }
}

/// Un cambio que no salió. Nunca lleva nada descifrado: de `BadObject`, solo su nombre.
fn edit_fail(id: &str, e: EditError) -> Fail {
    match e {
        EditError::NoKey => fail(
            StatusCode::SERVICE_UNAVAILABLE,
            "no hay clave del tablero: prueba otra vez",
        ),
        EditError::Missing => fail(
            StatusCode::NOT_FOUND,
            format!("el objeto {id} ya no está en el tablero"),
        ),
        EditError::Unreadable(why) => unreadable_fail(id, why),
        EditError::Blob(BlobError::TooBig(n)) => fail(
            StatusCode::PAYLOAD_TOO_LARGE,
            format!(
                "demasiado grande: {n} bytes (como mucho {MAX_PLAIN}); acorta la descripción o la \
                 nota"
            ),
        ),
        EditError::Blob(e) => fail(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("no se pudo cifrar el objeto ({})", blob_error_kind(&e)),
        ),
        EditError::Board(e) => board_fail(e),
    }
}

// ---------- Lo común a las herramientas ----------

/// Al día con el hub. Sin hub, error.
async fn synced(st: &AppState) -> Result<(), Fail> {
    if st.hub.url().is_none() {
        return Err(fail(StatusCode::SERVICE_UNAVAILABLE, NO_HUB));
    }
    st.board.sync().await.map(drop).map_err(board_fail)
}

/// Para escribir: al día con el hub y con clave actual (la primera se crea aquí, como en
/// `GET /api/v1/board/keys`: una sesión puede crear el primer proyecto). El llavero y lo leído.
async fn for_writing(st: &Arc<AppState>) -> Result<(Keyring, Decrypted), Fail> {
    synced(st).await?;
    server::ensure_board_key(st)
        .await
        .map_err(|msg| fail(StatusCode::SERVICE_UNAVAILABLE, msg))?;
    let keys = st.group.board_keys();
    let d = st.board.decrypted(&keys);
    Ok((keys, d))
}

/// Un texto opcional dado y con algo (sin espacios a los lados).
fn given(s: &Option<String>) -> Option<&str> {
    s.as_deref().map(str::trim).filter(|s| !s.is_empty())
}

/// Un nombre que vale: sin saltos de línea ni caracteres de control, ni vacío, ni demasiado
/// largo. Sin espacios a los lados.
fn clean_name(s: &str) -> Result<String, Fail> {
    let n = s.trim();
    if n.is_empty() {
        return Err(fail(StatusCode::BAD_REQUEST, "el nombre está vacío"));
    }
    if n.chars().any(char::is_control) {
        return Err(fail(
            StatusCode::BAD_REQUEST,
            "el nombre no puede llevar saltos de línea",
        ));
    }
    if n.chars().count() > MAX_NAME {
        return Err(fail(
            StatusCode::BAD_REQUEST,
            format!("el nombre es demasiado largo (como mucho {MAX_NAME} caracteres)"),
        ));
    }
    Ok(n.to_owned())
}

fn project_state(s: &str) -> Result<ProjectState, Fail> {
    match s.trim().to_ascii_lowercase().as_str() {
        "active" => Ok(ProjectState::Active),
        "archived" => Ok(ProjectState::Archived),
        _ => Err(fail(
            StatusCode::BAD_REQUEST,
            format!(
                "estado de proyecto desconocido «{}»: active o archived",
                s.trim()
            ),
        )),
    }
}

fn objective_state(s: &str) -> Result<ObjectiveState, Fail> {
    match s.trim().to_ascii_lowercase().as_str() {
        "pending" => Ok(ObjectiveState::Pending),
        "doing" => Ok(ObjectiveState::Doing),
        "blocked" => Ok(ObjectiveState::Blocked),
        "done" => Ok(ObjectiveState::Done),
        "dropped" => Ok(ObjectiveState::Dropped),
        _ => Err(fail(
            StatusCode::BAD_REQUEST,
            format!(
                "estado de objetivo desconocido «{}»: pending, doing, blocked, done o dropped",
                s.trim()
            ),
        )),
    }
}

/// Un estado como se escribe en el objeto (`"doing"`, o uno que no conocemos, tal cual).
fn token<T: serde::Serialize>(state: &T) -> String {
    match serde_json::to_value(state) {
        Ok(Value::String(s)) => s,
        _ => String::new(),
    }
}

/// Borra `id`; que ya no estuviera cuenta como hecho.
async fn remove(st: &AppState, keys: &Keyring, id: &str) -> Result<(), Fail> {
    match st.board.remove(keys, id).await {
        Ok(()) | Err(EditError::Missing) => Ok(()),
        Err(e) => Err(edit_fail(id, e)),
    }
}

/// Borra en orden, de las hojas a la raíz: si se corta, lo que queda sigue colgando de lo que no
/// se borró, y volver a pedirlo acaba.
async fn remove_all(st: &AppState, keys: &Keyring, ids: &[&str]) -> Result<(), Fail> {
    for id in ids {
        if let Err(f) = remove(st, keys, id).await {
            return Err(Fail(
                f.0,
                format!(
                    "{} (el borrado se quedó a medias: vuelve a pedirlo para acabar)",
                    f.1
                ),
            ));
        }
    }
    Ok(())
}

/// Los enlaces de los objetivos `of`.
fn links_of<'a>(d: &'a Decrypted, of: &[&str]) -> Vec<&'a str> {
    links(d)
        .filter(|(_, l)| of.contains(&l.objective.as_str()))
        .map(|(e, _)| e.id.as_str())
        .collect()
}

// ---------- `board` ----------

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BoardArgs {
    #[serde(default)]
    project: Option<String>,
    #[serde(default)]
    archived: bool,
}

/// `board`: el tablero en texto compacto (`render`). Si no se puede hablar con el hub pero hay
/// copia, la copia, y se dice; sin hub, o sin copia, el error.
pub async fn tool_board(st: &Arc<AppState>, _s: &Session, args: Value) -> Reply {
    let a: BoardArgs = args_of(args)?;
    let stale = match synced(st).await {
        Ok(()) => None,
        Err(f) if st.hub.url().is_some() && !st.board.ids().is_empty() => Some(f.1),
        Err(f) => return Err(f),
    };
    let d = st.board.decrypted(&st.group.board_keys());
    let only = match given(&a.project) {
        Some(q) => Some(find(&d, q, Kind::Project, None)?.id.clone()),
        None => None,
    };
    let lang = i18n::lang();
    let stale = stale.map(|e| {
        i18n::in_lang(
            lang,
            &format!("sin poder hablar con el hub: {e}; es la copia de este equipo"),
        )
    });
    let view = View {
        only: only.as_deref(),
        archived: a.archived,
        stale: stale.as_deref(),
    };
    Ok(Value::String(render(&d, &view, lang, MAX_READ)))
}

/// Qué enseña `render`.
pub struct View<'a> {
    /// Solo este proyecto (aunque esté archivado), con las descripciones.
    pub only: Option<&'a str>,
    /// También los proyectos archivados.
    pub archived: bool,
    /// Que no está al día, y por qué (ya en su idioma): va arriba, entre paréntesis.
    pub stale: Option<&'a str>,
}

/// Un texto de otros en una línea: los caracteres de control (saltos de línea incluidos), como
/// espacios; y como mucho `max` caracteres (con «…»).
fn oneline(s: &str, max: usize) -> String {
    let s: String = s
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let s = s.trim();
    if s.chars().count() > max {
        let cut: String = s.chars().take(max).collect();
        format!("{}…", cut.trim_end())
    } else {
        s.to_owned()
    }
}

/// Lo hecho de los planes: `(hecho, total)`.
#[derive(Default, Clone, Copy)]
struct Done(usize, usize);

impl Done {
    fn add(&mut self, o: Done) {
        self.0 += o.0;
        self.1 += o.1;
    }

    /// ` · plan 12/20`, si hay algo.
    fn suffix(self) -> String {
        if self.1 == 0 {
            String::new()
        } else {
            format!(" · plan {}/{}", self.0, self.1)
        }
    }
}

/// `12/20, 1 bloqueada` (sin plan, «sin plan»).
fn plan_text(p: Option<&PlanSummary>, t: &dyn Fn(&str) -> String) -> String {
    let Some(p) = p else {
        return t("sin plan");
    };
    let mut s = format!("{}/{}", p.done, p.total);
    if p.blocked > 0 {
        s.push_str(", ");
        s.push_str(&match p.blocked {
            1 => t("1 bloqueada"),
            n => t(&format!("{n} bloqueadas")),
        });
    }
    if p.failed > 0 {
        s.push_str(", ");
        s.push_str(&match p.failed {
            1 => t("1 fallida"),
            n => t(&format!("{n} fallidas")),
        });
    }
    s
}

/// El tablero en texto compacto: por proyecto, sus objetivos (en su orden) con su estado, su
/// nota y las sesiones que trabajan en ellos con lo que llevan de su plan (los enlaces cerrados,
/// marcados, no cuentan en el total). Los proyectos archivados, solo con `archived` o pedidos;
/// los huérfanos, no. Al final, cuántos archivados no salen y lo que no se puede leer (id y por
/// qué). Como mucho `max` bytes, por líneas enteras: lo que no cabe se cuenta.
pub fn render(d: &Decrypted, v: &View, lang: Lang, max: usize) -> String {
    let t = |s: &str| i18n::in_lang(lang, s);
    let archived = |p: &Project| p.state == ProjectState::Archived;
    let mut shown: Vec<(&Entry, &Project)> = projects(d)
        .filter(|(e, p)| match v.only {
            Some(only) => e.id == only,
            None => v.archived || !archived(p),
        })
        .collect();
    shown.sort_by(|(a, pa), (b, pb)| {
        (archived(pa), pa.created, &a.id).cmp(&(archived(pb), pb.created, &b.id))
    });
    let hidden = if v.only.is_none() && !v.archived {
        projects(d).filter(|(_, p)| archived(p)).count()
    } else {
        0
    };
    let mut by_project: HashMap<&str, Vec<(&Entry, &Objective)>> = HashMap::new();
    for (e, o) in objectives(d) {
        by_project
            .entry(o.project.as_str())
            .or_default()
            .push((e, o));
    }
    for list in by_project.values_mut() {
        list.sort_by(|(a, oa), (b, ob)| {
            oa.order
                .total_cmp(&ob.order)
                .then(oa.created.cmp(&ob.created))
                .then(a.id.cmp(&b.id))
        });
    }
    let mut by_objective: HashMap<&str, Vec<&Link>> = HashMap::new();
    for (_, l) in links(d) {
        by_objective
            .entry(l.objective.as_str())
            .or_default()
            .push(l);
    }
    for list in by_objective.values_mut() {
        list.sort_by(|a, b| {
            (a.closed.is_some(), &a.session.name, &a.session.id).cmp(&(
                b.closed.is_some(),
                &b.session.name,
                &b.session.id,
            ))
        });
    }
    let done_of = |o: &str| {
        let mut sum = Done::default();
        for l in by_objective.get(o).into_iter().flatten() {
            if let (None, Some(p)) = (l.closed, &l.plan) {
                sum.add(Done(p.done, p.total));
            }
        }
        sum
    };

    // Los trozos, en orden: cada proyecto (su línea) y cada objetivo (con sus líneas).
    let mut chunks: Vec<(Kind, String)> = Vec::new();
    for (e, p) in &shown {
        let objs = by_project.get(e.id.as_str()).map_or(&[][..], Vec::as_slice);
        let mut total = Done::default();
        for (oe, _) in objs {
            total.add(done_of(oe.id.as_str()));
        }
        let mut s = format!(
            "{} {} [{}]{}\n",
            oneline(&p.name, MAX_NAME),
            short(&e.id),
            token(&p.state),
            total.suffix()
        );
        if v.only.is_some() && !p.desc.trim().is_empty() {
            s.push_str(&format!(
                "    {}{}\n",
                t("descripción: "),
                oneline(&p.desc, SHOWN_DESC)
            ));
        }
        chunks.push((Kind::Project, s));
        for (oe, o) in objs {
            let mut s = format!(
                "  - {} {} [{}]{}\n",
                oneline(&o.name, MAX_NAME),
                short(&oe.id),
                token(&o.state),
                done_of(oe.id.as_str()).suffix()
            );
            if v.only.is_some() && !o.desc.trim().is_empty() {
                s.push_str(&format!(
                    "      {}{}\n",
                    t("descripción: "),
                    oneline(&o.desc, SHOWN_DESC)
                ));
            }
            if !o.note.trim().is_empty() {
                s.push_str(&format!(
                    "      {}{}\n",
                    t("nota: "),
                    oneline(&o.note, SHOWN_NOTE)
                ));
            }
            for l in by_objective.get(oe.id.as_str()).into_iter().flatten() {
                let who = match oneline(&l.session.name, MAX_NAME) {
                    n if n.is_empty() => oneline(&l.session.id, MAX_NAME),
                    n => n,
                };
                let closed = if l.closed.is_some() {
                    format!(" · {}", t("cerrada"))
                } else {
                    String::new()
                };
                s.push_str(&format!(
                    "      {who} · {}{closed}\n",
                    plan_text(l.plan.as_ref(), &t)
                ));
            }
            chunks.push((Kind::Objective, s));
        }
    }

    // Lo que va al final, siempre.
    let mut tail = String::new();
    match hidden {
        0 => {}
        1 => tail.push_str(&t(
            "1 proyecto archivado sin enseñar (archived: true para verlo)",
        )),
        n => tail.push_str(&t(&format!(
            "{n} proyectos archivados sin enseñar (archived: true para verlos)"
        ))),
    }
    if hidden > 0 {
        tail.push('\n');
    }
    if !d.unreadable.is_empty() {
        let mut lost: Vec<&(String, Unreadable)> = d.unreadable.iter().collect();
        lost.sort();
        tail.push_str(&match lost.len() {
            1 => t("No se puede leer 1 objeto:"),
            n => t(&format!("No se pueden leer {n} objetos:")),
        });
        tail.push('\n');
        for (id, why) in lost.iter().take(SHOWN_UNREADABLE) {
            tail.push_str(&format!(
                "  {} {} · {}\n",
                short(id),
                t("(no se puede leer)"),
                t(&why.to_string())
            ));
        }
        if lost.len() > SHOWN_UNREADABLE {
            tail.push_str(&t(&format!("  … y {} más", lost.len() - SHOWN_UNREADABLE)));
            tail.push('\n');
        }
    }

    let mut out = String::new();
    if let Some(s) = v.stale {
        out.push_str(&format!("({s})\n"));
    }
    if chunks.is_empty() && v.only.is_none() && hidden == 0 && d.unreadable.is_empty() {
        out.push_str(&t("El tablero está vacío: crea un proyecto con `project`."));
        out.push('\n');
        return out;
    }
    // Sitio para decir lo que no cabe.
    const CUT_NOTE: usize = 200;
    let room = max.saturating_sub(tail.len() + CUT_NOTE);
    let mut cut = None;
    for (i, (_, s)) in chunks.iter().enumerate() {
        if out.len() + s.len() > room {
            cut = Some(i);
            break;
        }
        out.push_str(s);
    }
    if let Some(i) = cut {
        let rest = &chunks[i..];
        let p = rest.iter().filter(|(k, _)| *k == Kind::Project).count();
        let o = rest.len() - p;
        out.push_str(&t(&format!(
            "… se omiten {o} objetivos y {p} proyectos: pide uno con `project`"
        )));
        out.push('\n');
    }
    out.push_str(&tail);
    out
}

// ---------- `project` ----------

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProjectArgs {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    desc: Option<String>,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    delete: bool,
}

fn project_reply(id: &str, p: &Project) -> Value {
    json!({"ok": true, "id": id, "name": p.name, "state": token(&p.state)})
}

/// `project`: sin `id` crea uno; con `id` (o su nombre) lo cambia o, con `delete`, lo borra con
/// sus objetivos y sus enlaces.
pub async fn tool_project(st: &Arc<AppState>, s: &Session, args: Value) -> Reply {
    let a: ProjectArgs = args_of(args)?;
    let name = a.name.as_deref().map(clean_name).transpose()?;
    let state = given(&a.state).map(project_state).transpose()?;
    let changes = name.is_some() || a.desc.is_some() || state.is_some();
    let Some(q) = given(&a.id) else {
        if a.delete {
            return Err(fail(StatusCode::BAD_REQUEST, "para borrar hace falta id"));
        }
        let Some(name) = name else {
            return Err(fail(
                StatusCode::BAD_REQUEST,
                "para crear un proyecto hace falta name (o id, para cambiar uno)",
            ));
        };
        let who = Who::of(st, s);
        let (keys, d) = for_writing(st).await?;
        if let Some(e) = taken(&d, &name, Kind::Project, None, None) {
            return Err(name_taken(e, Kind::Project));
        }
        let now = now_ms();
        let p = Project {
            name,
            desc: a.desc.unwrap_or_default(),
            state: state.unwrap_or(ProjectState::Active),
            created: now,
            changed: now,
            by: who.by,
            extra: Map::new(),
        };
        let id = st
            .board
            .create(&keys, &Object::Project(p.clone()))
            .await
            .map_err(|e| edit_fail("", e))?;
        return Ok(project_reply(&id, &p));
    };
    if a.delete && changes {
        return Err(fail(
            StatusCode::BAD_REQUEST,
            "delete va solo, sin otros cambios",
        ));
    }
    if !a.delete && !changes {
        return Err(fail(
            StatusCode::BAD_REQUEST,
            "nada que cambiar: da name, desc, state o delete",
        ));
    }
    let who = Who::of(st, s);
    let (keys, d) = for_writing(st).await?;
    let id = find(&d, q, Kind::Project, None)?.id.clone();
    if a.delete {
        let objs: Vec<&str> = objectives(&d)
            .filter(|(_, o)| o.project == id)
            .map(|(e, _)| e.id.as_str())
            .collect();
        let ls = links_of(&d, &objs);
        let all: Vec<&str> = ls
            .iter()
            .chain(&objs)
            .copied()
            .chain([id.as_str()])
            .collect();
        remove_all(st, &keys, &all).await?;
        return Ok(json!({"ok": true, "deleted": id, "objectives": objs.len(),
            "links": ls.len()}));
    }
    if let Some(e) = name
        .as_deref()
        .and_then(|n| taken(&d, n, Kind::Project, None, Some(&id)))
    {
        return Err(name_taken(e, Kind::Project));
    }
    let desc = a.desc;
    let obj = st
        .board
        .update(&keys, &id, |o| {
            if let Object::Project(p) = o {
                if let Some(n) = &name {
                    p.name = n.clone();
                }
                if let Some(x) = &desc {
                    p.desc = x.clone();
                }
                if let Some(x) = &state {
                    p.state = x.clone();
                }
                p.changed = now_ms();
                p.by = who.by.clone();
            }
        })
        .await
        .map_err(|e| edit_fail(&id, e))?;
    match obj {
        Object::Project(p) => Ok(project_reply(&id, &p)),
        _ => Err(fail(
            StatusCode::BAD_REQUEST,
            format!("{} no es un proyecto", short(&id)),
        )),
    }
}

// ---------- `objective` ----------

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ObjectiveArgs {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    project: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    desc: Option<String>,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    note: Option<String>,
    #[serde(default)]
    order: Option<f64>,
    #[serde(default)]
    delete: bool,
}

fn objective_reply(id: &str, o: &Objective) -> Value {
    json!({"ok": true, "id": id, "project": o.project, "name": o.name,
        "state": token(&o.state), "order": o.order})
}

/// `objective`: sin `id` crea uno en `project` (al final, si no se da `order`); con `id` (o su
/// nombre, en `project` si se da) lo cambia o, con `delete`, lo borra con sus enlaces.
pub async fn tool_objective(st: &Arc<AppState>, s: &Session, args: Value) -> Reply {
    let a: ObjectiveArgs = args_of(args)?;
    let name = a.name.as_deref().map(clean_name).transpose()?;
    let state = given(&a.state).map(objective_state).transpose()?;
    if a.order.is_some_and(|o| !o.is_finite()) {
        return Err(fail(
            StatusCode::BAD_REQUEST,
            "order tiene que ser un número",
        ));
    }
    let changes = name.is_some()
        || a.desc.is_some()
        || state.is_some()
        || a.note.is_some()
        || a.order.is_some();
    let Some(q) = given(&a.id) else {
        if a.delete {
            return Err(fail(StatusCode::BAD_REQUEST, "para borrar hace falta id"));
        }
        let Some(pq) = given(&a.project) else {
            return Err(fail(
                StatusCode::BAD_REQUEST,
                "para crear un objetivo hace falta project (o id, para cambiar uno)",
            ));
        };
        let Some(name) = name else {
            return Err(fail(
                StatusCode::BAD_REQUEST,
                "para crear un objetivo hace falta name (o id, para cambiar uno)",
            ));
        };
        let who = Who::of(st, s);
        let (keys, d) = for_writing(st).await?;
        let pid = find(&d, pq, Kind::Project, None)?.id.clone();
        if let Some(e) = taken(&d, &name, Kind::Objective, Some(&pid), None) {
            return Err(name_taken(e, Kind::Objective));
        }
        let order = a.order.unwrap_or_else(|| {
            objectives(&d)
                .filter(|(_, o)| o.project == pid)
                .map(|(_, o)| o.order)
                .filter(|o| o.is_finite())
                .fold(0.0, f64::max)
                .floor()
                + 1.0
        });
        let now = now_ms();
        let o = Objective {
            project: pid,
            name,
            desc: a.desc.unwrap_or_default(),
            state: state.unwrap_or(ObjectiveState::Pending),
            note: a.note.unwrap_or_default(),
            order,
            created: now,
            changed: now,
            by: who.by,
            extra: Map::new(),
        };
        let id = st
            .board
            .create(&keys, &Object::Objective(o.clone()))
            .await
            .map_err(|e| edit_fail("", e))?;
        return Ok(objective_reply(&id, &o));
    };
    if a.delete && changes {
        return Err(fail(
            StatusCode::BAD_REQUEST,
            "delete va solo, sin otros cambios",
        ));
    }
    if !a.delete && !changes {
        return Err(fail(
            StatusCode::BAD_REQUEST,
            "nada que cambiar: da name, desc, state, note, order o delete",
        ));
    }
    let who = Who::of(st, s);
    let (keys, d) = for_writing(st).await?;
    let within = match given(&a.project) {
        Some(pq) => Some(find(&d, pq, Kind::Project, None)?.id.clone()),
        None => None,
    };
    let e = find(&d, q, Kind::Objective, within.as_deref())?;
    let id = e.id.clone();
    if a.delete {
        let ls = links_of(&d, &[id.as_str()]);
        let all: Vec<&str> = ls.iter().copied().chain([id.as_str()]).collect();
        remove_all(st, &keys, &all).await?;
        return Ok(json!({"ok": true, "deleted": id, "links": ls.len()}));
    }
    if let (Some(n), Object::Objective(o)) = (&name, &e.object)
        && let Some(other) = taken(&d, n, Kind::Objective, Some(&o.project), Some(&id))
    {
        return Err(name_taken(other, Kind::Objective));
    }
    let (desc, note, order) = (a.desc, a.note, a.order);
    let obj = st
        .board
        .update(&keys, &id, |o| {
            if let Object::Objective(o) = o {
                if let Some(n) = &name {
                    o.name = n.clone();
                }
                if let Some(x) = &desc {
                    o.desc = x.clone();
                }
                if let Some(x) = &state {
                    o.state = x.clone();
                }
                if let Some(x) = &note {
                    o.note = x.clone();
                }
                if let Some(x) = order {
                    o.order = x;
                }
                o.changed = now_ms();
                o.by = who.by.clone();
            }
        })
        .await
        .map_err(|e| edit_fail(&id, e))?;
    match obj {
        Object::Objective(o) => Ok(objective_reply(&id, &o)),
        _ => Err(fail(
            StatusCode::BAD_REQUEST,
            format!("{} no es un objetivo", short(&id)),
        )),
    }
}

// ---------- `join` y `leave` ----------

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct JoinArgs {
    objective: String,
}

/// `join`: un enlace del objetivo con la sesión que llama. Si ya tiene uno abierto, nada; si
/// tiene uno cerrado (la Tarea 12 los cierra al borrar la sesión), se vuelve a abrir. El resumen
/// del plan (`plan`, `copied`) lo sube la Tarea 12 (`board_progress`): aquí va vacío.
pub async fn tool_join(st: &Arc<AppState>, s: &Session, args: Value) -> Reply {
    let a: JoinArgs = args_of(args)?;
    let who = Who::of(st, s);
    let (keys, d) = for_writing(st).await?;
    let machine = who
        .machine
        .clone()
        .ok_or_else(|| fail(StatusCode::SERVICE_UNAVAILABLE, NO_HUB_ID))?;
    let o = find(&d, &a.objective, Kind::Objective, None)?;
    let oid = o.id.clone();
    let name = name_of(&o.object).to_owned();
    let mine: Vec<(&Entry, &Link)> = session_links(&d, &machine, &who.id)
        .filter(|(_, l)| l.objective == oid)
        .collect();
    let reply = |link: &str, already: bool| json!({"ok": true, "objective": oid, "name": name, "link": link, "already": already});
    if let Some((e, _)) = mine.iter().find(|(_, l)| l.closed.is_none()) {
        return Ok(reply(&e.id, true));
    }
    if let Some((e, _)) = mine.first() {
        let lid = e.id.clone();
        st.board
            .update(&keys, &lid, |o| {
                if let Object::Link(l) = o {
                    l.closed = None;
                    l.session.name = who.by.clone();
                    l.changed = now_ms();
                    l.by = who.by.clone();
                }
            })
            .await
            .map_err(|e| edit_fail(&lid, e))?;
        return Ok(reply(&lid, false));
    }
    let now = now_ms();
    let link = Link {
        objective: oid.clone(),
        session: LinkSession {
            machine,
            id: who.id.clone(),
            name: who.by.clone(),
            extra: Map::new(),
        },
        // Lo pone la Tarea 12 (`board_progress`) con el resumen del plan de la sesión.
        plan: None,
        copied: 0,
        closed: None,
        created: now,
        changed: now,
        by: who.by,
        extra: Map::new(),
    };
    let lid = st
        .board
        .create(&keys, &Object::Link(link))
        .await
        .map_err(|e| edit_fail("", e))?;
    Ok(reply(&lid, false))
}

/// `leave`: quita los enlaces de la sesión que llama con el objetivo. Se borran, no se cierran:
/// `closed` es cuándo se cerró la sesión (lo pone la Tarea 12), y esta sigue.
pub async fn tool_leave(st: &Arc<AppState>, s: &Session, args: Value) -> Reply {
    let a: JoinArgs = args_of(args)?;
    let who = Who::of(st, s);
    synced(st).await?;
    let keys = st.group.board_keys();
    let d = st.board.decrypted(&keys);
    let machine = who
        .machine
        .clone()
        .ok_or_else(|| fail(StatusCode::SERVICE_UNAVAILABLE, NO_HUB_ID))?;
    let oid = find(&d, &a.objective, Kind::Objective, None)?.id.clone();
    let mine: Vec<&str> = session_links(&d, &machine, &who.id)
        .filter(|(_, l)| l.objective == oid)
        .map(|(e, _)| e.id.as_str())
        .collect();
    remove_all(st, &keys, &mine).await?;
    Ok(json!({"ok": true, "objective": oid, "removed": mine.len()}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::sync::Mutex;
    use std::time::Duration;

    use base64::Engine;
    use base64::engine::general_purpose::STANDARD as B64;
    use serde_json::{Map, json};

    use crate::board::{
        self, Entry, Link, LinkSession, Object, Objective, ObjectiveState, PlanSummary, Project,
        ProjectState, Unreadable,
    };
    use crate::hub;
    use crate::i18n;
    use crate::mcp_api::{Fail, MAX_READ, Machine};
    use crate::registry::{ClaudeLink, new_id};

    type FakeBoard = Arc<Mutex<hub::fake::Board>>;

    /// Un nido conectado al hub de mentira, con una sesión «api».
    struct World {
        st: Arc<AppState>,
        s: Session,
        board: FakeBoard,
        _rec: hub::fake::Recorder,
        _tmp: tempfile::TempDir,
    }

    fn session(dir: &Path, name: &str) -> Session {
        Session {
            id: new_id(),
            name: name.into(),
            cwd: dir.to_owned(),
            argv: vec!["claude".into()],
            env: vec![],
            autostart: false,
            created: 0,
            claude: Some(ClaudeLink {
                session_id: "conv".into(),
                config_dir: Some(dir.join("claude")),
                account: None,
                inside_shell: false,
            }),
            folder: None,
            finished: 0,
            stopped: 0,
            seen: 0,
            opened_by: None,
            alerts: vec![],
        }
    }

    fn state(dir: &Path) -> Arc<AppState> {
        let paths = crate::launch::Paths {
            runtime: dir.join("run"),
            data: dir.join("data"),
            state: dir.join("state"),
            config: dir.join("config"),
            home: dir.to_owned(),
            server: None,
        };
        AppState::new(paths, crate::launch::Launcher::Detached, None, true).unwrap()
    }

    async fn world() -> World {
        let tmp = tempfile::tempdir().unwrap();
        let st = state(tmp.path());
        let s = st
            .registry
            .lock()
            .await
            .add(session(tmp.path(), "api"))
            .unwrap();
        let board: FakeBoard = Default::default();
        let rec = hub::fake::board(board.clone()).await;
        hub::fake::connect(&st.hub, st.clone(), rec.addr).await;
        World {
            st,
            s,
            board,
            _rec: rec,
            _tmp: tmp,
        }
    }

    impl World {
        async fn call(&self, tool: &str, args: Value) -> Reply {
            self.call_as(&self.s, tool, args).await
        }

        async fn call_as(&self, s: &Session, tool: &str, args: Value) -> Reply {
            match tool {
                "board" => tool_board(&self.st, s, args).await,
                "project" => tool_project(&self.st, s, args).await,
                "objective" => tool_objective(&self.st, s, args).await,
                "join" => tool_join(&self.st, s, args).await,
                "leave" => tool_leave(&self.st, s, args).await,
                _ => panic!("{tool}"),
            }
        }

        /// Lo que da `board`, en español.
        async fn text(&self, args: Value) -> String {
            let r = i18n::scope(Lang::Es, tool_board(&self.st, &self.s, args)).await;
            r.unwrap().as_str().unwrap().to_owned()
        }

        /// Cómo se llama una sesión de aquí en `by` y en los enlaces.
        fn by(&self, s: &Session) -> String {
            format!("{}/{}", Machine::of(&self.st).name, s.name)
        }

        fn key(&self) -> (String, [u8; 32]) {
            let k = self.st.group.ensure_board_key().unwrap();
            (
                k.kid.clone(),
                B64.decode(&k.key).unwrap().try_into().unwrap(),
            )
        }

        /// Otro cliente escribe `id` en el hub, con la clave actual.
        fn put(&self, id: &str, v: Value) {
            let (kid, raw) = self.key();
            let blob = board::seal(&raw, id, v.to_string().as_bytes()).unwrap();
            self.board.lock().unwrap().write(id, Some((&kid, &blob)));
        }

        /// `id` tal como está en el hub, descifrado; `None` si es una lápida o no está.
        fn stored(&self, id: &str) -> Option<Value> {
            let item = self.board.lock().unwrap().items.get(id).cloned()?;
            if item.gone {
                return None;
            }
            let ring = self.st.group.board_keys();
            let k = ring.keys.iter().find(|k| k.kid == item.kid).unwrap();
            let raw: [u8; 32] = B64.decode(&k.key).unwrap().try_into().unwrap();
            let text = board::open(&raw, id, item.blob.as_deref().unwrap()).unwrap();
            Some(serde_json::from_slice(&text).unwrap())
        }

        /// Los objetos vivos del hub de este tipo: id y texto.
        fn live(&self, t: &str) -> Vec<(String, Value)> {
            let ids: Vec<String> = self.board.lock().unwrap().items.keys().cloned().collect();
            ids.into_iter()
                .filter_map(|id| self.stored(&id).map(|v| (id, v)))
                .filter(|(_, v)| v["t"] == t)
                .collect()
        }

        fn writes(&self) -> usize {
            self.board.lock().unwrap().writes.len()
        }
    }

    fn id_of(r: &Value) -> String {
        r["id"].as_str().unwrap().to_owned()
    }

    fn has(e: &Fail, want: &str) -> bool {
        e.1.contains(want)
    }

    /// Crear, editar y borrar proyectos y objetivos; `board` lo enseña; borrar un proyecto se
    /// lleva sus objetivos y sus enlaces.
    #[tokio::test]
    async fn create_change_and_delete() {
        let _limit = crate::watchdog::limit(Duration::from_secs(60));
        let w = world().await;
        let me = w.by(&w.s);

        let r = w
            .call(
                "project",
                json!({"name": "nido 0.10", "desc": "La versión del tablero"}),
            )
            .await
            .unwrap();
        let p = id_of(&r);
        assert!(board::is_id(&p), "{r}");
        assert_eq!(
            (&r["name"], &r["state"]),
            (&json!("nido 0.10"), &json!("active"))
        );
        let v = w.stored(&p).unwrap();
        assert_eq!(v["t"], "project");
        assert_eq!(v["name"], "nido 0.10");
        assert_eq!(v["desc"], "La versión del tablero");
        assert_eq!(v["state"], "active");
        assert_eq!(v["by"], me.as_str());
        let created = v["created"].as_u64().unwrap();
        assert!(created > 0);
        assert_eq!(v["changed"], created);

        // Objetivos: el proyecto por su nombre (sin mayúsculas) o por el principio de su id; van
        // al final.
        let r = w
            .call(
                "objective",
                json!({"project": "NIDO 0.10", "name": "Herramientas MCP"}),
            )
            .await
            .unwrap();
        let o1 = id_of(&r);
        let v = w.stored(&o1).unwrap();
        assert_eq!(v["t"], "objective");
        assert_eq!(v["project"], p.as_str());
        assert_eq!(v["state"], "pending");
        assert_eq!(v["order"], 1.0);
        assert_eq!(v["by"], me.as_str());
        let r = w
            .call(
                "objective",
                json!({"project": format!("#{}", &p[..8]), "name": "Resumen del plan"}),
            )
            .await
            .unwrap();
        let o2 = id_of(&r);
        assert_eq!(w.stored(&o2).unwrap()["order"], 2.0);

        // Editar: solo lo dado; lo demás queda.
        let before = w.stored(&o1).unwrap();
        let r = w
            .call(
                "objective",
                json!({"project": "nido 0.10", "id": "herramientas mcp", "state": "doing",
                    "note": "falta la web"}),
            )
            .await
            .unwrap();
        assert_eq!((&r["id"], &r["state"]), (&json!(o1), &json!("doing")));
        let v = w.stored(&o1).unwrap();
        assert_eq!(
            (&v["state"], &v["note"]),
            (&json!("doing"), &json!("falta la web"))
        );
        assert_eq!(v["name"], "Herramientas MCP");
        assert_eq!(v["created"], before["created"]);
        assert!(v["changed"].as_u64() >= before["changed"].as_u64());
        // Sin proyecto, se busca en todos.
        w.call("objective", json!({"id": "Resumen del plan", "order": 0.5}))
            .await
            .unwrap();
        assert_eq!(w.stored(&o2).unwrap()["order"], 0.5);
        w.call(
            "project",
            json!({"id": "nido 0.10", "name": "nido 0.10.0", "state": "archived"}),
        )
        .await
        .unwrap();
        let v = w.stored(&p).unwrap();
        assert_eq!(
            (&v["name"], &v["state"]),
            (&json!("nido 0.10.0"), &json!("archived"))
        );
        assert_eq!(v["desc"], "La versión del tablero");

        // `board`: los archivados, solo con `archived` (o pidiéndolo).
        let t = w.text(json!({})).await;
        assert!(!t.contains("nido 0.10.0"), "{t}");
        assert!(t.contains("archivado"), "{t}");
        let t = w.text(json!({"archived": true})).await;
        assert!(t.contains("nido 0.10.0"), "{t}");
        assert!(t.contains("Herramientas MCP"), "{t}");
        assert!(t.contains("[doing]") && t.contains("falta la web"), "{t}");
        // El orden: el de 0.5 antes.
        assert!(
            t.find("Resumen del plan") < t.find("Herramientas MCP"),
            "{t}"
        );
        let t = w.text(json!({"project": "nido 0.10.0"})).await;
        assert!(t.contains("La versión del tablero"), "{t}");

        // Borrar un objetivo; borrar el proyecto se lleva sus objetivos y sus enlaces.
        w.call("join", json!({"objective": "Herramientas MCP"}))
            .await
            .unwrap();
        let r = w
            .call("objective", json!({"id": o2, "delete": true}))
            .await
            .unwrap();
        assert_eq!((&r["deleted"], &r["links"]), (&json!(o2), &json!(0)));
        assert!(w.stored(&o2).is_none());
        let r = w
            .call("project", json!({"id": "nido 0.10.0", "delete": true}))
            .await
            .unwrap();
        assert_eq!(
            (&r["deleted"], &r["objectives"], &r["links"]),
            (&json!(p), &json!(1), &json!(1))
        );
        for t in ["project", "objective", "link"] {
            assert!(w.live(t).is_empty(), "{t}");
        }
        let t = w.text(json!({"archived": true})).await;
        assert!(t.contains("vacío"), "{t}");
    }

    /// `by` es la sesión que llama, con el nombre del equipo y el de la sesión; `created` no
    /// cambia al editar.
    #[tokio::test]
    async fn by_is_the_calling_session() {
        let _limit = crate::watchdog::limit(Duration::from_secs(60));
        let w = world().await;
        let other =
            w.st.registry
                .lock()
                .await
                .add(session(w._tmp.path(), "otra"))
                .unwrap();
        let p = id_of(&w.call("project", json!({"name": "P"})).await.unwrap());
        let first = w.stored(&p).unwrap();
        assert_eq!(first["by"], w.by(&w.s).as_str());
        w.call_as(&other, "project", json!({"id": "P", "desc": "de la otra"}))
            .await
            .unwrap();
        let v = w.stored(&p).unwrap();
        assert_eq!(v["by"], w.by(&other).as_str());
        assert_ne!(w.by(&other), w.by(&w.s));
        assert_eq!(v["created"], first["created"]);
        let o = id_of(
            &w.call_as(&other, "objective", json!({"project": "P", "name": "O"}))
                .await
                .unwrap(),
        );
        assert_eq!(w.stored(&o).unwrap()["by"], w.by(&other).as_str());
        w.call_as(&other, "join", json!({"objective": "O"}))
            .await
            .unwrap();
        let links = w.live("link");
        assert_eq!(links[0].1["by"], w.by(&other).as_str());
        assert_eq!(links[0].1["session"]["id"], other.id.as_str());
    }

    /// `join` crea un enlace con la sesión que llama (sin plan: lo pone la Tarea 12) y no lo
    /// repite; `leave` lo quita. Uno cerrado de la misma sesión se vuelve a abrir.
    #[tokio::test]
    async fn join_and_leave() {
        let _limit = crate::watchdog::limit(Duration::from_secs(60));
        let w = world().await;
        let me = w.by(&w.s);
        w.call("project", json!({"name": "P"})).await.unwrap();
        let o = id_of(
            &w.call("objective", json!({"project": "P", "name": "O"}))
                .await
                .unwrap(),
        );

        let r = w.call("join", json!({"objective": "o"})).await.unwrap();
        assert_eq!((&r["objective"], &r["already"]), (&json!(o), &json!(false)));
        let links = w.live("link");
        assert_eq!(links.len(), 1);
        let (lid, l) = &links[0];
        assert_eq!(l["objective"], o.as_str());
        assert_eq!(
            l["session"],
            json!({"machine": "este", "id": w.s.id, "name": me})
        );
        assert_eq!((&l["plan"], &l["closed"]), (&Value::Null, &Value::Null));
        assert_eq!(l["by"], me.as_str());

        // Otra vez: nada.
        let writes = w.writes();
        let r = w.call("join", json!({"objective": o})).await.unwrap();
        assert_eq!(r["already"], true);
        assert_eq!(w.writes(), writes);
        assert_eq!(w.live("link").len(), 1);
        let t = w.text(json!({})).await;
        assert!(t.contains(&format!("{me} · sin plan")), "{t}");

        let r = w.call("leave", json!({"objective": "O"})).await.unwrap();
        assert_eq!(r["removed"], 1);
        assert!(w.live("link").is_empty());
        assert!(w.stored(lid).is_none());
        let r = w.call("leave", json!({"objective": "O"})).await.unwrap();
        assert_eq!(r["removed"], 0);

        // Uno cerrado (lo cierra la Tarea 12 al borrar la sesión) se vuelve a abrir.
        let closed = "c".repeat(32);
        w.put(
            &closed,
            json!({"t": "link", "objective": o, "session": {"machine": "este", "id": w.s.id,
                "name": "viejo"}, "plan": {"done": 1, "total": 2}, "copied": 5, "closed": 9,
                "created": 1, "changed": 9, "by": "web"}),
        );
        let r = w.call("join", json!({"objective": "O"})).await.unwrap();
        assert_eq!(r["already"], false);
        let links = w.live("link");
        assert_eq!(links.len(), 1, "{links:?}");
        assert_eq!(links[0].0, closed);
        let l = &links[0].1;
        assert_eq!(l["closed"], Value::Null);
        assert_eq!(l["session"]["name"], me.as_str());
        assert_eq!(l["plan"]["done"], 1, "el resumen se queda");
        // El enlace de otra sesión no cuenta.
        let theirs = "d".repeat(32);
        w.put(
            &theirs,
            json!({"t": "link", "objective": o, "session": {"machine": "mac", "id": w.s.id,
                "name": "Mac/x"}, "created": 1, "changed": 1}),
        );
        let r = w.call("leave", json!({"objective": o})).await.unwrap();
        assert_eq!(r["removed"], 1);
        assert_eq!(w.live("link").len(), 1);
        assert!(w.stored(&theirs).is_some());
    }

    /// Por nombre, sin mayúsculas; si hay varios, error con los candidatos (nombre e id).
    #[tokio::test]
    async fn ambiguous_and_missing_names() {
        let _limit = crate::watchdog::limit(Duration::from_secs(60));
        let w = world().await;
        let (a, b, c, d) = (
            "a".repeat(32),
            "b".repeat(32),
            "c".repeat(32),
            "d".repeat(32),
        );
        w.put(
            &a,
            json!({"t": "project", "name": "Web", "state": "active", "created": 1, "changed": 1}),
        );
        w.put(
            &b,
            json!({"t": "project", "name": "web", "state": "active", "created": 2, "changed": 2}),
        );
        let e = w
            .call("project", json!({"id": "WEB", "state": "archived"}))
            .await
            .unwrap_err();
        assert_eq!(e.0, StatusCode::BAD_REQUEST, "{}", e.1);
        assert!(
            has(&e, &a) && has(&e, &b) && has(&e, "«Web»") && has(&e, "«web»"),
            "{}",
            e.1
        );
        // Por su id (o su principio), sí.
        w.call("project", json!({"id": &a[..8], "state": "archived"}))
            .await
            .unwrap();
        assert_eq!(w.stored(&a).unwrap()["state"], "archived");
        assert_eq!(w.stored(&b).unwrap()["state"], "active");

        // Dos objetivos que se llaman igual en dos proyectos.
        w.put(
            &c,
            json!({"t": "objective", "project": a, "name": "Docs", "state": "pending",
                "created": 1, "changed": 1}),
        );
        w.put(
            &d,
            json!({"t": "objective", "project": b, "name": "docs", "state": "pending",
                "created": 1, "changed": 1}),
        );
        let e = w
            .call("join", json!({"objective": "DOCS"}))
            .await
            .unwrap_err();
        assert_eq!(e.0, StatusCode::BAD_REQUEST, "{}", e.1);
        assert!(has(&e, &c) && has(&e, &d), "{}", e.1);
        assert!(
            has(&e, "«Web»") && has(&e, "«web»"),
            "con su proyecto: {}",
            e.1
        );
        let e = w
            .call("objective", json!({"id": "docs", "state": "done"}))
            .await
            .unwrap_err();
        assert!(has(&e, &c) && has(&e, &d), "{}", e.1);
        // Dentro de su proyecto no hay duda.
        w.call(
            "objective",
            json!({"project": b, "id": "DOCS", "state": "done"}),
        )
        .await
        .unwrap();
        assert_eq!(w.stored(&d).unwrap()["state"], "done");
        assert_eq!(w.stored(&c).unwrap()["state"], "pending");
        // Por id, pero de otro proyecto: no.
        let e = w
            .call(
                "objective",
                json!({"project": a, "id": &d[..8], "state": "pending"}),
            )
            .await
            .unwrap_err();
        assert_eq!(e.0, StatusCode::BAD_REQUEST, "{}", e.1);
        assert!(has(&e, "no es de ese proyecto"), "{}", e.1);

        // No se crea otro con el mismo nombre (ni se renombra a uno que ya está).
        let e = w.call("project", json!({"name": "WEB"})).await.unwrap_err();
        assert_eq!(e.0, StatusCode::CONFLICT, "{}", e.1);
        let e = w
            .call("objective", json!({"project": a, "name": "docs"}))
            .await
            .unwrap_err();
        assert_eq!(e.0, StatusCode::CONFLICT, "{}", e.1);
        let n = id_of(&w.call("project", json!({"name": "Nuevo"})).await.unwrap());
        let e = w
            .call("project", json!({"id": n, "name": "nuevo"}))
            .await
            .unwrap();
        assert_eq!(e["name"], "nuevo", "el mismo puede cambiar sus mayúsculas");
        let e = w
            .call("project", json!({"id": "Nuevo", "name": "Web"}))
            .await
            .unwrap_err();
        assert_eq!(e.0, StatusCode::CONFLICT, "{}", e.1);

        // Lo que no está, con los que hay; un id de otro tipo.
        let e = w
            .call("project", json!({"id": "nadie", "state": "active"}))
            .await
            .unwrap_err();
        assert_eq!(e.0, StatusCode::NOT_FOUND, "{}", e.1);
        assert!(has(&e, "nadie") && has(&e, "Web"), "{}", e.1);
        let e = w
            .call(
                "objective",
                json!({"project": a, "id": "Nada", "state": "done"}),
            )
            .await
            .unwrap_err();
        assert_eq!(e.0, StatusCode::NOT_FOUND, "{}", e.1);
        let e = w
            .call("project", json!({"id": c, "state": "active"}))
            .await
            .unwrap_err();
        assert_eq!(e.0, StatusCode::BAD_REQUEST, "{}", e.1);
        assert!(has(&e, "no es un proyecto"), "{}", e.1);
    }

    /// Lo que no conocemos (de una versión futura) se queda al editar.
    #[tokio::test]
    async fn an_edit_keeps_what_we_do_not_know() {
        let _limit = crate::watchdog::limit(Duration::from_secs(60));
        let w = world().await;
        let (p, o) = ("1".repeat(32), "2".repeat(32));
        w.put(
            &p,
            json!({"t": "project", "name": "P", "state": "active", "created": 1, "changed": 1,
                "futuro": {"x": [1, 2]}}),
        );
        w.put(
            &o,
            json!({"t": "objective", "project": p, "name": "O", "state": "pausado",
                "created": 1, "changed": 1, "color": "verde"}),
        );
        w.call("project", json!({"id": "P", "desc": "d"}))
            .await
            .unwrap();
        let v = w.stored(&p).unwrap();
        assert_eq!(v["futuro"], json!({"x": [1, 2]}));
        assert_eq!(v["desc"], "d");
        w.call("objective", json!({"id": "O", "note": "n"}))
            .await
            .unwrap();
        let v = w.stored(&o).unwrap();
        assert_eq!(v["color"], "verde");
        assert_eq!(v["state"], "pausado", "un estado que no conocemos se queda");
        assert_eq!(v["note"], "n");
        w.call("join", json!({"objective": "O"})).await.unwrap();
        let lid = w.live("link")[0].0.clone();
        let mut l = w.stored(&lid).unwrap();
        l["session"]["futuro"] = json!(1);
        l["closed"] = json!(3);
        w.put(&lid, l);
        w.call("join", json!({"objective": "O"})).await.unwrap();
        assert_eq!(w.stored(&lid).unwrap()["session"]["futuro"], 1);
    }

    /// Lo que no se puede leer sale como «(no se puede leer)», con su id y por qué, y nunca se
    /// edita ni se borra; nada de lo descifrado sale en los errores.
    #[tokio::test]
    async fn unreadable_objects_are_shown_but_never_touched() {
        let _limit = crate::watchdog::limit(Duration::from_secs(60));
        let w = world().await;
        w.put(
            &"1".repeat(32),
            json!({"t": "project", "name": "Visible", "state": "active", "created": 1,
                "changed": 1}),
        );
        // Con una clave que no está en el llavero.
        let other = [9u8; 32];
        let hidden = "e".repeat(32);
        let blob = board::seal(
            &other,
            &hidden,
            br#"{"t":"project","name":"SECRETO","state":"active","created":1,"changed":1}"#,
        )
        .unwrap();
        w.board
            .lock()
            .unwrap()
            .write(&hidden, Some((&board::kid(&other), &blob)));
        // Se descifra, pero no es un objeto (sin `state`): serde diría trozos del texto.
        let bad = "f".repeat(32);
        w.put(
            &bad,
            json!({"t": "project", "name": "SECRETO", "created": 1, "changed": 1}),
        );

        let t = w.text(json!({})).await;
        assert!(t.contains("Visible"), "{t}");
        assert!(
            t.contains("#eeeeeeee (no se puede leer) · sin su clave"),
            "{t}"
        );
        assert!(
            t.contains("#ffffffff (no se puede leer) · no es un objeto"),
            "{t}"
        );
        assert!(!t.contains("SECRETO"), "{t}");

        let items = w.board.lock().unwrap().items.clone();
        let writes = w.writes();
        let tries: Vec<(&str, Value)> = vec![
            ("project", json!({"id": hidden, "state": "archived"})),
            ("project", json!({"id": hidden, "delete": true})),
            ("project", json!({"id": bad, "name": "x"})),
            ("project", json!({"id": &bad[..8], "delete": true})),
            ("objective", json!({"id": hidden, "delete": true})),
            ("objective", json!({"project": hidden, "name": "x"})),
            ("join", json!({"objective": bad})),
        ];
        for (tool, args) in tries {
            let e = w.call(tool, args.clone()).await.unwrap_err();
            assert_eq!(e.0, StatusCode::CONFLICT, "{tool} {args}: {}", e.1);
            assert!(has(&e, "no se puede leer"), "{}", e.1);
            assert!(has(&e, &hidden) || has(&e, &bad), "{}", e.1);
            assert!(!has(&e, "SECRETO") && !has(&e, "missing"), "{}", e.1);
        }
        assert_eq!(w.writes(), writes);
        assert_eq!(w.board.lock().unwrap().items, items);
    }

    /// Los errores del hub, con mensajes que dicen qué hacer; y lo que no vale, antes de
    /// escribir.
    #[tokio::test]
    async fn hub_errors_and_bad_arguments() {
        let _limit = crate::watchdog::limit(Duration::from_secs(60));
        let w = world().await;
        w.board.lock().unwrap().busy = 1;
        let e = w.call("project", json!({"name": "P"})).await.unwrap_err();
        assert_eq!(e.0, StatusCode::TOO_MANY_REQUESTS, "{}", e.1);
        assert!(has(&e, "prueba en un rato"), "{}", e.1);
        w.board.lock().unwrap().limit = 0;
        let e = w.call("project", json!({"name": "P"})).await.unwrap_err();
        assert_eq!(e.0, StatusCode::PAYLOAD_TOO_LARGE, "{}", e.1);
        assert!(has(&e, "lleno"), "{}", e.1);
        w.board.lock().unwrap().limit = 2000;
        let e = w
            .call("project", json!({"name": "P", "desc": "x".repeat(9000)}))
            .await
            .unwrap_err();
        assert_eq!(e.0, StatusCode::PAYLOAD_TOO_LARGE, "{}", e.1);
        assert!(has(&e, "demasiado grande"), "{}", e.1);

        // Un 409 tras otro: se rinde y lo dice.
        let p = id_of(&w.call("project", json!({"name": "P"})).await.unwrap());
        {
            let mut b = w.board.lock().unwrap();
            let cur = b.items[&p].clone();
            for _ in 0..board::MAX_TRIES {
                b.clash(&p, Some((cur.kid.clone(), cur.blob.clone().unwrap())));
            }
        }
        let e = w
            .call("project", json!({"id": "P", "desc": "d"}))
            .await
            .unwrap_err();
        assert_eq!(e.0, StatusCode::CONFLICT, "{}", e.1);
        assert!(has(&e, "prueba otra vez"), "{}", e.1);

        let writes = w.writes();
        let bad: Vec<(&str, Value, &str)> = vec![
            ("project", json!({}), "name"),
            ("project", json!({"name": " "}), "vacío"),
            ("project", json!({"name": "a\nb"}), "saltos de línea"),
            ("project", json!({"name": "x".repeat(121)}), "largo"),
            ("project", json!({"name": "Q", "state": "hecho"}), "active"),
            ("project", json!({"id": "P"}), "nada que cambiar"),
            (
                "project",
                json!({"id": "P", "delete": true, "desc": "x"}),
                "delete",
            ),
            ("project", json!({"delete": true}), "id"),
            ("objective", json!({"name": "O"}), "project"),
            ("objective", json!({"project": "P"}), "name"),
            (
                "objective",
                json!({"project": "P", "name": "O", "state": "hecho"}),
                "pending",
            ),
            (
                "project",
                json!({"name": "Q", "otro": 1}),
                "argumentos no válidos",
            ),
            ("join", json!({}), "argumentos no válidos"),
        ];
        for (tool, args, want) in bad {
            let e = w.call(tool, args.clone()).await.unwrap_err();
            assert_eq!(e.0, StatusCode::BAD_REQUEST, "{tool} {args}: {}", e.1);
            assert!(has(&e, want), "{tool} {args}: {}", e.1);
        }
        assert_eq!(w.writes(), writes);
    }

    /// Sin hub, un error claro (no un tablero vacío).
    #[tokio::test]
    async fn without_a_hub_it_says_so() {
        let _limit = crate::watchdog::limit(Duration::from_secs(60));
        let tmp = tempfile::tempdir().unwrap();
        let st = state(tmp.path());
        let s = st
            .registry
            .lock()
            .await
            .add(session(tmp.path(), "api"))
            .unwrap();
        for (tool, args) in [
            ("board", json!({})),
            ("project", json!({"name": "P"})),
            ("join", json!({"objective": "O"})),
        ] {
            let e = match tool {
                "board" => tool_board(&st, &s, args).await,
                "project" => tool_project(&st, &s, args).await,
                _ => tool_join(&st, &s, args).await,
            }
            .unwrap_err();
            assert_eq!(e.0, StatusCode::SERVICE_UNAVAILABLE, "{tool}");
            assert!(has(&e, "ningún hub"), "{tool}: {}", e.1);
        }
        assert!(st.group.board_keys().keys.is_empty(), "no crea clave");
    }

    // ---------- `render`, sin hub ----------

    fn project(name: &str, state: ProjectState, created: u64) -> Object {
        Object::Project(Project {
            name: name.into(),
            desc: String::new(),
            state,
            created,
            changed: created,
            by: "web".into(),
            extra: Map::new(),
        })
    }

    fn objective(p: &str, name: &str, state: ObjectiveState, note: &str, order: f64) -> Object {
        Object::Objective(Objective {
            project: p.into(),
            name: name.into(),
            desc: String::new(),
            state,
            note: note.into(),
            order,
            created: 1,
            changed: 1,
            by: "web".into(),
            extra: Map::new(),
        })
    }

    fn link(o: &str, name: &str, plan: Option<(usize, usize, usize)>, closed: bool) -> Object {
        Object::Link(Link {
            objective: o.into(),
            session: LinkSession {
                machine: "m".into(),
                id: "abcd1234".into(),
                name: name.into(),
                extra: Map::new(),
            },
            plan: plan.map(|(done, total, blocked)| PlanSummary {
                title: None,
                done,
                total,
                active: 0,
                blocked,
                failed: 0,
                extra: Map::new(),
            }),
            copied: 0,
            closed: closed.then_some(5),
            created: 1,
            changed: 1,
            by: "WSL/x".into(),
            extra: Map::new(),
        })
    }

    fn entry(id: String, object: Object) -> Entry {
        Entry { id, v: 1, object }
    }

    fn hex(n: usize) -> String {
        format!("{n:032x}")
    }

    /// Un id de muestra que se distingue por su principio: `"1a"` → `"1a1a…"`.
    fn sid(c: &str) -> String {
        c.repeat(16)
    }

    fn sample() -> Decrypted {
        let (p1, p2, p3) = (sid("1a"), sid("2b"), sid("3c"));
        let (o1, o2, o3) = (sid("11"), sid("12"), sid("13"));
        let o4 = sid("14");
        Decrypted {
            objects: vec![
                entry(p2.clone(), project("Web", ProjectState::Active, 2)),
                entry(p1.clone(), project("nido 0.10", ProjectState::Active, 1)),
                entry(p3.clone(), project("Viejo", ProjectState::Archived, 3)),
                entry(
                    o2.clone(),
                    objective(&p1, "Resumen del plan", ObjectiveState::Pending, "", 2.0),
                ),
                entry(
                    o1.clone(),
                    objective(
                        &p1,
                        "Herramientas MCP",
                        ObjectiveState::Doing,
                        "falta la\nweb",
                        1.0,
                    ),
                ),
                entry(
                    o3.clone(),
                    objective(&p2, "Cliente", ObjectiveState::Blocked, "", 1.0),
                ),
                entry(
                    o4.clone(),
                    objective(&p3, "Antiguo", ObjectiveState::Done, "", 1.0),
                ),
                entry(
                    sid("21"),
                    link(&o1, "WSL/nido-hub", Some((12, 20, 1)), false),
                ),
                entry(sid("22"), link(&o1, "Mac/app", None, false)),
                entry(sid("23"), link(&o1, "WSL/vieja", Some((3, 3, 0)), true)),
                entry(sid("24"), link(&o3, "Mac/web", Some((2, 7, 2)), false)),
                // Huérfanos: no salen.
                entry(
                    sid("31"),
                    objective(&sid("99"), "Perdido", ObjectiveState::Pending, "", 1.0),
                ),
                entry(sid("32"), link(&sid("98"), "WSL/x", None, false)),
            ],
            unreadable: vec![("e".repeat(32), Unreadable::NoKey)],
        }
    }

    /// La salida de `board`, entera (es el ejemplo del informe).
    #[test]
    fn the_board_text() {
        let view = View {
            only: None,
            archived: false,
            stale: None,
        };
        let t = render(&sample(), &view, Lang::Es, MAX_READ);
        let want = "\
nido 0.10 #1a1a1a1a [active] · plan 12/20
  - Herramientas MCP #11111111 [doing] · plan 12/20
      nota: falta la web
      Mac/app · sin plan
      WSL/nido-hub · 12/20, 1 bloqueada
      WSL/vieja · 3/3 · cerrada
  - Resumen del plan #12121212 [pending]
Web #2b2b2b2b [active] · plan 2/7
  - Cliente #13131313 [blocked] · plan 2/7
      Mac/web · 2/7, 2 bloqueadas
1 proyecto archivado sin enseñar (archived: true para verlo)
No se puede leer 1 objeto:
  #eeeeeeee (no se puede leer) · sin su clave
";
        assert_eq!(t, want, "\n{t}");
        let en = render(&sample(), &view, Lang::En, MAX_READ);
        assert!(en.contains("WSL/nido-hub · 12/20, 1 blocked"), "{en}");
        assert!(
            en.contains("#eeeeeeee (unreadable) · without its key"),
            "{en}"
        );
        assert!(en.contains("note: falta la web"), "{en}");
    }

    /// Con un proyecto, solo ese (aunque esté archivado) y con las descripciones; con
    /// `archived`, también los archivados.
    #[test]
    fn one_project_or_the_archived_too() {
        let mut d = sample();
        if let Object::Project(p) = &mut d.objects[2].object {
            p.desc = "Lo de antes".into();
        }
        let p3 = sid("3c");
        let t = render(
            &d,
            &View {
                only: Some(&p3),
                archived: false,
                stale: None,
            },
            Lang::Es,
            MAX_READ,
        );
        assert!(t.starts_with("Viejo #3c3c3c3c [archived]\n"), "{t}");
        assert!(t.contains("    descripción: Lo de antes"), "{t}");
        assert!(t.contains("Antiguo") && !t.contains("Web"), "{t}");
        let t = render(
            &d,
            &View {
                only: None,
                archived: true,
                stale: Some("sin conexión con el hub"),
            },
            Lang::Es,
            MAX_READ,
        );
        assert!(t.starts_with("(sin conexión con el hub)\n"), "{t}");
        assert!(t.contains("Viejo") && t.contains("Web"), "{t}");
        assert!(!t.contains("archivado sin enseñar"), "{t}");
        let empty = render(
            &Decrypted::default(),
            &View {
                only: None,
                archived: false,
                stale: None,
            },
            Lang::Es,
            MAX_READ,
        );
        assert_eq!(
            empty,
            "El tablero está vacío: crea un proyecto con `project`.\n"
        );
    }

    /// Con 2000 objetos (lo más que cabe en una cuenta) la salida entera sería enorme: se corta
    /// en `MAX_READ` y se dice cuánto falta.
    #[test]
    fn a_full_board_is_cut() {
        let mut d = Decrypted::default();
        let mut n = 0;
        let mut next = || {
            n += 1;
            hex(n)
        };
        for p in 0..40 {
            let pid = next();
            d.objects.push(entry(
                pid.clone(),
                project(
                    &format!("Proyecto número {p} con un nombre normal"),
                    ProjectState::Active,
                    p,
                ),
            ));
            for o in 0..24 {
                let oid = next();
                d.objects.push(entry(
                    oid.clone(),
                    objective(
                        &pid,
                        &format!("Objetivo {o} del proyecto {p}, con algo de texto"),
                        ObjectiveState::Doing,
                        "una nota corta de cómo va, con un poco de detalle",
                        o as f64,
                    ),
                ));
                d.objects.push(entry(
                    next(),
                    link(&oid, "WSL/nido-hub", Some((3, 9, 1)), false),
                ));
            }
        }
        while d.objects.len() + d.unreadable.len() < 2000 {
            let id = next();
            d.unreadable.push((id, Unreadable::Broken));
        }
        assert_eq!(d.objects.len() + d.unreadable.len(), 2000);
        let view = View {
            only: None,
            archived: false,
            stale: None,
        };
        let whole = render(&d, &view, Lang::Es, usize::MAX);
        eprintln!("board con 2000 objetos, sin cortar: {} bytes", whole.len());
        assert!(whole.len() > MAX_READ);
        let cut = render(&d, &view, Lang::Es, MAX_READ);
        eprintln!("cortado: {} bytes", cut.len());
        assert!(cut.len() <= MAX_READ, "{}", cut.len());
        assert!(cut.contains("se omiten"), "{}", &cut[cut.len() - 400..]);
        assert!(cut.contains("pide uno con `project`"));
        // Lo que no se puede leer se dice siempre, aunque no quepa la lista.
        assert!(
            cut.contains("No se pueden leer 40 objetos:"),
            "{}",
            &cut[cut.len() - 600..]
        );
        assert!(cut.contains("… y 20 más"));
        // Por líneas enteras.
        assert!(cut.ends_with('\n'));
    }
}
