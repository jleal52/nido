//! Los menús de Claude Code en pantalla (sus preguntas, los permisos, aprobar el plan, confiar
//! en la carpeta…), para contestarlos con botones desde la web y la app en vez de con el teclado.
//!
//! [`parse`] reconoce el menú en el texto de la pantalla; [`answer`] lo contesta pulsando las
//! teclas que tocan, y antes de cada pulsación comprueba en la pantalla que sigue ahí lo que se
//! quiere contestar: si algo ha cambiado (otra pregunta, ya contestada), falla sin pulsar nada.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncWrite};

use crate::proto::{self, Frame, HistoryRequest};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// Una pregunta de Claude (AskUserQuestion), una a una si son varias.
    Question,
    /// Repaso de las respuestas antes de mandarlas.
    Review,
    /// Permiso para una herramienta («Do you want to proceed?»).
    Permission,
    /// Aprobar el plan («Would you like to proceed?»).
    Plan,
    /// Se acabó el límite de uso: qué hacer («What do you want to do?»).
    Limit,
    /// Cualquier otro (confiar en la carpeta, `/model`…).
    Menu,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Opt {
    /// La tecla que la elige; `None` en los menús sin números (se llega con las flechas).
    pub n: Option<u32>,
    pub label: String,
    /// Lo que va debajo: su descripción (o el resto de la etiqueta, si no cabe en una línea).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Donde está el cursor.
    pub selected: bool,
    /// En las preguntas de varias respuestas: si está marcada.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checked: Option<bool>,
    /// Es un campo de texto («Type something», «Tell Claude what to change»).
    pub input: bool,
    /// En una pregunta con vista previa (el recuadro a la derecha de las opciones): la suya.
    /// La pantalla solo enseña la de la opción del cursor; las demás las pone `nido serve` desde
    /// la transcripción.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Tab {
    pub label: String,
    /// Ya contestada.
    pub done: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Menu {
    pub kind: Kind,
    /// La pregunta o lo que se pide («Do you want to proceed?»).
    pub title: Option<String>,
    /// Las preguntas, cuando Claude hace varias a la vez.
    pub tabs: Vec<Tab>,
    pub options: Vec<Opt>,
    /// Se pueden marcar varias.
    pub multi: bool,
    /// La línea de ayuda de abajo («Esc to cancel · Tab to amend»).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
    /// En un permiso, lo que Claude enseña encima de la pregunta: la herramienta, su descripción,
    /// la orden y los avisos (ver `permission_context`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
    /// Por dónde pasa el cursor, en orden: las opciones y la fila «Next»/«Submit».
    #[serde(skip)]
    rows: Vec<Row>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Row {
    Opt(usize),
    /// «Next» o «Submit» de las de varias respuestas.
    Nav {
        selected: bool,
    },
}

impl Menu {
    fn selected_row(&self) -> Option<usize> {
        self.rows.iter().position(|r| match *r {
            Row::Opt(i) => self.options[i].selected,
            Row::Nav { selected } => selected,
        })
    }

    fn row_of(&self, opt: usize) -> Option<usize> {
        self.rows.iter().position(|r| *r == Row::Opt(opt))
    }

    fn nav_row(&self) -> Option<usize> {
        self.rows.iter().position(|r| matches!(r, Row::Nav { .. }))
    }

    /// La opción que se quiere, por su etiqueta (la de la pantalla puede venir partida en varias
    /// líneas si no cabe: se compara con lo que lleva debajo).
    pub fn find(&self, want: &str) -> Option<usize> {
        let w = norm(want);
        if w.is_empty() {
            return None;
        }
        self.options
            .iter()
            .position(|o| norm(&o.label) == w)
            .or_else(|| {
                self.options.iter().position(|o| {
                    let full = norm(&format!(
                        "{} {}",
                        o.label,
                        o.detail.as_deref().unwrap_or("")
                    ));
                    full.starts_with(&w)
                        && full[w.len()..]
                            .chars()
                            .next()
                            .is_none_or(|c| !c.is_alphanumeric())
                })
            })
    }

    fn input(&self) -> Option<usize> {
        self.options.iter().position(|o| o.input)
    }

    fn shows_question(&self, question: &str) -> bool {
        self.kind == Kind::Question
            && self.title.as_deref().is_some_and(|t| {
                let (t, q) = (norm(t), norm(question));
                !q.is_empty() && (t == q || t.starts_with(&q) || q.starts_with(&t))
            })
    }

    /// Las vistas previas de todas las opciones, de la llamada a AskUserQuestion (`ask`: sus
    /// `questions`, como las da la transcripción): la pantalla solo enseña la de la opción del
    /// cursor, y partida en líneas. La pregunta es la del título (o, si no se reconoce, la que
    /// tenga todas las opciones de la pantalla); cada opción, la de su etiqueta («Tilde» vale
    /// para «Tilde (Recommended)»). Lo que no case se queda como estaba.
    pub fn fill_previews(&mut self, ask: &serde_json::Value) {
        use serde_json::Value;
        if self.kind != Kind::Question {
            return;
        }
        let Some(questions) = ask.get("questions").and_then(Value::as_array) else {
            return;
        };
        let options = |q: &Value| -> Vec<(String, Option<String>)> {
            q.get("options")
                .and_then(Value::as_array)
                .map(|o| o.as_slice())
                .unwrap_or_default()
                .iter()
                .map(|o| {
                    let label = o.get("label").and_then(Value::as_str).unwrap_or_default();
                    let preview = o
                        .get("preview")
                        .and_then(Value::as_str)
                        .filter(|p| !p.trim().is_empty())
                        .map(|p| p.trim_end().to_owned());
                    (norm(label), preview)
                })
                .collect()
        };
        let find = |opts: &[(String, Option<String>)], o: &Opt| -> Option<usize> {
            let label = norm(o.label.trim_end_matches('…'));
            if label.is_empty() {
                return None;
            }
            opts.iter()
                .position(|(l, _)| *l == label)
                .or_else(|| opts.iter().position(|(l, _)| l.starts_with(&label)))
        };
        let chosen = |o: &&Opt| !o.input && !o.label.starts_with("Chat about this");
        let q = questions
            .iter()
            .find(|q| {
                q.get("question")
                    .and_then(Value::as_str)
                    .is_some_and(|t| self.shows_question(t))
            })
            .or_else(|| {
                questions.iter().find(|q| {
                    let opts = options(q);
                    self.options.iter().filter(chosen).count() > 0
                        && self
                            .options
                            .iter()
                            .filter(chosen)
                            .all(|o| find(&opts, o).is_some())
                })
            });
        let Some(q) = q else {
            return;
        };
        let opts = options(q);
        for o in self.options.iter_mut() {
            if !o.input
                && let Some(i) = find(&opts, o)
                && let Some(p) = &opts[i].1
            {
                o.preview = Some(p.clone());
            }
        }
    }
}

/// Minúsculas y los espacios juntos, para comparar lo de la pantalla (que puede venir partido en
/// varias líneas) con lo que se pide.
fn norm(s: &str) -> String {
    s.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Sin los colores (la pantalla llega con secuencias SGR).
fn strip_sgr(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for c in chars.by_ref() {
                    if c.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

/// El texto empieza atenuado (SGR 2), mirando todas las secuencias de color que tenga delante
/// (`ESC[39;2m`, `ESC[7mP…`: el cursor encima de la primera letra no cuenta). El 2 de un color
/// RGB (`38;2;r;g;b`) no es atenuado.
fn starts_dim(s: &str) -> bool {
    let mut dim = false;
    let mut rest = s;
    loop {
        let Some(seq) = rest.strip_prefix("\x1b[") else {
            break;
        };
        let Some(end) = seq.find(|c: char| c.is_ascii_alphabetic()) else {
            break;
        };
        if &seq[end..end + 1] == "m" {
            dim = sgr_dim(&seq[..end], dim);
        }
        rest = &seq[end + 1..];
    }
    if dim {
        return true;
    }
    // El cursor (inverso) sobre la primera letra y luego atenuado.
    if let Some(after) = rest.strip_prefix(|_: char| true)
        && after.starts_with("\x1b[")
        && s.contains("\x1b[7m")
    {
        return starts_dim(after);
    }
    false
}

/// Si sigue atenuado tras los parámetros `ps` de una secuencia SGR (`2;38;2;1;2;3`): el 2 de un
/// color RGB (`38;2;r;g;b`) no es atenuado.
fn sgr_dim(ps: &str, mut dim: bool) -> bool {
    let ps: Vec<&str> = ps.split(';').collect();
    let mut i = 0;
    while i < ps.len() {
        match ps[i] {
            "38" | "48" | "58" => i += if ps.get(i + 1) == Some(&"2") { 5 } else { 2 },
            "" | "0" | "22" => dim = false,
            "2" => dim = true,
            _ => {}
        }
        i += 1;
    }
    dim
}

/// Solo el texto que no está atenuado (sin colores): lo escrito, sin las ayudas de Claude.
fn undimmed(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut dim = false;
    let mut rest = s;
    while let Some(c) = rest.chars().next() {
        if let Some(seq) = rest.strip_prefix("\x1b[") {
            let Some(end) = seq.find(|c: char| c.is_ascii_alphabetic()) else {
                break;
            };
            if &seq[end..end + 1] == "m" {
                dim = sgr_dim(&seq[..end], dim);
            }
            rest = &seq[end + 1..];
            continue;
        }
        if c != '\x1b' && !dim {
            out.push(c);
        }
        rest = &rest[c.len_utf8()..];
    }
    out
}

/// Columna (en caracteres) del primer carácter que no es espacio.
fn indent(l: &str) -> usize {
    l.chars().take_while(|c| *c == ' ').count()
}

fn is_rule_char(c: char) -> bool {
    matches!(c, '─' | '╌' | '▔' | '━' | '═')
}

/// Una raya: solo de rayas, o con un rótulo dentro (Claude pone ahí el nombre de la sesión,
/// `-n`: «──────── hija-b ─»). Con rótulo, estricta para que no pase una línea de texto: rayas
/// desde el principio (al menos `RULE_LEFT`), un espacio, el rótulo (como mucho `LABEL_MAX`), un
/// espacio y una o dos rayas hasta el final (Claude pinta siempre una). En una pantalla estrecha
/// (el teléfono) el nombre puede ser más largo que las rayas de la izquierda.
fn is_rule(l: &str) -> bool {
    const RULE_LEFT: usize = 3;
    const LABEL_MAX: usize = 64;
    let t = l.trim();
    if t.chars().count() >= 6 && t.chars().all(is_rule_char) {
        return true;
    }
    let left = t.chars().take_while(|&c| is_rule_char(c)).count();
    let right = t.chars().rev().take_while(|&c| is_rule_char(c)).count();
    let total = t.chars().count();
    if left < RULE_LEFT || !(1..=2).contains(&right) || left + right + 2 >= total {
        return false;
    }
    let label: String = t.chars().skip(left).take(total - left - right).collect();
    let Some(label) = label.strip_prefix(' ').and_then(|r| r.strip_suffix(' ')) else {
        return false;
    };
    !label.is_empty()
        && label.trim() == label
        && !label.chars().any(is_rule_char)
        && label.chars().count() <= LABEL_MAX
}

/// `❯ 3. Texto` → (seleccionada, 3, columna donde empieza «Texto», «Texto»).
fn option_line(l: &str) -> Option<(bool, u32, usize, String)> {
    let t = l.trim_start();
    let mut col = indent(l);
    let (selected, t) = match t.strip_prefix('❯') {
        Some(rest) => {
            let r = rest.trim_start();
            col += 1 + rest.chars().count() - r.chars().count();
            (true, r)
        }
        None => (false, t),
    };
    let digits: String = t.chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() || digits.len() > 2 {
        return None;
    }
    let rest = t[digits.len()..].strip_prefix(". ")?;
    let text = rest.trim_start();
    col += digits.len() + 2 + rest.chars().count() - text.chars().count();
    if text.is_empty() {
        return None;
    }
    Some((
        selected,
        digits.parse().ok()?,
        col,
        text.trim_end().to_owned(),
    ))
}

/// «Next» o «Submit» solos en su línea (con o sin cursor).
fn nav_line(l: &str) -> Option<bool> {
    let t = l.trim();
    let (selected, t) = match t.strip_prefix('❯') {
        Some(r) => (true, r.trim()),
        None => (false, t),
    };
    matches!(t, "Next" | "Submit").then_some(selected)
}

fn is_hint(l: &str) -> bool {
    [
        "Esc to cancel",
        "Enter to select",
        "Enter to confirm",
        "to navigate",
        "Tab to amend",
        "ctrl+g to edit",
    ]
    .iter()
    .any(|h| l.contains(h))
}

const INPUTS: [&str; 2] = ["Type something", "Tell Claude what to change"];

/// El menú que haya abajo en la pantalla, si hay uno.
pub fn parse(screen: &str) -> Option<Menu> {
    let plain = strip_sgr(screen);
    let lines: Vec<&str> = plain.lines().map(str::trim_end).collect();
    numbered(&lines).or_else(|| unnumbered(&lines))
}

fn numbered(lines: &[&str]) -> Option<Menu> {
    // El menú está abajo: se buscan sus opciones de la última hacia arriba hasta la 1.
    let last = lines.iter().rposition(|l| option_line(l).is_some())?;
    let (_, top_n, _, _) = option_line(lines[last])?;
    let mut found = vec![last];
    let mut want = top_n;
    for i in (0..last).rev() {
        if want == 1 {
            break;
        }
        if let Some((_, n, _, _)) = option_line(lines[i]) {
            if n != want - 1 {
                return None;
            }
            found.push(i);
            want = n;
        } else if last - i > 60 {
            return None;
        }
    }
    if want != 1 || found.len() < 2 {
        return None;
    }
    // Con un menú de verdad no está la caja de escribir; si sigue debajo, la lista es otra cosa
    // (un mensaje tuyo en cola, que Claude pinta con «❯» delante).
    let edge = |l: &str| l.trim_start().starts_with("──────");
    if (last + 1..lines.len()).any(|i| lines[i].trim_start().starts_with('❯') && edge(lines[i - 1]))
    {
        return None;
    }
    found.reverse();

    // Con vista previa, Claude pinta a la derecha de las opciones un recuadro con la de la opción
    // del cursor (y debajo, «Notes: press n to add notes»): se quita de las líneas antes de leerlas.
    let shown = preview_box(lines, found[0]);
    let cut: Vec<String>;
    let lines: Vec<&str> = match &shown {
        Some(b) => {
            cut = lines
                .iter()
                .enumerate()
                .map(|(i, l)| {
                    if (b.top..b.end).contains(&i) {
                        l.chars()
                            .take(b.col)
                            .collect::<String>()
                            .trim_end()
                            .to_owned()
                    } else {
                        (*l).to_owned()
                    }
                })
                .collect();
            cut.iter().map(String::as_str).collect()
        }
        None => lines.to_vec(),
    };
    let lines = lines.as_slice();

    // Cada opción con lo que lleva debajo; la fila «Next»/«Submit»; lo que hay al final.
    let mut options = Vec::new();
    let mut rows = Vec::new();
    let mut hint = None;
    for (k, &at) in found.iter().enumerate() {
        let (selected, n, col, text) = option_line(lines[at])?;
        let end = found.get(k + 1).copied().unwrap_or(lines.len());
        let mut detail = Vec::new();
        let mut done = false;
        for l in &lines[at + 1..end] {
            if let Some(sel) = nav_line(l) {
                rows.push(Row::Opt(options.len()));
                rows.push(Row::Nav { selected: sel });
                done = true;
                continue;
            }
            if is_hint(l) {
                hint.get_or_insert_with(|| l.trim().to_owned());
                continue;
            }
            if done || l.trim().is_empty() || is_rule(l) || indent(l) < col {
                if !detail.is_empty() || l.trim().is_empty() || is_rule(l) {
                    done |= !detail.is_empty();
                }
                continue;
            }
            detail.push(l.trim());
        }
        if !rows.contains(&Row::Opt(options.len())) {
            rows.push(Row::Opt(options.len()));
        }
        let (checked, label) = if let Some(r) = text.strip_prefix("[ ] ") {
            (Some(false), r)
        } else if let Some(r) = text.strip_prefix("[✔] ") {
            (Some(true), r)
        } else {
            (None, text.as_str())
        };
        // «Rojo ✔»: la ya elegida al volver a una pregunta contestada.
        let label = label.strip_suffix(" ✔").unwrap_or(label).trim().to_owned();
        options.push(Opt {
            n: Some(n),
            input: INPUTS.iter().any(|p| label.starts_with(p)),
            label,
            detail: (!detail.is_empty()).then(|| detail.join(" ")),
            selected,
            checked,
            preview: shown.as_ref().filter(|_| selected).map(|b| b.text.clone()),
        });
    }
    // Lo que no esté justo debajo de la última opción: la ayuda.
    for l in &lines[found[found.len() - 1] + 1..] {
        if is_hint(l) {
            hint.get_or_insert_with(|| l.trim().to_owned());
        }
    }

    // Lo de arriba: la pregunta (las líneas seguidas justo encima) y las pestañas.
    let first = found[0];
    let mut title = Vec::new();
    let mut tabs = Vec::new();
    let mut i = first;
    while i > 0 && lines[i - 1].trim().is_empty() {
        i -= 1;
    }
    while i > 0 && !lines[i - 1].trim().is_empty() && !is_rule(lines[i - 1]) && title.len() < 6 {
        if is_tabs(lines[i - 1]) {
            break;
        }
        // Claude pinta la pregunta con una barra a la izquierda («│ ¿Qué…»): no es del texto.
        title.push(lines[i - 1].trim().trim_start_matches('│').trim_start());
        i -= 1;
    }
    title.reverse();
    for l in lines[first.saturating_sub(12)..first].iter().rev() {
        if is_tabs(l) {
            tabs = parse_tabs(l);
            break;
        }
        if is_rule(l) {
            break;
        }
    }
    let title = (!title.is_empty()).then(|| title.join(" "));

    // En una pregunta, el campo de texto es la última opción antes de «Chat about this» (aunque
    // ya se haya escrito en él y no diga «Type something»).
    let chat = options
        .iter()
        .position(|o| o.label.starts_with("Chat about this"));
    let question = !tabs.is_empty() || chat.is_some();
    if question {
        let last_choice = match chat {
            Some(c) if c > 0 => Some(c - 1),
            _ => None,
        };
        if let Some(i) = last_choice {
            options[i].input = true;
        }
    }
    let t = title.as_deref().unwrap_or("");
    let kind = if options.iter().any(|o| o.label == "Submit answers") {
        Kind::Review
    } else if question {
        Kind::Question
    } else if t.starts_with("Do you want to") {
        Kind::Permission
    } else if t.contains("Would you like to proceed") {
        Kind::Plan
    } else if options.iter().any(|o| {
        [
            "limit to reset",
            "continue automatically",
            "limit reset",
            "Upgrade your plan",
        ]
        .iter()
        .any(|p| o.label.contains(p))
    }) {
        Kind::Limit
    } else {
        Kind::Menu
    };
    // En un permiso, lo de encima de la pregunta: qué se está aprobando.
    let context = (kind == Kind::Permission)
        .then(|| permission_context(&lines[..i]))
        .flatten();
    let menu = Menu {
        kind,
        title,
        tabs,
        multi: options.iter().any(|o| o.checked.is_some()),
        options,
        hint,
        context,
        rows,
    };
    // Un menú de verdad tiene el cursor en alguna fila (una lista numerada de Claude, no).
    menu.selected_row().is_some().then_some(menu)
}

/// El recuadro de la vista previa de una pregunta: su columna, de qué línea a cuál hay que
/// cortar (`top..end`: hasta la raya de debajo de las opciones, por el «Notes:») y lo de dentro.
struct PreviewBox {
    col: usize,
    top: usize,
    end: usize,
    text: String,
}

/// Busca el recuadro (`┌…┐` / `│…│` / `└…┘`) a la derecha de las opciones, desde la primera
/// (`first`).
fn preview_box(lines: &[&str], first: usize) -> Option<PreviewBox> {
    let (top, col) = lines
        .iter()
        .enumerate()
        .skip(first)
        .take(4)
        .find_map(|(i, l)| {
            let col = l.chars().position(|c| c == '┌')?;
            let before: String = l.chars().take(col).collect();
            (col > 0 && before.ends_with(' ') && !before.trim().is_empty()).then_some((i, col))
        })?;
    let at = |l: &str| l.chars().nth(col);
    let bottom = (top + 1..lines.len()).find(|&i| at(lines[i]) != Some('│'))?;
    if at(lines[bottom]) != Some('└') {
        return None;
    }
    let text = lines[top + 1..bottom]
        .iter()
        .map(|l| {
            let inner: String = l.chars().skip(col + 1).collect();
            let inner = inner.trim_end();
            let inner = inner.strip_suffix('│').unwrap_or(inner);
            inner
                .strip_prefix(' ')
                .unwrap_or(inner)
                .trim_end()
                .to_owned()
        })
        .collect::<Vec<_>>();
    // Sin la sangría que tengan todas las líneas en común.
    let common = text
        .iter()
        .filter(|l| !l.is_empty())
        .map(|l| indent(l))
        .min()
        .unwrap_or(0);
    let text = text
        .iter()
        .map(|l| l.chars().skip(common).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n");
    let end = (bottom + 1..lines.len())
        .find(|&i| is_rule(lines[i]))
        .unwrap_or(lines.len());
    Some(PreviewBox {
        col,
        top,
        end,
        text,
    })
}

/// Una raya de trazos (`╌╌╌`): dentro del recuadro de un permiso rodea la orden (la de arriba del
/// recuadro es continua, `───`).
fn is_dashed_rule(l: &str) -> bool {
    let t = l.trim();
    !t.is_empty() && t.chars().all(|c| c == '╌')
}

/// Como mucho estas líneas en el contexto de un permiso (una orden puede ser muy larga).
const CONTEXT_MAX: usize = 120;

/// Lo que enseña un permiso encima de la pregunta (`above`: las líneas de la pantalla hasta la
/// pregunta): desde la raya de arriba del recuadro, la herramienta («Bash command»), lo que hace,
/// la orden y los avisos, sin los «Tip:» ni las rayas de trazos, sin la sangría común y con los
/// huecos de varias líneas en blanco dejados en uno. Las líneas van como las partió la terminal.
/// Si la raya no se ve (el recuadro no cabe en la pantalla), desde arriba del todo.
fn permission_context(above: &[&str]) -> Option<String> {
    let start = above
        .iter()
        .rposition(|l| is_rule(l) && !is_dashed_rule(l))
        .map_or(0, |r| r + 1);
    let mut kept: Vec<&str> = Vec::new();
    for l in &above[start..] {
        if is_dashed_rule(l) || l.trim_start().starts_with("Tip:") {
            continue;
        }
        let l = l.trim_end();
        if l.is_empty() && kept.last().is_none_or(|p| p.is_empty()) {
            continue;
        }
        kept.push(l);
    }
    while kept.last().is_some_and(|l| l.is_empty()) {
        kept.pop();
    }
    let cut = kept
        .iter()
        .filter(|l| !l.is_empty())
        .map(|l| indent(l))
        .min()?;
    let mut out: Vec<String> = kept.iter().map(|l| l.chars().skip(cut).collect()).collect();
    if out.len() > CONTEXT_MAX {
        out.truncate(CONTEXT_MAX);
        out.push("…".into());
    }
    Some(out.join("\n"))
}

fn is_tabs(l: &str) -> bool {
    l.contains('☐') || l.contains('☒')
}

/// `←  ☒ Frutas  ☐ Color  ✔ Submit  →`
fn parse_tabs(l: &str) -> Vec<Tab> {
    let mut tabs = Vec::new();
    let mut cur: Option<(bool, String)> = None;
    for c in l.chars() {
        match c {
            '☐' | '☒' | '✔' | '→' => {
                if let Some((done, label)) = cur.take() {
                    tabs.push(Tab {
                        label: label.trim().to_owned(),
                        done,
                    });
                }
                if c != '✔' && c != '→' {
                    cur = Some((c == '☒', String::new()));
                }
            }
            _ => {
                if let Some((_, label)) = cur.as_mut() {
                    label.push(c);
                }
            }
        }
    }
    if let Some((done, label)) = cur {
        tabs.push(Tab {
            label: label.trim().to_owned(),
            done,
        });
    }
    tabs.retain(|t| !t.label.is_empty());
    tabs
}

/// Menús sin números (confiar en la carpeta): la fila del cursor y las de su misma columna,
/// con «Enter to confirm» debajo.
fn unnumbered(lines: &[&str]) -> Option<Menu> {
    let hint_at = lines
        .iter()
        .rposition(|l| l.contains("Enter to confirm") || l.contains("Enter to select"))?;
    let sel = (hint_at.saturating_sub(12)..hint_at)
        .rev()
        .find(|&i| lines[i].trim_start().starts_with('❯'))?;
    let l = lines[sel];
    let t = l.trim_start().strip_prefix('❯')?;
    let col = indent(l) + 1 + t.chars().count() - t.trim_start().chars().count();
    let same = |i: usize| !lines[i].trim().is_empty() && indent(lines[i]) == col;
    let mut top = sel;
    while top > 0 && same(top - 1) {
        top -= 1;
    }
    let mut bottom = sel;
    while bottom + 1 < hint_at && same(bottom + 1) {
        bottom += 1;
    }
    if top == bottom {
        return None;
    }
    let options: Vec<Opt> = (top..=bottom)
        .map(|i| Opt {
            n: None,
            label: lines[i].trim().trim_start_matches('❯').trim().to_owned(),
            detail: None,
            selected: i == sel,
            checked: None,
            preview: None,
            input: false,
        })
        .collect();
    let mut i = top;
    while i > 0 && lines[i - 1].trim().is_empty() {
        i -= 1;
    }
    let title = (i > 0 && !is_rule(lines[i - 1])).then(|| lines[i - 1].trim().to_owned());
    Some(Menu {
        kind: Kind::Menu,
        title,
        tabs: Vec::new(),
        multi: false,
        rows: (0..options.len()).map(Row::Opt).collect(),
        options,
        hint: Some(lines[hint_at].trim().to_owned()),
        context: None,
    })
}

// ---------- Lo que sigue en marcha en segundo plano ----------

/// Lo que Claude tiene trabajando aparte aunque él esté esperando: subagentes y órdenes en
/// segundo plano (abajo en su pantalla: «● main / ◯ general-purpose  Tarea  20m 51s · ↓ …» y
/// «· 2 shells» en la línea de estado).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Background {
    pub agents: Vec<Agent>,
    pub shells: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Agent {
    /// `general-purpose`, `Explore`…
    pub kind: String,
    /// Lo que está haciendo (recortado si la pantalla es estrecha).
    pub task: String,
    /// Cuánto lleva, en segundos.
    pub elapsed: u64,
}

/// «20m 51s», «1h 2m 3s», «45s» → segundos.
fn duration_secs(s: &str) -> Option<u64> {
    let mut total = 0;
    let mut any = false;
    for part in s.split_whitespace() {
        let (n, unit) = part.split_at(part.find(|c: char| !c.is_ascii_digit())?);
        let n: u64 = n.parse().ok()?;
        total += n * match unit {
            "h" => 3600,
            "m" => 60,
            "s" => 1,
            _ => return None,
        };
        any = true;
    }
    any.then_some(total)
}

pub fn background(screen: &str) -> Option<Background> {
    let plain = strip_sgr(screen);
    let lines: Vec<&str> = plain.lines().collect();
    let mut bg = Background::default();
    // Las órdenes: «· 1 shell ·» en la línea de estado (la de «auto mode», «↓ to manage»…).
    if let Some(status) = lines
        .iter()
        .rev()
        .find(|l| l.contains("to manage") || l.contains("shift+tab to cycle"))
    {
        for part in status.split('·') {
            let p = part.trim();
            if let Some(n) = p
                .strip_suffix(" shells")
                .or_else(|| p.strip_suffix(" shell"))
            {
                bg.shells = n.trim().parse().unwrap_or(0);
            }
        }
    }
    // Los subagentes, debajo de «● main».
    if let Some(main) = lines.iter().rposition(|l| l.trim() == "● main") {
        for l in &lines[main + 1..] {
            let t = l.trim();
            let Some(rest) = t.strip_prefix('◯').or_else(|| t.strip_prefix('●')) else {
                if t.is_empty() {
                    continue;
                }
                break;
            };
            // «general-purpose  Tarea   20m 51s · ↓ 622.2k tokens» (o, estrecha, «Tarea… 7m 29s»).
            let rest = rest.trim();
            let before_tokens = rest.split(" · ").next().unwrap_or(rest).trim_end();
            let words: Vec<&str> = before_tokens.split_whitespace().collect();
            let mut k = words.len();
            while k > 0 && duration_secs(words[k - 1]).is_some() {
                k -= 1;
            }
            let elapsed = duration_secs(&words[k..].join(" ")).unwrap_or(0);
            let head = words[..k].join(" ");
            let (kind, task) = match rest.split_once("  ") {
                Some((kind, _)) => {
                    let kind = kind.trim().to_owned();
                    let task = head.strip_prefix(&kind).unwrap_or(&head).trim().to_owned();
                    (kind, task)
                }
                None => (head.clone(), String::new()),
            };
            bg.agents.push(Agent {
                kind,
                task,
                elapsed,
            });
        }
    }
    (!bg.agents.is_empty() || bg.shells > 0).then_some(bg)
}

// ---------- El modo de permisos y el modelo ----------

/// El modo de permisos de Claude, como lo dice el pie de su caja de escribir («⏵⏵ auto mode on»,
/// «⏸ plan mode on»…). shift+tab los recorre: auto → manual → accept edits → plan → auto (y
/// bypass, si la sesión arrancó con él).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Mode {
    Auto,
    Manual,
    AcceptEdits,
    Plan,
    BypassPermissions,
}

impl Mode {
    /// Por su nombre en la API (`auto`, `manual`, `acceptEdits`, `plan`, `bypassPermissions`).
    pub fn from_name(name: &str) -> Option<Mode> {
        serde_json::from_value(serde_json::Value::String(name.to_owned())).ok()
    }

    pub fn name(self) -> &'static str {
        match self {
            Mode::Auto => "auto",
            Mode::Manual => "manual",
            Mode::AcceptEdits => "acceptEdits",
            Mode::Plan => "plan",
            Mode::BypassPermissions => "bypassPermissions",
        }
    }
}

/// Las filas de debajo de la caja de escribir de Claude (el pie, los subagentes…): las que
/// siguen a la última raya. Sin raya (no se ve la caja), ninguna.
fn below_box(lines: &[&str]) -> Vec<String> {
    match lines.iter().rposition(|l| is_rule(l)) {
        Some(i) => lines[i + 1..].iter().map(|l| l.trim().to_owned()).collect(),
        None => Vec::new(),
    }
}

/// El modo de permisos que dice el pie de Claude, sin fijarse en lo que lleva detrás («(shift+tab
/// to cycle)», «· esc to interrupt», «· 3 shells»…). Un pie que no se entiende (o que no se ve,
/// con un menú en pantalla): `None`.
pub fn mode(screen: &str) -> Option<Mode> {
    let plain = strip_sgr(screen);
    let lines: Vec<&str> = plain.lines().collect();
    for l in below_box(&lines) {
        let rest = l.trim_start_matches(['⏵', '⏸', '\u{fe0e}', '\u{fe0f}']);
        if rest.len() == l.len() {
            continue;
        }
        let rest = rest.trim_start();
        const KNOWN: [(&str, Mode); 5] = [
            ("auto mode on", Mode::Auto),
            ("manual mode on", Mode::Manual),
            ("accept edits on", Mode::AcceptEdits),
            ("plan mode on", Mode::Plan),
            ("bypass permissions on", Mode::BypassPermissions),
        ];
        return KNOWN
            .iter()
            .find(|(text, _)| {
                rest.strip_prefix(text)
                    .is_some_and(|after| after.chars().next().is_none_or(|c| !c.is_alphanumeric()))
            })
            .map(|(_, m)| *m);
    }
    None
}

/// Claude está trabajando: su pie dice «esc to interrupt».
pub fn working(screen: &str) -> bool {
    let plain = strip_sgr(screen);
    let lines: Vec<&str> = plain.lines().collect();
    below_box(&lines)
        .iter()
        .any(|l| l.contains("esc to interrupt"))
}

/// Una opción del menú de `/model`: «❯ 4. Fable ✔   Fable 5.2 · Most capable…».
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelOption {
    /// «Default (recommended)», «Opus», «Fable»…
    pub label: String,
    /// El modelo, como lo enseña Claude: «Opus 5.5» (lo de antes de « · »).
    pub model: String,
    /// Donde está el cursor.
    pub selected: bool,
    /// El que se usa ahora (`✔`).
    pub current: bool,
}

impl ModelOption {
    /// «Default (recommended)»: nido no la elige nunca.
    pub fn is_default(&self) -> bool {
        self.label.starts_with("Default")
    }
}

/// El menú de `/model` («Select model»).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelMenu {
    pub options: Vec<ModelOption>,
    /// La ayuda dice «s to use this session only» (con `s`, el modelo solo vale para esta sesión;
    /// con Enter se guardaría como el de todas las nuevas).
    pub session_only: bool,
    /// Ya se ve su ayuda (la última línea: «… Esc to cancel»): no está a medio pintar.
    pub complete: bool,
}

impl ModelMenu {
    fn selected(&self) -> Option<usize> {
        self.options.iter().position(|o| o.selected)
    }

    /// Los modelos que se pueden elegir, sin `Default` y sin repetir: «Opus 5.5», «Fable 5.1»…
    pub fn models(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for o in self.options.iter().filter(|o| !o.is_default()) {
            if !out.contains(&o.model) {
                out.push(o.model.clone());
            }
        }
        out
    }

    /// La opción del modelo que se pide (nunca `Default`): la de ese modelo («Opus 5.5»); si no,
    /// la de esa etiqueta («Sonnet»); si no, una cuya etiqueta o modelo empiece así («Opus»,
    /// «fable 5»). Si casan varias, la de la versión más nueva («Fable» es Fable 5.2, no 5.1).
    pub fn find(&self, want: &str) -> Option<usize> {
        let w = norm(want);
        if w.is_empty() {
            return None;
        }
        let choices = || {
            self.options
                .iter()
                .enumerate()
                .filter(|(_, o)| !o.is_default())
        };
        let starts = |s: &str| {
            let s = norm(s);
            s.starts_with(&w)
                && s[w.len()..]
                    .chars()
                    .next()
                    .is_none_or(|c| !c.is_alphanumeric())
        };
        // De las que casan, la más nueva (a igual versión, la primera).
        let newest = |found: Vec<(usize, &ModelOption)>| {
            found
                .into_iter()
                .fold(None::<(usize, Vec<u32>)>, |best, (i, o)| {
                    let v = version(&o.model);
                    match best {
                        Some((_, ref b)) if *b >= v => best,
                        _ => Some((i, v)),
                    }
                })
                .map(|(i, _)| i)
        };
        choices()
            .find(|(_, o)| norm(&o.model) == w)
            .map(|(i, _)| i)
            .or_else(|| newest(choices().filter(|(_, o)| norm(&o.label) == w).collect()))
            .or_else(|| {
                newest(
                    choices()
                        .filter(|(_, o)| starts(&o.label) || starts(&o.model))
                        .collect(),
                )
            })
    }
}

/// La versión de un modelo («Fable 5.2» → `[5, 2]`), para comparar como números (5.10 > 5.9).
fn version(model: &str) -> Vec<u32> {
    model
        .split_whitespace()
        .rev()
        .find_map(|w| {
            let v: Option<Vec<u32>> = w.split('.').map(|n| n.parse().ok()).collect();
            v
        })
        .unwrap_or_default()
}

/// La ayuda del menú de `/model` (o su primera línea, si viene partida).
fn model_help(l: &str) -> bool {
    let l = norm(l);
    l.contains("enter to set as default") || l.contains("s to use this session only")
}

/// El menú de `/model` que haya en pantalla: por su título («Select model») o, si no se ve, por
/// su ayuda («Enter to set as default · s to use this session only»).
pub fn model_menu(screen: &str) -> Option<ModelMenu> {
    let plain = strip_sgr(screen);
    let lines: Vec<&str> = plain.lines().collect();
    let start = match lines.iter().rposition(|l| l.trim() == "Select model") {
        Some(title) => title,
        None => {
            let help = lines.iter().rposition(|l| model_help(l))?;
            lines[..help]
                .iter()
                .rposition(|l| is_rule(l))
                .map_or(0, |i| i + 1)
        }
    };
    let below = lines[start..].join("\n");
    let menu = parse(&below)?;
    let mut options = Vec::with_capacity(menu.options.len());
    for o in &menu.options {
        // «Fable ✔                Fable 5.2 · Most…» (y lo que no cupo, debajo).
        // Lo de debajo, con dos espacios: si no cupo nada más que la etiqueta, lo que sigue es
        // el modelo.
        let full = match &o.detail {
            Some(d) => format!("{}  {d}", o.label),
            None => o.label.clone(),
        };
        let (name, rest) = full.split_once("  ").unwrap_or((full.as_str(), ""));
        let name = name.trim();
        let (label, current) = match name.strip_suffix('✔') {
            Some(l) => (l.trim_end().to_owned(), true),
            None => (name.to_owned(), false),
        };
        // Dos formas: «Opus   Opus 5.5 · Lo que hace» (el modelo, antes de « · ») u «Opus 5.5
        // Lo que hace» (la etiqueta es el modelo).
        let rest = rest.split_whitespace().collect::<Vec<_>>().join(" ");
        let model = match rest.split_once(" · ") {
            Some((m, _)) if !m.trim().is_empty() => m.trim().to_owned(),
            _ => label.clone(),
        };
        options.push(ModelOption {
            model,
            label,
            selected: o.selected,
            current,
        });
    }
    // La ayuda puede venir partida en dos líneas.
    let hints = norm(&below);
    Some(ModelMenu {
        options,
        session_only: hints.contains("s to use this session only"),
        complete: hints.contains("esc to cancel"),
    })
}

/// Cuántas veces dice la pantalla «Set model to `model` for this session only», lo que contesta
/// Claude tras `s` en el menú de `/model` (aunque venga partido en varias líneas). Una nueva es
/// que se cambió: se compara con la pantalla de antes de escribir `/model`.
fn model_set_count(screen: &str, model: &str) -> usize {
    let want = norm(&format!("Set model to {model} for this session only"));
    norm(&strip_sgr(screen)).matches(&want).count()
}

// ---------- Formularios de un servidor MCP ----------

/// Un servidor MCP pide datos o confirmación («MCP server "x" requests your input»): un mensaje,
/// unos campos y los botones Accept / Decline. Suelen ser acciones peligrosas (borrar un
/// índice…), así que la web y la app piden confirmarlo dos veces.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Form {
    pub server: String,
    /// El mensaje, tal como se ve (Claude Code lo recorta si es largo).
    pub message: String,
    pub fields: Vec<Field>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Field {
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub required: bool,
    /// Ya tiene valor.
    pub filled: bool,
    /// Lo que se ve: «☐», «☒», «▸ prod», «not set», un texto…
    pub value: String,
    /// `bool`, `choice` o `text` cuando se nota en pantalla (si no, `None`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<&'static str>,
    #[serde(skip)]
    focused: bool,
}

/// Dónde está el cursor en un formulario.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FormCursor {
    Field(usize),
    /// En los botones; `true` si en Accept.
    Buttons(bool),
}

/// El formulario que hay en pantalla, con dónde está el cursor y la ayuda de abajo.
fn form_state(screen: &str) -> Option<(Form, FormCursor, String)> {
    let plain = strip_sgr(screen);
    let lines: Vec<&str> = plain.lines().map(str::trim_end).collect();
    let head = lines.iter().rposition(|l| {
        l.trim_start().starts_with("MCP server") && l.contains("requests your input")
    })?;
    let h = indent(lines[head]);
    let server = lines[head]
        .split(['“', '”', '"'])
        .nth(1)
        .unwrap_or("")
        .to_owned();
    let col = |l: &str, c: usize| l.chars().nth(c);
    let mut message: Vec<&str> = Vec::new();
    let mut fields: Vec<Field> = Vec::new();
    let mut cursor = None;
    let mut hint = String::new();
    let mut buttons = false;
    for l in &lines[head + 1..] {
        if l.trim().is_empty() {
            continue;
        }
        let t = l.trim();
        if t.starts_with("Esc to cancel") {
            hint = t.to_owned();
            break;
        }
        let arrow = col(l, h) == Some('❯');
        let rest = t.trim_start_matches('❯').trim();
        if rest.starts_with("Accept") && rest.contains("Decline")
            || rest.starts_with("Accept") && l.contains("❯ Decline")
        {
            buttons = true;
            if arrow {
                cursor = Some(FormCursor::Buttons(true));
            } else if t.contains("❯ Decline") || t.contains("❯Decline") {
                cursor = Some(FormCursor::Buttons(false));
            }
            continue;
        }
        let marker = col(l, h + 2);
        let is_field = matches!(col(l, h), Some('❯' | ' '))
            && col(l, h + 1) == Some(' ')
            && matches!(marker, Some('*' | '✔' | ' '))
            && col(l, h + 3) == Some(' ')
            && col(l, h + 4).is_some_and(|c| c != ' ')
            && t.contains(':');
        if is_field && !buttons {
            let body: String = l.chars().skip(h + 4).collect();
            let (label, value) = match body.find(": ") {
                Some(i) => (body[..i].to_owned(), body[i + 2..].trim().to_owned()),
                None => (body.trim_end_matches(':').to_owned(), String::new()),
            };
            let kind = if value.starts_with(['☐', '☒']) {
                Some("bool")
            } else if value.starts_with(['▸', '▾']) {
                Some("choice")
            } else if value.starts_with("Type something") {
                Some("text")
            } else {
                None
            };
            if arrow {
                cursor = Some(FormCursor::Field(fields.len()));
            }
            fields.push(Field {
                label,
                description: None,
                // El asterisco desaparece al rellenarlo (pasa a ✔): obligatorio solo se sabe si falta.
                required: marker == Some('*'),
                filled: marker == Some('✔'),
                value,
                kind,
                focused: arrow,
            });
            continue;
        }
        if fields.is_empty() {
            if indent(l) == h {
                message.push(t);
            }
        } else if indent(l) >= h + 6 && !t.starts_with(['◯', '◉', '❯']) {
            let f = fields.last_mut().expect("campo");
            if f.description.is_none() {
                f.description = Some(t.to_owned());
            }
        }
    }
    if fields.is_empty() || !buttons {
        return None;
    }
    Some((
        Form {
            server,
            message: message.join("\n"),
            fields,
        },
        cursor.unwrap_or(FormCursor::Field(0)),
        hint,
    ))
}

/// El formulario de un servidor MCP que haya en pantalla.
pub fn form(screen: &str) -> Option<Form> {
    form_state(screen).map(|(f, _, _)| f)
}

/// Las opciones de una lista desplegada (`◯ dev`, `◉ pre`…), y en cuál está el cursor.
fn choice_options(screen: &str) -> (Vec<String>, Option<usize>) {
    let plain = strip_sgr(screen);
    let mut opts = Vec::new();
    let mut at = None;
    for l in plain.lines() {
        let t = l.trim();
        let (cur, t) = match t.strip_prefix('❯') {
            Some(r) => (true, r.trim_start()),
            None => (false, t),
        };
        if let Some(label) = t.strip_prefix('◯').or_else(|| t.strip_prefix('◉')) {
            if cur {
                at = Some(opts.len());
            }
            opts.push(label.trim().to_owned());
        }
    }
    (opts, at)
}

/// Lo que se contesta a un formulario: aceptarlo (con lo que se quiera poner en sus campos de
/// texto o lista, por su etiqueta) o rechazarlo.
#[derive(Debug, Clone, Deserialize)]
pub struct FormAnswer {
    pub accept: bool,
    #[serde(default)]
    pub values: std::collections::HashMap<String, String>,
}

const YES: [&str; 8] = [
    "yes",
    "sí",
    "si",
    "true",
    "confirm",
    "confirmar",
    "accept",
    "ok",
];

/// Contesta el formulario. Al aceptar, los obligatorios sin valor que son una confirmación se
/// rellenan solos (la casilla marcada, o la opción «yes» de la lista); los de texto, con lo que
/// venga en `values` (si falta, no se manda nada).
pub async fn answer_form<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    scr: &mut Screen<R, W>,
    ans: &FormAnswer,
) -> Result<(), String> {
    let (form0, _, _) =
        form_state(&scr.read().await?).ok_or("no hay ningún formulario en pantalla")?;
    if ans.accept {
        for (i, f) in form0.fields.iter().enumerate() {
            let wanted = ans
                .values
                .get(&f.label)
                .map(|v| v.trim())
                .filter(|v| !v.is_empty());
            if f.filled && wanted.is_none() {
                continue;
            }
            if !f.required && wanted.is_none() {
                continue;
            }
            let hint = form_go(scr, FormCursor::Field(i)).await?;
            let screen = scr.read().await?;
            let (now, _, _) = form_state(&screen).ok_or("el formulario ha desaparecido")?;
            let value = now.fields[i].value.clone();
            if hint.contains("Space to toggle") || value.starts_with(['☐', '☒']) {
                let want = wanted.is_none_or(|w| YES.contains(&w.to_lowercase().as_str()));
                let checked = |v: &str| v.starts_with('☒');
                if checked(&value) != want {
                    scr.keys(b" ").await?;
                    form_until(scr, |f, _| {
                        f.fields.get(i).is_some_and(|x| checked(&x.value) == want)
                    })
                    .await?;
                }
            } else if hint.contains("to expand") || value.starts_with('▸') {
                scr.keys(b"\x1b[C").await?;
                let deadline = tokio::time::Instant::now() + WAIT;
                let (opts, mut at) = loop {
                    let (o, a) = choice_options(&scr.read().await?);
                    if !o.is_empty() {
                        break (o, a);
                    }
                    if tokio::time::Instant::now() >= deadline {
                        return Err(format!("no se despliega la lista de «{}»", f.label));
                    }
                    tokio::time::sleep(STEP).await;
                };
                let target = match wanted {
                    Some(w) => opts.iter().position(|o| norm(o) == norm(w)),
                    None => opts
                        .iter()
                        .position(|o| YES.contains(&o.to_lowercase().as_str())),
                }
                .ok_or_else(|| {
                    format!(
                        "«{}»: no sé qué opción elegir ({})",
                        f.label,
                        opts.join(", ")
                    )
                })?;
                for _ in 0..8 {
                    let cur = at.unwrap_or(0);
                    if cur == target {
                        break;
                    }
                    scr.keys(if target > cur { DOWN } else { UP }).await?;
                    tokio::time::sleep(STEP).await;
                    at = choice_options(&scr.read().await?).1;
                }
                if at != Some(target) {
                    return Err("no se pudo elegir la opción".into());
                }
                scr.keys(b" ").await?;
                form_until(scr, |f, _| f.fields.get(i).is_some_and(|x| x.filled)).await?;
            } else {
                let Some(text) = wanted else {
                    return Err(format!("falta «{}»", f.label));
                };
                if f.filled {
                    // Lo que hubiera, fuera (Backspace lo deja sin valor).
                    scr.keys(b"\x7f").await?;
                    tokio::time::sleep(STEP).await;
                }
                scr.type_text(text).await?;
                form_until(scr, |f, _| f.fields.get(i).is_some_and(|x| x.filled)).await?;
            }
        }
    }
    // Un Enter justo detrás de mover el cursor a veces se pierde: un respiro, y si el formulario
    // sigue ahí con el cursor en el botón, otra vez.
    for _ in 0..3 {
        form_go(scr, FormCursor::Buttons(ans.accept)).await?;
        tokio::time::sleep(Duration::from_millis(300)).await;
        scr.keys(b"\r").await?;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            tokio::time::sleep(STEP).await;
            if form_state(&scr.read().await?).is_none() {
                return Ok(());
            }
            if tokio::time::Instant::now() >= deadline {
                break;
            }
        }
    }
    Err("el formulario sigue ahí (¿falta algún campo obligatorio?)".into())
}

