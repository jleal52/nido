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
    /// Por dónde pasa el cursor, en orden: las opciones y la fila «Next»/«Submit».
    #[serde(skip)]
    rows: Vec<Row>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Row {
    Opt(usize),
    /// «Next» o «Submit» de las de varias respuestas.
    Nav { selected: bool },
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
                    let full = norm(&format!("{} {}", o.label, o.detail.as_deref().unwrap_or("")));
                    full.starts_with(&w)
                        && full[w.len()..].chars().next().is_none_or(|c| !c.is_alphanumeric())
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
}

/// Minúsculas y los espacios juntos, para comparar lo de la pantalla (que puede venir partido en
/// varias líneas) con lo que se pide.
fn norm(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
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
        let Some(seq) = rest.strip_prefix("\x1b[") else { break };
        let Some(end) = seq.find(|c: char| c.is_ascii_alphabetic()) else { break };
        if &seq[end..end + 1] == "m" {
            let ps: Vec<&str> = seq[..end].split(';').collect();
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

/// Columna (en caracteres) del primer carácter que no es espacio.
fn indent(l: &str) -> usize {
    l.chars().take_while(|c| *c == ' ').count()
}

fn is_rule(l: &str) -> bool {
    let t = l.trim();
    t.chars().count() >= 6 && t.chars().all(|c| matches!(c, '─' | '╌' | '▔' | '━' | '═'))
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
    Some((selected, digits.parse().ok()?, col, text.trim_end().to_owned()))
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
    ["Esc to cancel", "Enter to select", "Enter to confirm", "to navigate", "Tab to amend", "ctrl+g to edit"]
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
    if (last + 1..lines.len()).any(|i| lines[i].trim_start().starts_with('❯') && edge(lines[i - 1])) {
        return None;
    }
    found.reverse();

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
        title.push(lines[i - 1].trim());
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
    let chat = options.iter().position(|o| o.label.starts_with("Chat about this"));
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
        ["limit to reset", "continue automatically", "limit reset", "Upgrade your plan"]
            .iter()
            .any(|p| o.label.contains(p))
    }) {
        Kind::Limit
    } else {
        Kind::Menu
    };
    let menu = Menu {
        kind,
        title,
        tabs,
        multi: options.iter().any(|o| o.checked.is_some()),
        options,
        hint,
        rows,
    };
    // Un menú de verdad tiene el cursor en alguna fila (una lista numerada de Claude, no).
    menu.selected_row().is_some().then_some(menu)
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
                    tabs.push(Tab { label: label.trim().to_owned(), done });
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
        tabs.push(Tab { label: label.trim().to_owned(), done });
    }
    tabs.retain(|t| !t.label.is_empty());
    tabs
}

