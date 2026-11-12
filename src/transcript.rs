//! La conversación de Claude como chat, a partir de su transcripción
//! (`<config>/projects/<carpeta>/<sessionId>.jsonl`).
//!
//! Es un formato interno de Claude Code: solo se leen unos pocos campos y lo que no se entiende
//! se salta. Fuera quedan el razonamiento, las subtareas (`isSidechain`), los mensajes internos
//! (`isMeta`) y todo lo que no es conversación (peticiones a la API, estados…).

use std::collections::HashMap;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::Value;

/// Recortes: la vista es para leer en el móvil, no un volcado.
const MAX_INPUT: usize = 2000;
const MAX_RESULT: usize = 3000;
/// El resumen de la conversación (va plegado: se puede ver entero).
const MAX_SUMMARY: usize = 40_000;

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum ChatItem {
    User {
        id: String,
        /// Dónde empieza su línea en la transcripción (bytes): la identifica (`around`, búsqueda).
        at: u64,
        ts: Option<String>,
        text: String,
        images: usize,
    },
    Assistant {
        id: String,
        /// Dónde empieza su línea en la transcripción (bytes): la identifica (`around`, búsqueda).
        at: u64,
        ts: Option<String>,
        text: String,
    },
    /// Aviso de Claude Code dentro de la conversación (p. ej. «terminó la orden en segundo plano»):
    /// llega como un mensaje tuyo, pero no lo es.
    Notice {
        id: String,
        /// Dónde empieza su línea en la transcripción (bytes): la identifica (`around`, búsqueda).
        at: u64,
        ts: Option<String>,
        text: String,
        /// `completed`, `failed`, `killed`…
        status: Option<String>,
        /// Lo que devolvió la tarea, si viene en el aviso.
        result: Option<String>,
    },
    Tool {
        id: String,
        /// Dónde empieza su línea en la transcripción (bytes): la identifica (`around`, búsqueda).
        at: u64,
        ts: Option<String>,
        tool_id: String,
        name: String,
        /// Lo esencial en una línea: la orden, el fichero, la búsqueda…
        summary: String,
        input: String,
        result: Option<String>,
        is_error: bool,
        /// Las preguntas de AskUserQuestion, enteras (para contestarlas con un formulario).
        #[serde(skip_serializing_if = "Option::is_none")]
        ask: Option<Value>,
        /// El plan de ExitPlanMode, en Markdown.
        #[serde(skip_serializing_if = "Option::is_none")]
        plan: Option<String>,
        /// Lo que se contestó a AskUserQuestion: pregunta → respuesta.
        #[serde(skip_serializing_if = "Option::is_none")]
        answers: Option<Value>,
    },
}

impl ChatItem {
    /// Dónde empieza su línea en la transcripción.
    pub fn at(&self) -> u64 {
        match self {
            ChatItem::User { at, .. }
            | ChatItem::Assistant { at, .. }
            | ChatItem::Notice { at, .. }
            | ChatItem::Tool { at, .. } => *at,
        }
    }
}

/// El resultado de una herramienta que llegó en un trozo posterior a la llamada.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ToolResult {
    pub tool_id: String,
    pub result: String,
    pub is_error: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub answers: Option<Value>,
}

#[derive(Debug, Default, Serialize)]
pub struct Chunk {
    /// Dónde seguir leyendo la próxima vez.
    pub offset: u64,
    pub items: Vec<ChatItem>,
    /// Resultados de herramientas llamadas en trozos anteriores.
    pub results: Vec<ToolResult>,
    /// Se recortaron mensajes antiguos (pide más con un `limit` mayor).
    pub truncated: bool,
    /// Se empezó de cero (el fichero se rehízo): lo que tuviera el cliente ya no vale.
    pub reset: bool,
}

/// `<config>/projects/*/<session_id>.jsonl`, si existe.
pub fn find(config_dir: &Path, session_id: &str) -> Option<PathBuf> {
    let file = format!("{session_id}.jsonl");
    std::fs::read_dir(config_dir.join("projects"))
        .ok()?
        .flatten()
        .map(|d| d.path().join(&file))
        .find(|p| p.is_file())
}

fn clip(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_owned();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

/// Quita los bloques `<system-reminder>…</system-reminder>` que Claude Code mete en los mensajes.
fn strip_reminders(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(i) = rest.find("<system-reminder>") {
        out.push_str(&rest[..i]);
        match rest[i..].find("</system-reminder>") {
            Some(j) => rest = &rest[i + j + "</system-reminder>".len()..],
            None => {
                rest = "";
                break;
            }
        }
    }
    out.push_str(rest);
    out.trim().to_owned()
}

/// El contenido de `<tag>…</tag>` dentro de `s`.
fn tag<'a>(s: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}>");
    let i = s.find(&open)? + open.len();
    let j = s[i..].find(&format!("</{tag}>"))?;
    Some(s[i..i + j].trim())
}

/// `<task-notification>…</task-notification>` → el aviso con su resumen.
fn task_notice(text: &str, id: &str, ts: &Option<String>, at: u64) -> Option<ChatItem> {
    if !text.starts_with("<task-notification>") {
        return None;
    }
    let status = tag(text, "status").map(str::to_owned);
    let summary = tag(text, "summary").map(str::to_owned).unwrap_or_else(|| {
        format!(
            "Tarea en segundo plano: {}",
            status.as_deref().unwrap_or("aviso")
        )
    });
    Some(ChatItem::Notice {
        id: id.to_owned(),
        at,
        ts: ts.clone(),
        text: clip(&summary, 500),
        status,
        result: tag(text, "result")
            .filter(|r| !r.is_empty())
            .map(|r| clip(r, MAX_RESULT)),
    })
}

fn tool_summary(name: &str, input: &Value) -> String {
    let field = |k: &str| input.get(k).and_then(Value::as_str).map(str::to_owned);
    let s = match name {
        "Bash" => field("command"),
        "Read" | "Write" | "Edit" | "NotebookEdit" => {
            field("file_path").or_else(|| field("notebook_path"))
        }
        "Grep" | "Glob" => field("pattern"),
        "WebSearch" => field("query"),
        "WebFetch" => field("url"),
        "Agent" | "Task" => field("description"),
        "Skill" => field("skill"),
        _ => None,
    };
    let s = s.or_else(|| {
        input
            .as_object()?
            .values()
            .find_map(|v| v.as_str().map(str::to_owned))
    });
    clip(s.unwrap_or_default().lines().next().unwrap_or(""), 200)
}

fn tool_input(name: &str, input: &Value) -> String {
    let text = match name {
        "Bash" => input
            .get("command")
            .and_then(Value::as_str)
            .map(str::to_owned),
        _ => None,
    };
    clip(
        &text.unwrap_or_else(|| serde_json::to_string_pretty(input).unwrap_or_default()),
        MAX_INPUT,
    )
}

/// Las preguntas de AskUserQuestion con lo que hace falta para el formulario (los textos largos,
/// recortados uno a uno: recortar el JSON entero lo dejaría sin cerrar).
fn ask_data(input: &Value) -> Option<Value> {
    let questions = input.get("questions")?.as_array()?;
    let s = |v: &Value, k: &str, max: usize| v.get(k).and_then(Value::as_str).map(|t| clip(t, max));
    let qs: Vec<Value> = questions
        .iter()
        .map(|q| {
            let options: Vec<Value> = q
                .get("options")
                .and_then(Value::as_array)
                .map(|o| o.as_slice())
                .unwrap_or_default()
                .iter()
                .map(|o| {
                    serde_json::json!({
                        "label": s(o, "label", 300),
                        "description": s(o, "description", 1000),
                        "preview": s(o, "preview", 4000),
                    })
                })
                .collect();
            serde_json::json!({
                "question": s(q, "question", 1000),
                "header": s(q, "header", 100),
                "multiSelect": q.get("multiSelect").and_then(Value::as_bool).unwrap_or(false),
                "options": options,
            })
        })
        .collect();
    Some(serde_json::json!({ "questions": qs }))
}