/// Hasta que el formulario cumpla `done`.
async fn form_until<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    scr: &mut Screen<R, W>,
    done: impl Fn(&Form, FormCursor) -> bool,
) -> Result<(), String> {
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        if let Some((f, c, _)) = form_state(&scr.read().await?)
            && done(&f, c)
        {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err("el formulario no responde".into());
        }
        tokio::time::sleep(STEP).await;
    }
}

/// Lleva el cursor del formulario a `to` (↑/↓ entre campos y botones; ←/→ entre Accept y
/// Decline). Devuelve la ayuda de abajo al llegar (dice qué tipo de campo es).
async fn form_go<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    scr: &mut Screen<R, W>,
    to: FormCursor,
) -> Result<String, String> {
    for _ in 0..20 {
        let (form, at, hint) =
            form_state(&scr.read().await?).ok_or("el formulario ha desaparecido")?;
        let row = |c: FormCursor| match c {
            FormCursor::Field(i) => i,
            FormCursor::Buttons(_) => form.fields.len(),
        };
        if at == to {
            return Ok(hint);
        }
        let key = match (at, to) {
            (FormCursor::Buttons(_), FormCursor::Buttons(accept)) => {
                if accept {
                    b"\x1b[D" as &[u8]
                } else {
                    b"\x1b[C"
                }
            }
            _ if row(to) > row(at) => DOWN,
            _ if row(to) < row(at) => UP,
            _ => return Ok(hint),
        };
        scr.keys(key).await?;
        let before = at;
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            tokio::time::sleep(STEP).await;
            match form_state(&scr.read().await?) {
                Some((_, c, _)) if c != before => break,
                None => return Err("el formulario ha desaparecido".into()),
                _ => {}
            }
            if tokio::time::Instant::now() >= deadline {
                return Err("el cursor no se mueve".into());
            }
        }
    }
    Err("no se llega al campo".into())
}

// ---------- Contestar ----------

/// Lo que se contesta: las preguntas de Claude (todas las que hizo a la vez, en orden) o una
/// opción de cualquier otro menú.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Request {
    #[serde(default)]
    pub questions: Vec<QuestionAnswer>,
    /// La etiqueta de la opción (o su número).
    pub option: Option<String>,
    /// Mandar la siguiente petición que propone Claude (el texto que se ve, para comprobarlo).
    pub suggestion: Option<String>,
    /// Mandar un mensaje (lo que escribes en la caja del chat, con las rutas de los adjuntos).
    pub message: Option<String>,
    /// Contestar el formulario de un servidor MCP.
    pub form: Option<FormAnswer>,
    /// Con un campo de texto, lo que se escribe; con otra opción (p. ej. «No» en un permiso), lo
    /// que se le manda a Claude después, como un mensaje.
    pub text: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct QuestionAnswer {
    /// La pregunta tal cual la hizo Claude (para comprobar que es la que está en pantalla).
    pub question: String,
    /// Las opciones elegidas, por su etiqueta.
    #[serde(default)]
    pub choose: Vec<String>,
    /// Una respuesta propia («Type something»).
    pub text: Option<String>,
}

/// Por qué no llegó lo pedido al guardián de una sesión.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unread {
    /// No contestó a tiempo, pero sigue ahí. La conexión ya no vale: su respuesta podría
    /// llegar tarde y pasar por la de la siguiente pregunta.
    Slow,
    /// Se cerró (la sesión terminó).
    Closed,
}

/// Lo más que se pide de lo que ha sacado una sesión desde que arrancó (`Screen::output`): el
/// saludo de Claude está al principio. Con muchas sesiones arrancando a la vez, no se reúnen
/// historiales enteros.
pub const LAUNCH_CAP: usize = 256 * 1024;

/// La pantalla de una sesión, a través de su guardián: leerla y teclear.
pub struct Screen<R, W> {
    rd: R,
    wr: W,
}