/// Menús sin números (confiar en la carpeta): la fila del cursor y las de su misma columna,
/// con «Enter to confirm» debajo.
fn unnumbered(lines: &[&str]) -> Option<Menu> {
    let hint_at = lines.iter().rposition(|l| l.contains("Enter to confirm") || l.contains("Enter to select"))?;
    let sel = (hint_at.saturating_sub(12)..hint_at).rev().find(|&i| lines[i].trim_start().starts_with('❯'))?;
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
    })
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
    let head = lines.iter().rposition(|l| l.trim_start().starts_with("MCP server") && l.contains("requests your input"))?;
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
        if rest.starts_with("Accept") && rest.contains("Decline") || rest.starts_with("Accept") && l.contains("❯ Decline") {
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
        Form { server, message: message.join("\n"), fields },
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

const YES: [&str; 8] = ["yes", "sí", "si", "true", "confirm", "confirmar", "accept", "ok"];

/// Contesta el formulario. Al aceptar, los obligatorios sin valor que son una confirmación se
/// rellenan solos (la casilla marcada, o la opción «yes» de la lista); los de texto, con lo que
/// venga en `values` (si falta, no se manda nada).
pub async fn answer_form<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    scr: &mut Screen<R, W>,
    ans: &FormAnswer,
) -> Result<(), String> {
    let (form0, _, _) = form_state(&scr.read().await?).ok_or("no hay ningún formulario en pantalla")?;
    if ans.accept {
        for (i, f) in form0.fields.iter().enumerate() {
            let wanted = ans.values.get(&f.label).map(|v| v.trim()).filter(|v| !v.is_empty());
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
                    form_until(scr, |f, _| f.fields.get(i).is_some_and(|x| checked(&x.value) == want)).await?;
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
                    None => opts.iter().position(|o| YES.contains(&o.to_lowercase().as_str())),
                }
                .ok_or_else(|| format!("«{}»: no sé qué opción elegir ({})", f.label, opts.join(", ")))?;
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
        let (form, at, hint) = form_state(&scr.read().await?).ok_or("el formulario ha desaparecido")?;
        let row = |c: FormCursor| match c {
            FormCursor::Field(i) => i,
            FormCursor::Buttons(_) => form.fields.len(),
        };
        if at == to {
            return Ok(hint);
        }
        let key = match (at, to) {
            (FormCursor::Buttons(_), FormCursor::Buttons(accept)) => {
                if accept { b"\x1b[D" as &[u8] } else { b"\x1b[C" }
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
#[derive(Debug, Clone, Deserialize)]
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

    async fn read(&mut self) -> Result<String, String> {
        let req = Frame::HistoryRequest(HistoryRequest { all: false, screen: true });
        proto::write_frame(&mut self.wr, &req).await.map_err(|_| "la sesión se ha cerrado")?;
        loop {
            let frame = tokio::time::timeout(WAIT, proto::read_frame(&mut self.rd))
                .await
                .map_err(|_| "la sesión no contesta")?;
            match frame {
                Ok(Some(Frame::History(b))) => return Ok(String::from_utf8_lossy(&b).into_owned()),
                Ok(Some(Frame::Exit(_)) | None) | Err(_) => return Err("la sesión se ha cerrado".into()),
                Ok(Some(_)) => {}
            }
        }
    }

    async fn keys(&mut self, bytes: &[u8]) -> Result<(), String> {
        proto::write_frame(&mut self.wr, &Frame::Input(bytes.to_vec()))
            .await
            .map_err(|_| "la sesión se ha cerrado".into())
    }

    async fn menu(&mut self) -> Result<Option<Menu>, String> {
        Ok(parse(&self.read().await?))
    }

    /// Hasta que la pantalla cumpla `done` (o `WAIT`): lo que hay entonces.
    async fn until(&mut self, done: impl Fn(Option<&Menu>) -> bool) -> Result<(bool, Option<Menu>), String> {
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
            let (_, now) = self.until(|x| x.and_then(Menu::selected_row) == Some(row)).await?;
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
    let i = (1..plain.len().saturating_sub(1)).rev().find(|&i| plain[i].trim_start().starts_with('❯'))?;
    // Las rayas de la caja (la de arriba puede llevar dentro el nombre de la sesión).
    let edge = |l: &str| l.trim_start().starts_with("──────");
    if !edge(&plain[i - 1]) || !edge(&plain[i + 1]) {
        return None;
    }
    // Atenuado justo detrás de «❯ »: lo propuesto, no lo que has escrito tú.
    let raw = lines[i];
    let after = &raw[raw.find('❯')? + '❯'.len_utf8()..];
    let after = after.trim_start_matches(|c: char| c == ' ' || c == '\u{a0}');
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
        let l = if k == 0 { l.trim_start().trim_start_matches('❯') } else { l.as_str() };
        text.push(l.trim_matches([' ', '\u{a0}']).to_owned());
    }
    Some(text.join(" ").trim().to_owned())
}

/// Manda un mensaje a Claude: lo pega, espera a que la caja se quede quieta (con adjuntos,
/// Claude tarda en cambiar las rutas por «[Image #1]»; un Enter que llega antes se pierde) y
/// pulsa Enter; si la caja no se vacía, lo vuelve a pulsar.
pub async fn send_message<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    scr: &mut Screen<R, W>,
    text: &str,
) -> Result<(), String> {
    let text = text.trim();
    if !text.is_empty() {
        let mut paste = b"\x1b[200~".to_vec();
        paste.extend_from_slice(text.replace("\r\n", "\n").as_bytes());
        paste.extend_from_slice(b"\x1b[201~");
        // Si Claude no lo coge (p. ej. aún arrancando), la caja sigue vacía: se pega otra vez.
        let mut written = false;
        for _ in 0..2 {
            if input_box(&scr.read().await?).is_some_and(|t| !t.is_empty()) {
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

/// Tras cambiar de cuenta: si la sesión se quedó parada por el límite (`stopped`, según su
/// transcripción, o por lo que hay en pantalla), quita el aviso (su menú o la espera) y le dice a
/// Claude que siga. Devuelve si lo ha hecho.
pub async fn resume_after_limit<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    scr: &mut Screen<R, W>,
    stopped: bool,
) -> Result<bool, String> {
    let text = scr.read().await?;
    let menu = parse(&text);
    let on_screen = menu.as_ref().is_some_and(|m| m.kind == Kind::Limit) || waiting_for_limit(&text);
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

/// Contesta el menú que hay en pantalla.
pub async fn answer<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    scr: &mut Screen<R, W>,
    req: &Request,
) -> Result<(), String> {
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
        scr.until(|x| x.and_then(Menu::selected_row) == Some(row)).await?;
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
    if let Some(text) = req.text.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
        let (gone, _) = scr.until(|x| x.is_none()).await?;
        if !gone {
            return Err("eligió la opción, pero no se pudo mandar el texto".into());
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
        let mut paste = b"\x1b[200~".to_vec();
        paste.extend_from_slice(text.as_bytes());
        paste.extend_from_slice(b"\x1b[201~");
        scr.keys(&paste).await?;
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
        let (ok, m) = scr.until(|m| m.is_some_and(|m| m.shows_question(&qa.question))).await?;
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
                .ok_or_else(|| format!("la pregunta «{}» no tiene la opción «{want}»", qa.question))?;
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
                    scr.until(|x| x.is_some_and(|x| x.options.get(i).and_then(|o| o.checked) == Some(want))).await?;
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
            scr.until(|x| x.and_then(Menu::selected_row) == Some(row)).await?;
            scr.type_text(text).await?;
            scr.keys(b"\r").await?;
        } else {
            scr.pick(&m, chosen[0]).await?;
        }
        let (moved, _) = scr.until(|x| !x.is_some_and(|x| x.shows_question(&qa.question))).await?;
        if !moved {
            return Err(format!("Claude no ha pasado de la pregunta «{}»", qa.question));
        }
    }
    // Contestada la última de varias, sale el repaso: mandar.
    if let Some(m) = scr.menu().await?.filter(|m| m.kind == Kind::Review) {
        let i = m.find("Submit answers").ok_or("no se encuentra «Submit answers»")?;
        scr.pick(&m, i).await?;
        let (sent, _) = scr.until(|x| x.is_none_or(|x| x.kind != Kind::Review)).await?;
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
        assert_eq!(labels(&m), ["Rojo", "Verde", "Azul", "Type something.", "Chat about this"]);
        assert_eq!(m.options[0].detail.as_deref(), Some("El color rojo"));
        assert!(m.options[0].selected && m.options[3].input && !m.options[4].input);
        assert_eq!(m.tabs.iter().map(|t| (t.label.as_str(), t.done)).collect::<Vec<_>>(), [("Color", false), ("Frutas", false)]);
        assert!(!m.multi);
        assert_eq!(m.find("verde"), Some(1));
    }

    #[test]
    fn single_question() {
        let m = screen("pregunta_sola").unwrap();
        assert_eq!(m.kind, Kind::Question);
        assert_eq!(m.tabs.len(), 1);
        assert_eq!(m.input(), Some(2));
        assert_eq!(m.hint.as_deref(), Some("Enter to select · ↑/↓ to navigate · Esc to cancel"));
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
        assert_eq!(labels(&m), ["Manzana", "Pera", "Type something", "Chat about this"]);
        assert_eq!(m.options.iter().map(|o| o.checked).collect::<Vec<_>>(), [Some(false), Some(true), Some(false), None]);
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
        assert_eq!(m.options[1].detail.as_deref(), Some("/home/ana/proyecto from this project"));
        assert_eq!(m.hint.as_deref(), Some("Esc to cancel · Tab to amend"));
        assert_eq!(m.find("Yes"), Some(0));
        assert_eq!(m.find("No"), Some(3));
        assert_eq!(m.find("Yes, and always allow access to /home/ana/proyecto"), Some(1));
        assert_eq!(m.find("4"), None);
    }

    #[test]
    fn plan() {
        let m = screen("plan").unwrap();
        assert_eq!(m.kind, Kind::Plan);
        assert_eq!(labels(&m), ["Yes, and use auto mode", "Yes, manually approve edits", "Tell Claude what to change"]);
        assert!(m.options[2].input);
        assert_eq!(m.options[2].detail.as_deref(), Some("shift+tab to approve with this feedback"));
    }

    #[test]
    fn trust_folder_has_no_numbers() {
        let m = screen("confianza").unwrap();
        assert_eq!(m.kind, Kind::Menu);
        assert_eq!(labels(&m), ["No, exit", "Yes, I trust this folder"]);
        assert_eq!(m.options[0].n, None);
        assert!(m.options[0].selected);
        assert_eq!(m.title.as_deref(), Some("Security guide"));
    }

    #[test]
    fn usage_limit_menu() {
        let s = "● You've hit your session limit · resets 5pm (Europe/Madrid)\n\n What do you want to do?\n\n ❯ 1. Stop and wait for limit to reset\n   2. Wait here, then continue automatically when the limit resets\n   3. Upgrade your plan\n\n Enter to confirm · Esc to cancel";
        let m = parse(s).unwrap();
        assert_eq!(m.kind, Kind::Limit);
        assert_eq!(m.title.as_deref(), Some("What do you want to do?"));
        assert!(waiting_for_limit("Claude Code will continue automatically at 5pm. Keep this session open; it may still pause for permission prompts. Press esc to cancel the wait."));
    }

    #[test]
    fn suggested_next_prompt() {
        let rule = "\x1b[38;2;136;136;136m────────────────────────\x1b[m";
        let box_ = |line: &str| format!("✻ Brewed for 11s\n\n{rule}──── nido ─\n{line}\n{rule}\n  ⏵⏵ auto mode on");
        assert_eq!(
            suggestion(&box_("\x1b[m❯\u{a0} \x1b[2mok a shara-media, y borra tú feature/…")).as_deref(),
            Some("ok a shara-media, y borra tú feature/…")
        );
        // Lo que has escrito tú (sin atenuar), la caja vacía o el ejemplo del principio: no.
        assert_eq!(suggestion(&box_("\x1b[m❯\u{a0} esto lo escribo yo")), None);
        assert_eq!(suggestion(&box_("\x1b[m❯\u{a0} ")), None);
        assert_eq!(suggestion(&box_("\x1b[m❯\u{a0} \x1b[2mTry \"fix lint errors\"")), None);
        assert_eq!(suggestion(&box_("\x1b[m❯\u{a0} \x1b[7mP\x1b[2mress up to edit queued messages")), None);
        assert_eq!(suggestion(&box_("\x1b[m❯\u{a0} \x1b[2mPress up to edit queued messages")), None);
        // Un menú no es la caja de escribir.
        assert_eq!(suggestion(" Do you want to proceed?\n ❯ 1. Yes\n   2. No"), None);
    }

    #[test]
    fn a_queued_message_with_a_list_is_not_a_menu() {
        assert_eq!(screen("cola"), None);
    }

    #[test]
    fn what_is_written_in_the_box() {
        let rule = "────────────────────────";
        let b = |inner: &str| format!("✻ Brewed\n\n{rule}──── nido ─\n{inner}\n{rule}\n  ⏵⏵ auto mode on");
        assert_eq!(input_box(&b("❯\u{a0}[Image #3] [Image #4]al mandar yo un mensaje con\n  puntos, el parser")).as_deref(),
            Some("[Image #3] [Image #4]al mandar yo un mensaje con puntos, el parser"));
        assert_eq!(input_box(&b("❯\u{a0}")).as_deref(), Some(""));
        assert_eq!(input_box(&b("\x1b[m❯\u{a0} \x1b[2mleer el fichero")).as_deref(), Some(""));
        assert_eq!(input_box(&b("\x1b[m❯\u{a0} \x1b[7mP\x1b[2mress up to edit queued messages")).as_deref(), Some(""));
        assert_eq!(input_box(&b("\x1b[38;2;153;153;153m❯\u{a0} \x1b[39;2mPress up to edit queued messages")).as_deref(), Some(""));
        // Un color RGB con un 2 dentro no es atenuado.
        assert_eq!(input_box(&b("\x1b[m❯\u{a0} \x1b[38;2;200;200;200mhola")).as_deref(), Some("hola"));
        assert_eq!(input_box(&std::fs::read_to_string(format!("{}/tests/pantallas/permiso.txt", env!("CARGO_MANIFEST_DIR"))).unwrap()), None);
    }

    #[test]
    fn mcp_form_with_a_checkbox() {
        let text = std::fs::read_to_string(format!("{}/tests/pantallas/formulario.txt", env!("CARGO_MANIFEST_DIR"))).unwrap();
        let (f, cur, hint) = form_state(&text).unwrap();
        assert_eq!(f.server, "prueba");
        assert!(f.message.starts_with("You are about to drop the index named **bp_assignee_1**"));
        assert_eq!(f.fields.len(), 1);
        let c = &f.fields[0];
        assert_eq!((c.label.as_str(), c.required, c.filled, c.kind), ("Would you like to confirm?", true, false, Some("bool")));
        assert_eq!(c.description.as_deref(), Some("Would you like to confirm?"));
        assert_eq!(cur, FormCursor::Field(0));
        assert!(hint.contains("Space to toggle"));
        // No es un menú numerado.
        assert_eq!(parse(&text), None);
    }

    #[test]
    fn mcp_form_with_several_fields_and_a_list() {
        let text = std::fs::read_to_string(format!("{}/tests/pantallas/formulario_varios.txt", env!("CARGO_MANIFEST_DIR"))).unwrap();
        let f = form(&text).unwrap();
        assert_eq!(f.fields.iter().map(|x| x.label.as_str()).collect::<Vec<_>>(), ["Would you like to confirm?", "Motivo", "Entorno"]);
        assert_eq!(f.fields[1].description.as_deref(), Some("Por qué lo borras"));
        assert!(!f.fields[1].required && f.fields[1].value == "not set");
        let lista = std::fs::read_to_string(format!("{}/tests/pantallas/formulario_lista.txt", env!("CARGO_MANIFEST_DIR"))).unwrap();
        let (f, cur, _) = form_state(&lista).unwrap();
        assert_eq!(f.server, "shara-dev");
        assert_eq!(f.fields[0].kind, Some("choice"));
        assert!(f.message.contains("(+2 more lines)"));
        assert_eq!(cur, FormCursor::Field(0));
        let open = "      Entorno: ▾\n        ❯ ◯ dev\n          ◉ pre\n          ◯ prod";
        assert_eq!(choice_options(open), (vec!["dev".into(), "pre".into(), "prod".into()], Some(0)));
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
    }
}