fn result_text(content: &Value) -> String {
    let text = match content {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|b| match b.get("type").and_then(Value::as_str) {
                Some("text") => b.get("text").and_then(Value::as_str).map(str::to_owned),
                Some("image") => Some("[imagen]".into()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    };
    clip(&strip_reminders(&text), MAX_RESULT)
}

fn queued_prompt(d: &Value, at: u64) -> Option<ChatItem> {
    let att = d.get("attachment")?;
    if att.get("type")?.as_str()? != "queued_command"
        || att.pointer("/origin/kind").and_then(Value::as_str) != Some("human")
    {
        return None;
    }
    let (text, images) = match att.get("prompt")? {
        Value::String(s) => (s.clone(), 0),
        Value::Array(blocks) => {
            let text = blocks
                .iter()
                .filter_map(|b| b.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n");
            let images = blocks
                .iter()
                .filter(|b| b.get("type").and_then(Value::as_str) == Some("image"))
                .count();
            (text, images)
        }
        _ => return None,
    };
    let text = strip_reminders(&text);
    (!text.is_empty() || images > 0).then(|| ChatItem::User {
        id: d
            .get("uuid")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        at,
        ts: att
            .get("timestamp")
            .or_else(|| d.get("timestamp"))
            .and_then(Value::as_str)
            .map(str::to_owned),
        text,
        images,
    })
}

/// Lo que no es tuyo aunque llegue como un mensaje tuyo: órdenes (`/model`…), su salida, avisos
/// de interrupción y de tareas en segundo plano. Recibe el texto ya sin los `<system-reminder>`.
fn user_text_shown(text: &str) -> bool {
    !text.is_empty()
        && !text.starts_with("<local-command")
        && !text.starts_with("<command-")
        && !text.starts_with("[Request interrupted")
}

/// Convierte líneas de la transcripción en mensajes, de una en una. `tools` acumula dónde está
/// cada llamada para enganchar su resultado, aunque llegue en otra línea o en otro trozo.
#[derive(Default)]
struct Parser {
    tools: HashMap<String, usize>,
    items: Vec<ChatItem>,
    results: Vec<ToolResult>,
}

impl Parser {
    /// Una línea, que empieza en `at` (bytes) en el fichero.
    fn line(&mut self, at: u64, line: &str) {
        let Ok(d) = serde_json::from_str::<Value>(line) else {
            return;
        };
        let (items, results, tools) = (&mut self.items, &mut self.results, &mut self.tools);
        let kind = d.get("type").and_then(Value::as_str);
        // Lo que escribes mientras Claude trabaja llega como un adjunto «queued_command».
        if kind == Some("attachment") {
            if let Some(item) = queued_prompt(&d, at) {
                items.push(item);
            }
            return;
        }
        if !matches!(kind, Some("user" | "assistant")) {
            return;
        }
        if d.get("isSidechain").and_then(Value::as_bool) == Some(true) {
            return;
        }
        let id = d
            .get("uuid")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let ts = d
            .get("timestamp")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let meta = d.get("isMeta").and_then(Value::as_bool) == Some(true);
        let content = d.get("message").and_then(|m| m.get("content"));
        // El resumen que pone Claude Code al quedarse sin contexto: llega como un mensaje tuyo
        // enorme. Un aviso, con el resumen plegado.
        if kind == Some("user") && d.get("isCompactSummary").and_then(Value::as_bool) == Some(true)
        {
            let text = match content {
                Some(Value::String(s)) => s.clone(),
                Some(c) => result_text(c),
                None => String::new(),
            };
            items.push(ChatItem::Notice {
                id,
                at,
                ts,
                text: "Claude resumió la conversación para seguir (se le acababa el contexto)"
                    .into(),
                status: Some("compact".into()),
                result: Some(clip(text.trim(), MAX_SUMMARY)),
            });
            return;
        }
        match (kind, content) {
            (Some("user"), Some(Value::String(s))) if !meta => {
                let text = strip_reminders(s);
                if let Some(notice) = task_notice(&text, &id, &ts, at) {
                    items.push(notice);
                } else if user_text_shown(&text) {
                    items.push(ChatItem::User {
                        id,
                        at,
                        ts,
                        text,
                        images: 0,
                    });
                }
            }
            (Some("user"), Some(Value::Array(blocks))) => {
                let mut text = Vec::new();
                let mut images = 0;
                for b in blocks {
                    match b.get("type").and_then(Value::as_str) {
                        Some("text") if !meta => {
                            if let Some(t) = b.get("text").and_then(Value::as_str) {
                                text.push(strip_reminders(t));
                            }
                        }
                        Some("image") if !meta => images += 1,
                        Some("tool_result") => {
                            let tool_id = b
                                .get("tool_use_id")
                                .and_then(Value::as_str)
                                .unwrap_or_default();
                            let result = result_text(b.get("content").unwrap_or(&Value::Null));
                            let is_error = b.get("is_error").and_then(Value::as_bool) == Some(true);
                            let answers = d
                                .pointer("/toolUseResult/answers")
                                .filter(|a| a.is_object())
                                .cloned();
                            match tools.get(tool_id).and_then(|&i| items.get_mut(i)) {
                                Some(ChatItem::Tool {
                                    result: r,
                                    is_error: e,
                                    answers: a,
                                    ..
                                }) => {
                                    *r = Some(result);
                                    *e = is_error;
                                    *a = answers;
                                }
                                _ => results.push(ToolResult {
                                    tool_id: tool_id.to_owned(),
                                    result,
                                    is_error,
                                    answers,
                                }),
                            }
                        }
                        _ => {}
                    }
                }
                let text = text.join("\n").trim().to_owned();
                // «[Request interrupted by user…]» lo pone Claude Code, no tú.
                let text = if text.starts_with("[Request interrupted") {
                    String::new()
                } else {
                    text
                };
                if let Some(notice) = task_notice(&text, &id, &ts, at) {
                    items.push(notice);
                } else if !text.is_empty() || images > 0 {
                    items.push(ChatItem::User {
                        id,
                        at,
                        ts,
                        text,
                        images,
                    });
                }
            }
            (Some("assistant"), Some(Value::Array(blocks))) => {
                for (n, b) in blocks.iter().enumerate() {
                    let bid = if n == 0 {
                        id.clone()
                    } else {
                        format!("{id}:{n}")
                    };
                    match b.get("type").and_then(Value::as_str) {
                        Some("text") => {
                            let text = b
                                .get("text")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .trim();
                            if text.is_empty() {
                            } else if is_limit(&d) {
                                // «You've hit your weekly limit · resets…»: no lo dice Claude.
                                items.push(ChatItem::Notice {
                                    id: bid,
                                    at,
                                    ts: ts.clone(),
                                    text: text.to_owned(),
                                    status: Some("limit".into()),
                                    result: None,
                                });
                            } else {
                                items.push(ChatItem::Assistant {
                                    id: bid,
                                    at,
                                    ts: ts.clone(),
                                    text: text.to_owned(),
                                });
                            }
                        }
                        Some("tool_use") => {
                            let name = b
                                .get("name")
                                .and_then(Value::as_str)
                                .unwrap_or("?")
                                .to_owned();
                            let input = b.get("input").cloned().unwrap_or(Value::Null);
                            let tool_id = b
                                .get("id")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_owned();
                            tools.insert(tool_id.clone(), items.len());
                            let ask = (name == "AskUserQuestion")
                                .then(|| ask_data(&input))
                                .flatten();
                            let plan = (name == "ExitPlanMode")
                                .then(|| {
                                    input
                                        .get("plan")
                                        .and_then(Value::as_str)
                                        .map(|p| clip(p, 30_000))
                                })
                                .flatten();
                            items.push(ChatItem::Tool {
                                id: bid,
                                at,
                                ts: ts.clone(),
                                tool_id,
                                summary: tool_summary(&name, &input),
                                input: tool_input(&name, &input),
                                name,
                                result: None,
                                is_error: false,
                                ask,
                                plan,
                                answers: None,
                            });
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
}

/// El aviso de que se acabó el límite de uso: un mensaje que pone Claude Code, no el modelo.
fn is_limit(d: &Value) -> bool {
    d.get("error").and_then(Value::as_str) == Some("rate_limit")
}

/// La conversación se quedó parada por el límite de uso: lo último que hay en ella es ese aviso.
pub fn stopped_by_limit(path: &Path) -> bool {
    const TAIL: u64 = 512 * 1024;
    let Ok(mut f) = std::fs::File::open(path) else {
        return false;
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    if f.seek(SeekFrom::Start(len.saturating_sub(TAIL))).is_err() {
        return false;
    }
    let mut buf = Vec::new();
    if f.read_to_end(&mut buf).is_err() {
        return false;
    }
    let text = String::from_utf8_lossy(&buf);
    for line in text.lines().rev() {
        let Ok(d) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if !matches!(
            d.get("type").and_then(Value::as_str),
            Some("user" | "assistant")
        ) || d.get("isSidechain").and_then(Value::as_bool) == Some(true)
            || d.get("isMeta").and_then(Value::as_bool) == Some(true)
        {
            continue;
        }
        return is_limit(&d);
    }
    false
}

/// El modelo del último mensaje de Claude en la conversación principal (sin los de los subagentes,
/// `isSidechain`, ni los que pone Claude Code, `<synthetic>`), tal como lo da la API
/// (`claude-opus-5-5`), y dónde empieza esa línea en el fichero. Se busca desde el final, cada
/// vez más atrás: hay líneas muy largas (las de las herramientas) que también dicen `model`.
pub fn last_model(path: &Path) -> Option<(String, u64)> {
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    let mut tail: u64 = 1024 * 1024;
    loop {
        let start = len.saturating_sub(tail);
        f.seek(SeekFrom::Start(start)).ok()?;
        let mut buf = Vec::new();
        f.read_to_end(&mut buf).ok()?;
        // Las líneas enteras: la primera puede venir cortada (si no se empezó por el principio).
        let mut at = if start == 0 {
            0
        } else {
            buf.iter()
                .position(|&b| b == b'\n')
                .map_or(buf.len(), |i| i + 1)
        };
        let mut found = None;
        while at < buf.len() {
            let end = buf[at..]
                .iter()
                .position(|&b| b == b'\n')
                .map_or(buf.len(), |i| at + i);
            if let Some(model) = assistant_model(&buf[at..end]) {
                found = Some((model, start + at as u64));
            }
            at = end + 1;
        }
        if found.is_some() || start == 0 {
            return found;
        }
        tail *= 8;
    }
}

/// Lo nuevo de la transcripción desde `from` (el principio de una línea): el modelo de la última
/// respuesta en las líneas ya enteras (y dónde empieza esa línea), y dónde empieza lo que aún no
/// es una línea entera (de ahí se sigue la próxima vez).
pub fn models_since(path: &Path, from: u64) -> (Option<(String, u64)>, u64) {
    let Ok(mut f) = std::fs::File::open(path) else {
        return (None, from);
    };
    let mut buf = Vec::new();
    if f.seek(SeekFrom::Start(from)).is_err() || f.read_to_end(&mut buf).is_err() {
        return (None, from);
    }
    let mut found = None;
    let mut at = 0;
    while let Some(i) = buf[at..].iter().position(|&b| b == b'\n') {
        if let Some(model) = assistant_model(&buf[at..at + i]) {
            found = Some((model, from + at as u64));
        }
        at += i + 1;
    }
    (found, from + at as u64)
}

/// Dónde acaba la última línea entera de los primeros `len` bytes (tras su `\n`): mirando hacia
/// atrás desde el final, a trozos.
pub fn whole_lines_end(path: &Path, len: u64) -> u64 {
    let Ok(mut f) = std::fs::File::open(path) else {
        return 0;
    };
    const CHUNK: u64 = 64 * 1024;
    let mut end = len;
    while end > 0 {
        let start = end.saturating_sub(CHUNK);
        let mut buf = vec![0; (end - start) as usize];
        if f.seek(SeekFrom::Start(start)).is_err() || f.read_exact(&mut buf).is_err() {
            return 0;
        }
        if let Some(i) = buf.iter().rposition(|&b| b == b'\n') {
            return start + i as u64 + 1;
        }
        end = start;
    }
    0
}

fn assistant_model(line: &[u8]) -> Option<String> {
    // Antes de entender la línea, que diga algo de eso (las hay de megas).
    if !line.windows(11).any(|w| w == b"\"assistant\"") {
        return None;
    }
    let d: Value = serde_json::from_slice(line).ok()?;
    if d.get("type").and_then(Value::as_str) != Some("assistant")
        || d.get("isSidechain").and_then(Value::as_bool) == Some(true)
    {
        return None;
    }
    let model = d.pointer("/message/model")?.as_str()?;
    (!model.is_empty() && !model.starts_with('<')).then(|| model.to_owned())
}

/// El nombre de un modelo para enseñarlo, como lo pone Claude en `/model`: `claude-opus-5-5` →
/// «Opus 5.5», `claude-haiku-4-5-20251001` → «Haiku 4.5». Lo que no se entiende, tal cual.
pub fn model_name(id: &str) -> String {
    let base = id.split('[').next().unwrap_or(id);
    let mut parts = base.split('-');
    if parts.next() != Some("claude") {
        return id.to_owned();
    }
    let (mut family, mut version) = (None, Vec::new());
    for p in parts {
        if !p.is_empty() && p.chars().all(|c| c.is_ascii_alphabetic()) && family.is_none() {
            family = Some(p);
        } else if !p.is_empty() && p.len() <= 2 && p.chars().all(|c| c.is_ascii_digit()) {
            version.push(p);
        }
    }
    let Some(family) = family else {
        return id.to_owned();
    };
    let mut name: String = family[..1].to_uppercase() + &family[1..];
    if !version.is_empty() {
        name.push(' ');
        name.push_str(&version.join("."));
    }
    name
}

/// Una conversación de Claude guardada, para retomarla.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Conversation {
    pub id: String,
    /// El nombre que se le puso (`/rename`) o el que le sacó Claude.
    pub title: Option<String>,
    /// Lo último que escribiste.
    pub last_prompt: Option<String>,
    /// Última vez que cambió (ms desde 1970).
    pub modified: u64,
    pub size: u64,
}

/// La carpeta de `<config>/projects` de un directorio: Claude Code cambia por «-» todo lo que no
/// sea letra o número.
pub fn project_dir(config_dir: &Path, cwd: &Path) -> PathBuf {
    let name: String = cwd
        .to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    config_dir.join("projects").join(name)
}

/// Las conversaciones de un directorio, de la más reciente a la más antigua (como mucho `limit`).
/// Solo se lee el final de cada fichero: ahí están el título y lo último que escribiste.
pub fn conversations(dir: &Path, limit: usize) -> Vec<Conversation> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<(PathBuf, u64, u64)> = rd
        .flatten()
        .filter_map(|e| {
            let p = e.path();
            if p.extension()? != "jsonl" {
                return None;
            }
            let m = e.metadata().ok()?;
            let modified = m
                .modified()
                .ok()?
                .duration_since(std::time::UNIX_EPOCH)
                .ok()?
                .as_millis() as u64;
            Some((p, modified, m.len()))
        })
        .collect();
    files.sort_by(|a, b| b.1.cmp(&a.1));
    files
        .into_iter()
        .take(limit)
        .filter_map(|(p, modified, size)| {
            let id = p.file_stem()?.to_str()?.to_owned();
            let (mut title, last_prompt, has_messages) = summary(&p);
            if !has_messages {
                return None; // abierta y cerrada sin escribir nada
            }
            // El nombre puesto a mano manda (Claude Code lo guarda también aparte).
            let custom = std::fs::read(dir.join(&id).join("custom-title.json"))
                .ok()
                .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
                .and_then(|v| v.get("customTitle")?.as_str().map(str::to_owned));
            if custom.is_some() {
                title = custom;
            }
            Some(Conversation {
                id,
                title,
                last_prompt,
                modified,
                size,
            })
        })
        .collect()
}

/// Del final de una transcripción: (título, lo último que escribiste, si tiene mensajes).
fn summary(path: &Path) -> (Option<String>, Option<String>, bool) {
    const TAIL: u64 = 256 * 1024;
    let Ok(mut f) = std::fs::File::open(path) else {
        return (None, None, false);
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let start = len.saturating_sub(TAIL);
    if f.seek(SeekFrom::Start(start)).is_err() {
        return (None, None, false);
    }
    let mut buf = Vec::new();
    if f.read_to_end(&mut buf).is_err() {
        return (None, None, false);
    }
    let text = String::from_utf8_lossy(&buf);
    let (mut custom, mut ai, mut prompt, mut user) = (None, None, None, None);
    let mut messages = false;
    // La primera línea puede venir cortada (si no se empezó por el principio): se salta sola.
    for line in text.lines() {
        let Ok(d) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let field = |k: &str| d.get(k).and_then(Value::as_str).map(str::to_owned);
        match d.get("type").and_then(Value::as_str) {
            Some("custom-title") => custom = field("customTitle").or(custom),
            Some("ai-title") => ai = field("aiTitle").or(ai),
            Some("last-prompt") => prompt = field("lastPrompt").or(prompt),
            Some("assistant") => messages = true,
            Some("user") => {
                messages = true;
                let content = d.pointer("/message/content");
                let raw = match content {
                    Some(Value::String(t)) => Some(t.clone()),
                    // En bloques: solo si es texto tuyo (no el resultado de una herramienta).
                    Some(Value::Array(blocks)) => {
                        let texts: Vec<&str> = blocks
                            .iter()
                            .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
                            .filter_map(|b| b.get("text").and_then(Value::as_str))
                            .collect();
                        (!texts.is_empty()).then(|| texts.join("\n"))
                    }
                    _ => None,
                };
                if d.get("isMeta").and_then(Value::as_bool) != Some(true)
                    && let Some(t) = raw
                {
                    let t = strip_reminders(&t);
                    if !t.is_empty()
                        && !t.starts_with('<')
                        && !t.starts_with("[Request interrupted")
                    {
                        user = Some(t);
                    }
                }
            }
            _ => {}
        }
    }
    let one_line = |s: String| {
        clip(
            s.split_whitespace().collect::<Vec<_>>().join(" ").as_str(),
            200,
        )
    };
    (
        custom.or(ai),
        prompt.or(user).map(one_line),
        messages || start > 0,
    )
}

/// Las líneas enteras de `buf` (que está en `base` en el fichero), cada una con dónde empieza:
/// sobre los bytes, para que las posiciones sean las del fichero aunque haya bytes que no sean
/// UTF-8 o finales `\r\n`.
fn lines_at(buf: &[u8], base: u64) -> impl Iterator<Item = (u64, &[u8])> {
    let mut at = 0;
    std::iter::from_fn(move || {
        if at >= buf.len() {
            return None;
        }
        let end = buf[at..]
            .iter()
            .position(|&b| b == b'\n')
            .map_or(buf.len(), |i| at + i);
        let line = (base + at as u64, &buf[at..end]);
        at = end + 1;
        Some(line)
    })
}

/// Lee desde `offset` hasta la última línea completa (Claude puede estar escribiendo la
/// siguiente). Si se lee desde el principio (`offset = 0`, o el fichero se rehízo), con `limit`
/// devuelve solo los últimos `limit` mensajes.
pub fn read_chunk(path: &Path, offset: u64, limit: Option<usize>) -> io::Result<Chunk> {
    let mut f = std::fs::File::open(path)?;
    let len = f.metadata()?.len();
    // El fichero se rehízo (más corto que lo ya leído): empezar de cero.
    let reset = offset > len;
    let offset = if reset { 0 } else { offset };
    f.seek(SeekFrom::Start(offset))?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf)?;
    let complete = buf.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
    let mut p = Parser::default();
    for (at, line) in lines_at(&buf[..complete], offset) {
        p.line(at, &String::from_utf8_lossy(line));
    }
    let mut chunk = Chunk {
        offset: offset + complete as u64,
        reset,
        items: p.items,
        results: p.results,
        ..Default::default()
    };
    // El límite vale al leer desde el principio (también si el fichero se rehízo).
    if let Some(limit) = limit.filter(|_| offset == 0)
        && chunk.items.len() > limit
    {
        chunk.items.drain(..chunk.items.len() - limit);
        chunk.truncated = true;
    }
    Ok(chunk)
}

/// Una ventana de la conversación alrededor de un punto (`around`): líneas enteras, de `start`
/// a `offset`.
#[derive(Debug, Default, Serialize)]
pub struct Window {
    /// Dónde empieza la primera línea de la ventana: lo anterior se pide con `around=start`.
    pub start: u64,
    /// Dónde acaba la última (tras su `\n`): lo siguiente se pide con `around=offset`, y si ya no
    /// hay nada más (`more_after`), desde ahí se sigue en vivo.
    pub offset: u64,
    pub items: Vec<ChatItem>,
    /// Resultados que traen las líneas de la ventana de herramientas de antes de ella.
    pub results: Vec<ToolResult>,
    /// Hay mensajes antes de `start`.
    pub more_before: bool,
    /// Hay mensajes después de `offset`.
    pub more_after: bool,
}

/// Cuánto se sigue leyendo tras la ventana para encontrar los resultados de sus herramientas.
const LOOKAHEAD: u64 = 8 * 1024 * 1024;

/// Dónde empieza la línea que acaba en `end` (justo tras su `\n`), mirando hacia atrás a trozos.
fn line_start_before(f: &mut std::fs::File, end: u64) -> io::Result<u64> {
    const CHUNK: u64 = 64 * 1024;
    let mut hi = end.saturating_sub(1);
    while hi > 0 {
        let lo = hi.saturating_sub(CHUNK);
        let mut buf = vec![0; (hi - lo) as usize];
        f.seek(SeekFrom::Start(lo))?;
        f.read_exact(&mut buf)?;
        if let Some(i) = buf.iter().rposition(|&b| b == b'\n') {
            return Ok(lo + i as u64 + 1);
        }
        hi = lo;
    }
    Ok(0)
}

/// El principio de la primera línea que empieza en `at` o después (`end` si no hay ninguna
/// entera): un `at` a media línea cuenta como el principio de la siguiente.
fn line_start_from(f: &mut std::fs::File, at: u64, end: u64) -> io::Result<u64> {
    if at == 0 || at >= end {
        return Ok(at.min(end));
    }
    f.seek(SeekFrom::Start(at - 1))?;
    let mut r = io::BufReader::new(f.take(end - (at - 1)));
    let mut skipped = Vec::new();
    let n = io::BufRead::read_until(&mut r, b'\n', &mut skipped)?;
    Ok(at - 1 + n as u64)
}

/// Los mensajes de la conversación alrededor de `at` (el principio de una línea, como el `at` de
/// cada mensaje): como mucho `before` de las líneas anteriores y `after` desde esa línea en
/// adelante, más los que vengan en la misma línea que el último de cada lado (la ventana no
/// parte líneas). Solo se lee la ventana (hacia atrás desde `at`) y un poco más allá: lo que
/// cuesta no depende del tamaño de la transcripción.
pub fn read_around(path: &Path, at: u64, before: usize, after: usize) -> io::Result<Window> {
    let mut f = std::fs::File::open(path)?;
    let len = f.metadata()?.len();
    let end = whole_lines_end(path, len);
    let anchor = line_start_from(&mut f, at, end)?;
    let mut w = Window {
        start: anchor,
        ..Default::default()
    };

    // Hacia atrás: las líneas que hagan falta para `before` mensajes, y si queda alguno antes.
    let (mut probe, mut count) = (anchor, 0);
    while probe > 0 {
        let from = line_start_before(&mut f, probe)?;
        let mut buf = vec![0; (probe - from) as usize];
        f.seek(SeekFrom::Start(from))?;
        f.read_exact(&mut buf)?;
        let mut p = Parser::default();
        p.line(from, &String::from_utf8_lossy(&buf));
        probe = from;
        if count < before {
            count += p.items.len();
            w.start = from;
        } else if !p.items.is_empty() {
            w.more_before = true;
            break;
        }
    }

    // Hacia delante desde `start`: la ventana, y luego lo justo para saber si hay más y para
    // los resultados de sus herramientas.
    f.seek(SeekFrom::Start(w.start))?;
    let mut r = io::BufReader::with_capacity(1 << 20, f.take(end - w.start));
    let mut p = Parser::default();
    let (mut pos, mut line) = (w.start, Vec::new());
    // Mensajes antes de `anchor`, y los de la ventana con sus resultados (cuando se cierra).
    let mut before_items = None;
    let mut closed: Option<(usize, usize)> = None;
    loop {
        if before_items.is_none() && pos >= anchor {
            before_items = Some(p.items.len());
            if after == 0 {
                closed = Some((p.items.len(), p.results.len()));
                w.offset = pos;
            }
        }
        if let Some((n, _)) = closed {
            let more = p.items.len() > n;
            let pending = p.items[..n]
                .iter()
                .any(|i| matches!(i, ChatItem::Tool { result: None, .. }));
            // Se sabe si hay más y no falta ningún resultado (o ya se ha mirado bastante).
            if more && (!pending || pos - w.offset > LOOKAHEAD) {
                break;
            }
        }
        line.clear();
        let got = io::BufRead::read_until(&mut r, b'\n', &mut line)?;
        if got == 0 {
            break;
        }
        p.line(pos, &String::from_utf8_lossy(&line[..got - 1]));
        pos += got as u64;
        if closed.is_none()
            && let Some(b) = before_items
            && p.items.len() - b >= after
        {
            closed = Some((p.items.len(), p.results.len()));
            w.offset = pos;
        }
    }
    let (n, results) = closed.unwrap_or_else(|| {
        w.offset = pos;
        (p.items.len(), p.results.len())
    });
    w.more_after = p.items.len() > n;
    p.items.truncate(n);
    p.results.truncate(results);
    w.items = p.items;
    w.results = p.results;
    Ok(w)
}

/// Algo que dijiste tú o Claude, solo el texto: lo que se busca (`search`).
#[derive(Debug, Clone, PartialEq)]
pub struct Said {
    /// Dónde empieza su línea (el `at` del mensaje en el chat).
    pub at: u64,
    /// El `id` del mensaje en el chat.
    pub id: String,
    pub ts: Option<String>,
    /// Tuyo (si no, de Claude).
    pub user: bool,
    pub text: String,
}

/// Lo poco que hace falta de una línea para `said`: lo demás (resultados de herramientas de
/// megas, peticiones a la API…) se salta sin copiarlo.
#[derive(serde::Deserialize)]
struct Head<'a> {
    #[serde(rename = "type", borrow, default)]
    kind: Option<&'a serde_json::value::RawValue>,
    #[serde(rename = "isSidechain", borrow, default)]
    sidechain: Option<&'a serde_json::value::RawValue>,
    #[serde(rename = "isMeta", borrow, default)]
    meta: Option<&'a serde_json::value::RawValue>,
    #[serde(rename = "isCompactSummary", borrow, default)]
    compact: Option<&'a serde_json::value::RawValue>,
    #[serde(rename = "isApiErrorMessage", borrow, default)]
    api_error: Option<&'a serde_json::value::RawValue>,
    #[serde(borrow, default)]
    error: Option<&'a serde_json::value::RawValue>,
    #[serde(borrow, default)]
    uuid: Option<&'a serde_json::value::RawValue>,
    #[serde(borrow, default)]
    timestamp: Option<&'a serde_json::value::RawValue>,
    #[serde(borrow, default)]
    message: Option<Body<'a>>,
}

#[derive(serde::Deserialize)]
struct Body<'a> {
    #[serde(borrow, default)]
    content: Option<&'a serde_json::value::RawValue>,
    #[serde(borrow, default)]
    model: Option<&'a serde_json::value::RawValue>,
}

#[derive(serde::Deserialize)]
struct Block<'a> {
    #[serde(rename = "type", borrow, default)]
    kind: Option<&'a serde_json::value::RawValue>,
    #[serde(borrow, default)]
    text: Option<&'a serde_json::value::RawValue>,
}

fn raw_is(r: Option<&serde_json::value::RawValue>, json: &str) -> bool {
    r.is_some_and(|r| r.get() == json)
}

fn raw_string(r: Option<&serde_json::value::RawValue>) -> Option<String> {
    serde_json::from_str(r?.get()).ok()
}

/// Lo que se ve en el chat como mensaje tuyo o de Claude en una línea de la transcripción (que
/// empieza en `at`), solo su texto, con el mismo criterio que la vista chat (`Parser`): fuera
/// herramientas y sus resultados, órdenes, avisos, subagentes (`isSidechain`), mensajes
/// internos (`isMeta`), el resumen al compactar y los mensajes que pone Claude Code en nombre
/// de Claude (`<synthetic>`, errores de la API, el límite de uso), y los mensajes sin texto
/// (solo imágenes). Mucho más rápido que `Parser`: no construye la línea entera.
pub fn said(line: &[u8], at: u64) -> Vec<Said> {
    let text = String::from_utf8_lossy(line);
    let Ok(h) = serde_json::from_str::<Head>(&text) else {
        return Vec::new();
    };
    let kind = h.kind.map(|k| k.get());
    let shown = |user: bool, id: String, text: String| Said {
        at,
        id,
        ts: raw_string(h.timestamp),
        user,
        text,
    };
    if kind == Some("\"attachment\"") {
        if !text.contains("queued_command") {
            return Vec::new();
        }
        let Ok(d) = serde_json::from_str::<Value>(&text) else {
            return Vec::new();
        };
        return match queued_prompt(&d, at) {
            Some(ChatItem::User { id, ts, text, .. }) if !text.is_empty() => vec![Said {
                at,
                id,
                ts,
                user: true,
                text,
            }],
            _ => Vec::new(),
        };
    }
    let user = match kind {
        Some("\"user\"") => true,
        Some("\"assistant\"") => false,
        _ => return Vec::new(),
    };
    if raw_is(h.sidechain, "true") || (user && raw_is(h.compact, "true")) {
        return Vec::new();
    }
    let Some(body) = &h.message else {
        return Vec::new();
    };
    let Some(content) = body.content.map(|c| c.get().trim_start()) else {
        return Vec::new();
    };
    let id = raw_string(h.uuid).unwrap_or_default();
    if user {
        if raw_is(h.meta, "true") {
            return Vec::new();
        }
        let text = if content.starts_with('"') {
            let Ok(s) = serde_json::from_str::<String>(content) else {
                return Vec::new();
            };
            let text = strip_reminders(&s);
            if !user_text_shown(&text) {
                return Vec::new();
            }
            text
        } else if content.starts_with('[') {
            let Ok(blocks) = serde_json::from_str::<Vec<Block>>(content) else {
                return Vec::new();
            };
            let texts: Vec<String> = blocks
                .iter()
                .filter(|b| raw_is(b.kind, "\"text\""))
                .filter_map(|b| raw_string(b.text))
                .map(|t| strip_reminders(&t))
                .collect();
            let text = texts.join("\n").trim().to_owned();
            if text.starts_with("[Request interrupted") {
                return Vec::new();
            }
            text
        } else {
            return Vec::new();
        };
        if text.is_empty() || text.starts_with("<task-notification>") {
            return Vec::new();
        }
        return vec![shown(true, id, text)];
    }
    // De Claude, salvo lo que pone Claude Code en su nombre.
    let synthetic = body.model.is_some_and(|m| m.get().starts_with("\"<"));
    if raw_is(h.error, "\"rate_limit\"") || raw_is(h.api_error, "true") || synthetic {
        return Vec::new();
    }
    if !content.starts_with('[') {
        return Vec::new();
    }
    let Ok(blocks) = serde_json::from_str::<Vec<Block>>(content) else {
        return Vec::new();
    };
    blocks
        .iter()
        .enumerate()
        .filter(|(_, b)| raw_is(b.kind, "\"text\""))
        .filter_map(|(n, b)| {
            let text = raw_string(b.text)?.trim().to_owned();
            let bid = if n == 0 {
                id.clone()
            } else {
                format!("{id}:{n}")
            };
            (!text.is_empty()).then(|| shown(false, bid, text))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn line(v: Value) -> String {
        format!("{}\n", v)
    }

    #[test]
    fn model_names() {
        assert_eq!(model_name("claude-opus-5-5"), "Opus 5.5");
        assert_eq!(model_name("claude-fable-5-2"), "Fable 5.2");
        assert_eq!(model_name("claude-haiku-4-5-20251001"), "Haiku 4.5");
        assert_eq!(model_name("claude-sonnet-4-20250514"), "Sonnet 4");
        assert_eq!(model_name("claude-3-5-sonnet-20241022"), "Sonnet 3.5");
        assert_eq!(model_name("claude-opus-4-6[1m]"), "Opus 4.6");
        assert_eq!(model_name("gpt-x"), "gpt-x");
        assert_eq!(model_name("claude-7"), "claude-7");
    }

    #[test]
    fn the_model_of_the_last_reply() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let reply = |model: &str, side: bool| {
            line(
                serde_json::json!({"type":"assistant","isSidechain":side,"message":{"model":model,"content":[{"type":"text","text":"hola"}]}}),
            )
        };
        let mut text = String::new();
        text += &line(serde_json::json!({"type":"user","message":{"content":"hola"}}));
        text += &reply("claude-opus-5-5", false);
        let at = text.len() as u64;
        text += &reply("claude-haiku-5-5", false);
        // Lo que viene detrás no cuenta: un subagente, un aviso de Claude Code, una herramienta
        // que dice `model`, la línea que se está escribiendo.
        text += &reply("claude-fable-5-2", true);
        text += &reply("<synthetic>", false);
        text += &line(
            serde_json::json!({"type":"attachment","attachment":{"type":"advisor_tool","model":"claude-fable-5-2"}}),
        );
        text += &line(
            serde_json::json!({"type":"api-request-shape","shape":{"tools":[{"model":"claude-fable-5-2"}]}}),
        );
        text += "{\"type\":\"assistant\",\"message\":{\"model\":\"claude-son";
        std::fs::write(&path, &text).unwrap();
        assert_eq!(last_model(&path), Some(("claude-haiku-5-5".to_owned(), at)));
        // Lejos del final (más de lo que se mira de primeras): se va más atrás.
        let filler =
            line(serde_json::json!({"type":"api-request-shape","shape":"x".repeat(600 * 1024)}));
        std::fs::write(&path, text.clone() + &filler + &filler).unwrap();
        assert_eq!(last_model(&path), Some(("claude-haiku-5-5".to_owned(), at)));
        std::fs::write(&path, &filler).unwrap();
        assert_eq!(last_model(&path), None);
        assert_eq!(last_model(&dir.path().join("no")), None);
    }

    #[test]
    fn the_models_of_what_is_new() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        let reply = |model: &str| {
            format!(
                "{}\n",
                serde_json::json!({"type": "assistant", "message": {"model": model}})
            )
        };
        let a = reply("claude-haiku-5-5");
        let b = reply("claude-opus-5-5");
        std::fs::write(&path, format!("{a}{b}{}", &a[..10])).unwrap();
        let whole = (a.len() + b.len()) as u64;
        assert_eq!(whole_lines_end(&path, whole + 10), whole);
        assert_eq!(whole_lines_end(&path, 5), 0);
        // Desde el principio: el último, y hasta donde las líneas están enteras.
        assert_eq!(
            models_since(&path, 0),
            (Some(("claude-opus-5-5".to_owned(), a.len() as u64)), whole)
        );
        // Desde el final de las enteras: nada aún.
        assert_eq!(models_since(&path, whole), (None, whole));
        std::fs::write(&path, format!("{a}{b}{a}")).unwrap();
        assert_eq!(
            models_since(&path, whole),
            (
                Some(("claude-haiku-5-5".to_owned(), whole)),
                whole + a.len() as u64
            )
        );
    }

    #[test]
    fn builds_the_chat_and_joins_tool_results() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let mut f = std::fs::File::create(&path).unwrap();
        let lines = [
            serde_json::json!({"type":"user","uuid":"u1","timestamp":"t1","message":{"content":"hola <system-reminder>interno</system-reminder>"}}),
            serde_json::json!({"type":"assistant","uuid":"a1","message":{"content":[
                {"type":"thinking","thinking":"..."},
                {"type":"text","text":"Voy a mirar."},
                {"type":"tool_use","id":"t-1","name":"Bash","input":{"command":"ls -la\necho fin"}}]}}),
            serde_json::json!({"type":"user","uuid":"u2","message":{"content":[{"type":"tool_result","tool_use_id":"t-1","content":"total 0"}]}}),
            serde_json::json!({"type":"user","uuid":"u3","isMeta":true,"message":{"content":"mensaje interno"}}),
            serde_json::json!({"type":"assistant","uuid":"s1","isSidechain":true,"message":{"content":[{"type":"text","text":"subagente"}]}}),
            serde_json::json!({"type":"api-request","uuid":"x"}),
            serde_json::json!({"type":"user","uuid":"u4","message":{"content":[{"type":"text","text":"mira esto"},{"type":"image","source":{}}]}}),
            serde_json::json!({"type":"attachment","uuid":"q1","attachment":{"type":"queued_command","prompt":"y en móvil","origin":{"kind":"human"},"timestamp":"t9"}}),
            serde_json::json!({"type":"attachment","uuid":"q2","attachment":{"type":"queued_command","prompt":"automático","origin":{"kind":"hook"}}}),
        ];
        let mut starts = Vec::new();
        let mut pos = 0;
        for l in lines {
            let l = line(l);
            starts.push(pos as u64);
            pos += l.len();
            f.write_all(l.as_bytes()).unwrap();
        }
        // Una línea a medio escribir no se lee todavía.
        f.write_all(b"{\"type\":\"assistant\",\"uuid\":\"a2\"")
            .unwrap();
        drop(f);

        let c = read_chunk(&path, 0, None).unwrap();
        assert_eq!(c.items.len(), 5, "{:#?}", c.items);
        assert_eq!(
            c.items[4],
            ChatItem::User {
                id: "q1".into(),
                at: starts[7],
                ts: Some("t9".into()),
                text: "y en móvil".into(),
                images: 0
            }
        );
        assert_eq!(
            c.items[0],
            ChatItem::User {
                id: "u1".into(),
                at: 0,
                ts: Some("t1".into()),
                text: "hola".into(),
                images: 0
            }
        );
        assert!(matches!(&c.items[1], ChatItem::Assistant { text, .. } if text == "Voy a mirar."));
        // Cada mensaje, con dónde empieza su línea; los de una misma línea, con la misma.
        assert_eq!(
            c.items.iter().map(ChatItem::at).collect::<Vec<_>>(),
            [starts[0], starts[1], starts[1], starts[6], starts[7]]
        );
        match &c.items[2] {
            ChatItem::Tool {
                name,
                summary,
                result,
                ..
            } => {
                assert_eq!(name, "Bash");
                assert_eq!(summary, "ls -la");
                assert_eq!(result.as_deref(), Some("total 0"));
            }
            other => panic!("{other:?}"),
        }
        assert!(
            matches!(&c.items[3], ChatItem::User { images: 1, text, .. } if text == "mira esto")
        );
        assert!(c.results.is_empty());

        // Sigue donde lo dejó: se completa la línea a medias y llega el resultado de otra herramienta.
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        f.write_all(b",\"message\":{\"content\":[{\"type\":\"tool_use\",\"id\":\"t-2\",\"name\":\"Read\",\"input\":{\"file_path\":\"/a/b.rs\"}}]}}\n").unwrap();
        f.write_all(line(serde_json::json!({"type":"user","uuid":"u5","message":{"content":[{"type":"tool_result","tool_use_id":"t-9","content":[{"type":"text","text":"tarde"}],"is_error":true}]}})).as_bytes()).unwrap();
        drop(f);
        let next = read_chunk(&path, c.offset, None).unwrap();
        assert!(matches!(&next.items[0], ChatItem::Tool { summary, .. } if summary == "/a/b.rs"));
        assert_eq!(next.items[0].at(), c.offset);
        assert_eq!(
            next.results,
            [ToolResult {
                tool_id: "t-9".into(),
                result: "tarde".into(),
                is_error: true,
                answers: None,
            }]
        );
    }

    #[test]
    fn lists_conversations_newest_first() {
        let config = tempfile::tempdir().unwrap();
        let dir = project_dir(config.path(), Path::new("/home/ana/mi.proyecto"));
        assert!(dir.ends_with("projects/-home-ana-mi-proyecto"));
        std::fs::create_dir_all(dir.join("b")).unwrap();
        let old = line(
            serde_json::json!({"type":"user","uuid":"u1","message":{"content":"arregla el login"}}),
        ) + &line(serde_json::json!({"type":"ai-title","aiTitle":"Arreglar el login"}));
        std::fs::write(dir.join("a.jsonl"), old).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        let new = line(serde_json::json!({"type":"custom-title","customTitle":"viejo"}))
            + &line(serde_json::json!({"type":"user","uuid":"u1","message":{"content":"primero"}}))
            + &line(
                serde_json::json!({"type":"last-prompt","lastPrompt":"lo último\nque escribí"}),
            );
        std::fs::write(dir.join("b.jsonl"), new).unwrap();
        std::fs::write(
            dir.join("b").join("custom-title.json"),
            r#"{"customTitle":"monitor"}"#,
        )
        .unwrap();
        // Abierta y cerrada sin escribir: no sale.
        std::fs::write(
            dir.join("c.jsonl"),
            line(serde_json::json!({"type":"permission-mode"})),
        )
        .unwrap();
        let list = conversations(&dir, 10);
        assert_eq!(
            list.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
            ["b", "a"]
        );
        assert_eq!(list[0].title.as_deref(), Some("monitor"));
        assert_eq!(
            list[0].last_prompt.as_deref(),
            Some("lo último que escribí")
        );
        assert_eq!(list[1].title.as_deref(), Some("Arreglar el login"));
        assert_eq!(list[1].last_prompt.as_deref(), Some("arregla el login"));
        assert!(conversations(&config.path().join("no-existe"), 10).is_empty());
    }

    #[test]
    fn compaction_summary_is_a_folded_notice() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let summary = format!(
            "This session is being continued from a previous conversation…\n{}",
            "x".repeat(5000)
        );
        std::fs::write(
            &path,
            line(
                serde_json::json!({"type":"user","uuid":"u1","isCompactSummary":true,
            "isVisibleInTranscriptOnly":true,"message":{"content":summary}}),
            ),
        )
        .unwrap();
        let c = read_chunk(&path, 0, None).unwrap();
        assert_eq!(c.items.len(), 1);
        match &c.items[0] {
            ChatItem::Notice {
                status: Some(s),
                result: Some(r),
                ..
            } => {
                assert_eq!(s, "compact");
                assert!(r.starts_with("This session") && r.len() > 5000);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn usage_limit_is_a_notice() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let hit = serde_json::json!({"type":"assistant","uuid":"a1","error":"rate_limit","isApiErrorMessage":true,
            "message":{"model":"<synthetic>","content":[{"type":"text","text":"You've hit your weekly limit · resets Oct 19, 7am (Europe/Madrid)"}]}});
        std::fs::write(
            &path,
            line(hit) + &line(serde_json::json!({"type":"system","subtype":"x"})),
        )
        .unwrap();
        let c = read_chunk(&path, 0, None).unwrap();
        assert!(matches!(&c.items[0], ChatItem::Notice { status: Some(s), .. } if s == "limit"));
        assert!(stopped_by_limit(&path));
        // Al seguir la conversación, ya no.
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        f.write_all(
            line(serde_json::json!({"type":"user","uuid":"u1","message":{"content":"continúa"}}))
                .as_bytes(),
        )
        .unwrap();
        drop(f);
        assert!(!stopped_by_limit(&path));
    }

    #[test]
    fn questions_plans_and_answers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let long = "x".repeat(5000);
        let ask = serde_json::json!({"type":"assistant","uuid":"a1","message":{"content":[
            {"type":"tool_use","id":"q-1","name":"AskUserQuestion","input":{"questions":[
                {"question":"¿Color?","header":"Color","multiSelect":false,"options":[
                    {"label":"Rojo","description":long},{"label":"Verde","description":"El verde"}]}]}},
            {"type":"tool_use","id":"p-1","name":"ExitPlanMode","input":{"plan":"# Plan\n\n1. Uno"}}]}});
        std::fs::write(&path, line(ask)).unwrap();
        let c = read_chunk(&path, 0, None).unwrap();
        match &c.items[0] {
            ChatItem::Tool {
                ask: Some(a),
                result: None,
                ..
            } => {
                let q = &a["questions"][0];
                assert_eq!(q["question"], "¿Color?");
                assert_eq!(q["multiSelect"], false);
                assert_eq!(q["options"][1]["label"], "Verde");
                // Recortado el texto, no el JSON.
                assert!(q["options"][0]["description"].as_str().unwrap().len() < 1100);
            }
            other => panic!("{other:?}"),
        }
        assert!(
            matches!(&c.items[1], ChatItem::Tool { plan: Some(p), .. } if p.starts_with("# Plan"))
        );

        // La respuesta llega después: con lo contestado.
        let answer = serde_json::json!({"type":"user","uuid":"u1","message":{"content":[
            {"type":"tool_result","tool_use_id":"q-1","content":"Your questions have been answered"}]},
            "toolUseResult":{"questions":[],"answers":{"¿Color?":"Verde"}}});
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        f.write_all(line(answer).as_bytes()).unwrap();
        drop(f);
        let next = read_chunk(&path, c.offset, None).unwrap();
        assert_eq!(
            next.results[0].answers,
            Some(serde_json::json!({"¿Color?":"Verde"}))
        );
        let all = read_chunk(&path, 0, None).unwrap();
        assert!(
            matches!(&all.items[0], ChatItem::Tool { answers: Some(a), .. } if a["¿Color?"] == "Verde")
        );
    }

    #[test]
    fn limit_keeps_the_last_messages() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let body: String = (0..10)
            .map(|i| line(serde_json::json!({"type":"user","uuid":format!("u{i}"),"message":{"content":format!("m{i}")}})))
            .collect();
        std::fs::write(&path, body).unwrap();
        let c = read_chunk(&path, 0, Some(3)).unwrap();
        assert!(c.truncated);
        assert_eq!(c.items.len(), 3);
        assert!(matches!(&c.items[0], ChatItem::User { text, .. } if text == "m7"));
    }

    #[test]
    fn task_notifications_are_notices() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let text = "<task-notification>\n<task-id>b7</task-id>\n<status>completed</status>\n<summary>Background command \"Esperar CI\" completed (exit code 0)</summary>\n<result>todo verde</result>\n</task-notification>";
        std::fs::write(
            &path,
            line(serde_json::json!({"type":"user","uuid":"n1","timestamp":"t1","message":{"content":text}})),
        )
        .unwrap();
        let c = read_chunk(&path, 0, None).unwrap();
        assert_eq!(
            c.items,
            vec![ChatItem::Notice {
                id: "n1".into(),
                at: 0,
                ts: Some("t1".into()),
                text: "Background command \"Esperar CI\" completed (exit code 0)".into(),
                status: Some("completed".into()),
                result: Some("todo verde".into()),
            }]
        );
    }

    /// Una conversación de `n` turnos: tu mensaje, y la respuesta de Claude con un texto y una
    /// herramienta en la misma línea, el resultado en la siguiente y un texto final. Con líneas
    /// que no son mensajes por medio. Devuelve el texto y dónde empieza cada línea con mensajes.
    fn conversation(n: usize) -> (String, Vec<u64>) {
        let mut text = String::new();
        let mut starts = Vec::new();
        let mut push = |text: &mut String, v: Value, msg: bool| {
            if msg {
                starts.push(text.len() as u64);
            }
            *text += &line(v);
        };
        for i in 0..n {
            push(
                &mut text,
                serde_json::json!({"type":"user","uuid":format!("u{i}"),"message":{"content":format!("pregunta {i}")}}),
                true,
            );
            push(
                &mut text,
                serde_json::json!({"type":"api-request","x":"y".repeat(100)}),
                false,
            );
            push(
                &mut text,
                serde_json::json!({"type":"assistant","uuid":format!("a{i}"),"message":{"content":[
                    {"type":"text","text":format!("miro {i}")},
                    {"type":"tool_use","id":format!("t{i}"),"name":"Bash","input":{"command":"ls"}}]}}),
                true,
            );
            push(
                &mut text,
                serde_json::json!({"type":"user","uuid":format!("r{i}"),"message":{"content":[
                    {"type":"tool_result","tool_use_id":format!("t{i}"),"content":format!("salida {i}")}]}}),
                false,
            );
            push(
                &mut text,
                serde_json::json!({"type":"assistant","uuid":format!("f{i}"),"message":{"content":[{"type":"text","text":format!("listo {i}")}]}}),
                true,
            );
        }
        (text, starts)
    }

    fn ids(items: &[ChatItem]) -> Vec<String> {
        items
            .iter()
            .map(|i| match i {
                ChatItem::User { id, .. }
                | ChatItem::Assistant { id, .. }
                | ChatItem::Notice { id, .. }
                | ChatItem::Tool { id, .. } => id.clone(),
            })
            .collect()
    }

    #[test]
    fn a_window_around_a_message() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let (text, starts) = conversation(10);
        std::fs::write(&path, &text).unwrap();
        let all = read_chunk(&path, 0, None).unwrap().items;
        assert_eq!(all.len(), 40);
        // Alrededor de «pregunta 5» (la línea starts[15]): 2 antes y 3 desde ella.
        let w = read_around(&path, starts[15], 2, 3).unwrap();
        assert_eq!(ids(&w.items), ["a4", "a4:1", "f4", "u5", "a5", "a5:1"]);
        // «miro 4» y su herramienta van en la misma línea: entran las dos, y 3 desde `at` acaba
        // en una línea con dos (también las dos).
        assert_eq!(w.start, starts[13]);
        let end_of = |at: u64| at + text[at as usize..].find('\n').unwrap() as u64 + 1;
        assert_eq!(w.offset, end_of(starts[16]));
        assert!(w.more_before && w.more_after);
        // Las herramientas de la ventana, con su resultado aunque esté justo detrás.
        assert!(matches!(&w.items[5], ChatItem::Tool { result: Some(r), .. } if r == "salida 5"));
        assert!(matches!(&w.items[1], ChatItem::Tool { result: Some(r), .. } if r == "salida 4"));
        assert!(w.results.is_empty());
        // Cada mensaje igual que en el chat entero.
        let from = all.iter().position(|i| i.at() == starts[13]).unwrap();
        assert_eq!(w.items, all[from..from + 6]);
    }

    #[test]
    fn paging_up_and_down_covers_everything_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let (text, starts) = conversation(7);
        std::fs::write(&path, &text).unwrap();
        let all = read_chunk(&path, 0, None).unwrap().items;
        let first = read_around(&path, starts[10], 3, 3).unwrap();
        let mut got = first.items.clone();
        // Hacia arriba con `start`, hasta que no haya más.
        let mut w = first;
        while w.more_before {
            let start = w.start;
            w = read_around(&path, start, 4, 0).unwrap();
            assert!(w.items.iter().all(|i| i.at() < start));
            assert_eq!(w.offset, start);
            got.splice(0..0, w.items.clone());
        }
        assert_eq!(w.start, 0);
        // Hacia abajo con `offset`.
        let mut w = read_around(&path, starts[10], 3, 3).unwrap();
        while w.more_after {
            let offset = w.offset;
            w = read_around(&path, offset, 0, 5).unwrap();
            assert!(w.items.iter().all(|i| i.at() >= offset));
            assert_eq!(w.start, offset);
            got.extend(w.items.clone());
        }
        assert_eq!(ids(&got), ids(&all));
        assert_eq!(got, all);
        // Al final, `offset` es donde sigue el chat en vivo.
        assert_eq!(w.offset, text.len() as u64);
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        f.write_all(
            line(serde_json::json!({"type":"user","uuid":"nuevo","message":{"content":"otra"}}))
                .as_bytes(),
        )
        .unwrap();
        drop(f);
        let live = read_chunk(&path, w.offset, Some(300)).unwrap();
        assert_eq!(ids(&live.items), ["nuevo"]);
    }

    #[test]
    fn around_the_ends_and_odd_points() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let (text, starts) = conversation(3);
        // Y una línea a medio escribir, que no cuenta.
        std::fs::write(&path, text.clone() + "{\"type\":\"user\",\"uuid\":\"x\"").unwrap();
        let all = read_chunk(&path, 0, None).unwrap().items;
        let end = text.len() as u64;
        // Al principio: nada antes.
        let w = read_around(&path, 0, 5, 2).unwrap();
        assert_eq!(ids(&w.items), ["u0", "a0", "a0:1"]);
        assert!(!w.more_before && w.more_after);
        assert_eq!(w.start, 0);
        // Al final, o más allá: los últimos, nada después; `offset`, las líneas enteras.
        for at in [end, end + 5, u64::MAX] {
            let w = read_around(&path, at, 2, 30).unwrap();
            assert_eq!(ids(&w.items), ["a2", "a2:1", "f2"], "{at}");
            assert!(w.more_before && !w.more_after);
            assert_eq!(w.offset, end);
        }
        // Todo de una vez.
        let w = read_around(&path, starts[4], 100, 100).unwrap();
        assert_eq!(w.items, all);
        assert!(!w.more_before && !w.more_after);
        // A media línea: como el principio de la siguiente.
        let w = read_around(&path, starts[4] + 3, 0, 1).unwrap();
        assert_eq!(ids(&w.items), ["f1"]);
        // Nada a ningún lado: vacío, pero dice si hay.
        let w = read_around(&path, starts[4], 0, 0).unwrap();
        assert!(w.items.is_empty() && w.more_before && w.more_after);
        assert_eq!((w.start, w.offset), (starts[4], starts[4]));
        // Un fichero vacío, o sin mensajes.
        std::fs::write(&path, line(serde_json::json!({"type":"permission-mode"}))).unwrap();
        let w = read_around(&path, 0, 30, 30).unwrap();
        assert!(w.items.is_empty() && !w.more_before && !w.more_after);
    }

    #[test]
    fn around_brings_results_of_earlier_tools() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let (text, starts) = conversation(2);
        std::fs::write(&path, &text).unwrap();
        // Desde «listo 0»: el resultado de la herramienta de antes viene en `results`.
        let res = starts[1] as usize + text[starts[1] as usize..].find('\n').unwrap() + 1;
        let w = read_around(&path, res as u64, 0, 2).unwrap();
        assert_eq!(ids(&w.items), ["f0", "u1"]);
        assert_eq!(w.results.len(), 1);
        assert_eq!(w.results[0].tool_id, "t0");
        assert_eq!(w.results[0].result, "salida 0");
    }

    /// Lo que se busca es justo lo que el chat enseña como tuyo o de Claude, con el mismo `at` e
    /// `id`, salvo lo que pone Claude Code en nombre de Claude.
    #[test]
    fn said_is_what_the_chat_shows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let lines = [
            serde_json::json!({"type":"user","uuid":"u1","timestamp":"2026-11-12T10:00:00.000Z","message":{"content":"Hola <system-reminder>interno</system-reminder>"}}),
            serde_json::json!({"type":"assistant","uuid":"a1","timestamp":"t","message":{"model":"claude-opus-5-5","content":[
                {"type":"thinking","thinking":"pienso"},
                {"type":"text","text":"  Voy a mirar.  "},
                {"type":"tool_use","id":"t-1","name":"Bash","input":{"command":"ls","text":"no"}},
                {"type":"text","text":"Y esto también."}]}}),
            serde_json::json!({"type":"user","uuid":"u2","message":{"content":[{"type":"tool_result","tool_use_id":"t-1","content":[{"type":"text","text":"salida"}]}]}}),
            serde_json::json!({"type":"user","uuid":"u3","isMeta":true,"message":{"content":"interno"}}),
            serde_json::json!({"type":"user","uuid":"u3b","isMeta":true,"message":{"content":[{"type":"text","text":"interno"}]}}),
            serde_json::json!({"type":"assistant","uuid":"s1","isSidechain":true,"message":{"content":[{"type":"text","text":"subagente"}]}}),
            serde_json::json!({"type":"user","uuid":"s2","isSidechain":true,"message":{"content":"al subagente"}}),
            serde_json::json!({"type":"user","uuid":"c1","message":{"content":"<command-name>/model</command-name>"}}),
            serde_json::json!({"type":"user","uuid":"c2","message":{"content":"<local-command-stdout>ok</local-command-stdout>"}}),
            serde_json::json!({"type":"user","uuid":"c3","message":{"content":[{"type":"text","text":"[Request interrupted by user]"}]}}),
            serde_json::json!({"type":"user","uuid":"n1","message":{"content":"<task-notification><status>completed</status><summary>hecho</summary></task-notification>"}}),
            serde_json::json!({"type":"user","uuid":"k1","isCompactSummary":true,"message":{"content":"resumen"}}),
            serde_json::json!({"type":"assistant","uuid":"l1","error":"rate_limit","isApiErrorMessage":true,"message":{"model":"<synthetic>","content":[{"type":"text","text":"You've hit your limit"}]}}),
            serde_json::json!({"type":"assistant","uuid":"x1","message":{"model":"<synthetic>","content":[{"type":"text","text":"No response requested."}]}}),
            serde_json::json!({"type":"assistant","uuid":"x2","error":"authentication_failed","isApiErrorMessage":true,"message":{"model":"<synthetic>","content":[{"type":"text","text":"Not logged in"}]}}),
            serde_json::json!({"type":"user","uuid":"i1","message":{"content":[{"type":"text","text":"mira esto"},{"type":"image","source":{}}]}}),
            serde_json::json!({"type":"user","uuid":"i2","message":{"content":[{"type":"image","source":{}}]}}),
            serde_json::json!({"type":"attachment","uuid":"q1","attachment":{"type":"queued_command","prompt":"y en móvil","origin":{"kind":"human"},"timestamp":"t9"}}),
            serde_json::json!({"type":"attachment","uuid":"q2","attachment":{"type":"queued_command","prompt":"automático","origin":{"kind":"hook"}}}),
            serde_json::json!({"type":"api-request","uuid":"r","message":{"content":"no es un mensaje"}}),
            serde_json::json!({"type":"user","uuid":"e1","message":{"content":"Código \u{1F600} ñ \"comillas\""}}),
        ];
        let mut text = String::new();
        let mut said_all = Vec::new();
        for l in lines {
            let l = line(l);
            said_all.extend(said(l.trim_end().as_bytes(), text.len() as u64));
            text += &l;
        }
        // Lo que no se entiende, fuera.
        assert!(said(b"{no es json", 0).is_empty());
        std::fs::write(&path, &text).unwrap();
        let chat: Vec<Said> = read_chunk(&path, 0, None)
            .unwrap()
            .items
            .into_iter()
            .filter_map(|i| match i {
                ChatItem::User {
                    id, at, ts, text, ..
                } if !text.is_empty() => Some(Said {
                    at,
                    id,
                    ts,
                    user: true,
                    text,
                }),
                ChatItem::Assistant { id, at, ts, text } => Some(Said {
                    at,
                    id,
                    ts,
                    user: false,
                    text,
                }),
                _ => None,
            })
            .filter(|s| !matches!(s.id.as_str(), "x1" | "x2"))
            .collect();
        assert_eq!(said_all, chat);
        assert_eq!(
            said_all.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
            ["u1", "a1:1", "a1:3", "i1", "q1", "e1"]
        );
        assert_eq!(said_all[0].text, "Hola");
        assert_eq!(said_all[0].ts.as_deref(), Some("2026-11-12T10:00:00.000Z"));
        assert_eq!(said_all[1].text, "Voy a mirar.");
        assert_eq!(said_all[5].text, "Código \u{1F600} ñ \"comillas\"");
    }

    #[test]
    fn clips_on_char_boundaries() {
        let s = "ñ".repeat(10);
        assert_eq!(clip(&s, 5), "ññ…");
    }
}