const STEP: Duration = Duration::from_millis(100);
const WAIT: Duration = Duration::from_secs(4);

const UP: &[u8] = b"\x1b[A";
const DOWN: &[u8] = b"\x1b[B";

impl<R: AsyncRead + Unpin, W: AsyncWrite + Unpin> Screen<R, W> {
    pub fn new(rd: R, wr: W) -> Self {
        Self { rd, wr }
    }

    /// La pantalla tal cual (con colores).
    pub async fn screen(&mut self) -> Result<String, String> {
        self.read().await
    }

    /// La pantalla, diciendo si la sesión se cerró o solo tardó (ver `Unread`).
    pub async fn look(&mut self) -> Result<String, Unread> {
        self.request(HistoryRequest {
            all: false,
            screen: true,
            launch: None,
        })
        .await
    }

    /// Lo que ha sacado la sesión desde que arrancó (su historial desde la marca de rearranque,
    /// con las de repintado, y la pantalla), hasta `LAUNCH_CAP`: ver `launch_refusal`.
    pub async fn output(&mut self) -> Result<String, Unread> {
        self.request(HistoryRequest::launch(LAUNCH_CAP)).await
    }

    async fn read(&mut self) -> Result<String, String> {
        self.look().await.map_err(|e| {
            match e {
                Unread::Slow => "la sesión no contesta",
                Unread::Closed => "la sesión se ha cerrado",
            }
            .into()
        })
    }

    async fn request(&mut self, req: HistoryRequest) -> Result<String, Unread> {
        proto::write_frame(&mut self.wr, &Frame::HistoryRequest(req))
            .await
            .map_err(|_| Unread::Closed)?;
        loop {
            let frame = tokio::time::timeout(WAIT, proto::read_frame(&mut self.rd))
                .await
                .map_err(|_| Unread::Slow)?;
            match frame {
                Ok(Some(Frame::History(b))) => return Ok(String::from_utf8_lossy(&b).into_owned()),
                Ok(Some(Frame::Exit(_)) | None) | Err(_) => return Err(Unread::Closed),
                Ok(Some(_)) => {}
            }
        }
    }

    pub(crate) async fn keys(&mut self, bytes: &[u8]) -> Result<(), String> {
        proto::write_frame(&mut self.wr, &Frame::Input(bytes.to_vec()))
            .await
            .map_err(|_| "la sesión se ha cerrado".into())
    }

    async fn menu(&mut self) -> Result<Option<Menu>, String> {
        Ok(parse(&self.read().await?))
    }

    /// Hasta que la pantalla cumpla `done` (o `WAIT`): lo que hay entonces.
    async fn until(
        &mut self,
        done: impl Fn(Option<&Menu>) -> bool,
    ) -> Result<(bool, Option<Menu>), String> {
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            let m = self.menu().await?;
            if done(m.as_ref()) {
                return Ok((true, m));
            }
            if tokio::time::Instant::now() >= deadline {
                return Ok((false, m));
            }
            tokio::time::sleep(STEP).await;
        }
    }

    /// Lleva el cursor a la fila `row` con las flechas.
    async fn go(&mut self, m: &Menu, row: usize) -> Result<Menu, String> {
        let mut m = m.clone();
        for _ in 0..3 {
            let Some(at) = m.selected_row() else {
                return Err("no se ve dónde está el cursor".into());
            };
            if at == row {
                return Ok(m);
            }
            let key = if row > at { DOWN } else { UP };
            for _ in 0..at.abs_diff(row) {
                self.keys(key).await?;
            }
            let (_, now) = self
                .until(|x| x.and_then(Menu::selected_row) == Some(row))
                .await?;
            m = now.ok_or("el menú ha desaparecido")?;
        }
        Err("no se pudo llevar el cursor a la opción".into())
    }

    /// Escribe en un campo de texto (sin saltos de línea: Enter lo manda).
    async fn type_text(&mut self, text: &str) -> Result<(), String> {
        let clean: String = text
            .chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .collect();
        self.keys(clean.trim().as_bytes()).await?;
        tokio::time::sleep(Duration::from_millis(150)).await;
        Ok(())
    }

    /// Elige una opción: con su número, o con las flechas y Enter si no tiene (o pasa del 9).
    async fn pick(&mut self, m: &Menu, i: usize) -> Result<(), String> {
        match m.options[i].n {
            Some(n) if n <= 9 => self.keys(n.to_string().as_bytes()).await,
            _ => {
                let row = m.row_of(i).ok_or("opción sin fila")?;
                self.go(m, row).await?;
                self.keys(b"\r").await
            }
        }
    }
}

/// La siguiente petición que propone Claude al terminar: el texto atenuado en la caja de escribir
/// (en la terminal, Tab lo escribe). Si no cabe, Claude lo corta con «…».
pub fn suggestion(screen: &str) -> Option<String> {
    let lines: Vec<&str> = screen.lines().collect();
    let plain: Vec<String> = lines.iter().map(|l| strip_sgr(l)).collect();
    // La caja es la última línea con «❯» entre dos rayas.
    let i = (1..plain.len().saturating_sub(1))
        .rev()
        .find(|&i| plain[i].trim_start().starts_with('❯'))?;
    // Las rayas de la caja (la de arriba puede llevar dentro el nombre de la sesión).
    let edge = |l: &str| l.trim_start().starts_with("──────");
    if !edge(&plain[i - 1]) || !edge(&plain[i + 1]) {
        return None;
    }
    // Atenuado justo detrás de «❯ »: lo propuesto, no lo que has escrito tú.
    let raw = lines[i];
    let after = &raw[raw.find('❯')? + '❯'.len_utf8()..];
    let after = after.trim_start_matches([' ', '\u{a0}']);
    if !starts_dim(after) {
        return None;
    }
    let text = strip_sgr(after).trim().to_owned();
    // Las ayudas de la caja, atenuadas igual: «Try "fix lint errors"» al abrir Claude, «Press up
    // to edit queued messages» con mensajes en cola…
    const HINTS: [&str; 3] = ["Try \"", "Press up to", "? for shortcuts"];
    if text.is_empty() || HINTS.iter().any(|h| text.starts_with(h)) {
        return None;
    }
    Some(text)
}

/// Lo escrito en la caja de Claude (lo atenuado no cuenta: es una propuesta o una ayuda), o
/// `None` si no se ve la caja (hay un menú, o no es Claude).
pub fn input_box(screen: &str) -> Option<String> {
    let lines: Vec<&str> = screen.lines().collect();
    let plain: Vec<String> = lines.iter().map(|l| strip_sgr(l)).collect();
    let edge = |l: &str| l.trim_start().starts_with("──────");
    // La raya de abajo es la última; la de arriba, la anterior. Entre ellas, lo escrito.
    let bottom = plain.iter().rposition(|l| edge(l))?;
    let top = plain[..bottom].iter().rposition(|l| edge(l))?;
    let first = plain.get(top + 1)?;
    if bottom == top + 1 || !first.trim_start().starts_with('❯') {
        return None;
    }
    let raw = lines[top + 1];
    let after = raw[raw.find('❯')? + '❯'.len_utf8()..].trim_start_matches([' ', '\u{a0}']);
    if starts_dim(after) {
        return Some(String::new());
    }
    let mut text: Vec<String> = Vec::new();
    for (k, l) in plain[top + 1..bottom].iter().enumerate() {
        let l = if k == 0 {
            l.trim_start().trim_start_matches('❯')
        } else {
            l.as_str()
        };
        text.push(l.trim_matches([' ', '\u{a0}']).to_owned());
    }
    Some(text.join(" ").trim().to_owned())
}

/// Lo escrito en la caja de Claude sin nada de lo atenuado (las ayudas que Claude pinta detrás,
/// como « [model]» tras `/model`), o `None` si no se ve la caja.
fn typed_in_box(screen: &str) -> Option<String> {
    input_box(screen)?;
    let lines: Vec<&str> = screen.lines().collect();
    let plain: Vec<String> = lines.iter().map(|l| strip_sgr(l)).collect();
    let edge = |l: &str| l.trim_start().starts_with("──────");
    let bottom = plain.iter().rposition(|l| edge(l))?;
    let top = plain[..bottom].iter().rposition(|l| edge(l))?;
    let mut text: Vec<String> = Vec::new();
    for (k, l) in lines[top + 1..bottom].iter().enumerate() {
        let u = undimmed(l);
        let u = if k == 0 {
            u.trim_start().trim_start_matches('❯').to_owned()
        } else {
            u
        };
        text.push(u.trim_matches([' ', '\u{a0}']).to_owned());
    }
    Some(text.join(" ").trim().to_owned())
}

/// Error si en pantalla está el menú de `/model` o su pregunta de si cambiar: ahí un número o
/// Enter guardaría el modelo como el de todas las sesiones nuevas.
fn not_in_model_dialog(screen: &str) -> Result<(), String> {
    if model_dialog(screen) {
        return Err(MODEL_DIALOG.into());
    }
    Ok(())
}

const MODEL_DIALOG: &str =
    "en pantalla está el menú de /model: ciérralo desde la terminal (Esc), aquí no se contesta";

const ONLY_CONTROL: &str = "el mensaje solo tiene caracteres de control";

/// El texto que se pega, sin caracteres de control (C0 y C1) salvo los saltos de línea y los
/// tabuladores: dentro de un pegado, `ESC[201~` lo terminaría y lo de detrás serían teclas
/// (shift+tab cambia el modo, Esc interrumpe, ctrl+c sale). Los saltos de línea, con `\n`.
fn paste_text(text: &str) -> String {
    text.replace("\r\n", "\n")
        .replace('\r', "\n")
        .chars()
        .filter(|&c| c == '\n' || c == '\t' || !c.is_control())
        .collect()
}

/// `text` (ya limpio, ver [`paste_text`]) como un pegado: así varias líneas no se mandan una a
/// una.
fn bracketed(text: &str) -> Vec<u8> {
    let mut paste = b"\x1b[200~".to_vec();
    paste.extend_from_slice(text.as_bytes());
    paste.extend_from_slice(b"\x1b[201~");
    paste
}

/// Manda un mensaje a Claude: lo pega, espera a que la caja se quede quieta (con adjuntos,
/// Claude tarda en cambiar las rutas por «[Image #1]»; un Enter que llega antes se pierde) y
/// pulsa Enter; si la caja no se vacía, lo vuelve a pulsar.
pub async fn send_message<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    scr: &mut Screen<R, W>,
    text: &str,
) -> Result<(), String> {
    let screen = scr.read().await?;
    not_in_model_dialog(&screen)?;
    let clean = paste_text(text);
    // Solo caracteres de control: no es un Enter suelto.
    if clean.trim().is_empty() && !text.trim().is_empty() {
        return Err(ONLY_CONTROL.into());
    }
    let text = clean.trim();
    if !text.is_empty() {
        let paste = bracketed(text);
        // Si Claude no lo coge (p. ej. aún arrancando), la caja sigue vacía: se pega otra vez.
        let mut written = false;
        for _ in 0..2 {
            let screen = scr.read().await?;
            if input_box(&screen).is_some_and(|t| !t.is_empty()) {
                return Err("en la caja de Claude ya hay algo escrito".into());
            }
            scr.keys(&paste).await?;
            // Quieta: dos lecturas seguidas iguales, con algo escrito.
            let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
            let mut last: Option<String> = None;
            loop {
                tokio::time::sleep(Duration::from_millis(150)).await;
                let now = input_box(&scr.read().await?);
                let has = now.as_deref().is_some_and(|t| !t.is_empty());
                if has && now == last {
                    written = true;
                    break;
                }
                // Sin nada al cabo de 2 s: no lo ha cogido.
                if !has && tokio::time::Instant::now() + Duration::from_secs(3) >= deadline {
                    break;
                }
                if tokio::time::Instant::now() >= deadline {
                    written = has;
                    break;
                }
                last = now;
            }
            if written {
                break;
            }
        }
        if !written {
            return Err("Claude no ha cogido el texto (¿está arrancando?)".into());
        }
    }
    for _ in 0..3 {
        scr.keys(b"\r").await?;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            tokio::time::sleep(STEP).await;
            match input_box(&scr.read().await?) {
                // Vacía (enviado, o en cola si Claude trabajaba) o ya no está la caja.
                None => return Ok(()),
                Some(t) if t.is_empty() => return Ok(()),
                Some(_) => {}
            }
            if tokio::time::Instant::now() >= deadline {
                break;
            }
        }
    }
    if text.is_empty() {
        return Ok(()); // un Enter suelto (confirmar algo): no había nada que vaciar
    }
    Err("Claude no ha cogido el mensaje: sigue en su caja".into())
}

/// Manda la siguiente petición que propone Claude (`expect`, como se ve en pantalla): Tab la
/// escribe entera en la caja y Enter la envía.
pub async fn send_suggestion<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    scr: &mut Screen<R, W>,
    expect: &str,
) -> Result<(), String> {
    let now = suggestion(&scr.read().await?);
    if now.as_deref() != Some(expect) {
        return Err("Claude ya no propone eso".into());
    }
    scr.keys(b"\t").await?;
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        tokio::time::sleep(STEP).await;
        if suggestion(&scr.read().await?).is_none() {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            return Err("Claude no ha escrito la propuesta".into());
        }
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    scr.keys(b"\r").await
}

/// Claude espera a que se renueve el límite para seguir solo («…Press esc to cancel the wait»).
pub fn waiting_for_limit(screen: &str) -> bool {
    strip_sgr(screen).contains("esc to cancel the wait")
}

/// El aviso de `--dangerously-load-development-channels`, leído fila a fila: los channels que
/// lista y si el cursor está en «1. I am using this for local development» (la opción que nido
/// acepta). Cada fila tiene que ser exactamente la del aviso y en su orden: el rótulo, la fila de
/// `Channels:` (con las que la siguen si no cabe en una) y la opción. En la pregunta de confiar
/// en una carpeta no se reconoce nunca: su ruta podría llevar esas filas. Tampoco si está la caja
/// de Claude (el aviso no la deja ver) ni en lo que haya debajo del arranque (ver
/// `startup_rows`): ahí puede estar citado.
fn channels_warning(screen: &str) -> Option<(Vec<String>, bool)> {
    const OPTION: &str = "1. I am using this for local development";
    if trust_screen(screen) {
        return None;
    }
    let screen = strip_sgr(screen);
    let all: Vec<&str> = screen.lines().collect();
    if prompt_box(&all).is_some() {
        return None;
    }
    let lines: Vec<&str> = startup_rows(&all).iter().map(|l| l.trim()).collect();
    let warning = lines
        .iter()
        .position(|l| *l == "WARNING: Loading development channels")?;
    let header = warning
        + lines[warning..]
            .iter()
            .position(|l| l.starts_with("Channels:"))?;
    let option = header
        + lines[header..]
            .iter()
            .position(|l| l.strip_prefix('❯').map_or(*l, str::trim_start) == OPTION)?;
    let mut list: Vec<String> = Vec::new();
    let first = lines[header].strip_prefix("Channels:").unwrap_or_default();
    for l in std::iter::once(first).chain(lines[header + 1..option].iter().copied()) {
        list.extend(l.split_whitespace().map(str::to_owned));
    }
    Some((list, lines[option].starts_with('❯')))
}

/// La pantalla de aviso de `--dangerously-load-development-channels`: los channels que lista
/// (ver `channels_warning`).
pub fn dev_channels(screen: &str) -> Option<Vec<String>> {
    channels_warning(screen).map(|(list, _)| list)
}

/// La pregunta de Claude de si confías en la carpeta. La contesta el usuario (en su terminal, o
/// en la web o la app, que la enseñan como un menú más: ver `parse`); nunca nido por su cuenta
/// ni otra sesión con la herramienta `answer` del MCP, que se niega. Solo se mira en las filas
/// del arranque (ver `startup_rows`): una conversación que la cite (p. ej. el primer mensaje de
/// una hija de `open`) no es la pregunta.
pub fn trust_screen(screen: &str) -> bool {
    let screen = strip_sgr(screen);
    let all: Vec<&str> = screen.lines().collect();
    let rows = startup_rows(&all);
    let has = |words: &[&str]| rows.iter().any(|l| words.iter().any(|t| l.contains(t)));
    has(&[
        "trust this folder",
        "Yes, I trust",
        "Quick safety check",
        "Accessing workspace",
        // La de Claude Code de antes: «Do you trust the files in this folder?», con
        // «Yes, proceed» (que solo, sin hablar de confiar, puede ser otra cosa).
        "trust the files in this folder",
    ]) || (has(&["Yes, proceed"]) && has(&["trust"]))
}

/// Dónde empieza la caja de Claude: una raya y, debajo, la fila de `❯`.
fn prompt_box(lines: &[&str]) -> Option<usize> {
    lines
        .windows(2)
        .position(|w| is_rule(w[0]) && w[1].trim_start().starts_with('❯'))
}

/// Las filas del arranque de Claude (su rótulo, el aviso de los channels): las de encima de su
/// caja y de la conversación (la primera petición, `> …` o `❯ …`, o respuesta, `● …`). Lo de más
/// abajo es la conversación, que puede citar esas mismas filas (p. ej. el primer mensaje de una
/// hija de `open`, que escribe quien la abre).
fn startup_rows<'a>(lines: &'a [&'a str]) -> &'a [&'a str] {
    let talk = lines
        .iter()
        .position(|l| l.starts_with("> ") || l.starts_with('●') || l.starts_with('⏺') || asked(l));
    let end = [prompt_box(lines), talk]
        .into_iter()
        .flatten()
        .min()
        .unwrap_or(lines.len());
    &lines[..end]
}

/// Una petición tuya en la conversación: Claude la pinta con `❯ ` delante. No una opción de un
/// menú (`❯ 1. …`, como la del aviso de los channels).
fn asked(l: &str) -> bool {
    l.strip_prefix("❯ ").is_some_and(|r| {
        let digits = r.chars().take_while(char::is_ascii_digit).count();
        !(digits > 0 && r[digits..].starts_with('.'))
    })
}

/// Lo que hay que hacer con la pantalla de una sesión de Claude recién lanzada con el channel
/// de nido.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Startup {
    /// El aviso de los channels de desarrollo, solo con el de nido y el cursor en «I am using
    /// this for local development»: se acepta (Enter).
    Accept,
    /// El aviso, con otra lista de channels: no se toca (que conteste el usuario).
    Foreign(Vec<String>),
    /// Claude no tiene el servidor `nido` (su fila de `Channels:` dice «server:nido · no MCP
    /// server configured»): la sesión no tiene channel.
    NoServer,
    /// La organización de la cuenta de Claude no permite channels («blocked by org policy
    /// (server:nido)… Inbound messages will be silently dropped»): la sesión no tiene channel.
    Blocked,
    /// Nada de eso (aún, o la pregunta de confiar en la carpeta, que es del usuario).
    Nothing,
}

/// Claude ya espera peticiones: su caja está en pantalla (y no la pregunta de confiar en la
/// carpeta).
pub fn repl(screen: &str) -> bool {
    if trust_screen(screen) {
        return false;
    }
    let screen = strip_sgr(screen);
    let all: Vec<&str> = screen.lines().collect();
    prompt_box(&all).is_some()
}

/// El aviso de Claude de que la organización de la cuenta no deja usar channels, para el de
/// nido: «--dangerously-load-development-channels blocked by org policy (…server:nido…)» y,
/// justo detrás, «Inbound messages will be silently dropped». Va en filas con `▎` delante, que
/// se juntan: en una terminal estrecha, o con varios channels, se parte en varias.
fn channels_blocked(rows: &[&str]) -> bool {
    const BLOCKED: &str = "--dangerously-load-development-channels blocked by org policy (";
    const DROPPED: &str = "Inbound messages will be silently dropped";
    let mut blocks: Vec<String> = Vec::new();
    let mut open = false;
    for l in rows {
        match l.trim().strip_prefix('▎') {
            Some(r) if open => {
                if let Some(b) = blocks.last_mut() {
                    b.push(' ');
                    b.push_str(r);
                }
            }
            Some(r) => {
                blocks.push(r.to_owned());
                open = true;
            }
            None => open = false,
        }
    }
    blocks.iter().any(|b| {
        let b = b.split_whitespace().collect::<Vec<_>>().join(" ");
        let Some(rest) = b.find(BLOCKED).map(|i| &b[i + BLOCKED.len()..]) else {
            return false;
        };
        let Some((list, after)) = rest.split_once(')') else {
            return false;
        };
        after.trim_start().starts_with(DROPPED)
            && list.split([',', ' ']).any(|c| c == "server:nido")
    })
}

/// Lo que dice del channel de nido el saludo de Claude (las filas de arranque, ver
/// `startup_rows`, con su rótulo): que la organización no lo permite, o que no tiene el
/// servidor `nido`.
fn greeting_refusal(rows: &[&str]) -> Option<Startup> {
    if !rows.iter().any(|l| l.contains("Claude Code")) {
        return None;
    }
    if channels_blocked(rows) {
        return Some(Startup::Blocked);
    }
    let no_server = rows.iter().map(|l| l.trim()).any(|l| {
        l.starts_with("Channels:") && l.contains("server:nido · no MCP server configured")
    });
    no_server.then_some(Startup::NoServer)
}

/// Lo mismo (`Blocked` o `NoServer`), en todo lo que ha sacado la sesión desde que arrancó (su
/// historial con la pantalla, ver `Screen::output`): retomada con mucha conversación, el saludo
/// ya no está en pantalla. Desde la última marca de rearranque, y cada trozo entre repintados
/// (`ESC[3J` deja otra marca) por separado.
pub fn launch_refusal(history: &str) -> Option<Startup> {
    let history = strip_sgr(history);
    let lines: Vec<&str> = history.lines().collect();
    let from = lines
        .iter()
        .rposition(|l| l.starts_with("\u{1}restart"))
        .map_or(0, |i| i + 1);
    lines[from..]
        .split(|l| l.starts_with('\u{1}'))
        .find_map(|part| greeting_refusal(startup_rows(part)))
}

pub fn startup(screen: &str) -> Startup {
    if trust_screen(screen) {
        return Startup::Nothing;
    }
    match channels_warning(screen) {
        Some((list, true)) if list == ["server:nido"] => return Startup::Accept,
        // Con el cursor en otra opción, Enter no sería aceptar: se espera.
        Some((list, false)) if list == ["server:nido"] => return Startup::Nothing,
        Some((list, _)) => return Startup::Foreign(list),
        None => {}
    }
    // En el saludo de Claude (con su rótulo), no en algo citado en la conversación.
    let screen = strip_sgr(screen);
    let all: Vec<&str> = screen.lines().collect();
    greeting_refusal(startup_rows(&all)).unwrap_or(Startup::Nothing)
}

/// Tras cambiar de cuenta: si la sesión se quedó parada por el límite (`stopped`, según su
/// transcripción, o por lo que hay en pantalla), quita el aviso (su menú o la espera) y le dice a
/// Claude que siga. Devuelve si lo ha hecho.
pub async fn resume_after_limit<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    scr: &mut Screen<R, W>,
    stopped: bool,
) -> Result<bool, String> {
    let text = scr.read().await?;
    let menu = parse(&text);
    let on_screen =
        menu.as_ref().is_some_and(|m| m.kind == Kind::Limit) || waiting_for_limit(&text);
    if !stopped && !on_screen {
        return Ok(false);
    }
    if on_screen {
        scr.keys(b"\x1b").await?;
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            let t = scr.read().await?;
            if parse(&t).is_none_or(|m| m.kind != Kind::Limit) && !waiting_for_limit(&t) {
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                return Err("no se pudo quitar el aviso del límite".into());
            }
            tokio::time::sleep(STEP).await;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    if let Some(m) = scr.menu().await? {
        return Err(format!("hay otro menú en pantalla ({:?})", m.kind));
    }
    scr.keys("\x1b[200~continúa\x1b[201~".as_bytes()).await?;
    tokio::time::sleep(Duration::from_millis(100)).await;
    scr.keys(b"\r").await?;
    Ok(true)
}

/// shift+tab: el siguiente modo de permisos.
const SHIFT_TAB: &[u8] = b"\x1b[Z";
/// Cuántas veces se pulsa shift+tab como mucho para llegar a un modo.
const MODE_PRESSES: usize = 8;

/// Hay un menú, una pregunta o un formulario en pantalla: ahí shift+tab, las letras o Enter
/// harían otra cosa (en el del plan, shift+tab lo aprueba).
fn something_open(screen: &str) -> bool {
    parse(screen).is_some() || form(screen).is_some() || model_menu(screen).is_some()
}

/// Cambia el modo de permisos de Claude a `want` con shift+tab, mirando el pie tras cada
/// pulsación. Vale con Claude trabajando. Antes de cada pulsación comprueba que no hay ningún
/// menú ni pregunta en pantalla y que el pie dice lo que se espera. Si vuelve al modo de partida
/// (no se llega a `want`), pasan `MODE_PRESSES` pulsaciones o el pie deja de entenderse, falla.
/// Por el camino puede pasar un instante por otros modos (de plan a manual pasa por auto).
pub async fn set_mode<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    scr: &mut Screen<R, W>,
    want: Mode,
) -> Result<Mode, String> {
    let text = scr.read().await?;
    if something_open(&text) {
        return Err("hay un menú o una pregunta en pantalla".into());
    }
    let start = mode(&text).ok_or(NO_MODE)?;
    let mut cur = start;
    for _ in 0..MODE_PRESSES {
        if cur == want {
            return Ok(cur);
        }
        let text = scr.read().await?;
        if something_open(&text) {
            return Err("hay un menú o una pregunta en pantalla".into());
        }
        if mode(&text) != Some(cur) {
            return Err(NO_MODE.into());
        }
        // En plan, con Claude trabajando, en cualquier momento puede salir el menú de aprobar el
        // plan, que acepta shift+tab: si ya estaba en plan, no se pulsa. Pasando por plan de
        // camino (de accept edits a auto) sí: Claude no sabe aún que está en plan y no puede
        // tener un plan que aprobar; pararse ahí lo dejaría en plan, trabajando.
        if start == Mode::Plan && cur == Mode::Plan && working(&text) {
            return Err(PLAN_WORKING.into());
        }
        scr.keys(SHIFT_TAB).await?;
        // El pie nuevo (un instante puede no entenderse mientras Claude lo repinta).
        let deadline = tokio::time::Instant::now() + WAIT;
        let now = loop {
            tokio::time::sleep(STEP).await;
            let text = scr.read().await?;
            if something_open(&text) {
                return Err("hay un menú o una pregunta en pantalla".into());
            }
            match mode(&text) {
                Some(m) if m != cur => break m,
                _ => {}
            }
            if tokio::time::Instant::now() >= deadline {
                return Err("Claude no ha cambiado de modo".into());
            }
        };
        if now == want {
            return Ok(now);
        }
        if now == start {
            return Err(format!("no se llega al modo {} con shift+tab", want.name()));
        }
        cur = now;
    }
    Err(format!("no se llega al modo {} con shift+tab", want.name()))
}

