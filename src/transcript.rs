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
        ts: Option<String>,
        text: String,
        images: usize,
    },
    Assistant {
        id: String,
        ts: Option<String>,
        text: String,
    },
    /// Aviso de Claude Code dentro de la conversación (p. ej. «terminó la orden en segundo plano»):
    /// llega como un mensaje tuyo, pero no lo es.
    Notice {
        id: String,
        ts: Option<String>,
        text: String,
        /// `completed`, `failed`, `killed`…
        status: Option<String>,
        /// Lo que devolvió la tarea, si viene en el aviso.
        result: Option<String>,
    },
    Tool {
        id: String,
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
fn task_notice(text: &str, id: &str, ts: &Option<String>) -> Option<ChatItem> {
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

fn queued_prompt(d: &Value) -> Option<ChatItem> {
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
        ts: att
            .get("timestamp")
            .or_else(|| d.get("timestamp"))
            .and_then(Value::as_str)
            .map(str::to_owned),
        text,
        images,
    })
}

/// Convierte líneas de la transcripción en mensajes. `tools` acumula dónde está cada llamada
/// para enganchar su resultado, aunque llegue en otro trozo.
fn parse_lines<'a>(
    lines: impl Iterator<Item = &'a str>,
    items: &mut Vec<ChatItem>,
    results: &mut Vec<ToolResult>,
) {
    let mut tools: HashMap<String, usize> = HashMap::new();
    for line in lines {
        let Ok(d) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let kind = d.get("type").and_then(Value::as_str);
        // Lo que escribes mientras Claude trabaja llega como un adjunto «queued_command».
        if kind == Some("attachment") {
            if let Some(item) = queued_prompt(&d) {
                items.push(item);
            }
            continue;
        }
        if !matches!(kind, Some("user" | "assistant")) {
            continue;
        }
        if d.get("isSidechain").and_then(Value::as_bool) == Some(true) {
            continue;
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
        if kind == Some("user") && d.get("isCompactSummary").and_then(Value::as_bool) == Some(true) {
            let text = match content {
                Some(Value::String(s)) => s.clone(),
                Some(c) => result_text(c),
                None => String::new(),
            };
            items.push(ChatItem::Notice {
                id,
                ts,
                text: "Claude resumió la conversación para seguir (se le acababa el contexto)".into(),
                status: Some("compact".into()),
                result: Some(clip(text.trim(), MAX_SUMMARY)),
            });
            continue;
        }
        match (kind, content) {
            (Some("user"), Some(Value::String(s))) if !meta => {
                let text = strip_reminders(s);
                if let Some(notice) = task_notice(&text, &id, &ts) {
                    items.push(notice);
                } else if !text.is_empty()
                    && !text.starts_with("<local-command")
                    && !text.starts_with("<command-")
                    && !text.starts_with("[Request interrupted")
                {
                    items.push(ChatItem::User {
                        id,
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
                            let answers = d.pointer("/toolUseResult/answers").filter(|a| a.is_object()).cloned();
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
                let text = if text.starts_with("[Request interrupted") { String::new() } else { text };
                if let Some(notice) = task_notice(&text, &id, &ts) {
                    items.push(notice);
                } else if !text.is_empty() || images > 0 {
                    items.push(ChatItem::User {
                        id,
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
                                    ts: ts.clone(),
                                    text: text.to_owned(),
                                    status: Some("limit".into()),
                                    result: None,
                                });
                            } else {
                                items.push(ChatItem::Assistant {
                                    id: bid,
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
                            let ask = (name == "AskUserQuestion").then(|| ask_data(&input)).flatten();
                            let plan = (name == "ExitPlanMode")
                                .then(|| input.get("plan").and_then(Value::as_str).map(|p| clip(p, 30_000)))
                                .flatten();
                            items.push(ChatItem::Tool {
                                id: bid,
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
        if !matches!(d.get("type").and_then(Value::as_str), Some("user" | "assistant"))
            || d.get("isSidechain").and_then(Value::as_bool) == Some(true)
            || d.get("isMeta").and_then(Value::as_bool) == Some(true)
        {
            continue;
        }
        return is_limit(&d);
    }
    false
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
            Some(Conversation { id, title, last_prompt, modified, size })
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
                    if !t.is_empty() && !t.starts_with('<') && !t.starts_with("[Request interrupted") {
                        user = Some(t);
                    }
                }
            }
            _ => {}
        }
    }
    let one_line = |s: String| clip(s.split_whitespace().collect::<Vec<_>>().join(" ").as_str(), 200);
    (custom.or(ai), prompt.or(user).map(one_line), messages || start > 0)
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
    let text = String::from_utf8_lossy(&buf[..complete]);
    let mut chunk = Chunk {
        offset: offset + complete as u64,
        reset,
        ..Default::default()
    };
    parse_lines(text.lines(), &mut chunk.items, &mut chunk.results);
    // El límite vale al leer desde el principio (también si el fichero se rehízo).
    if let Some(limit) = limit.filter(|_| offset == 0)
        && chunk.items.len() > limit
    {
        chunk.items.drain(..chunk.items.len() - limit);
        chunk.truncated = true;
    }
    Ok(chunk)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn line(v: Value) -> String {
        format!("{}\n", v)
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
        for l in lines {
            f.write_all(line(l).as_bytes()).unwrap();
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
                ts: Some("t9".into()),
                text: "y en móvil".into(),
                images: 0
            }
        );
        assert_eq!(
            c.items[0],
            ChatItem::User {
                id: "u1".into(),
                ts: Some("t1".into()),
                text: "hola".into(),
                images: 0
            }
        );
        assert!(matches!(&c.items[1], ChatItem::Assistant { text, .. } if text == "Voy a mirar."));
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
        let old = line(serde_json::json!({"type":"user","uuid":"u1","message":{"content":"arregla el login"}}))
            + &line(serde_json::json!({"type":"ai-title","aiTitle":"Arreglar el login"}));
        std::fs::write(dir.join("a.jsonl"), old).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        let new = line(serde_json::json!({"type":"custom-title","customTitle":"viejo"}))
            + &line(serde_json::json!({"type":"user","uuid":"u1","message":{"content":"primero"}}))
            + &line(serde_json::json!({"type":"last-prompt","lastPrompt":"lo último\nque escribí"}));
        std::fs::write(dir.join("b.jsonl"), new).unwrap();
        std::fs::write(dir.join("b").join("custom-title.json"), r#"{"customTitle":"monitor"}"#).unwrap();
        // Abierta y cerrada sin escribir: no sale.
        std::fs::write(dir.join("c.jsonl"), line(serde_json::json!({"type":"permission-mode"}))).unwrap();
        let list = conversations(&dir, 10);
        assert_eq!(list.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(), ["b", "a"]);
        assert_eq!(list[0].title.as_deref(), Some("monitor"));
        assert_eq!(list[0].last_prompt.as_deref(), Some("lo último que escribí"));
        assert_eq!(list[1].title.as_deref(), Some("Arreglar el login"));
        assert_eq!(list[1].last_prompt.as_deref(), Some("arregla el login"));
        assert!(conversations(&config.path().join("no-existe"), 10).is_empty());
    }

    #[test]
    fn compaction_summary_is_a_folded_notice() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let summary = format!("This session is being continued from a previous conversation…\n{}", "x".repeat(5000));
        std::fs::write(&path, line(serde_json::json!({"type":"user","uuid":"u1","isCompactSummary":true,
            "isVisibleInTranscriptOnly":true,"message":{"content":summary}}))).unwrap();
        let c = read_chunk(&path, 0, None).unwrap();
        assert_eq!(c.items.len(), 1);
        match &c.items[0] {
            ChatItem::Notice { status: Some(s), result: Some(r), .. } => {
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
        std::fs::write(&path, line(hit) + &line(serde_json::json!({"type":"system","subtype":"x"}))).unwrap();
        let c = read_chunk(&path, 0, None).unwrap();
        assert!(matches!(&c.items[0], ChatItem::Notice { status: Some(s), .. } if s == "limit"));
        assert!(stopped_by_limit(&path));
        // Al seguir la conversación, ya no.
        let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(line(serde_json::json!({"type":"user","uuid":"u1","message":{"content":"continúa"}})).as_bytes()).unwrap();
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
            ChatItem::Tool { ask: Some(a), result: None, .. } => {
                let q = &a["questions"][0];
                assert_eq!(q["question"], "¿Color?");
                assert_eq!(q["multiSelect"], false);
                assert_eq!(q["options"][1]["label"], "Verde");
                // Recortado el texto, no el JSON.
                assert!(q["options"][0]["description"].as_str().unwrap().len() < 1100);
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(&c.items[1], ChatItem::Tool { plan: Some(p), .. } if p.starts_with("# Plan")));

        // La respuesta llega después: con lo contestado.
        let answer = serde_json::json!({"type":"user","uuid":"u1","message":{"content":[
            {"type":"tool_result","tool_use_id":"q-1","content":"Your questions have been answered"}]},
            "toolUseResult":{"questions":[],"answers":{"¿Color?":"Verde"}}});
        let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(line(answer).as_bytes()).unwrap();
        drop(f);
        let next = read_chunk(&path, c.offset, None).unwrap();
        assert_eq!(next.results[0].answers, Some(serde_json::json!({"¿Color?":"Verde"})));
        let all = read_chunk(&path, 0, None).unwrap();
        assert!(matches!(&all.items[0], ChatItem::Tool { answers: Some(a), .. } if a["¿Color?"] == "Verde"));
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
                ts: Some("t1".into()),
                text: "Background command \"Esperar CI\" completed (exit code 0)".into(),
                status: Some("completed".into()),
                result: Some("todo verde".into()),
            }]
        );
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