#[cfg(test)]
mod real {
    /// `NIDO_TRANSCRIPT=<ruta> cargo test --release real_transcript -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn real_transcript() {
        let Ok(p) = std::env::var("NIDO_TRANSCRIPT") else {
            return;
        };
        let t = std::time::Instant::now();
        let c = super::read_chunk(std::path::Path::new(&p), 0, Some(300)).unwrap();
        let json = serde_json::to_string(&c).unwrap();
        eprintln!(
            "{:?}: {} mensajes (recortado={}), {} KB de JSON, offset {}",
            t.elapsed(),
            c.items.len(),
            c.truncated,
            json.len() / 1024,
            c.offset
        );
        let all = super::read_chunk(std::path::Path::new(&p), 0, None).unwrap();
        let (mut u, mut a, mut tl) = (0, 0, 0);
        for i in &all.items {
            match i {
                super::ChatItem::User { .. } => u += 1,
                super::ChatItem::Assistant { .. } => a += 1,
                super::ChatItem::Tool { .. } => tl += 1,
                super::ChatItem::Notice { .. } => {}
            }
        }
        for i in &all.items {
            if let super::ChatItem::User { text, images, .. } = i {
                let head: String = text.chars().take(60).collect();
                eprintln!(
                    "  · {}{}",
                    head.replace('\n', " "),
                    if *images > 0 { " [imagen]" } else { "" }
                );
            }
        }
        eprintln!(
            "todo: {u} tuyos, {a} de Claude, {tl} herramientas; {} KB",
            serde_json::to_string(&all).unwrap().len() / 1024
        );
    }
}