const NO_MODE: &str = "no se entiende el modo en el pie de Claude";
const PLAN_WORKING: &str =
    "Claude está trabajando en modo plan: espera a que termine para cambiar de modo";

/// Lo que pone Claude, una sola vez, debajo de los mensajes en cola.
const SEND_NOW: &str = "ctrl+x ctrl+s to send now";
/// ctrl+x ctrl+s: los mensajes en cola, a Claude ya (sin esperar a que acabe el turno).
const CTRL_X: &[u8] = b"\x18";
const CTRL_S: &[u8] = b"\x13";
const BACKSPACE: &[u8] = b"\x7f";

/// Claude tiene mensajes en cola: encima de su caja, cada uno como `❯ texto` (las líneas que
/// siguen, sangradas) y, debajo de todos, una vez, «ctrl+x ctrl+s to send now». Cuántos son, lo
/// dice [`queue_count`]; qué dicen, la transcripción.
pub fn queue_shown(screen: &str) -> bool {
    let plain: Vec<String> = screen.lines().map(strip_sgr).collect();
    let Some(at) = plain.iter().rposition(|l| l.trim().starts_with(SEND_NOW)) else {
        return false;
    };
    // Justo encima, los mensajes: líneas sangradas (o en blanco) hasta la primera, `❯ …`. Una
    // respuesta de Claude que dijera lo mismo empezaría por `⏺`.
    let Some(first) = plain[..at]
        .iter()
        .rposition(|l| !l.is_empty() && !l.starts_with([' ', '\u{a0}']))
    else {
        return false;
    };
    if !plain[first].starts_with('❯') {
        return false;
    }
    // Debajo, hasta la caja, nada más de la conversación (ni de Claude ni tuyo).
    let edge = |l: &str| l.trim_start().starts_with("──────");
    plain[at + 1..]
        .iter()
        .take_while(|l| !edge(l))
        .all(|l| !l.trim_start().starts_with(['❯', '⏺', '●']))
}

/// Cuántos mensajes enseña Claude en cola (ver [`queue_shown`]), si la tiene: los `❯` del bloque
/// que acaba en «ctrl+x ctrl+s to send now». Se cuentan de abajo arriba: las líneas sangradas
/// siguen el mensaje de encima, y una en blanco también si debajo tiene una sangrada (un mensaje
/// con una línea vacía); en blanco justo encima de un `❯`, el bloque empieza ahí (lo de más
/// arriba es la conversación, que puede acabar en un `❯` tuyo). Si el bloque llega arriba del
/// todo, `None`: la pantalla no tiene por qué enseñarlo entero.
pub fn queue_count(screen: &str) -> Option<usize> {
    if !queue_shown(screen) {
        return None;
    }
    let plain: Vec<String> = screen.lines().map(strip_sgr).collect();
    let at = plain.iter().rposition(|l| l.trim().starts_with(SEND_NOW))?;
    let indented = |l: &str| l.starts_with([' ', '\u{a0}']);
    let mut count = 0;
    // Lo de debajo de la línea que se mira: si era un `❯`, una en blanco cierra el bloque.
    let mut below_entry = false;
    for l in plain[..at].iter().rev() {
        if l.trim().is_empty() {
            if below_entry {
                return (count > 0).then_some(count);
            }
        } else if l.starts_with('❯') {
            count += 1;
            below_entry = true;
            continue;
        } else if !indented(l) {
            return (count > 0).then_some(count);
        }
        below_entry = false;
    }
    // Llega arriba del todo sin ver dónde empieza: la cola puede seguir por encima de la
    // pantalla, y contar de menos escondería mensajes.
    None
}

/// Lo que hay en cola según la transcripción (`queued`), con lo que dice la pantalla: si se ven
/// N, los N últimos (o los que haya, si son menos). Lo que no se haya quitado bien de la
/// transcripción queda delante y no se enseña.
pub fn queued_on_screen(screen: &str, mut queued: Vec<String>) -> Vec<String> {
    if let Some(n) = queue_count(screen) {
        let extra = queued.len().saturating_sub(n);
        queued.drain(..extra);
    }
    queued
}

const NO_QUEUE: &str = "Claude no tiene mensajes en cola";
const MENU_OPEN: &str = "hay un menú o una pregunta en pantalla";

/// La pantalla de ahora, si se puede tocar la cola: con la cola a la vista y sin ningún menú,
/// pregunta ni formulario (ahí las teclas harían otra cosa).
async fn queue_screen<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    scr: &mut Screen<R, W>,
) -> Result<String, String> {
    let text = scr.read().await?;
    if something_open(&text) {
        return Err(MENU_OPEN.into());
    }
    if !queue_shown(&text) {
        return Err(NO_QUEUE.into());
    }
    Ok(text)
}

/// Manda ya a Claude los mensajes en cola, como ctrl+x ctrl+s en la terminal (sin esperar a que
/// acabe lo que está haciendo), y espera a que la cola desaparezca de la pantalla.
pub async fn send_queue<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    scr: &mut Screen<R, W>,
) -> Result<(), String> {
    queue_screen(scr).await?;
    scr.keys(CTRL_X).await?;
    tokio::time::sleep(STEP).await;
    scr.keys(CTRL_S).await?;
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        tokio::time::sleep(STEP).await;
        if !queue_shown(&scr.read().await?) {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err("la cola sigue en la pantalla de Claude".into());
        }
    }
}

/// La ayuda de la caja con mensajes en cola. Va atenuada (no cuenta como escrito: `input_box`),
/// pero por si se ve sin colores.
const QUEUE_HINT: &str = "Press up to edit queued messages";

const STILL_IN_BOX: &str =
    "no se pudo vaciar la caja de Claude: el texto de la cola sigue en ella (se ve en la terminal)";

/// Saca de la cola los mensajes para editarlos: con la caja de Claude vacía, ↑ los pasa todos a
/// la caja (uno por línea) y vacía la cola; luego se borran de la caja con retroceso (el cursor
/// queda al final), tantos como caracteres y otra vez si no queda vacía. Devuelve el texto, los
/// mensajes unidos con saltos de línea, tal como los apuntó la transcripción: lo que ↑ sacó
/// (`popped`, que se mira después de pulsarla), o si no aparece, lo que había en cola antes
/// (`queued`); si tampoco, lo que se vio en la caja.
pub async fn edit_queue<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    scr: &mut Screen<R, W>,
    queued: &[String],
    popped: impl Fn() -> Vec<String>,
) -> Result<String, String> {
    let text = queue_screen(scr).await?;
    // Los que se ven: lo que la transcripción no haya quitado bien no cuenta.
    let n = queue_count(&text).unwrap_or(queued.len()).min(queued.len());
    let queued = &queued[queued.len() - n..];
    match input_box(&text) {
        None => return Err("no se ve la caja de escribir de Claude".into()),
        Some(t) if !t.is_empty() && t != QUEUE_HINT => {
            return Err("hay algo escrito en la caja de Claude".into());
        }
        Some(_) => {}
    }
    scr.keys(UP).await?;
    // Hasta que el texto esté en la caja y la cola se haya ido.
    let deadline = tokio::time::Instant::now() + WAIT;
    let boxed = loop {
        tokio::time::sleep(STEP).await;
        let text = scr.read().await?;
        if something_open(&text) {
            return Err(STILL_IN_BOX.into());
        }
        match input_box(&text) {
            Some(t) if !t.is_empty() && t != QUEUE_HINT && !queue_shown(&text) => break t,
            _ => {}
        }
        if tokio::time::Instant::now() >= deadline {
            return Err("Claude no ha sacado la cola a la caja".into());
        }
    };
    // Lo que apuntó la transcripción al sacarlos: puede llegar un poco después que la pantalla, y
    // es una línea por mensaje (una lectura a medias daría solo los primeros). Vale cuando dos
    // lecturas seguidas dicen lo mismo y no son menos de los que había en cola.
    let mut msgs = popped();
    for _ in 0..5 {
        tokio::time::sleep(STEP).await;
        let again = popped();
        let settled = again == msgs && !msgs.is_empty() && msgs.len() >= queued.len();
        msgs = again;
        if settled {
            break;
        }
    }
    if msgs.is_empty() || msgs.len() < queued.len() {
        msgs = queued.to_vec();
    }
    let out = if msgs.is_empty() {
        boxed
    } else {
        msgs.join("\n")
    };
    let n = out.chars().count();
    for _ in 0..2 {
        for _ in 0..n {
            scr.keys(BACKSPACE).await?;
        }
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            tokio::time::sleep(STEP).await;
            let text = scr.read().await?;
            if something_open(&text) {
                return Err(STILL_IN_BOX.into());
            }
            if input_box(&text).as_deref() == Some("") {
                return Ok(out);
            }
            if tokio::time::Instant::now() >= deadline {
                break;
            }
        }
    }
    Err(STILL_IN_BOX.into())
}

/// Cambia el modelo de Claude solo para esta sesión: escribe `/model` en la caja vacía, Enter,
/// lleva el cursor del menú a la opción de `want` (solo con ↑/↓: ←/→ cambian el esfuerzo) y pulsa
/// `s` («s to use this session only»). Nunca Enter dentro del menú: lo guardaría como el modelo
/// de todas las sesiones nuevas. Si algo falla (también una lectura que no llega) después de
/// escribir `/model`, se deja todo como estaba (`clean_model`).
///
/// Las opciones del menú, si se llega a ver entero, quedan en `seen` (los modelos que se pueden
/// elegir). Devuelve el modelo elegido, como lo enseña Claude («Opus 5.5»).
pub async fn set_model<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    scr: &mut Screen<R, W>,
    want: &str,
    seen: &mut Option<Vec<String>>,
) -> Result<String, String> {
    let before = scr.read().await?;
    if something_open(&before) {
        return Err("hay un menú o una pregunta en pantalla".into());
    }
    if working(&before) {
        return Err("Claude está trabajando: espera a que termine".into());
    }
    match input_box(&before) {
        None => return Err("no se ve la caja de escribir de Claude".into()),
        Some(t) if !t.is_empty() => {
            return Err("en la caja de Claude ya hay algo escrito".into());
        }
        Some(_) => {}
    }
    scr.keys(b"/model").await?;
    let mut entered = false;
    match model_steps(scr, want, seen, &before, &mut entered).await {
        Ok(model) => Ok(model),
        Err(e) => {
            clean_model(scr, entered).await;
            Err(e)
        }
    }
}

/// Lo de `set_model` tras escribir `/model`; `entered`: si ya se pulsó Enter. Cualquier error
/// deja la limpieza a quien llama.
async fn model_steps<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    scr: &mut Screen<R, W>,
    want: &str,
    seen: &mut Option<Vec<String>>,
    before: &str,
    entered: &mut bool,
) -> Result<String, String> {
    // En la caja, exactamente `/model` (lo atenuado es una ayuda de Claude): con `/model sonnet`,
    // Enter guardaría ese modelo por defecto.
    let typed = |t: &str| typed_in_box(t).as_deref() == Some("/model") && !something_open(t);
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        tokio::time::sleep(STEP).await;
        if typed(&scr.read().await?) {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            return Err("Claude no ha cogido /model".into());
        }
    }
    // Justo antes de Enter, otra vez: `/model` en la caja y ningún menú abierto.
    if !typed(&scr.read().await?) {
        return Err("Claude no ha cogido /model".into());
    }
    scr.keys(b"\r").await?;
    *entered = true;
    // El menú entero: con su ayuda (la última línea) o, si no, igual en dos lecturas seguidas.
    let deadline = tokio::time::Instant::now() + WAIT;
    let mut last: Option<String> = None;
    let menu = loop {
        tokio::time::sleep(STEP).await;
        let text = scr.read().await?;
        if let Some(m) = model_menu(&text)
            && !m.options.is_empty()
            && (m.complete || last.as_deref() == Some(text.as_str()))
        {
            break m;
        }
        if tokio::time::Instant::now() >= deadline {
            return Err("no se abre el menú de /model".into());
        }
        last = Some(text);
    };
    let models = menu.models();
    if !models.is_empty() {
        *seen = Some(models);
    }
    pick_model(scr, menu, want, before).await
}

/// Con el menú de `/model` abierto: el cursor a la opción de `want` y `s`. Vale cuando Claude
/// dice que lo cambió con una línea nueva (que no estaba en `before`, la pantalla de antes de
/// `/model`) y ya no hay nada abierto. Si falla, puede quedar algo abierto (lo cierra quien
/// llama).
async fn pick_model<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    scr: &mut Screen<R, W>,
    mut menu: ModelMenu,
    want: &str,
    before: &str,
) -> Result<String, String> {
    if !menu.session_only {
        return Err("el menú de /model no deja usar un modelo solo en esta sesión".into());
    }
    let target = menu
        .find(want)
        .ok_or_else(|| format!("en el menú de /model no está «{want}»"))?;
    let model = menu.options[target].model.clone();
    for _ in 0..menu.options.len() + 2 {
        let at = menu.selected().ok_or("no se ve dónde está el cursor")?;
        if at == target {
            break;
        }
        scr.keys(if target > at { DOWN } else { UP }).await?;
        let deadline = tokio::time::Instant::now() + WAIT;
        menu = loop {
            tokio::time::sleep(STEP).await;
            let m = model_menu(&scr.read().await?).ok_or("el menú de /model ha desaparecido")?;
            if m.selected() != Some(at) {
                break m;
            }
            if tokio::time::Instant::now() >= deadline {
                return Err("el cursor del menú de /model no se mueve".into());
            }
        };
    }
    // Justo antes de `s`, otra vez: el menú, la ayuda y el cursor en la opción.
    let now = model_menu(&scr.read().await?).ok_or("el menú de /model ha desaparecido")?;
    if !now.session_only
        || now.selected() != Some(target)
        || now.options.get(target).map(|o| &o.model) != Some(&model)
    {
        return Err("no se pudo llevar el cursor a la opción".into());
    }
    let said = model_set_count(before, &model);
    scr.keys(b"s").await?;
    // Con conversación, Claude pregunta antes si cambiar («Switch model?»): sí, una vez. La
    // pregunta se mira antes que el éxito.
    let mut confirmed = false;
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        tokio::time::sleep(STEP).await;
        let text = scr.read().await?;
        if let Some(n) = switch_confirm(&text, &model) {
            if !confirmed {
                scr.keys(n.to_string().as_bytes()).await?;
                confirmed = true;
            }
        } else if !something_open(&text) && model_set_count(&text, &model) > said {
            return Ok(model);
        }
        if tokio::time::Instant::now() >= deadline {
            return Err("Claude no ha dicho que cambió el modelo".into());
        }
    }
}

/// La pregunta de Claude tras elegir otro modelo con conversación ya empezada («Switch model?
/// … ❯ 1. Yes, switch to Sonnet 5.5 / 2. No, go back»): el número de la opción que cambia a
/// `model`.
fn switch_confirm(screen: &str, model: &str) -> Option<u32> {
    if !switch_question(screen) {
        return None;
    }
    let want = norm(&format!("Yes, switch to {model}"));
    parse(screen)?
        .options
        .iter()
        .find(|o| norm(&o.label) == want)
        .and_then(|o| o.n)
        .filter(|n| *n <= 9)
}

/// La pregunta de si cambiar de modelo («Switch model?»), con sus opciones.
fn switch_question(screen: &str) -> bool {
    parse(screen).is_some()
        && strip_sgr(screen)
            .lines()
            .any(|l| l.trim() == "Switch model?")
}

/// Qué hay abierto en pantalla, para saber cuándo ha hecho efecto un Esc.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Dialog {
    None,
    /// El menú de `/model`.
    Menu,
    /// Su pregunta de si cambiar.
    Switch,
    /// Otro menú, pregunta o formulario.
    Other,
}

fn dialog(screen: &str) -> Dialog {
    if switch_question(screen) {
        Dialog::Switch
    } else if model_menu(screen).is_some() {
        Dialog::Menu
    } else if something_open(screen) {
        Dialog::Other
    } else {
        Dialog::None
    }
}

/// El menú de `/model` o su pregunta de si cambiar («Switch model?»).
fn model_dialog(screen: &str) -> bool {
    matches!(dialog(screen), Dialog::Menu | Dialog::Switch)
}

/// Lo que hay en pantalla (el diálogo y lo escrito en la caja) cuando dos lecturas seguidas
/// dicen lo mismo: tras una lectura que no llegó a tiempo, la siguiente puede traer la pantalla
/// de entonces. `None` si la sesión no contesta.
async fn settled<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    scr: &mut Screen<R, W>,
) -> Option<(Dialog, Option<String>)> {
    let read = |t: &str| (dialog(t), typed_in_box(t));
    let mut last = read(&scr.read().await.ok()?);
    for _ in 0..10 {
        let now = read(&scr.read().await.ok()?);
        if now == last {
            return Some(now);
        }
        last = now;
        tokio::time::sleep(STEP).await;
    }
    None
}

/// Tras un fallo en `set_model`, una sola limpieza: Esc si está el menú de `/model` o su
/// pregunta (o, si ya se pulsó Enter, cualquier otro menú, que antes no estaba: lo ha abierto
/// `/model`); si no hay nada abierto y la caja dice exactamente `/model`, se borra.
async fn clean_model<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    scr: &mut Screen<R, W>,
    entered: bool,
) {
    let Some((open, typed)) = settled(scr).await else {
        return;
    };
    match open {
        Dialog::Menu | Dialog::Switch => escape_model_menu(scr, false).await,
        Dialog::Other if entered => escape_model_menu(scr, true).await,
        Dialog::Other => {}
        Dialog::None => {
            if typed.as_deref() == Some("/model") {
                if scr.keys(&b"\x7f".repeat("/model".len())).await.is_err() {
                    return;
                }
                // Hasta que se vea borrado.
                let deadline = tokio::time::Instant::now() + WAIT;
                while tokio::time::Instant::now() < deadline {
                    tokio::time::sleep(STEP).await;
                    match scr.read().await {
                        Ok(t) if typed_in_box(&t).as_deref() != Some("/model") => return,
                        Err(_) => return,
                        _ => {}
                    }
                }
            }
        }
    }
}

/// Cierra con Esc el menú de `/model` o su pregunta de si cambiar (con `other`, también otro
/// menú): de la pregunta se vuelve al menú, y del menú a nada. Tras cada Esc espera a que cambie
/// lo que hay abierto (no cualquier cosa de la pantalla); si no cambia, no pulsa más. Nunca
/// pulsa Esc sin nada abierto: dos seguidos con la caja vacía abrirían otra cosa.
async fn escape_model_menu<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    scr: &mut Screen<R, W>,
    other: bool,
) {
    for _ in 0..3 {
        let Some((open, _)) = settled(scr).await else {
            return;
        };
        let ours =
            matches!(open, Dialog::Menu | Dialog::Switch) || (other && open == Dialog::Other);
        if !ours || scr.keys(b"\x1b").await.is_err() {
            return;
        }
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            tokio::time::sleep(STEP).await;
            match scr.read().await {
                Ok(t) if dialog(&t) != open => break,
                Err(_) => return,
                _ => {}
            }
            if tokio::time::Instant::now() >= deadline {
                return;
            }
        }
    }
}

/// Contesta el menú que hay en pantalla.
pub async fn answer<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    scr: &mut Screen<R, W>,
    req: &Request,
) -> Result<(), String> {
    // Ni un número ni Enter en el menú de `/model`: lo guardaría por defecto.
    not_in_model_dialog(&scr.read().await?)?;
    if !req.questions.is_empty() {
        return questions(scr, &req.questions).await;
    }
    if let Some(s) = &req.suggestion {
        return send_suggestion(scr, s).await;
    }
    if let Some(m) = &req.message {
        return send_message(scr, m).await;
    }
    if let Some(f) = &req.form {
        return answer_form(scr, f).await;
    }
    let want = req.option.as_deref().ok_or("falta qué opción")?;
    let m = scr.menu().await?.ok_or("no hay ningún menú en pantalla")?;
    let i = want
        .parse::<u32>()
        .ok()
        .and_then(|n| m.options.iter().position(|o| o.n == Some(n)))
        .or_else(|| m.find(want))
        .ok_or_else(|| format!("en pantalla no está la opción «{want}»"))?;
    let before = m.clone();
    if m.options[i].input {
        let row = m.row_of(i).ok_or("opción sin fila")?;
        match m.options[i].n {
            Some(n) if n <= 9 && !m.multi => scr.keys(n.to_string().as_bytes()).await?,
            _ => {
                scr.go(&m, row).await?;
            }
        }
        scr.until(|x| x.and_then(Menu::selected_row) == Some(row))
            .await?;
        scr.type_text(req.text.as_deref().unwrap_or("")).await?;
        scr.keys(b"\r").await?;
        return Ok(());
    }
    scr.pick(&m, i).await?;
    let (changed, _) = scr.until(|x| x != Some(&before)).await?;
    if !changed {
        return Err("Claude no ha reaccionado a la opción".into());
    }
    // Lo que se escribe tras «No»: un mensaje a Claude, cuando vuelva a la caja de escribir.
    let text = req.text.as_deref().map(paste_text);
    if let Some(text) = text.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
        let (gone, _) = scr.until(|x| x.is_none()).await?;
        if !gone {
            return Err("eligió la opción, pero no se pudo mandar el texto".into());
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
        scr.keys(&bracketed(text)).await?;
        tokio::time::sleep(Duration::from_millis(100)).await;
        scr.keys(b"\r").await?;
    }
    Ok(())
}

async fn questions<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    scr: &mut Screen<R, W>,
    answers: &[QuestionAnswer],
) -> Result<(), String> {
    for qa in answers {
        let (ok, m) = scr
            .until(|m| m.is_some_and(|m| m.shows_question(&qa.question)))
            .await?;
        let m = match m {
            Some(m) if ok => m,
            _ => return Err(format!("en pantalla no está la pregunta «{}»", qa.question)),
        };
        let text = qa.text.as_deref().map(str::trim).filter(|t| !t.is_empty());
        let mut chosen = Vec::new();
        for want in &qa.choose {
            let i = m
                .find(want)
                .filter(|&i| !m.options[i].input)
                .ok_or_else(|| {
                    format!("la pregunta «{}» no tiene la opción «{want}»", qa.question)
                })?;
            chosen.push(i);
        }
        if chosen.is_empty() && text.is_none() {
            return Err(format!("falta la respuesta a «{}»", qa.question));
        }
        if m.multi {
            // Primero las casillas (con su número: marcan sin mover el cursor)…
            for (i, o) in m.options.iter().enumerate() {
                if o.input || o.checked.is_none() {
                    continue;
                }
                if o.checked != Some(chosen.contains(&i)) {
                    let n = o.n.ok_or("opción sin número")?;
                    scr.keys(n.to_string().as_bytes()).await?;
                    let want = chosen.contains(&i);
                    scr.until(|x| {
                        x.is_some_and(|x| x.options.get(i).and_then(|o| o.checked) == Some(want))
                    })
                    .await?;
                }
            }
            // …luego el texto (a su fila con las flechas: el número solo la marcaría)…
            let mut m = scr.menu().await?.ok_or("el menú ha desaparecido")?;
            if let Some(text) = text {
                let i = m.input().ok_or("esta pregunta no admite texto")?;
                let row = m.row_of(i).ok_or("opción sin fila")?;
                scr.go(&m, row).await?;
                scr.type_text(text).await?;
                m = scr.menu().await?.ok_or("el menú ha desaparecido")?;
            }
            // …y «Next»/«Submit».
            let nav = m.nav_row().ok_or("no se encuentra «Next»")?;
            scr.go(&m, nav).await?;
            scr.keys(b"\r").await?;
        } else if let Some(text) = text {
            let i = m.input().ok_or("esta pregunta no admite texto")?;
            scr.pick(&m, i).await?;
            let row = m.row_of(i).ok_or("opción sin fila")?;
            scr.until(|x| x.and_then(Menu::selected_row) == Some(row))
                .await?;
            scr.type_text(text).await?;
            scr.keys(b"\r").await?;
        } else {
            scr.pick(&m, chosen[0]).await?;
        }
        let (moved, _) = scr
            .until(|x| !x.is_some_and(|x| x.shows_question(&qa.question)))
            .await?;
        if !moved {
            return Err(format!(
                "Claude no ha pasado de la pregunta «{}»",
                qa.question
            ));
        }
    }
    // Contestada la última de varias, sale el repaso: mandar.
    if let Some(m) = scr.menu().await?.filter(|m| m.kind == Kind::Review) {
        let i = m
            .find("Submit answers")
            .ok_or("no se encuentra «Submit answers»")?;
        scr.pick(&m, i).await?;
        let (sent, _) = scr
            .until(|x| x.is_none_or(|x| x.kind != Kind::Review))
            .await?;
        if !sent {
            return Err("no se pudieron mandar las respuestas".into());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen(name: &str) -> Option<Menu> {
        let path = format!("{}/tests/pantallas/{name}.txt", env!("CARGO_MANIFEST_DIR"));
        parse(&std::fs::read_to_string(path).unwrap())
    }

    fn labels(m: &Menu) -> Vec<&str> {
        m.options.iter().map(|o| o.label.as_str()).collect()
    }

    #[test]
    fn question_among_several() {
        let m = screen("pregunta_varias").unwrap();
        assert_eq!(m.kind, Kind::Question);
        assert_eq!(m.title.as_deref(), Some("¿Cuál es tu color favorito?"));
        assert_eq!(
            labels(&m),
            [
                "Rojo",
                "Verde",
                "Azul",
                "Type something.",
                "Chat about this"
            ]
        );
        assert_eq!(m.options[0].detail.as_deref(), Some("El color rojo"));
        assert!(m.options[0].selected && m.options[3].input && !m.options[4].input);
        assert_eq!(
            m.tabs
                .iter()
                .map(|t| (t.label.as_str(), t.done))
                .collect::<Vec<_>>(),
            [("Color", false), ("Frutas", false)]
        );
        assert!(!m.multi);
        assert_eq!(m.find("verde"), Some(1));
    }

    #[test]
    fn single_question() {
        let m = screen("pregunta_sola").unwrap();
        assert_eq!(m.kind, Kind::Question);
        assert_eq!(m.tabs.len(), 1);
        assert_eq!(m.input(), Some(2));
        assert_eq!(
            m.hint.as_deref(),
            Some("Enter to select · ↑/↓ to navigate · Esc to cancel")
        );
    }

    #[test]
    fn question_with_bar() {
        let m = screen("pregunta_barra").unwrap();
        assert_eq!(m.kind, Kind::Question);
        assert_eq!(
            m.title.as_deref(),
            Some(
                "How should a coupon from coupons.json (WELCOME 10%, SUMMER26 20% with a 5000-cent \
                 minimum) combine with the existing applyDiscount percent discount in src/cart.js?"
            )
        );
        assert!(m.shows_question("How should a coupon from coupons.json (WELCOME 10%, SUMMER26 20% with a 5000-cent minimum) combine with the existing applyDiscount percent discount in src/cart.js?"));
        assert_eq!(labels(&m)[0], "Stack sequentially");
    }

    #[test]
    fn question_with_preview_box() {
        let m = screen("pregunta_vista_previa").unwrap();
        assert_eq!(m.kind, Kind::Question);
        assert!(m.title.as_deref().unwrap().ends_with("¿Cómo lo dejo?"));
        // El recuadro de la derecha no es de las etiquetas, ni «Notes:» una descripción.
        assert_eq!(
            labels(&m),
            [
                "Tilde en la píldora",
                "«estimado» en la píldora",
                "Solo en el detalle",
                "Dejarlo como está"
            ]
        );
        assert_eq!(m.options[0].detail.as_deref(), Some("(Recommended)"));
        assert!(m.options[1..].iter().all(|o| o.detail.is_none()), "{m:#?}");
        assert!(m.options.iter().all(|o| !o.input));
        // La vista previa que se ve es la de la opción del cursor.
        assert_eq!(
            m.options[0].preview.as_deref(),
            Some(
                "Bandeja:  [ Caduca en ~10 días ]  Pendiente\n\
                 Detalle:  Caduca (estimado)  23 nov, 06:35"
            )
        );
        assert!(m.options[1..].iter().all(|o| o.preview.is_none()));
    }

    /// La de verdad (Claude Code 2.1.323): el recuadro, más alto que la lista; «Notes:» debajo,
    /// tras una línea en blanco; sin descripciones.
    #[test]
    fn question_with_preview_box_real() {
        let m = screen("pregunta_vista_previa_real").unwrap();
        assert_eq!(m.kind, Kind::Question);
        assert_eq!(
            labels(&m),
            ["Todo (Recommended)", "nido y web", "Solo nido en el WSL"]
        );
        assert!(
            m.options.iter().all(|o| o.detail.is_none() && !o.input),
            "{m:#?}"
        );
        assert_eq!(
            m.options[0].preview.as_deref(),
            Some(
                "1. nido   → WSL (reinicio del servicio; las sesiones siguen)\n\
                 2. web    → hub.example.org (remoto)\n\
                 3. app    → los dos teléfonos   (por wifi)\n\
                 \n\
                 Mac y Windows siguen con nido 0.4.0"
            )
        );
        assert!(m.options[1..].iter().all(|o| o.preview.is_none()));
        assert!(m.title.as_deref().unwrap().starts_with("Ya están hechos"));
    }

    #[test]
    fn previews_from_the_transcript() {
        let mut m = screen("pregunta_vista_previa").unwrap();
        let ask = serde_json::json!({"questions": [
            {"question": "Otra cosa", "options": [{"label": "Tilde en la píldora", "preview": "no"}]},
            {"question": "Las notificaciones pendientes muestran «Caduca en 10 días · 23 nov, 06:35». Esa fecha es estimada (puesta a disposición + 10 × 24 h), y el README de plazos.json dice que en pantalla el plazo lleva siempre «estimado». La maqueta del cliente no lo pone. ¿Cómo lo dejo?",
             "options": [
                {"label": "Tilde en la píldora (Recommended)", "preview": "Bandeja: [ Caduca en ~10 días ]\nDetalle: Caduca (estimado)\n"},
                {"label": "«estimado» en la píldora", "preview": "Bandeja: [ Caduca (estimado) ]"},
                {"label": "Solo en el detalle", "preview": ""},
                {"label": "Dejarlo como está"}]}]});
        m.fill_previews(&ask);
        let previews: Vec<_> = m.options.iter().map(|o| o.preview.as_deref()).collect();
        assert_eq!(
            previews,
            [
                Some("Bandeja: [ Caduca en ~10 días ]\nDetalle: Caduca (estimado)"),
                Some("Bandeja: [ Caduca (estimado) ]"),
                None,
                None
            ]
        );
        // Sin el título, por las opciones.
        let mut m = screen("pregunta_vista_previa").unwrap();
        m.title = Some("cortado".into());
        m.fill_previews(&ask);
        assert_eq!(
            m.options[1].preview.as_deref(),
            Some("Bandeja: [ Caduca (estimado) ]")
        );
        // Una pregunta que no es esta: nada.
        let mut m = screen("pregunta_barra").unwrap();
        m.fill_previews(&ask);
        assert!(m.options.iter().all(|o| o.preview.is_none()));
    }

    #[test]
    fn typed_text_is_still_the_input() {
        let m = screen("pregunta_texto").unwrap();
        assert_eq!(m.options[2].label, "morado claro");
        assert!(m.options[2].input && m.options[2].selected);
    }

    #[test]
    fn multi_select_with_next_row() {
        let m = screen("pregunta_multiple").unwrap();
        assert_eq!(m.kind, Kind::Question);
        assert!(m.multi);
        assert_eq!(
            labels(&m),
            ["Manzana", "Pera", "Type something", "Chat about this"]
        );
        assert_eq!(
            m.options.iter().map(|o| o.checked).collect::<Vec<_>>(),
            [Some(false), Some(true), Some(false), None]
        );
        assert_eq!(m.options[2].detail, None, "«Next» no es su descripción");
        assert_eq!(m.nav_row(), Some(3));
        assert_eq!(m.selected_row(), Some(0));
        assert_eq!(m.row_of(3), Some(4));
        assert!(m.shows_question("¿Qué frutas te gustan?"));
    }

    #[test]
    fn multi_select_cursor_on_next() {
        let m = screen("pregunta_multiple_texto").unwrap();
        assert_eq!(m.options[2].label, "kiwi");
        assert_eq!(m.options[2].checked, Some(true));
        assert_eq!(m.selected_row(), m.nav_row());
    }

    #[test]
    fn answered_mark_is_not_part_of_the_label() {
        let m = screen("pregunta_respondida").unwrap();
        assert_eq!(m.options[0].label, "Rojo");
        assert_eq!(m.tabs.iter().filter(|t| t.done).count(), 2);
    }

    #[test]
    fn permission_context() {
        // Sin el «Tip:» ni las rayas de trazos que rodean la orden.
        let m = screen("permiso").unwrap();
        assert_eq!(
            m.context.as_deref(),
            Some("Bash command\n\nCreate empty file prueba.txt\ntouch prueba.txt")
        );
        // Desde la raya de arriba del recuadro (lo de la conversación no), con la orden sangrada
        // como la pinta Claude y los avisos.
        let m = screen("permiso_contexto").unwrap();
        assert_eq!(m.kind, Kind::Permission);
        assert_eq!(labels(&m), ["Yes", "No"]);
        let want = r#"Bash command
Generate old/new SQL pairs for all cases

  S=/tmp/borrador; cd $S && sed -i '/total_vieja/d' gen.js && node -e '
  const c=[]; const D="2026-10-13", H="2026-11-12";
  for (const t of ["alta","baja","cambio"]) c.push({tipo:t, desde:D, hasta:H});
  require("fs").writeFileSync("casos.json", JSON.stringify(c, null, 2));
  ' && rm -f cmp/* && node gen.js casos.json cmp 2>&1 | grep -v "^\s*$" | tail -60

Compound command contains cd with write operation - manual approval required to prevent path resolution bypass
⚠ Claude Code will automatically block this request in 1:30, to avoid holding up an unattended session"#;
        assert_eq!(m.context.as_deref(), Some(want));
        // Va en el JSON del menú (y no va si no hay).
        let v = serde_json::to_value(&m).unwrap();
        assert_eq!(v["context"], want);
        let q = serde_json::to_value(screen("pregunta_sola").unwrap()).unwrap();
        assert!(q.get("context").is_none());
    }

    #[test]
    fn permission_context_edges() {
        let menu = "\n Do you want to proceed?\n ❯ 1. Yes\n   2. No\n\n Esc to cancel";
        // La raya de arriba con el nombre de la sesión; los huecos en blanco, en uno.
        let s = format!("● Hecho.\n──────── hija-b ─\n\n Bash command\n\n\n\n   echo hola\n{menu}");
        let m = parse(&s).unwrap();
        assert_eq!(m.context.as_deref(), Some("Bash command\n\n  echo hola"));
        // Sin raya a la vista (el recuadro no cabe): desde arriba del todo.
        let s = format!(" Bash command\n   echo hola\n{menu}");
        assert_eq!(
            parse(&s).unwrap().context.as_deref(),
            Some("Bash command\n  echo hola")
        );
        // Nada encima de la pregunta.
        assert_eq!(parse(menu).unwrap().context, None);
        let s = format!("────────\n Tip: algo\n{menu}");
        assert_eq!(parse(&s).unwrap().context, None);
        // Una orden muy larga se corta.
        let long: String = (0..200).map(|n| format!(" línea {n}\n")).collect();
        let s = format!("────────\n{long}{menu}");
        let c = parse(&s).unwrap().context.unwrap();
        let got: Vec<&str> = c.lines().collect();
        assert_eq!(got.len(), CONTEXT_MAX + 1);
        assert_eq!(got[0], "línea 0");
        assert_eq!(got[CONTEXT_MAX - 1], format!("línea {}", CONTEXT_MAX - 1));
        assert_eq!(got[CONTEXT_MAX], "…");
    }

    #[test]
    fn only_permissions_have_context() {
        let dir = format!("{}/tests/pantallas", env!("CARGO_MANIFEST_DIR"));
        let mut seen = 0;
        for e in std::fs::read_dir(dir).unwrap() {
            let name = e.unwrap().file_name().into_string().unwrap();
            let name = name.strip_suffix(".txt").unwrap();
            if let Some(m) = screen(name)
                && m.kind != Kind::Permission
            {
                assert_eq!(m.context, None, "{name}");
                seen += 1;
            }
        }
        assert!(seen > 5);
        assert_eq!(screen("plan").unwrap().kind, Kind::Plan);
    }

    #[test]
    fn review() {
        let m = screen("repaso").unwrap();
        assert_eq!(m.kind, Kind::Review);
        assert_eq!(m.title.as_deref(), Some("Ready to submit your answers?"));
        assert_eq!(labels(&m), ["Submit answers", "Cancel"]);
    }

    #[test]
    fn permission() {
        let m = screen("permiso").unwrap();
        assert_eq!(m.kind, Kind::Permission);
        assert_eq!(m.title.as_deref(), Some("Do you want to proceed?"));
        assert_eq!(m.options.len(), 4);
        assert_eq!(m.options[1].label, "Yes, and always allow access to");
        assert_eq!(
            m.options[1].detail.as_deref(),
            Some("/home/ana/proyecto from this project")
        );
        assert_eq!(m.hint.as_deref(), Some("Esc to cancel · Tab to amend"));
        assert_eq!(m.find("Yes"), Some(0));
        assert_eq!(m.find("No"), Some(3));
        assert_eq!(
            m.find("Yes, and always allow access to /home/ana/proyecto"),
            Some(1)
        );
        assert_eq!(m.find("4"), None);
    }

    #[test]
    fn plan() {
        let m = screen("plan").unwrap();
        assert_eq!(m.kind, Kind::Plan);
        assert_eq!(
            labels(&m),
            [
                "Yes, and use auto mode",
                "Yes, manually approve edits",
                "Tell Claude what to change"
            ]
        );
        assert!(m.options[2].input);
        assert_eq!(
            m.options[2].detail.as_deref(),
            Some("shift+tab to approve with this feedback")
        );
    }

    #[test]
    fn trust_folder_has_no_numbers() {
        // Se lee como un menú (sin números) para que el usuario la conteste desde la web o la
        // app; pero es la pregunta de confiar en la carpeta, que nido no contesta nunca (ni
        // `answer` del MCP).
        let s = include_str!("../tests/pantallas/confianza.txt");
        assert!(trust_screen(s));
        let m = screen("confianza").unwrap();
        assert_eq!(m.kind, Kind::Menu);
        assert_eq!(labels(&m), ["No, exit", "Yes, I trust this folder"]);
        assert_eq!(m.options[0].n, None);
        assert!(m.options[0].selected);
        assert_eq!(m.title.as_deref(), Some("Security guide"));
    }

    #[test]
    fn trust_folder_in_older_claude() {
        // Claude Code de antes preguntaba con otras palabras.
        let s = "\
╭──────────────────────────────────────────────────────────────╮
│                                                              │
│ Do you trust the files in this folder?                       │
│                                                              │
│ /home/ana/proyecto                                           │
│                                                              │
│ Claude Code may read files in this folder. Reading untrusted │
│ files may lead Claude Code to behave in unexpected ways.     │
│                                                              │
│ ❯ 1. Yes, proceed                                            │
│   2. No, exit                                                │
│                                                              │
╰──────────────────────────────────────────────────────────────╯
";
        assert!(trust_screen(s));
        // Ya sin la pregunta a la vista (se fue hacia arriba), «Yes, proceed» con lo de confiar.
        let cut: String = s.lines().skip(5).map(|l| format!("{l}\n")).collect();
        assert!(trust_screen(&cut), "{cut}");
        // «Yes, proceed» solo, en otro menú del arranque, no lo es.
        let other = "╭────────────────────────╮\n│ Some other question?   │\n│                        │\n\
                     │ ❯ 1. Yes, proceed      │\n│   2. No                │\n╰────────────────────────╯\n";
        assert!(!trust_screen(other));
        assert!(trust_screen(
            &other.replace("Some other question?", "Do you trust it?    ")
        ));
    }

    #[test]
    fn trust_is_only_the_startup_question() {
        // El primer mensaje (o una respuesta) que cita la pregunta no lo es: Claude ya espera.
        let caja = "─".repeat(40);
        for talk in [
            "> ¿Te sale «Do you trust this folder»? Contesta «Yes, I trust this folder»",
            "❯ Quick safety check: Accessing workspace",
            "● Sale «Yes, I trust this folder» al arrancar",
        ] {
            let s = format!(" ✻ Welcome to Claude Code!\n\n{talk}\n\n{caja}\n❯ \n{caja}\n");
            assert!(!trust_screen(&s), "{s}");
            assert!(repl(&s), "{s}");
        }
        // Debajo de la caja tampoco.
        let s = format!(" ✻ Welcome\n{caja}\n❯ \n{caja}\n  trust this folder\n");
        assert!(!trust_screen(&s));
        // La de verdad, sí; y con una ruta que lleve una caja de mentira debajo del rótulo,
        // también (lo que hay encima ya la delata): ni caja de Claude ni aviso de los channels.
        let s = include_str!("../tests/pantallas/confianza.txt");
        assert!(trust_screen(s) && !repl(s));
        let trampa = s.replace(" /home/ana/proyecto", &format!(" /home/ana/x\n{caja}\n❯ y"));
        assert!(trust_screen(&trampa), "{trampa}");
        assert!(!repl(&trampa));
    }

    #[test]
    fn usage_limit_menu() {
        let s = "● You've hit your session limit · resets 5pm (Europe/Madrid)\n\n What do you want to do?\n\n ❯ 1. Stop and wait for limit to reset\n   2. Wait here, then continue automatically when the limit resets\n   3. Upgrade your plan\n\n Enter to confirm · Esc to cancel";
        let m = parse(s).unwrap();
        assert_eq!(m.kind, Kind::Limit);
        assert_eq!(m.title.as_deref(), Some("What do you want to do?"));
        assert!(waiting_for_limit(
            "Claude Code will continue automatically at 5pm. Keep this session open; it may still pause for permission prompts. Press esc to cancel the wait."
        ));
    }

    #[test]
    fn dev_channels_warning() {
        let s = include_str!("../tests/pantallas/aviso_channels.txt");
        assert_eq!(dev_channels(s), Some(vec!["server:nido".to_string()]));
        let o = include_str!("../tests/pantallas/aviso_channels_otro.txt");
        assert_eq!(
            dev_channels(o),
            Some(vec!["server:nido".into(), "server:otro".into()])
        );
        assert_eq!(dev_channels("❯ Try \"fix lint errors\""), None);
        // La pantalla llega con colores.
        let c = s
            .replace(
                "WARNING: Loading development channels",
                "\x1b[1mWARNING: Loading development channels\x1b[0m",
            )
            .replace(
                "Channels: server:nido",
                "\x1b[38;5;214mChannels:\x1b[39m \x1b[1mserver:nido\x1b[0m",
            )
            .replace(
                "❯ 1. I am using this for local development",
                "\x1b[38;5;214m❯ 1. I am using this for local development\x1b[39m",
            );
        assert_ne!(c, s);
        assert_eq!(dev_channels(&c), Some(vec!["server:nido".to_string()]));
    }

    #[tokio::test]
    async fn a_slow_holder_is_not_a_closed_one() {
        let _limit = crate::watchdog::limit(Duration::from_secs(60));
        // Sigue ahí pero no contesta: tarda, no se ha cerrado.
        let (a, b) = tokio::io::duplex(1 << 16);
        let (rd, wr) = tokio::io::split(a);
        let mut s = Screen::new(rd, wr);
        assert_eq!(s.output().await, Err(Unread::Slow));
        drop(b);
        // Cerrado.
        let (a, b) = tokio::io::duplex(1 << 16);
        drop(b);
        let (rd, wr) = tokio::io::split(a);
        let mut s = Screen::new(rd, wr);
        assert_eq!(s.output().await, Err(Unread::Closed));
        // Lo pide con tope (y `all`, por si es un guardián de antes, que no sabe de `launch`).
        let (a, mut b) = tokio::io::duplex(1 << 16);
        let (rd, wr) = tokio::io::split(a);
        let mut s = Screen::new(rd, wr);
        let holder = tokio::spawn(async move {
            let f = proto::read_frame(&mut b).await.unwrap();
            proto::write_frame(&mut b, &Frame::History(b"hola".to_vec()))
                .await
                .unwrap();
            f
        });
        assert_eq!(s.output().await.as_deref(), Ok("hola"));
        assert_eq!(
            holder.await.unwrap(),
            Some(Frame::HistoryRequest(HistoryRequest::launch(LAUNCH_CAP)))
        );
    }

    #[test]
    fn what_this_launch_said_about_the_channel() {
        // Una sesión retomada con mucha conversación: el saludo y las filas de «blocked» ya no
        // están en pantalla, sino en lo que ha sacado desde que arrancó (su historial).
        let bloqueado = include_str!("../tests/pantallas/channels_bloqueados.txt");
        let sin = include_str!("../tests/pantallas/channels_sin_servidor.txt");
        let (head, tail) = bloqueado.split_at(bloqueado.find('─').unwrap());
        let mut talk = String::from("> sigue con lo de ayer\n\n");
        for i in 0..60 {
            talk.push_str(&format!("● Paso {i}: hecho.\n\n"));
        }
        let shown: Vec<&str> = talk.lines().collect();
        let visible = format!("{}\n{tail}", shown[shown.len() - 20..].join("\n"));
        assert_eq!(
            startup(&visible),
            Startup::Nothing,
            "en pantalla ya no está"
        );
        let antes = "\u{1}restart 100\n● De otro arranque.\n";
        let since = format!("{antes}\u{1}restart 200\n{head}{talk}{tail}");
        assert_eq!(launch_refusal(&since), Some(Startup::Blocked));
        // Lo de antes de este arranque no cuenta (la política pudo cambiar).
        let viejo = format!("\u{1}restart 100\n{head}{talk}\u{1}restart 200\n{talk}{tail}");
        assert_eq!(launch_refusal(&viejo), None);
        // Sin marca de rearranque (la primera vez), todo es de este.
        assert_eq!(
            launch_refusal(&format!("{head}{talk}{tail}")),
            Some(Startup::Blocked)
        );
        // Claude repinta (ESC[3J deja una marca `reset`): se mira cada trozo por separado, aunque
        // en uno anterior saliera ya su caja.
        let repintado = format!("\u{1}restart 200\n{tail}\n\u{1}reset 201\n{head}{tail}");
        assert_eq!(launch_refusal(&repintado), Some(Startup::Blocked));
        let sin_head = &sin[..sin.find('─').unwrap()];
        assert_eq!(
            launch_refusal(&format!("\u{1}restart 1\n{sin_head}{talk}{tail}")),
            Some(Startup::NoServer)
        );
        // Citado en la conversación, no.
        let citado = format!(
            "\u{1}restart 1\n{talk}  {}\n{tail}",
            &head[head.find('▎').unwrap()..]
        );
        assert_eq!(launch_refusal(&citado), None);
        assert_eq!(launch_refusal(""), None);
    }

    #[test]
    fn what_to_do_with_the_startup_screen() {
        let aviso = include_str!("../tests/pantallas/aviso_channels.txt");
        assert_eq!(startup(aviso), Startup::Accept);
        assert_eq!(
            startup(include_str!("../tests/pantallas/aviso_channels_otro.txt")),
            Startup::Foreign(vec!["server:nido".into(), "server:otro".into()])
        );
        // Solo el channel de nido, y ninguno más.
        assert_eq!(
            startup(&aviso.replace("server:nido", "server:otro")),
            Startup::Foreign(vec!["server:otro".into()])
        );
        assert_eq!(
            startup(&aviso.replace("Channels: server:nido", "Channels:")),
            Startup::Foreign(vec![])
        );
        let sin = include_str!("../tests/pantallas/channels_sin_servidor.txt");
        assert_eq!(startup(sin), Startup::NoServer);
        assert_eq!(
            startup(&sin.replace(
                "no MCP server configured",
                "\x1b[33mno MCP server configured\x1b[39m"
            )),
            Startup::NoServer
        );
        // La de confiar en la carpeta la contesta el usuario: se sigue esperando.
        assert_eq!(
            startup(include_str!("../tests/pantallas/confianza.txt")),
            Startup::Nothing
        );
        assert_eq!(
            startup(include_str!("../tests/pantallas/normal.txt")),
            Startup::Nothing
        );
        assert_eq!(startup(""), Startup::Nothing);

        // Partida en dos filas (una terminal estrecha), la lista es la de las dos.
        let partido = include_str!("../tests/pantallas/aviso_channels_partido.txt");
        assert_eq!(
            dev_channels(partido),
            Some(vec!["server:nido".into(), "server:otro".into()])
        );
        assert_eq!(
            startup(partido),
            Startup::Foreign(vec!["server:nido".into(), "server:otro".into()])
        );
        // Una carpeta cuyo nombre lleva esas líneas, en la pregunta de confiar en ella: ni Enter
        // (sería confiar en la carpeta) ni «sin servidor».
        let trampa = include_str!("../tests/pantallas/confianza_trampa.txt");
        assert!(trust_screen(trampa));
        assert_eq!(dev_channels(trampa), None);
        assert_eq!(startup(trampa), Startup::Nothing);
        assert!(trust_screen(include_str!(
            "../tests/pantallas/confianza.txt"
        )));
        assert!(!trust_screen(aviso));
        // Los textos, sueltos o en otro orden, no son el aviso.
        assert_eq!(
            dev_channels(
                "WARNING: Loading development channels\n❯ 1. I am using this for local development\nChannels: server:nido"
            ),
            None
        );
        assert_eq!(
            dev_channels(
                "● dijo: WARNING: Loading development channels\nChannels: server:nido\n❯ 1. I am using this for local development"
            ),
            None
        );
        // Con el cursor en «2. Exit», Enter saldría de Claude: se espera.
        let en_salir = aviso
            .replace("❯ 1. I am", "  1. I am")
            .replace("    2. Exit", "  ❯ 2. Exit");
        assert_ne!(en_salir, aviso);
        assert_eq!(dev_channels(&en_salir), Some(vec!["server:nido".into()]));
        assert_eq!(startup(&en_salir), Startup::Nothing);
        // Una conversación (retomada) que cita el aviso y la fila de «sin servidor», con la caja
        // de Claude debajo: no cuenta, solo lo de arranque, sobre la caja y antes de la
        // conversación.
        let citado = include_str!("../tests/pantallas/conversacion_aviso.txt");
        assert_eq!(dev_channels(citado), None);
        assert_eq!(startup(citado), Startup::Nothing);
        // Aunque el principio del mensaje ya no se vea: con la caja de Claude en pantalla no
        // puede estar el aviso.
        let cortado: String = citado
            .lines()
            .skip_while(|l| !l.contains("────────────────"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(cortado.starts_with("    ────"), "{cortado}");
        assert_eq!(dev_channels(&cortado), None);
        assert_eq!(startup(&cortado), Startup::Nothing);
        // La de verdad, con la caja debajo, sí.
        assert_eq!(startup(sin), Startup::NoServer);
        // Channels prohibidos por la organización de la cuenta: sin aviso, ya con la caja.
        let bloqueado = include_str!("../tests/pantallas/channels_bloqueados.txt");
        assert_eq!(startup(bloqueado), Startup::Blocked);
        assert_eq!(dev_channels(bloqueado), None);
        let rows: Vec<&str> = bloqueado.lines().collect();
        let with =
            |f: &dyn Fn(&str) -> String| rows.iter().map(|l| f(l)).collect::<Vec<_>>().join("\n");
        // Las dos filas, y con `server:nido` en la lista.
        let solo = with(&|l| {
            if l.contains("Inbound messages") {
                String::new()
            } else {
                l.to_owned()
            }
        });
        assert_eq!(startup(&solo), Startup::Nothing);
        let otro = with(&|l| l.replace("(server:nido)", "(server:otro)"));
        assert_eq!(startup(&otro), Startup::Nothing);
        let varios = with(&|l| l.replace("(server:nido)", "(server:otro, server:nido)"));
        assert_eq!(startup(&varios), Startup::Blocked);
        // Citado en la conversación, debajo de una petición, no cuenta.
        let citado = with(&|l| {
            if l.starts_with('▎') && l.contains("blocked") {
                format!("> ¿qué es esto?\n\n● Esto:\n  {l}")
            } else if l.starts_with('▎') {
                format!("  {l}")
            } else {
                l.to_owned()
            }
        });
        assert_eq!(startup(&citado), Startup::Nothing);
        // Sin el rótulo de Claude Code encima (ya se fue de la pantalla), tampoco.
        let sin_rotulo = rows[3..].join("\n");
        assert_eq!(startup(&sin_rotulo), Startup::Nothing);
        // Partida en varias filas (terminal estrecha, o varios channels), igual.
        let partido = |rows_: &[&str]| {
            let mut v: Vec<String> = rows[..3].iter().map(|l| l.to_string()).collect();
            v.extend(rows_.iter().map(|l| l.to_string()));
            v.extend(rows[6..].iter().map(|l| l.to_string()));
            v.join("\n")
        };
        let p = partido(&[
            "▎ --dangerously-load-development-channels blocked by org policy",
            "▎ (server:otro, server:nido)",
            "▎ Inbound messages will be silently dropped",
        ]);
        assert_eq!(startup(&p), Startup::Blocked, "{p}");
        let p = partido(&[
            "▎ --dangerously-load-development-channels blocked by",
            "▎ org policy (server:nido)",
            "▎ Inbound messages will be silently",
            "▎ dropped",
            "▎ Have an administrator set channelsEnabled: true",
        ]);
        assert_eq!(startup(&p), Startup::Blocked, "{p}");
        let p = partido(&[
            "▎ --dangerously-load-development-channels blocked by org policy",
            "▎ (server:otro)",
            "▎ Inbound messages will be silently dropped",
        ]);
        assert_eq!(startup(&p), Startup::Nothing, "{p}");
        // Claude ya espera peticiones (su caja), y no en el aviso ni en la pregunta de confiar.
        assert!(repl(bloqueado));
        assert!(repl(sin));
        assert!(!repl(aviso));
        assert!(!repl(include_str!("../tests/pantallas/confianza.txt")));
        // Con nombre (`-n`), Claude lo pone en la raya de arriba de su caja: es su caja igual.
        let nombrada = include_str!("../tests/pantallas/caja_con_nombre.txt");
        assert!(repl(nombrada));
        assert_eq!(startup(nombrada), Startup::Nothing);
        assert_eq!(dev_channels(nombrada), None);
        // Y con ella lo de antes se lee igual.
        let named = |screen: &str| {
            let rows: Vec<&str> = screen.lines().collect();
            let top = rows
                .windows(2)
                .rposition(|w| w[0].starts_with("──────") && w[1].starts_with('❯'))
                .expect("caja");
            let mut out: Vec<String> = rows.iter().map(|l| l.to_string()).collect();
            let n = out[top].chars().count();
            out[top] = format!("{} hija-b ─", "─".repeat(n - 9));
            out.join("\n")
        };
        let b = named(bloqueado);
        assert_ne!(b, bloqueado.trim_end());
        assert!(repl(&b));
        assert_eq!(startup(&b), Startup::Blocked);
        assert_eq!(launch_refusal(&b), Some(Startup::Blocked));
        let b = named(sin);
        assert!(repl(&b));
        assert_eq!(startup(&b), Startup::NoServer);
        let c = named(&citado);
        assert!(repl(&c));
        assert_eq!(dev_channels(&c), None);
        assert_eq!(startup(&c), Startup::Nothing);
        let c = named(include_str!("../tests/pantallas/conversacion_aviso.txt"));
        let cortado: String = c
            .lines()
            .skip_while(|l| !l.contains("────────────────"))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(dev_channels(&cortado), None);
        assert_eq!(startup(&cortado), Startup::Nothing);
        // «no MCP server configured» solo cuenta en la fila de Channels de nido: no en lo que
        // diga la conversación retomada.
        let normal = include_str!("../tests/pantallas/normal.txt");
        for other in [
            "● Si sale «no MCP server configured with that name», el channel no va.",
            "  Channels: server:otro · no MCP server configured with that name",
            "  server:nido · no MCP server configured with that name",
        ] {
            assert_eq!(
                startup(&format!("{other}\n{normal}")),
                Startup::Nothing,
                "{other}"
            );
        }
    }

    /// El primer mensaje de una hija de `open` (lo escribe quien la abre, y Claude lo pinta con
    /// `❯ ` delante) es conversación, aunque cite las filas de «sin servidor» o «blocked».
    #[test]
    fn an_opened_childs_first_message_is_talk() {
        let cita = include_str!("../tests/pantallas/primer_mensaje_cita.txt");
        assert!(repl(cita));
        assert_eq!(startup(cita), Startup::Nothing);
        assert_eq!(launch_refusal(cita), None);
        // Aún sin contestar (sin `●` debajo), igual.
        let sin_respuesta = cita.replace("● Lo miro.", "");
        assert_ne!(sin_respuesta, cita);
        assert_eq!(startup(&sin_respuesta), Startup::Nothing);
        assert_eq!(launch_refusal(&sin_respuesta), None);
        // La opción del aviso («❯ 1. …») no es un mensaje: el aviso se sigue leyendo.
        let aviso = include_str!("../tests/pantallas/aviso_channels.txt");
        let a_la_izquierda: String = aviso
            .lines()
            .map(|l| l.trim_start())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(a_la_izquierda.contains("\n❯ 1. I am using"));
        assert_eq!(startup(&a_la_izquierda), Startup::Accept);
        assert_eq!(startup(aviso), Startup::Accept);
    }

    #[test]
    fn a_rule_may_carry_the_session_name() {
        let long = "─".repeat(40);
        assert!(is_rule(&long));
        assert!(is_rule(&format!("{long} hija-b ─")));
        assert!(is_rule(&format!("  {long} mi sesión ─")));
        assert!(is_rule(&format!("{long} nido ──")));
        // Un nombre largo en una pantalla estrecha (el teléfono): 20 caracteres en 40 columnas,
        // y aunque el nombre sea más largo que la raya de la izquierda.
        let name = "sesion-de-veinte-car";
        assert_eq!(name.chars().count(), 20);
        let narrow = format!("{} {name} ─", "─".repeat(40 - 20 - 3));
        assert_eq!(narrow.chars().count(), 40);
        assert!(is_rule(&narrow));
        assert!(is_rule(&format!("───── {name} ─")));
        // Texto con rayas y palabras: no.
        for l in [
            "── nota ─".to_owned(),
            format!("{long} hola ───"),
            format!("{long} {} ─", "x".repeat(65)),
            format!("Texto {long} hola ─"),
            format!("{long} hola"),
            format!("{long} hola ─ adiós"),
            format!("{long}  ─"),
            format!("{long}hola ─"),
            format!("{long} hola─"),
            format!("{long}  hola ─"),
            format!("{long} ── ─"),
            format!("● {long} hola ─"),
            format!("> {long} hola ─"),
        ] {
            assert!(!is_rule(&l), "{l}");
        }
    }

    #[test]
    fn suggested_next_prompt() {
        let rule = "\x1b[38;2;136;136;136m────────────────────────\x1b[m";
        let box_ = |line: &str| {
            format!("✻ Brewed for 11s\n\n{rule}──── nido ─\n{line}\n{rule}\n  ⏵⏵ auto mode on")
        };
        assert_eq!(
            suggestion(&box_(
                "\x1b[m❯\u{a0} \x1b[2mok a shara-media, y borra tú feature/…"
            ))
            .as_deref(),
            Some("ok a shara-media, y borra tú feature/…")
        );
        // Lo que has escrito tú (sin atenuar), la caja vacía o el ejemplo del principio: no.
        assert_eq!(suggestion(&box_("\x1b[m❯\u{a0} esto lo escribo yo")), None);
        assert_eq!(suggestion(&box_("\x1b[m❯\u{a0} ")), None);
        assert_eq!(
            suggestion(&box_("\x1b[m❯\u{a0} \x1b[2mTry \"fix lint errors\"")),
            None
        );
        assert_eq!(
            suggestion(&box_(
                "\x1b[m❯\u{a0} \x1b[7mP\x1b[2mress up to edit queued messages"
            )),
            None
        );
        assert_eq!(
            suggestion(&box_(
                "\x1b[m❯\u{a0} \x1b[2mPress up to edit queued messages"
            )),
            None
        );
        // Un menú no es la caja de escribir.
        assert_eq!(
            suggestion(" Do you want to proceed?\n ❯ 1. Yes\n   2. No"),
            None
        );
    }

    #[test]
    fn a_queued_message_with_a_list_is_not_a_menu() {
        assert_eq!(screen("cola"), None);
    }

    #[test]
    fn the_queue_on_screen() {
        let real = std::fs::read_to_string(format!(
            "{}/tests/pantallas/cola.txt",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap();
        assert!(queue_shown(&real));
        assert!(!something_open(&real));
        // (Sin colores: la ayuda atenuada se ve como texto.)
        assert_eq!(input_box(&real).as_deref(), Some(QUEUE_HINT));
        // Sin la cola, nada.
        let none = real.replace(SEND_NOW, "");
        assert!(!queue_shown(&none));
        assert!(!queue_shown(
            &std::fs::read_to_string(format!(
                "{}/tests/pantallas/normal.txt",
                env!("CARGO_MANIFEST_DIR")
            ))
            .unwrap()
        ));
        // Dos mensajes, uno de dos líneas, con colores; y Claude trabajando debajo.
        let rule = "────────────────────────────────────────";
        let two = format!(
            "⏺ Mirando los ficheros.\n\n\x1b[2m❯\u{a0}primero\x1b[22m\n\x1b[2m❯\u{a0}segundo, que\x1b[22m\n\x1b[2m  sigue aquí\x1b[22m\n\x1b[2m  ctrl+x ctrl+s to send now\x1b[22m\n\n✶ Pensando… (12s)\n\n{rule}\n❯\u{a0}\x1b[2mPress up to edit queued messages\x1b[22m\n{rule}\n  ⏵⏵ auto mode on · esc to interrupt"
        );
        assert!(queue_shown(&two));
        // El segundo, una lista numerada: no es un menú.
        let list = format!(
            "⏺ Mirando.\n\n❯\u{a0}primero\n❯\u{a0}1. sí\n  2. no\n  3. luego\n  ctrl+x ctrl+s to send now\n\n✶ Pensando… (3s)\n\n{rule}\n❯\u{a0}\x1b[2mPress up to edit queued messages\x1b[22m\n{rule}\n  ⏵⏵ auto mode on · esc to interrupt"
        );
        assert!(queue_shown(&list));
        assert_eq!(parse(&list), None);
        assert!(!something_open(&list));
        // Lo mismo dicho por Claude en una respuesta no es la cola.
        let said = format!(
            "⏺ Verás esto:\n  ctrl+x ctrl+s to send now\n\n{rule}\n❯\u{a0}\n{rule}\n  ⏵⏵ auto mode on"
        );
        assert!(!queue_shown(&said));
        // Ni lo que quedó más arriba, con la conversación ya debajo.
        let old = format!(
            "❯ primero\n  ctrl+x ctrl+s to send now\n❯ primero\n\n⏺ Hecho.\n\n{rule}\n❯\u{a0}\n{rule}\n  ⏵⏵ auto mode on"
        );
        assert!(!queue_shown(&old));
    }

    #[test]
    fn how_many_are_queued_on_screen() {
        let real = std::fs::read_to_string(format!(
            "{}/tests/pantallas/cola.txt",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap();
        // Uno de cinco líneas.
        assert_eq!(queue_count(&real), Some(1));
        assert_eq!(queue_count(&real.replace(SEND_NOW, "")), None);
        let rule = "────────────────────────────────────────";
        let tail = format!(
            "\n\n✶ Pensando… (3s)\n\n{rule}\n❯\u{a0}\x1b[2mPress up to edit queued messages\x1b[22m\n{rule}\n  ⏵⏵ auto mode on · esc to interrupt"
        );
        let screen = |queue: &str| {
            format!("⏺ Mirando los ficheros.\n\n{queue}\n  ctrl+x ctrl+s to send now{tail}")
        };
        assert_eq!(queue_count(&screen("❯\u{a0}solo")), Some(1));
        // Tres, uno de varias líneas (con una en blanco) y con colores.
        let three = screen(
            "\x1b[2m❯\u{a0}primero\x1b[22m\n\x1b[2m❯\u{a0}segundo, que\x1b[22m\n\x1b[2m  sigue aquí\x1b[22m\n\n\x1b[2m  y aquí\x1b[22m\n\x1b[2m❯\u{a0}1. tercero\x1b[22m\n\x1b[2m  2. con lista\x1b[22m",
        );
        assert!(queue_shown(&three));
        assert_eq!(queue_count(&three), Some(3));
        // Lo que dijo el usuario antes (más arriba, separado por una línea en blanco) no cuenta.
        let after_prompt = format!(
            "❯ lo de antes\n\n❯\u{a0}primero\n❯\u{a0}segundo\n  ctrl+x ctrl+s to send now{tail}"
        );
        assert_eq!(queue_count(&after_prompt), Some(2));
        // Al principio de la pantalla puede seguir por encima: no se sabe.
        assert_eq!(
            queue_count(&format!(
                "❯\u{a0}primero\n  ctrl+x ctrl+s to send now{tail}"
            )),
            None
        );
        // Sin cola, nada.
        assert_eq!(
            queue_count(&format!(
                "⏺ Hecho.\n\n{rule}\n❯\u{a0}\n{rule}\n  ⏵⏵ auto mode on"
            )),
            None
        );
    }

    /// Lo que se enseña en cola: los últimos de la transcripción, tantos como hay en la pantalla.
    #[test]
    fn the_queue_is_trimmed_to_the_screen() {
        let rule = "────────────────────────────────────────";
        let tail = format!("\n\n{rule}\n❯\u{a0}\n{rule}\n  ⏵⏵ auto mode on");
        let one = format!("⏺ Mirando.\n\n❯\u{a0}tercero\n  ctrl+x ctrl+s to send now{tail}");
        let two = format!(
            "⏺ Mirando.\n\n❯\u{a0}segundo\n❯\u{a0}tercero\n  ctrl+x ctrl+s to send now{tail}"
        );
        let q = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let all = q(&["fantasma", "segundo", "tercero"]);
        assert_eq!(queued_on_screen(&one, all.clone()), q(&["tercero"]));
        assert_eq!(
            queued_on_screen(&two, all.clone()),
            q(&["segundo", "tercero"])
        );
        // Menos de los que se ven: los que haya.
        assert_eq!(queued_on_screen(&two, q(&["tercero"])), q(&["tercero"]));
        assert_eq!(queued_on_screen(&two, vec![]), Vec::<String>::new());
        // Sin poder contarlos (aquí, la cola llega arriba de la pantalla), todos.
        let top = format!("❯\u{a0}segundo\n❯\u{a0}tercero\n  ctrl+x ctrl+s to send now{tail}");
        assert_eq!(queued_on_screen(&top, all.clone()), all);
        assert_eq!(queued_on_screen("nada", all.clone()), all);
    }

    #[test]
    fn what_is_written_in_the_box() {
        let rule = "────────────────────────";
        let b = |inner: &str| {
            format!("✻ Brewed\n\n{rule}──── nido ─\n{inner}\n{rule}\n  ⏵⏵ auto mode on")
        };
        assert_eq!(
            input_box(&b(
                "❯\u{a0}[Image #3] [Image #4]al mandar yo un mensaje con\n  puntos, el parser"
            ))
            .as_deref(),
            Some("[Image #3] [Image #4]al mandar yo un mensaje con puntos, el parser")
        );
        assert_eq!(input_box(&b("❯\u{a0}")).as_deref(), Some(""));
        assert_eq!(
            input_box(&b("\x1b[m❯\u{a0} \x1b[2mleer el fichero")).as_deref(),
            Some("")
        );
        assert_eq!(
            input_box(&b(
                "\x1b[m❯\u{a0} \x1b[7mP\x1b[2mress up to edit queued messages"
            ))
            .as_deref(),
            Some("")
        );
        assert_eq!(
            input_box(&b(
                "\x1b[38;2;153;153;153m❯\u{a0} \x1b[39;2mPress up to edit queued messages"
            ))
            .as_deref(),
            Some("")
        );
        // Un color RGB con un 2 dentro no es atenuado.
        assert_eq!(
            input_box(&b("\x1b[m❯\u{a0} \x1b[38;2;200;200;200mhola")).as_deref(),
            Some("hola")
        );
        assert_eq!(
            input_box(
                &std::fs::read_to_string(format!(
                    "{}/tests/pantallas/permiso.txt",
                    env!("CARGO_MANIFEST_DIR")
                ))
                .unwrap()
            ),
            None
        );
    }

    #[test]
    fn mcp_form_with_a_checkbox() {
        let text = std::fs::read_to_string(format!(
            "{}/tests/pantallas/formulario.txt",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap();
        let (f, cur, hint) = form_state(&text).unwrap();
        assert_eq!(f.server, "prueba");
        assert!(
            f.message
                .starts_with("You are about to drop the index named **bp_assignee_1**")
        );
        assert_eq!(f.fields.len(), 1);
        let c = &f.fields[0];
        assert_eq!(
            (c.label.as_str(), c.required, c.filled, c.kind),
            ("Would you like to confirm?", true, false, Some("bool"))
        );
        assert_eq!(c.description.as_deref(), Some("Would you like to confirm?"));
        assert_eq!(cur, FormCursor::Field(0));
        assert!(hint.contains("Space to toggle"));
        // No es un menú numerado.
        assert_eq!(parse(&text), None);
    }

    #[test]
    fn mcp_form_with_several_fields_and_a_list() {
        let text = std::fs::read_to_string(format!(
            "{}/tests/pantallas/formulario_varios.txt",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap();
        let f = form(&text).unwrap();
        assert_eq!(
            f.fields
                .iter()
                .map(|x| x.label.as_str())
                .collect::<Vec<_>>(),
            ["Would you like to confirm?", "Motivo", "Entorno"]
        );
        assert_eq!(
            f.fields[1].description.as_deref(),
            Some("Por qué lo borras")
        );
        assert!(!f.fields[1].required && f.fields[1].value == "not set");
        let lista = std::fs::read_to_string(format!(
            "{}/tests/pantallas/formulario_lista.txt",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap();
        let (f, cur, _) = form_state(&lista).unwrap();
        assert_eq!(f.server, "shara-dev");
        assert_eq!(f.fields[0].kind, Some("choice"));
        assert!(f.message.contains("(+2 more lines)"));
        assert_eq!(cur, FormCursor::Field(0));
        let open = "      Entorno: ▾\n        ❯ ◯ dev\n          ◉ pre\n          ◯ prod";
        assert_eq!(
            choice_options(open),
            (vec!["dev".into(), "pre".into(), "prod".into()], Some(0))
        );
    }

    #[test]
    fn subagents_and_shells_in_the_background() {
        let wide = "❯ \n────\n  ⏵⏵ auto mode on · 3 shells · ↓ to manage\n  \n\x1b[1m  ● main\n\x1b[38;2;153;153;153;22m  ◯ general-purpose  Rerunning e2e and test battery                    20m 51s · ↓ 622.2k tokens\n  ◯ general-purpose\x1b[m  \x1b[38;2;153;153;153mAdding grace tests\x1b[m                 \x1b[38;2;153;153;153m1h 2m 4s · ↓ 209.4k tokens";
        let bg = background(wide).unwrap();
        assert_eq!(bg.shells, 3);
        assert_eq!(
            bg.agents,
            [
                Agent {
                    kind: "general-purpose".into(),
                    task: "Rerunning e2e and test battery".into(),
                    elapsed: 20 * 60 + 51
                },
                Agent {
                    kind: "general-purpose".into(),
                    task: "Adding grace tests".into(),
                    elapsed: 3724
                },
            ]
        );
        let narrow = "  ⏵⏵ auto mode on (shift+tab to cycle) · ↓ to manage\n  ● main\n  ◯ general-purpose  Extending… 7m 29s · ↓ 688.5k tokens";
        let bg = background(narrow).unwrap();
        assert_eq!(
            (bg.shells, bg.agents[0].task.as_str(), bg.agents[0].elapsed),
            (0, "Extending…", 449)
        );
        assert_eq!(
            background("  ⏵⏵ auto mode on · 1 shell · ↓ to manage")
                .unwrap()
                .shells,
            1
        );
        assert_eq!(
            background("❯ \n  ⏵⏵ auto mode on (shift+tab to cycle)"),
            None
        );
    }

    #[test]
    fn the_prompt_box_is_not_a_menu() {
        assert_eq!(screen("normal"), None);
    }

    #[test]
    fn a_numbered_list_from_claude_is_not_a_menu() {
        let s = "● Pasos:\n  1. Uno\n  2. Dos\n\n────────\n❯ \n────────\n  ⏵⏵ auto mode on";
        assert_eq!(parse(s), None);
    }

    #[test]
    fn colours_are_ignored() {
        let s = " \x1b[1mDo you want to proceed?\x1b[m\n \x1b[38;2;1;2;3m❯ 1. Yes\x1b[m\n   2. No\n\n Esc to cancel";
        let m = parse(s).unwrap();
        assert_eq!(m.kind, Kind::Permission);
        assert_eq!(labels(&m), ["Yes", "No"]);
        assert_eq!(m.context, None);
    }
    // ---------- El modo y el modelo ----------

    fn footer(line: &str) -> String {
        format!("● Hecho.\n\n────────────────\n❯ \n────────────────\n{line}")
    }

    #[test]
    fn the_mode_from_the_footer() {
        let cases = [
            (
                "  ⏵⏵ auto mode on (shift+tab to cycle) · ← for agents",
                Mode::Auto,
            ),
            (
                "  ⏵⏵ auto mode on (shift+tab to cycle) · esc to interrupt",
                Mode::Auto,
            ),
            (
                "  ⏸ manual mode on · ? for shortcuts · ← for agents",
                Mode::Manual,
            ),
            ("  ⏸ manual mode on · esc to interrupt", Mode::Manual),
            (
                "  ⏵⏵ accept edits on (shift+tab to cycle)",
                Mode::AcceptEdits,
            ),
            (
                "  ⏸ plan mode on (shift+tab to cycle) · esc to interrupt",
                Mode::Plan,
            ),
            ("  ⏵⏵ auto mode on · 3 shells · ↓ to manage", Mode::Auto),
            (
                "  ⏵⏵ bypass permissions on (shift+tab to cycle)",
                Mode::BypassPermissions,
            ),
            ("  \x1b[38;2;175;135;255m⏸ plan mode on\x1b[39m", Mode::Plan),
        ];
        for (line, want) in cases {
            assert_eq!(mode(&footer(line)), Some(want), "{line}");
        }
        // Con los subagentes debajo.
        let s = footer(
            "  ⏵⏵ accept edits on · ↓ to manage\n  \n  ● main\n  ◯ general-purpose  Algo  2m 3s",
        );
        assert_eq!(mode(&s), Some(Mode::AcceptEdits));
        // Lo que no se conoce, o no se ve: None.
        assert_eq!(mode(&footer("  ⏵⏵ turbo mode on")), None);
        assert_eq!(mode(&footer("  ? for shortcuts")), None);
        assert_eq!(mode(&footer("  ⏸ planning mode on")), None);
        assert_eq!(mode("● Hola, sin caja"), None);
        // Un pie citado en la conversación no es el pie.
        let quoted =
            "● El pie decía:\n  ⏸ plan mode on\n\n────────\n❯ \n────────\n  ⏵⏵ auto mode on";
        assert_eq!(mode(quoted), Some(Mode::Auto));
        let quoted =
            "● El pie decía:\n  ⏸ plan mode on\n\n────────\n❯ \n────────\n  ? for shortcuts";
        assert_eq!(mode(quoted), None);
    }

    #[test]
    fn the_mode_in_real_screens() {
        let read = |name: &str| {
            std::fs::read_to_string(format!(
                "{}/tests/pantallas/{name}.txt",
                env!("CARGO_MANIFEST_DIR")
            ))
            .unwrap()
        };
        assert_eq!(mode(&read("normal")), Some(Mode::Auto));
        assert_eq!(mode(&read("caja_con_nombre")), Some(Mode::Manual));
        assert_eq!(mode(&read("cola")), Some(Mode::Auto));
        // Con el menú del plan (que dice «shift+tab to approve…») no se ve el pie.
        assert_eq!(mode(&read("plan")), None);
        assert_eq!(mode(&read("permiso")), None);
        assert!(working(&read("cola")));
        assert!(!working(&read("normal")));
    }

    #[test]
    fn mode_names() {
        for m in [
            Mode::Auto,
            Mode::Manual,
            Mode::AcceptEdits,
            Mode::Plan,
            Mode::BypassPermissions,
        ] {
            assert_eq!(Mode::from_name(m.name()), Some(m));
            assert_eq!(serde_json::to_value(m).unwrap(), m.name());
        }
        assert_eq!(Mode::from_name("default"), None);
        assert_eq!(Mode::from_name("Plan"), None);
    }

    const MODEL_MENU: &str = "\
● Hola.

────────────────────────────────────────────────────────────────────────────────────────────────────
   Select model
   Switch between Claude models. Your pick becomes the default for new sessions. For other/previous model names, specify with --model.

     1. Default (recommended)  Opus 5.5 · Best for everyday, complex tasks
     2. Opus                   Opus 5.5 · Best for everyday, complex tasks
     3. Fable                  Fable 5.1 · Most capable for your hardest and longest-running tasks
   ❯ 4. Fable ✔                Fable 5.2 · Most capable for your hardest and longest-running tasks
     5. Sonnet                 Sonnet 5.5 · Efficient for routine tasks
     6. Haiku                  Haiku 5.5 · Fastest for quick answers

   ◐ Medium effort (default) ←/→ to adjust

   Use /fast to turn on Fast mode (Fable 5.2).

   Enter to set as default · s to use this session only · Esc to cancel";

    fn options(m: &ModelMenu) -> Vec<(&str, &str, bool, bool)> {
        m.options
            .iter()
            .map(|o| (o.label.as_str(), o.model.as_str(), o.selected, o.current))
            .collect()
    }

    #[test]
    fn the_model_menu() {
        let m = model_menu(MODEL_MENU).unwrap();
        assert_eq!(
            options(&m),
            [
                ("Default (recommended)", "Opus 5.5", false, false),
                ("Opus", "Opus 5.5", false, false),
                ("Fable", "Fable 5.1", false, false),
                ("Fable", "Fable 5.2", true, true),
                ("Sonnet", "Sonnet 5.5", false, false),
                ("Haiku", "Haiku 5.5", false, false),
            ]
        );
        assert!(m.session_only);
        assert_eq!(
            m.models(),
            [
                "Opus 5.5",
                "Fable 5.1",
                "Fable 5.2",
                "Sonnet 5.5",
                "Haiku 5.5"
            ]
        );
        // Lo que se pide: el modelo, la etiqueta o su principio; nunca Default.
        assert_eq!(m.find("Opus 5.5"), Some(1));
        assert_eq!(m.find("opus"), Some(1));
        assert_eq!(m.find("Fable"), Some(3), "la más nueva");
        assert_eq!(m.find("Fable 5.2"), Some(3));
        assert_eq!(m.find("fable 5"), Some(3));
        assert_eq!(m.find("Sonnet"), Some(4));
        assert_eq!(m.find("Haiku 5.5"), Some(5));
        assert_eq!(m.find("Default"), None);
        assert_eq!(m.find("Default (recommended)"), None);
        assert_eq!(m.find("Op"), None);
        assert_eq!(m.find("Gemini"), None);
        assert_eq!(m.find("  "), None);
        // Sin la ayuda de `s`.
        let old = MODEL_MENU.replace(
            "Enter to set as default · s to use this session only · Esc to cancel",
            "Enter to confirm · Esc to cancel",
        );
        assert!(!model_menu(&old).unwrap().session_only);
        // Otros menús no son este.
        assert_eq!(
            model_menu(" Do you want to proceed?\n ❯ 1. Yes\n   2. No"),
            None
        );
        assert_eq!(model_menu(&footer("  ⏵⏵ auto mode on")), None);
        assert!(something_open(MODEL_MENU));
    }

    #[test]
    fn a_narrow_model_menu() {
        // En una pantalla estrecha, lo que no cabe baja a la línea siguiente.
        let narrow = "\
   Select model
   Switch between Claude models. Your pick
   becomes the default for new sessions.

     1. Default (recommended)  Opus 5.5 · Best
                               for everyday,
                               complex tasks
   ❯ 2. Opus ✔                 Opus 5.5 · Best
                               for everyday
     3. Fable
                               Fable 5.2 · Most
                               capable
     4. Haiku                  Haiku 5.5 · Fast

   ◐ Medium effort (default) ←/→ to adjust

   Enter to set as default · s to use this
   session only · Esc to cancel";
        let m = model_menu(narrow).unwrap();
        assert_eq!(
            options(&m),
            [
                ("Default (recommended)", "Opus 5.5", false, false),
                ("Opus", "Opus 5.5", true, true),
                ("Fable", "Fable 5.2", false, false),
                ("Haiku", "Haiku 5.5", false, false),
            ]
        );
        assert!(m.session_only, "la ayuda partida en dos líneas");
    }

    #[test]
    fn the_confirmations_of_a_model() {
        let s = "❯ /model\n  ⎿  Set model to Opus 5.5 for this session only\n\n────\n❯ \n────\n  ⏵⏵ auto mode on";
        assert_eq!(model_set_count(s, "Opus 5.5"), 1);
        assert_eq!(model_set_count(s, "Haiku 5.5"), 0);
        // Partida en dos líneas, y con colores.
        let s = "  ⎿  Set model to \x1b[1mOpus 5.5\x1b[22m for this\n     session only\n❯ /model\n  ⎿  Set model to Opus 5.5 for this session only";
        assert_eq!(model_set_count(s, "Opus 5.5"), 2);
    }

    use std::sync::{Arc, Mutex};

    fn claude(f: impl FnOnce(&mut fake::Claude)) -> Arc<Mutex<fake::Claude>> {
        let mut c = fake::Claude::default();
        f(&mut c);
        Arc::new(Mutex::new(c))
    }

    fn keys(c: &Arc<Mutex<fake::Claude>>) -> Vec<Vec<u8>> {
        c.lock().unwrap().keys.clone()
    }

    /// Nunca Enter con el menú de `/model` abierto, ni ←/→, ni números: como mucho un Enter, el
    /// de `/model` en la caja.
    fn never_enter_in_the_menu(c: &Arc<Mutex<fake::Claude>>) {
        let c = c.lock().unwrap();
        assert!(!c.saved_default, "Enter en el menú: {:?}", c.keys);
        assert!(!c.effort_changed, "←/→ en el menú: {:?}", c.keys);
        let enters = c.keys.iter().filter(|k| k.contains(&b'\r')).count();
        assert!(enters <= 1, "{:?}", c.keys);
        assert!(!c.digit_in_menu, "un número en el menú: {:?}", c.keys);
    }

    #[tokio::test(start_paused = true)]
    async fn mode_goes_round_until_it_gets_there() {
        let c = claude(|_| {});
        let mut scr = fake::screen_of(&c);
        assert_eq!(set_mode(&mut scr, Mode::Plan).await, Ok(Mode::Plan));
        assert_eq!(keys(&c), vec![SHIFT_TAB.to_vec(); 3]);
        assert_eq!(mode(&c.lock().unwrap().screen()), Some(Mode::Plan));
        // De plan a manual pasa por auto.
        assert_eq!(set_mode(&mut scr, Mode::Manual).await, Ok(Mode::Manual));
        assert_eq!(keys(&c).len(), 5);
        // Ya está: nada.
        assert_eq!(set_mode(&mut scr, Mode::Manual).await, Ok(Mode::Manual));
        assert_eq!(keys(&c).len(), 5);
    }

    #[tokio::test(start_paused = true)]
    async fn mode_works_while_claude_works() {
        let c = claude(|c| c.working = true);
        let mut scr = fake::screen_of(&c);
        assert_eq!(
            set_mode(&mut scr, Mode::AcceptEdits).await,
            Ok(Mode::AcceptEdits)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn an_unreachable_mode_fails_back_where_it_started() {
        let c = claude(|_| {});
        let mut scr = fake::screen_of(&c);
        let e = set_mode(&mut scr, Mode::BypassPermissions)
            .await
            .unwrap_err();
        assert!(e.contains("bypassPermissions"), "{e}");
        assert_eq!(keys(&c), vec![SHIFT_TAB.to_vec(); 4]);
        assert_eq!(mode(&c.lock().unwrap().screen()), Some(Mode::Auto));
    }

    #[tokio::test(start_paused = true)]
    async fn mode_is_not_touched_with_a_menu_or_an_odd_footer() {
        let c = claude(|c| c.permission = true);
        let mut scr = fake::screen_of(&c);
        assert!(set_mode(&mut scr, Mode::Plan).await.is_err());
        let c2 = claude(|c| c.odd_footer = true);
        let mut scr2 = fake::screen_of(&c2);
        assert_eq!(set_mode(&mut scr2, Mode::Plan).await, Err(NO_MODE.into()));
        assert!(keys(&c).is_empty() && keys(&c2).is_empty());
        let c3 = claude(|c| c.menu = Some(0));
        let mut scr3 = fake::screen_of(&c3);
        assert!(set_mode(&mut scr3, Mode::Plan).await.is_err());
        assert!(keys(&c3).is_empty());
    }

    fn queued(c: &mut fake::Claude) {
        c.working = true;
        c.queue = vec!["primero".into(), "segundo\ncon dos líneas".into()];
    }

    fn texts(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_owned()).collect()
    }

    #[tokio::test(start_paused = true)]
    async fn send_the_queue_now() {
        let c = claude(queued);
        let mut scr = fake::screen_of(&c);
        assert_eq!(send_queue(&mut scr).await, Ok(()));
        assert_eq!(keys(&c), vec![CTRL_X.to_vec(), CTRL_S.to_vec()]);
        let c = c.lock().unwrap();
        assert!(c.queue.is_empty());
        assert_eq!(c.sent_now, ["primero", "segundo\ncon dos líneas"]);
    }

    #[tokio::test(start_paused = true)]
    async fn send_the_queue_needs_a_queue_and_no_menu() {
        // Sin cola.
        let c = claude(|c| c.working = true);
        let mut scr = fake::screen_of(&c);
        assert_eq!(send_queue(&mut scr).await, Err(NO_QUEUE.into()));
        assert!(keys(&c).is_empty());
        // Con un permiso en pantalla.
        let c = claude(|c| {
            queued(c);
            c.permission = true;
        });
        let mut scr = fake::screen_of(&c);
        assert_eq!(send_queue(&mut scr).await, Err(MENU_OPEN.into()));
        assert!(keys(&c).is_empty());
        // Claude no la manda: sigue ahí.
        let c = claude(|c| {
            queued(c);
            c.send_now_ignored = true;
        });
        let mut scr = fake::screen_of(&c);
        let e = send_queue(&mut scr).await.unwrap_err();
        assert!(e.contains("sigue"), "{e}");
        assert_eq!(c.lock().unwrap().queue.len(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn edit_the_queue() {
        let c = claude(queued);
        let mut scr = fake::screen_of(&c);
        // Lo que dice la transcripción tras ↑ manda.
        let got = edit_queue(&mut scr, &texts(&["viejo"]), || {
            texts(&["primero", "segundo\ncon dos líneas"])
        })
        .await;
        let want = "primero\nsegundo\ncon dos líneas";
        assert_eq!(got.as_deref(), Ok(want));
        let k = keys(&c);
        assert_eq!(k[0], UP.to_vec());
        assert_eq!(k.len(), 1 + want.chars().count());
        assert!(k[1..].iter().all(|k| k == BACKSPACE));
        let c = c.lock().unwrap();
        assert!(c.queue.is_empty() && c.input.is_empty());
        assert!(c.sent_now.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn edit_the_queue_waits_for_every_line_of_the_transcript() {
        // La transcripción tiene primero un popAll y luego el otro.
        let c = claude(queued);
        let mut scr = fake::screen_of(&c);
        let reads = std::cell::Cell::new(0);
        let got = edit_queue(
            &mut scr,
            &texts(&["primero", "segundo\ncon dos líneas"]),
            || {
                reads.set(reads.get() + 1);
                if reads.get() == 1 {
                    texts(&["primero"])
                } else {
                    texts(&["primero", "segundo\ncon dos líneas"])
                }
            },
        )
        .await;
        assert_eq!(got.as_deref(), Ok("primero\nsegundo\ncon dos líneas"));
        // Sin la cola de antes (no se leyó), igual: espera a que no cambie.
        let c = claude(queued);
        let mut scr = fake::screen_of(&c);
        let reads = std::cell::Cell::new(0);
        let got = edit_queue(&mut scr, &[], || {
            reads.set(reads.get() + 1);
            if reads.get() == 1 {
                texts(&["primero"])
            } else {
                texts(&["primero", "segundo\ncon dos líneas"])
            }
        })
        .await;
        assert_eq!(got.as_deref(), Ok("primero\nsegundo\ncon dos líneas"));
        assert!(c.lock().unwrap().input.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn edit_the_queue_without_the_transcript() {
        // Sin lo de después de ↑: lo que había en cola.
        let c = claude(queued);
        let mut scr = fake::screen_of(&c);
        let got = edit_queue(
            &mut scr,
            &texts(&["primero", "segundo\ncon dos líneas"]),
            Vec::new,
        )
        .await;
        assert_eq!(got.as_deref(), Ok("primero\nsegundo\ncon dos líneas"));
        assert!(c.lock().unwrap().input.is_empty());
        // Sin nada: lo que se vio en la caja.
        let c = claude(queued);
        let mut scr = fake::screen_of(&c);
        let got = edit_queue(&mut scr, &[], Vec::new).await;
        assert_eq!(got.as_deref(), Ok("primero segundo con dos líneas"));
        assert!(c.lock().unwrap().input.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn edit_the_queue_touches_nothing_with_a_draft_or_a_menu() {
        let c = claude(|c| {
            queued(c);
            c.input = "a medio".into();
        });
        let mut scr = fake::screen_of(&c);
        let e = edit_queue(&mut scr, &[], Vec::new).await.unwrap_err();
        assert!(e.contains("escrito"), "{e}");
        assert!(keys(&c).is_empty());
        let c = claude(|c| {
            queued(c);
            c.permission = true;
        });
        let mut scr = fake::screen_of(&c);
        assert_eq!(
            edit_queue(&mut scr, &[], Vec::new).await,
            Err(MENU_OPEN.into())
        );
        assert!(keys(&c).is_empty());
        let c = claude(|c| c.working = true);
        let mut scr = fake::screen_of(&c);
        assert_eq!(
            edit_queue(&mut scr, &[], Vec::new).await,
            Err(NO_QUEUE.into())
        );
        assert!(keys(&c).is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn edit_the_queue_says_when_the_text_stays_in_the_box() {
        let c = claude(|c| {
            queued(c);
            c.deaf_backspace = true;
        });
        let mut scr = fake::screen_of(&c);
        let e = edit_queue(
            &mut scr,
            &texts(&["primero", "segundo\ncon dos líneas"]),
            Vec::new,
        )
        .await
        .unwrap_err();
        assert_eq!(e, STILL_IN_BOX);
        // Dos vueltas de retroceso.
        let n = "primero\nsegundo\ncon dos líneas".chars().count();
        assert_eq!(keys(&c).len(), 1 + 2 * n);
        assert_eq!(c.lock().unwrap().input, "primero\nsegundo\ncon dos líneas");
    }

    #[tokio::test(start_paused = true)]
    async fn model_for_this_session_only() {
        let c = claude(|_| {});
        let mut scr = fake::screen_of(&c);
        let mut seen = None;
        assert_eq!(
            set_model(&mut scr, "Opus 5.5", &mut seen).await,
            Ok("Opus 5.5".to_owned())
        );
        assert_eq!(
            keys(&c),
            [
                b"/model".to_vec(),
                b"\r".to_vec(),
                UP.to_vec(),
                UP.to_vec(),
                b"s".to_vec()
            ]
        );
        never_enter_in_the_menu(&c);
        assert_eq!(c.lock().unwrap().current, 1, "la opción Opus, no Default");
        assert_eq!(
            seen.unwrap(),
            [
                "Opus 5.5",
                "Fable 5.1",
                "Fable 5.2",
                "Sonnet 5.5",
                "Haiku 5.5"
            ]
        );
        // Por su etiqueta, hacia abajo.
        let mut seen = None;
        assert_eq!(
            set_model(&mut scr, "haiku", &mut seen).await,
            Ok("Haiku 5.5".to_owned())
        );
        assert_eq!(c.lock().unwrap().current, 5);
        assert_eq!(keys(&c)[5..].iter().filter(|k| *k == DOWN).count(), 4);
    }

    #[tokio::test(start_paused = true)]
    async fn model_without_the_session_only_hint_escapes() {
        let c = claude(|c| c.hint_s = false);
        let mut scr = fake::screen_of(&c);
        let mut seen = None;
        assert!(set_model(&mut scr, "Opus", &mut seen).await.is_err());
        assert_eq!(
            keys(&c),
            [b"/model".to_vec(), b"\r".to_vec(), b"\x1b".to_vec()]
        );
        never_enter_in_the_menu(&c);
        let c = c.lock().unwrap();
        assert_eq!((c.menu, c.current), (None, 3));
        assert!(seen.is_some(), "las opciones se vieron igual");
    }

    #[tokio::test(start_paused = true)]
    async fn model_not_in_the_menu_escapes() {
        let c = claude(|_| {});
        let mut scr = fake::screen_of(&c);
        let e = set_model(&mut scr, "Gemini", &mut None).await.unwrap_err();
        assert!(e.contains("Gemini"), "{e}");
        assert_eq!(keys(&c).last().unwrap(), b"\x1b");
        never_enter_in_the_menu(&c);
        assert_eq!(c.lock().unwrap().menu, None);
        // Default tampoco se elige.
        let c = claude(|_| {});
        let mut scr = fake::screen_of(&c);
        assert!(set_model(&mut scr, "Default", &mut None).await.is_err());
        never_enter_in_the_menu(&c);
        assert_eq!(c.lock().unwrap().current, 3);
    }

    #[tokio::test(start_paused = true)]
    async fn model_is_not_touched_with_a_draft_a_menu_or_claude_working() {
        for c in [
            claude(|c| c.input = "a medio escribir".into()),
            claude(|c| c.permission = true),
            claude(|c| c.working = true),
            claude(|c| c.menu = Some(3)),
        ] {
            let mut scr = fake::screen_of(&c);
            assert!(set_model(&mut scr, "Opus", &mut None).await.is_err());
            assert!(keys(&c).is_empty(), "{:?}", keys(&c));
        }
    }

    #[test]
    fn a_model_menu_with_the_models_as_labels() {
        // Otra forma del menú de Claude Code 2.1.322 (visto en una sesión de prueba).
        let s = "\
   Select model
   Switch between Claude models. Your pick becomes the default for new sessions. For other/previous model names,
   specify with --model.

     1.  Default (recommended)  Opus 5.5 · Best for everyday, complex tasks
     2.  Opus 5.5               For complex work and everyday tasks
     3.  Fable 5.2              For your toughest challenges
     4.  Sonnet 5.5             Most efficient for simpler tasks
   ❯ 5.  Haiku 5.5 ✔            Fastest for quick answers
     ▸   More models

   ◐ Medium effort (default) ←/→ to adjust

   Enter to set as default · s to use this session only · Esc to cancel";
        let m = model_menu(s).unwrap();
        assert_eq!(
            options(&m),
            [
                ("Default (recommended)", "Opus 5.5", false, false),
                ("Opus 5.5", "Opus 5.5", false, false),
                ("Fable 5.2", "Fable 5.2", false, false),
                ("Sonnet 5.5", "Sonnet 5.5", false, false),
                ("Haiku 5.5", "Haiku 5.5", true, true),
            ]
        );
        assert!(m.session_only);
        assert_eq!(
            m.models(),
            ["Opus 5.5", "Fable 5.2", "Sonnet 5.5", "Haiku 5.5"]
        );
        assert_eq!(m.find("Opus"), Some(1));
        assert_eq!(m.find("sonnet"), Some(3));
        assert_eq!(m.find("Fable 5.1"), None);
    }

    #[tokio::test(start_paused = true)]
    async fn model_with_the_models_as_labels() {
        let c = claude(|c| {
            c.models_as_labels = true;
            c.current = 5;
        });
        let mut scr = fake::screen_of(&c);
        let mut seen = None;
        assert_eq!(
            set_model(&mut scr, "Sonnet", &mut seen).await,
            Ok("Sonnet 5.5".to_owned())
        );
        assert_eq!(c.lock().unwrap().current, 4);
        never_enter_in_the_menu(&c);
    }

    #[test]
    fn the_real_switch_question() {
        // Visto con un Claude de verdad tras `s`, con conversación ya empezada.
        let s = "\
▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔
   Switch model?
   Your next response will be slower and use more tokens

   This conversation is cached for the current model. Switching to Sonnet 5.5 means the full history gets re-read.

   ❯ 1. Yes, switch to Sonnet 5.5
     2. No, go back";
        assert_eq!(switch_confirm(s, "Sonnet 5.5"), Some(1));
        assert_eq!(switch_confirm(s, "Opus 5.5"), None);
        assert!(model_dialog(s));
        assert_eq!(model_menu(s), None);
    }

    #[tokio::test(start_paused = true)]
    async fn model_asks_whether_to_switch() {
        // Con conversación, Claude pregunta si cambiar: «1. Yes, switch to …», con su número.
        let c = claude(|c| c.asks_switch = true);
        let mut scr = fake::screen_of(&c);
        assert_eq!(
            set_model(&mut scr, "Sonnet 5.5", &mut None).await,
            Ok("Sonnet 5.5".to_owned())
        );
        assert_eq!(
            keys(&c),
            [
                b"/model".to_vec(),
                b"\r".to_vec(),
                DOWN.to_vec(),
                b"s".to_vec(),
                b"1".to_vec()
            ]
        );
        never_enter_in_the_menu(&c);
        let c = c.lock().unwrap();
        assert_eq!((c.current, c.menu, c.switch), (4, None, None));
    }

    #[tokio::test(start_paused = true)]
    async fn a_stuck_switch_question_is_escaped_with_its_menu() {
        // Esc en la pregunta vuelve al menú, y otro Esc lo cierra.
        let c = claude(|c| {
            c.asks_switch = true;
            c.switch_stuck = true;
        });
        let mut scr = fake::screen_of(&c);
        assert!(set_model(&mut scr, "Sonnet", &mut None).await.is_err());
        let k = keys(&c);
        assert_eq!(k[k.len() - 2..], [b"\x1b".to_vec(), b"\x1b".to_vec()]);
        never_enter_in_the_menu(&c);
        let c = c.lock().unwrap();
        assert_eq!((c.current, c.menu, c.switch), (3, None, None));
    }

    // ---------- Lo que encontró la revisión ----------

    /// (1) Una lectura que no llega a tiempo a mitad de `/model` no deja nada abierto: ni
    /// `/model` en la caja ni el menú.
    #[tokio::test(start_paused = true)]
    async fn a_slow_read_in_the_middle_of_model_cleans_up() {
        // Tras escribir `/model`: se borra.
        let c = claude(|c| c.stall_after = Some(b"/model"));
        let mut scr = fake::screen_of(&c);
        assert!(set_model(&mut scr, "Opus", &mut None).await.is_err());
        {
            let c = c.lock().unwrap();
            assert_eq!((c.input.as_str(), c.menu), ("", None), "{:?}", c.keys);
        }
        never_enter_in_the_menu(&c);
        // Tras el Enter, con el menú ya abierto: Esc.
        let c = claude(|c| c.stall_after = Some(b"\r"));
        let mut scr = fake::screen_of(&c);
        assert!(set_model(&mut scr, "Opus", &mut None).await.is_err());
        {
            let c = c.lock().unwrap();
            assert_eq!((c.menu, c.current), (None, 3), "{:?}", c.keys);
            assert!(!c.esc_in_box, "{:?}", c.keys);
        }
        never_enter_in_the_menu(&c);
    }

    /// (1) Ni la web ni el MCP `answer` pueden pulsar nada en el menú de `/model` ni en su
    /// pregunta de si cambiar (un número o Enter lo guardaría por defecto).
    #[tokio::test(start_paused = true)]
    async fn nothing_is_answered_in_the_model_menu() {
        let reqs = [
            Request {
                option: Some("1".into()),
                ..Default::default()
            },
            Request {
                option: Some("Opus".into()),
                ..Default::default()
            },
            Request {
                message: Some(String::new()),
                ..Default::default()
            },
            Request {
                message: Some("hola".into()),
                ..Default::default()
            },
        ];
        for open in [
            (|c: &mut fake::Claude| c.menu = Some(3)) as fn(&mut fake::Claude),
            |c| c.switch = Some(4),
        ] {
            for req in &reqs {
                let c = claude(open);
                let mut scr = fake::screen_of(&c);
                assert!(answer(&mut scr, req).await.is_err(), "{req:?}");
                assert!(keys(&c).is_empty(), "{req:?}: {:?}", keys(&c));
            }
            let c = claude(open);
            let mut scr = fake::screen_of(&c);
            assert!(send_message(&mut scr, "").await.is_err());
            assert!(keys(&c).is_empty(), "{:?}", keys(&c));
        }
    }

    /// (3) Tras `s`, solo vale una confirmación nueva y del modelo elegido: no una de antes
    /// (aunque Claude no repita `/model` encima).
    #[tokio::test(start_paused = true)]
    async fn an_old_confirmation_is_not_success() {
        let c = claude(|c| {
            c.no_echo = true;
            c.s_silent = true;
            c.log
                .push("  ⎿  Set model to Opus 5.5 for this session only".into());
        });
        let mut scr = fake::screen_of(&c);
        assert!(set_model(&mut scr, "Opus", &mut None).await.is_err());
        // De otro modelo, tampoco.
        let c = claude(|c| {
            c.no_echo = true;
            c.s_silent = true;
            c.log
                .push("  ⎿  Set model to Haiku 5.5 for this session only".into());
        });
        let mut scr = fake::screen_of(&c);
        assert!(set_model(&mut scr, "Opus", &mut None).await.is_err());
        // Y la pregunta de si cambiar va antes: con una confirmación vieja en pantalla, se
        // contesta igual.
        let c = claude(|c| {
            c.no_echo = true;
            c.asks_switch = true;
            c.log
                .push("  ⎿  Set model to Sonnet 5.5 for this session only".into());
        });
        let mut scr = fake::screen_of(&c);
        assert_eq!(
            set_model(&mut scr, "Sonnet 5.5", &mut None).await,
            Ok("Sonnet 5.5".to_owned())
        );
        assert_eq!(keys(&c).last().unwrap(), b"1");
        assert_eq!(c.lock().unwrap().switch, None);
    }

    /// (4) En modo plan con Claude trabajando, shift+tab puede caer en el menú de aprobar el
    /// plan: no se pulsa. Pasando por plan de camino, sí (si no, se quedaría en plan).
    #[tokio::test(start_paused = true)]
    async fn no_shift_tab_in_plan_mode_while_working() {
        let c = claude(|c| {
            c.mode = 3;
            c.working = true;
        });
        let mut scr = fake::screen_of(&c);
        assert!(set_mode(&mut scr, Mode::Auto).await.is_err());
        assert!(keys(&c).is_empty());
        // Pasando por él, sí: de accept edits a auto se pasa por plan, y llega a auto.
        let c = claude(|c| {
            c.mode = 2;
            c.working = true;
        });
        let mut scr = fake::screen_of(&c);
        assert_eq!(set_mode(&mut scr, Mode::Auto).await, Ok(Mode::Auto));
        assert_eq!(keys(&c).len(), 2);
        assert_eq!(mode(&c.lock().unwrap().screen()), Some(Mode::Auto));
        // Sin trabajar, sí.
        let c = claude(|c| c.mode = 3);
        let mut scr = fake::screen_of(&c);
        assert_eq!(set_mode(&mut scr, Mode::Auto).await, Ok(Mode::Auto));
    }

    /// (5) La confirmación partida en dos líneas (pantalla estrecha) también vale.
    #[tokio::test(start_paused = true)]
    async fn a_narrow_confirmation() {
        let c = claude(|c| c.narrow = true);
        let mut scr = fake::screen_of(&c);
        assert_eq!(
            set_model(&mut scr, "Opus", &mut None).await,
            Ok("Opus 5.5".to_owned())
        );
    }

    /// (6) El menú de `/model` se reconoce también por su ayuda, sin el título.
    #[test]
    fn the_model_menu_without_its_title() {
        let s = MODEL_MENU.replace("   Select model\n", "");
        let m = model_menu(&s).unwrap();
        assert_eq!(m.options.len(), 6);
        assert!(m.session_only);
        assert!(model_dialog(&s));
    }

    /// (6) Si `/model` abre otro menú, se cierra con Esc.
    #[tokio::test(start_paused = true)]
    async fn another_menu_after_model_is_escaped() {
        let c = claude(|c| c.opens_other = true);
        let mut scr = fake::screen_of(&c);
        assert!(set_model(&mut scr, "Opus", &mut None).await.is_err());
        let c = c.lock().unwrap();
        assert!(!c.other, "{:?}", c.keys);
        assert_eq!(c.keys.last().unwrap(), b"\x1b");
        assert_eq!(c.keys.iter().filter(|k| k.contains(&b'\r')).count(), 1);
    }

    /// (7) Antes de Enter, la caja dice exactamente `/model` (lo atenuado no cuenta): con
    /// `/model sonnet`, Enter lo guardaría por defecto.
    #[tokio::test(start_paused = true)]
    async fn only_exactly_model_is_entered() {
        let c = claude(|c| c.typed_suffix = Some(" sonnet"));
        let mut scr = fake::screen_of(&c);
        assert!(set_model(&mut scr, "Opus", &mut None).await.is_err());
        assert!(
            !keys(&c).iter().any(|k| k.contains(&b'\r')),
            "{:?}",
            keys(&c)
        );
        // Una ayuda atenuada detrás: sí.
        let c = claude(|c| c.dim_suffix = Some(" [model]"));
        let mut scr = fake::screen_of(&c);
        assert_eq!(
            set_model(&mut scr, "Opus", &mut None).await,
            Ok("Opus 5.5".to_owned())
        );
    }

    /// (9) Esc espera a que cambie el diálogo, no cualquier cosa de la pantalla: nunca un Esc de
    /// más con la caja vacía.
    #[tokio::test(start_paused = true)]
    async fn escape_waits_for_the_dialog_to_change() {
        for switch in [false, true] {
            let c = claude(|c| {
                c.ticker = true;
                c.esc_lag = 3;
                c.asks_switch = switch;
                c.switch_stuck = true;
            });
            let mut scr = fake::screen_of(&c);
            let want = if switch { "Sonnet" } else { "Gemini" };
            assert!(set_model(&mut scr, want, &mut None).await.is_err());
            let c = c.lock().unwrap();
            assert!(!c.esc_in_box, "{:?}", c.keys);
            assert_eq!((c.menu, c.switch), (None, None), "{:?}", c.keys);
            let escs = c.keys.iter().filter(|k| *k == b"\x1b").count();
            assert_eq!(escs, if switch { 2 } else { 1 }, "{:?}", c.keys);
        }
    }

    /// (10) Un menú pintado a medias no vale: ni sus opciones ni su ayuda.
    #[tokio::test(start_paused = true)]
    async fn a_half_painted_menu_is_waited_for() {
        let c = claude(|c| c.slow_paint = true);
        let mut scr = fake::screen_of(&c);
        let mut seen = None;
        assert_eq!(
            set_model(&mut scr, "Haiku", &mut seen).await,
            Ok("Haiku 5.5".to_owned())
        );
        assert_eq!(seen.unwrap().len(), 5);
        never_enter_in_the_menu(&c);
    }

    /// (14) Un nombre corto elige la versión más nueva de esa familia.
    #[test]
    fn a_short_name_picks_the_newest_version() {
        let m = model_menu(MODEL_MENU).unwrap();
        assert_eq!(m.find("Fable"), Some(3));
        assert_eq!(m.find("fable 5"), Some(3));
        assert_eq!(m.find("Fable 5.1"), Some(2));
        assert_eq!(m.find("Opus"), Some(1));
        // Las versiones, como números: 5.10 es más nueva que 5.9.
        let s = MODEL_MENU
            .replace("Fable 5.1 ·", "Fable 5.10 ·")
            .replace("Fable 5.2 ·", "Fable 5.9 ·");
        assert_eq!(model_menu(&s).unwrap().find("Fable"), Some(2));
    }

    #[tokio::test(start_paused = true)]
    async fn model_when_the_menu_does_not_open() {
        // Claude toma `/model` por un mensaje: un solo Enter y nada más.
        let c = claude(|c| c.opens_menu = false);
        let mut scr = fake::screen_of(&c);
        assert!(set_model(&mut scr, "Opus", &mut None).await.is_err());
        assert_eq!(keys(&c), [b"/model".to_vec(), b"\r".to_vec()]);
        never_enter_in_the_menu(&c);
    }

    // ---------- Mensajes y respuestas en nombre del usuario (el agente de voz) ----------

    /// Lo pegado llega sin caracteres de control: ni el final del pegado (`ESC[201~`), que
    /// convertiría lo de detrás en teclas (shift+tab, Esc, ctrl+c), ni otros. Los saltos de línea
    /// y los tabuladores siguen.
    #[tokio::test(start_paused = true)]
    async fn a_message_arrives_without_control_characters() {
        let c = claude(|_| {});
        let mut scr = fake::screen_of(&c);
        let text = "hola\x1b[201~\x1b[Z\x03 mundo\r\nadiós\tya\rfin\u{7f}\u{9b}\u{0}!";
        assert_eq!(send_message(&mut scr, text).await, Ok(()));
        let clean = "hola[201~[Z mundo\nadiós\tya\nfin!";
        let mut want = b"\x1b[200~".to_vec();
        want.extend_from_slice(clean.as_bytes());
        want.extend_from_slice(b"\x1b[201~");
        let k = keys(&c);
        assert_eq!(k.first(), Some(&want), "{k:?}");
        let c = c.lock().unwrap();
        assert!(c.after_paste.is_empty(), "{:?}", c.after_paste);
        assert_eq!(c.mode, 0, "shift+tab: {k:?}");
        assert_eq!(
            c.log.last().map(String::as_str),
            Some(&*format!("❯ {clean}"))
        );
    }

    /// Lo que se escribe tras «No» en un permiso va igual: sin caracteres de control.
    #[tokio::test(start_paused = true)]
    async fn the_text_after_no_arrives_without_control_characters() {
        let c = claude(|c| c.permission = true);
        let mut scr = fake::screen_of(&c);
        let req = Request {
            option: Some("No".into()),
            text: Some("mejor así\x1b[201~\x1b[Z\x03".into()),
            ..Default::default()
        };
        assert_eq!(answer(&mut scr, &req).await, Ok(()));
        let k = keys(&c);
        assert_eq!(k[0], b"2");
        assert_eq!(
            k[1],
            "\x1b[200~mejor así[201~[Z\x1b[201~".as_bytes(),
            "{k:?}"
        );
        assert!(c.lock().unwrap().after_paste.is_empty());
    }

    /// Solo caracteres de control: no es un mensaje (ni un Enter suelto).
    #[tokio::test(start_paused = true)]
    async fn a_message_of_only_control_characters_is_refused() {
        let c = claude(|_| {});
        let mut scr = fake::screen_of(&c);
        assert!(send_message(&mut scr, "\x1b\x03\r\n").await.is_err());
        assert!(keys(&c).is_empty(), "{:?}", keys(&c));
    }
}

/// Un Claude de mentira detrás de un guardián de mentira, para probar las secuencias que
/// teclean (`set_mode`, `set_model`): pinta su pantalla como la de Claude Code y atiende las
/// teclas, apuntando todas.
#[cfg(test)]
pub(crate) mod fake {
    use super::*;
    use std::sync::{Arc, Mutex};

    const RULE: &str = "────────────────────────────────────────────────────────────";

    #[derive(Debug, Clone)]
    pub struct Claude {
        /// Los modos por los que pasa shift+tab, en orden, y en cuál está.
        pub cycle: Vec<Mode>,
        pub mode: usize,
        /// Un pie que no se entiende.
        pub odd_footer: bool,
        pub working: bool,
        /// Lo escrito en la caja.
        pub input: String,
        /// El menú de `/model`, abierto: dónde está el cursor.
        pub menu: Option<usize>,
        /// (etiqueta, modelo) de cada opción del menú de `/model`.
        pub options: Vec<(&'static str, &'static str)>,
        /// La opción que se usa ahora (`✔`).
        pub current: usize,
        /// La ayuda del menú dice «s to use this session only».
        pub hint_s: bool,
        /// `/model` + Enter abre el menú (si no, Claude lo toma por un mensaje).
        pub opens_menu: bool,
        /// Un permiso en pantalla.
        pub permission: bool,
        /// Lo que se ve encima de la caja.
        pub log: Vec<String>,
        /// Lo que ha llegado, trama a trama.
        pub keys: Vec<Vec<u8>>,
        /// Enter con el menú de `/model` abierto (lo guardaría para todas las sesiones).
        pub saved_default: bool,
        /// ←/→ con el menú abierto (el esfuerzo).
        pub effort_changed: bool,
        /// shift+tab con un menú en pantalla.
        pub shift_tab_in_menu: bool,
        /// Las opciones con el modelo por etiqueta («2.  Opus 5.5   Lo que hace»), como lo pinta
        /// a veces Claude Code, con «▸ More models» debajo.
        pub models_as_labels: bool,
        /// Tras `s`, pregunta si cambiar («Switch model?»): dónde está, si está abierta.
        pub asks_switch: bool,
        pub switch: Option<usize>,
        /// En esa pregunta, «1» no hace nada.
        pub switch_stuck: bool,
        /// Un número con el menú de `/model` abierto (lo elegiría como el de por defecto).
        pub digit_in_menu: bool,
        /// Encima de la caja, una línea que cambia en cada lectura (como el reloj de Claude).
        pub ticker: bool,
        pub reads: u32,
        /// Esc tarda en hacer efecto estas lecturas (Claude repinta más tarde).
        pub esc_lag: u32,
        pending_esc: Vec<u32>,
        /// Esc sin ningún diálogo abierto (con la caja vacía, dos seguidos abren otra cosa).
        pub esc_in_box: bool,
        /// El menú de `/model` se pinta poco a poco: una línea más en cada lectura.
        pub slow_paint: bool,
        painted: Option<usize>,
        /// Tras escribir `/model`, lo que añade Claude: atenuado (una ayuda) o como si se hubiera
        /// escrito.
        pub dim_suffix: Option<&'static str>,
        pub typed_suffix: Option<&'static str>,
        /// `/model` + Enter abre otro menú (no el de `/model`), que Esc cierra.
        pub opens_other: bool,
        pub other: bool,
        /// Tras esa tecla, la lectura siguiente no llega a tiempo (llega tarde, con la pantalla
        /// de entonces).
        pub stall_after: Option<&'static [u8]>,
        stall_next: bool,
        /// Al escribir `/model`, Claude no lo repite encima de la caja.
        pub no_echo: bool,
        /// `s` cierra el menú pero Claude no dice nada.
        pub s_silent: bool,
        /// La confirmación sale partida en dos líneas (pantalla estrecha).
        pub narrow: bool,
        /// El menú de `/model` se pinta sin su título («Select model»).
        pub no_title: bool,
        /// Los mensajes en cola (encima de la caja, con «ctrl+x ctrl+s to send now»).
        pub queue: Vec<String>,
        /// Ha llegado ctrl+x: ctrl+s manda la cola.
        chord: bool,
        /// ctrl+x ctrl+s no hace nada.
        pub send_now_ignored: bool,
        /// El retroceso no borra nada de la caja.
        pub deaf_backspace: bool,
        /// Lo que se ha mandado a Claude con ctrl+x ctrl+s.
        pub sent_now: Vec<String>,
        /// Una pantalla fija (una de `tests/pantallas`), en vez de la que se pinta.
        pub raw: Option<String>,
        /// Lo que llegó detrás del final de un pegado, en la misma trama: teclas.
        pub after_paste: Vec<Vec<u8>>,
        /// La opción que se eligió en el permiso (con su número).
        pub picked: Option<char>,
    }

    impl Default for Claude {
        fn default() -> Self {
            Self {
                cycle: vec![Mode::Auto, Mode::Manual, Mode::AcceptEdits, Mode::Plan],
                mode: 0,
                odd_footer: false,
                working: false,
                input: String::new(),
                menu: None,
                options: vec![
                    ("Default (recommended)", "Opus 5.5"),
                    ("Opus", "Opus 5.5"),
                    ("Fable", "Fable 5.1"),
                    ("Fable", "Fable 5.2"),
                    ("Sonnet", "Sonnet 5.5"),
                    ("Haiku", "Haiku 5.5"),
                ],
                current: 3,
                hint_s: true,
                opens_menu: true,
                permission: false,
                log: vec!["● Hola.".into()],
                keys: Vec::new(),
                saved_default: false,
                effort_changed: false,
                shift_tab_in_menu: false,
                models_as_labels: false,
                asks_switch: false,
                switch: None,
                switch_stuck: false,
                digit_in_menu: false,
                ticker: false,
                reads: 0,
                esc_lag: 0,
                pending_esc: Vec::new(),
                esc_in_box: false,
                slow_paint: false,
                painted: None,
                dim_suffix: None,
                typed_suffix: None,
                opens_other: false,
                other: false,
                stall_after: None,
                stall_next: false,
                no_echo: false,
                s_silent: false,
                narrow: false,
                no_title: false,
                queue: Vec::new(),
                chord: false,
                send_now_ignored: false,
                deaf_backspace: false,
                sent_now: Vec::new(),
                raw: None,
                after_paste: Vec::new(),
                picked: None,
            }
        }
    }

    impl Claude {
        /// Lo que ve nido al pedir la pantalla: pasa una lectura (los Esc con retraso, el
        /// reloj, el menú que se pinta poco a poco).
        pub fn read(&mut self) -> String {
            self.reads += 1;
            let mut fire = 0;
            self.pending_esc.retain_mut(|n| {
                *n = n.saturating_sub(1);
                if *n == 0 {
                    fire += 1;
                    false
                } else {
                    true
                }
            });
            for _ in 0..fire {
                self.esc();
            }
            let text = self.screen();
            if let Some(p) = self.painted.as_mut() {
                *p += 1;
            }
            text
        }

        fn esc(&mut self) {
            if let Some(at) = self.switch.take() {
                self.menu = Some(at);
            } else if self.menu.take().is_some() {
                self.painted = None;
                self.log.push("  ⎿  Kept model as it was".into());
            } else if self.other {
                self.other = false;
            } else if !self.permission {
                self.esc_in_box = true;
            }
        }

        fn said_set(&mut self, at: usize) {
            let model = self.options[at].1;
            if self.narrow {
                self.log.push(format!("  ⎿  Set model to {model} for this"));
                self.log.push("     session only".into());
            } else {
                self.log
                    .push(format!("  ⎿  Set model to {model} for this session only"));
            }
        }

        pub fn screen(&self) -> String {
            if let Some(raw) = &self.raw {
                return raw.clone();
            }
            let mut out: Vec<String> = self.log.clone();
            if self.ticker {
                out.push(format!("  ✻ {}s", self.reads));
            }
            if !self.queue.is_empty() {
                out.push(String::new());
                for m in &self.queue {
                    out.push(format!("❯ {}", m.replace('\n', "\n  ")));
                }
                out.push(format!("  {SEND_NOW}"));
            }
            out.push(String::new());
            if self.other {
                out.push(RULE.into());
                out.push("   Pick something".into());
                out.push("   ❯ 1. One".into());
                out.push("     2. Two".into());
                out.push(String::new());
                out.push("   Esc to cancel".into());
            } else if self.permission {
                out.push(RULE.into());
                out.push(" Bash command".into());
                out.push(String::new());
                out.push(" Do you want to proceed?".into());
                out.push(" ❯ 1. Yes".into());
                out.push("   2. No".into());
                out.push(String::new());
                out.push(" Esc to cancel · Tab to amend".into());
            } else if let Some(at) = self.switch {
                let model = self.options[at].1;
                out.push(RULE.into());
                out.push("   Switch model?".into());
                out.push("   Your next response will be slower and use more tokens".into());
                out.push(String::new());
                out.push(format!("   This conversation is cached for the current model. Switching to {model} means the full history gets re-read."));
                out.push(String::new());
                out.push(format!("   ❯ 1. Yes, switch to {model}"));
                out.push("     2. No, go back".into());
            } else if let Some(at) = self.menu {
                out.push(RULE.into());
                let top = out.len();
                if !self.no_title {
                    out.push("   Select model".into());
                }
                out.push("   Switch between Claude models. Your pick becomes the default for new sessions.".into());
                for (i, (label, model)) in self.options.iter().enumerate() {
                    let cursor = if i == at { "❯" } else { " " };
                    let mark = if i == self.current { " ✔" } else { "" };
                    if self.models_as_labels && i > 0 {
                        let name = format!("{}.  {model}{mark}", i + 1);
                        out.push(format!("   {cursor} {name:<26} Lo que hace"));
                    } else {
                        let name = format!("{}. {label}{mark}", i + 1);
                        out.push(format!("   {cursor} {name:<26} {model} · Lo que hace"));
                    }
                }
                if self.models_as_labels {
                    out.push("     ▸   More models".into());
                }
                out.push("   ◐ Medium effort (default) ←/→ to adjust".into());
                out.push(if self.hint_s {
                    "   Enter to set as default · s to use this session only · Esc to cancel".into()
                } else {
                    "   Enter to confirm · Esc to cancel".into()
                });
                if let Some(p) = self.painted {
                    out.truncate((top + p).min(out.len()));
                }
            } else {
                out.push(RULE.into());
                match self.dim_suffix.filter(|_| self.input == "/model") {
                    Some(d) => out.push(format!("❯ {}\x1b[2m{d}\x1b[22m", self.input)),
                    None if self.input.is_empty() && !self.queue.is_empty() => {
                        out.push("❯\u{a0}\x1b[2mPress up to edit queued messages\x1b[22m".into())
                    }
                    None => out.push(format!("❯ {}", self.input.replace('\n', "\n  "))),
                }
                out.push(RULE.into());
                let foot = if self.odd_footer {
                    "⏵⏵ algo nuevo".to_owned()
                } else {
                    match self.cycle[self.mode] {
                        Mode::Auto => "⏵⏵ auto mode on (shift+tab to cycle)",
                        Mode::Manual => "⏸ manual mode on",
                        Mode::AcceptEdits => "⏵⏵ accept edits on (shift+tab to cycle)",
                        Mode::Plan => "⏸ plan mode on (shift+tab to cycle)",
                        Mode::BypassPermissions => "⏵⏵ bypass permissions on",
                    }
                    .to_owned()
                };
                let tail = if self.working {
                    " · esc to interrupt"
                } else {
                    " · ? for shortcuts"
                };
                out.push(format!("  {foot}{tail}"));
            }
            out.join("\n")
        }

        fn key(&mut self, k: &[u8]) {
            self.keys.push(k.to_vec());
            if self.stall_after == Some(k) {
                self.stall_next = true;
            }
            // Un pegado (bracketed paste): hasta su final, texto tal cual; lo que venga detrás en
            // la misma trama ya serían teclas.
            if let Some(body) = k.strip_prefix(b"\x1b[200~".as_slice()) {
                let end = body.windows(6).position(|w| w == b"\x1b[201~");
                let (text, rest) = match end {
                    Some(e) => (&body[..e], &body[e + 6..]),
                    None => (body, &[][..]),
                };
                if self.menu.is_none() && !self.permission && self.raw.is_none() {
                    self.input.push_str(&String::from_utf8_lossy(text));
                }
                if !rest.is_empty() {
                    self.after_paste.push(rest.to_vec());
                }
                return;
            }
            let chord = std::mem::take(&mut self.chord);
            match k {
                b"\x18" => self.chord = true,
                b"\x13" => {
                    if chord && !self.send_now_ignored {
                        for m in std::mem::take(&mut self.queue) {
                            self.log.push(format!("❯ {m}"));
                            self.sent_now.push(m);
                        }
                    }
                }
                b"\x1b[A"
                    if self.menu.is_none()
                        && !self.permission
                        && self.input.is_empty()
                        && !self.queue.is_empty() =>
                {
                    self.input = std::mem::take(&mut self.queue).join("\n");
                }
                b"\x1b[Z" => {
                    if self.permission || self.menu.is_some() {
                        self.shift_tab_in_menu = true;
                    } else {
                        self.mode = (self.mode + 1) % self.cycle.len();
                    }
                }
                b"\x1b[A" | b"\x1b[B" => {
                    if let Some(at) = self.menu.as_mut() {
                        if k == b"\x1b[A" {
                            *at = at.saturating_sub(1);
                        } else {
                            *at = (*at + 1).min(self.options.len() - 1);
                        }
                    }
                }
                b"\x1b[C" | b"\x1b[D" => self.effort_changed |= self.menu.is_some(),
                b"\x1b" => {
                    if self.esc_lag > 0 {
                        self.pending_esc.push(self.esc_lag);
                    } else {
                        self.esc();
                    }
                }
                b"\r" => {
                    if self.menu.is_some() {
                        self.saved_default = true;
                        self.menu = None;
                    } else if self.input == "/model" && self.opens_other {
                        self.input.clear();
                        self.other = true;
                    } else if self.input == "/model" && self.opens_menu {
                        if !self.no_echo {
                            self.log.push("❯ /model".into());
                        }
                        self.input.clear();
                        self.menu = Some(self.current);
                        if self.slow_paint {
                            self.painted = Some(1);
                        }
                    } else if !self.input.is_empty() {
                        self.log.push(format!("❯ {}", self.input));
                        self.input.clear();
                    }
                }
                _ => {
                    for c in String::from_utf8_lossy(k).chars() {
                        if let Some(at) = self.switch {
                            if c == '1' {
                                if !self.switch_stuck {
                                    self.switch = None;
                                    self.current = at;
                                    self.said_set(at);
                                }
                            } else if c.is_ascii_digit() {
                                self.digit_in_menu = true;
                            }
                            continue;
                        }
                        if self.other {
                            continue;
                        }
                        match (c, self.menu) {
                            ('s', Some(at)) if self.hint_s && self.asks_switch => {
                                self.menu = None;
                                self.switch = Some(at);
                            }
                            ('s', Some(at)) if self.hint_s => {
                                self.current = at;
                                self.menu = None;
                                if !self.s_silent {
                                    self.said_set(at);
                                }
                            }
                            (c, Some(_)) if c.is_ascii_digit() => self.digit_in_menu = true,
                            (_, Some(_)) => {}
                            (c, None) if self.permission => {
                                if c.is_ascii_digit() {
                                    self.permission = false;
                                    self.picked = Some(c);
                                }
                            }
                            ('\x7f', None) => {
                                if !self.deaf_backspace {
                                    self.input.pop();
                                }
                            }
                            (c, None) => self.input.push(c),
                        }
                    }
                    if self.input == "/model"
                        && let Some(t) = self.typed_suffix
                    {
                        self.input.push_str(t);
                    }
                }
            }
        }
    }

    /// Atiende a un cliente del guardián: la pantalla cuando la pide; las teclas, al Claude.
    pub async fn serve<S: AsyncRead + AsyncWrite + Unpin>(mut s: S, claude: Arc<Mutex<Claude>>) {
        while let Ok(Some(f)) = proto::read_frame(&mut s).await {
            match f {
                Frame::HistoryRequest(_) => {
                    let (text, stall) = {
                        let mut c = claude.lock().unwrap();
                        let stall = std::mem::take(&mut c.stall_next);
                        (c.read(), stall)
                    };
                    if stall {
                        tokio::time::sleep(WAIT + Duration::from_secs(1)).await;
                    }
                    if proto::write_frame(&mut s, &Frame::History(text.into_bytes()))
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
                Frame::Input(k) => claude.lock().unwrap().key(&k),
                _ => {}
            }
        }
    }

    /// La pantalla de ese Claude, como la ve nido.
    pub fn screen_of(
        claude: &Arc<Mutex<Claude>>,
    ) -> Screen<
        tokio::io::ReadHalf<tokio::io::DuplexStream>,
        tokio::io::WriteHalf<tokio::io::DuplexStream>,
    > {
        let (a, b) = tokio::io::duplex(1 << 16);
        tokio::spawn(serve(b, claude.clone()));
        let (rd, wr) = tokio::io::split(a);
        Screen::new(rd, wr)
    }
}
