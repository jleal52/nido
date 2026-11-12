//! Buscar en las conversaciones con Claude de las sesiones (`GET /api/v1/search`).
//!
//! Por cada sesión, un índice en memoria con el texto de lo que se ve en su vista chat como
//! mensaje tuyo o de Claude (`transcript::said`), de su conversación actual. Se hace la primera
//! vez que se busca y después solo se lee lo nuevo de la transcripción; si la conversación
//! cambia (`/clear`, otra sesión de Claude) o el fichero se rehace, se empieza de cero.
//!
//! Mayúsculas y acentos no cuentan: se compara el texto normalizado (minúsculas y NFD sin las
//! marcas combinantes), carácter a carácter, para poder volver a las posiciones del original.

use std::collections::{HashMap, VecDeque};
use std::io::{self, BufRead, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use serde::Serialize;
use unicode_normalization::char::{decompose_canonical, is_combining_mark};

use crate::transcript;

/// Lo mínimo que hay que escribir para buscar (en caracteres, sin los espacios de los lados).
pub const MIN_CHARS: usize = 2;
/// Cuántos resultados, por defecto y como mucho.
pub const DEFAULT_LIMIT: usize = 50;
pub const MAX_LIMIT: usize = 200;
/// Texto (original, en bytes) que se guarda de cada conversación: lo último. Con el normalizado,
/// el índice ocupa algo más del doble.
pub const MAX_TEXT: usize = 8 * 1024 * 1024;
/// El trozo de mensaje que se devuelve, en caracteres, y cuántos van antes de la coincidencia.
const SNIPPET: usize = 160;
const CONTEXT: usize = 50;
/// Cuántas transcripciones se leen a la vez al hacer los índices.
const WORKERS: usize = 4;

fn norm_char(c: char, out: &mut String) {
    if c.is_ascii() {
        out.push(c.to_ascii_lowercase());
        return;
    }
    for l in c.to_lowercase() {
        decompose_canonical(l, |d| {
            if !is_combining_mark(d) {
                // La sigma final, como la otra.
                out.push(if d == 'ς' { 'σ' } else { d });
            }
        });
    }
}

/// En minúsculas y sin acentos ni otras marcas combinantes («Código» → «codigo»).
pub fn normalize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        norm_char(c, &mut out);
    }
    out
}

/// Las palabras de la búsqueda, normalizadas y sin repetir; `None` si es demasiado corta (menos
/// de `MIN_CHARS` caracteres sin los espacios de los lados) o no queda nada que buscar.
pub fn terms(q: &str) -> Option<Vec<String>> {
    if q.trim().chars().count() < MIN_CHARS {
        return None;
    }
    let mut terms: Vec<String> = q
        .split_whitespace()
        .map(normalize)
        .filter(|t| !t.is_empty())
        .collect();
    terms.sort();
    terms.dedup();
    (!terms.is_empty()).then_some(terms)
}

/// Si están todas las palabras en un texto ya normalizado.
fn matches(norm: &str, terms: &[String]) -> bool {
    terms.iter().all(|t| norm.contains(t.as_str()))
}

/// Si están todas las palabras en algo de lo que se dice (sin normalizar).
pub fn matches_any(fields: &[&str], terms: &[String]) -> bool {
    let all = normalize(&fields.join("\n"));
    matches(&all, terms)
}

/// Dónde está cada palabra en `text`, en caracteres (Unicode, no UTF-16) de `text`: `[inicio,
/// fin)`, ordenado y juntando lo que se solapa.
pub fn hits(text: &str, terms: &[String]) -> Vec<[usize; 2]> {
    // El texto normalizado y, por cada byte suyo, de qué carácter del original sale.
    let mut norm = String::with_capacity(text.len());
    let mut owner: Vec<u32> = Vec::with_capacity(text.len());
    for (i, c) in text.chars().enumerate() {
        norm_char(c, &mut norm);
        owner.resize(norm.len(), i as u32);
    }
    let mut out: Vec<[usize; 2]> = Vec::new();
    for t in terms {
        for (b, m) in norm.match_indices(t.as_str()) {
            let start = owner[b] as usize;
            let end = owner[b + m.len() - 1] as usize + 1;
            out.push([start, end]);
        }
    }
    out.sort_unstable();
    let mut merged: Vec<[usize; 2]> = Vec::with_capacity(out.len());
    for h in out {
        match merged.last_mut() {
            Some(last) if h[0] <= last[1] => last[1] = last[1].max(h[1]),
            _ => merged.push(h),
        }
    }
    merged
}

/// Unos `SNIPPET` caracteres de `text` alrededor de la primera coincidencia, con «…» donde se
/// corta y los saltos de línea como espacios, y las coincidencias dentro de ese trozo.
pub fn snippet(text: &str, hits: &[[usize; 2]]) -> (String, Vec<[usize; 2]>) {
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    let (mut start, mut end) = (0, n);
    if n > SNIPPET {
        let first = hits.first().map_or(0, |h| h[0]);
        start = first.saturating_sub(CONTEXT);
        end = (start + SNIPPET).min(n);
        start = end - SNIPPET;
    }
    let lead = usize::from(start > 0);
    let mut out = String::with_capacity((end - start) * 2 + 6);
    if start > 0 {
        out.push('…');
    }
    for &c in &chars[start..end] {
        out.push(if matches!(c, '\n' | '\r' | '\t') {
            ' '
        } else {
            c
        });
    }
    if end < n {
        out.push('…');
    }
    let inside = hits
        .iter()
        .filter(|h| h[1] > start && h[0] < end)
        .map(|h| [h[0].max(start) - start + lead, h[1].min(end) - start + lead])
        .collect();
    (out, inside)
}

/// `2026-11-12T10:20:30.123Z` (o con `+01:00`) → ms desde 1970; lo que no se entienda, 0.
pub fn ts_ms(s: &str) -> u64 {
    fn num(s: &str, from: usize, len: usize) -> Option<i64> {
        let d = s.get(from..from + len)?;
        d.bytes()
            .all(|b| b.is_ascii_digit())
            .then(|| d.parse().ok())?
    }
    let parse = || -> Option<u64> {
        let b = s.as_bytes();
        if b.len() < 19 || b[4] != b'-' || b[7] != b'-' || !matches!(b[10], b'T' | b' ') {
            return None;
        }
        let (y, mo, d) = (num(s, 0, 4)?, num(s, 5, 2)?, num(s, 8, 2)?);
        let (h, mi, sec) = (num(s, 11, 2)?, num(s, 14, 2)?, num(s, 17, 2)?);
        let mut i = 19;
        let mut ms = 0;
        if b.get(i) == Some(&b'.') {
            i += 1;
            let digits = b[i..].iter().take_while(|c| c.is_ascii_digit()).count();
            let frac = &s[i..i + digits];
            ms = format!("{frac:0<3}")[..3].parse().ok()?;
            i += digits;
        }
        let offset = match b.get(i) {
            None | Some(b'Z' | b'z') => 0,
            Some(sign @ (b'+' | b'-')) => {
                let m = num(s, i + 1, 2)? * 60 + num(s, i + 4, 2)?;
                if *sign == b'+' { m } else { -m }
            }
            _ => return None,
        };
        // Días desde 1970 (el algoritmo de Howard Hinnant, `days_from_civil`).
        let y = if mo <= 2 { y - 1 } else { y };
        let era = y.div_euclid(400);
        let yoe = y - era * 400;
        let mp = (mo + 9) % 12;
        let doy = (153 * mp + 2) / 5 + d - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        let days = era * 146_097 + doe - 719_468;
        let secs = days * 86_400 + h * 3600 + mi * 60 + sec - offset * 60;
        u64::try_from(secs * 1000 + ms).ok()
    };
    parse().unwrap_or(0)
}

/// Un mensaje en el índice.
struct Doc {
    /// Número de orden en el índice (no se repite aunque se empiece de cero).
    seq: u64,
    at: u64,
    ts: u64,
    user: bool,
    id: String,
    text: String,
    norm: String,
}

/// El índice de la conversación de una sesión.
#[derive(Default)]
pub struct Index {
    session_id: String,
    path: Option<PathBuf>,
    /// Hasta dónde se ha leído: el principio de la primera línea que aún no estaba entera.
    pos: u64,
    docs: VecDeque<Doc>,
    /// El texto original de `docs`, en bytes.
    bytes: usize,
    /// Se quitaron mensajes antiguos para no pasar de `MAX_TEXT`.
    truncated: bool,
    seq: u64,
    max_text: usize,
}

impl Index {
    fn new(session_id: &str, max_text: usize) -> Self {
        Index {
            session_id: session_id.to_owned(),
            max_text,
            ..Default::default()
        }
    }

    fn reset(&mut self) {
        self.docs.clear();
        self.bytes = 0;
        self.pos = 0;
        self.truncated = false;
    }

    /// Pone el índice al día con lo nuevo de la transcripción `path`. E/S síncrona.
    fn refresh(&mut self, path: &Path) -> io::Result<()> {
        if self.path.as_deref() != Some(path) {
            self.reset();
            self.path = Some(path.to_owned());
        }
        let mut f = std::fs::File::open(path)?;
        let len = f.metadata()?.len();
        // Más corta que lo leído, o lo leído ya no acaba en una línea: se rehízo.
        if len < self.pos || (self.pos > 0 && !ends_line(&mut f, self.pos)?) {
            self.reset();
        }
        if len == self.pos {
            return Ok(());
        }
        f.seek(SeekFrom::Start(self.pos))?;
        let mut r = io::BufReader::with_capacity(1 << 20, f.take(len - self.pos));
        let mut line = Vec::new();
        loop {
            line.clear();
            let n = r.read_until(b'\n', &mut line)?;
            if n == 0 || line.last() != Some(&b'\n') {
                break; // a medio escribir: la próxima vez
            }
            for m in transcript::said(&line[..n - 1], self.pos) {
                self.push(m);
            }
            self.pos += n as u64;
        }
        Ok(())
    }

    fn push(&mut self, m: transcript::Said) {
        self.bytes += m.text.len();
        self.docs.push_back(Doc {
            seq: self.seq,
            at: m.at,
            ts: m.ts.as_deref().map_or(0, ts_ms),
            user: m.user,
            norm: normalize(&m.text),
            id: m.id,
            text: m.text,
        });
        self.seq += 1;
        while self.bytes > self.max_text
            && let Some(old) = self.docs.pop_front()
        {
            self.bytes -= old.text.len();
            self.truncated = true;
        }
    }

    /// Los mensajes más recientes con todas las palabras (como mucho `limit`): `(ts, at, seq)`.
    fn find(&self, terms: &[String], limit: usize) -> Vec<(u64, u64, u64)> {
        self.docs
            .iter()
            .rev()
            .filter(|d| matches(&d.norm, terms))
            .take(limit)
            .map(|d| (d.ts, d.at, d.seq))
            .collect()
    }
}

/// Si lo que hay justo antes de `pos` es un fin de línea.
fn ends_line(f: &mut std::fs::File, pos: u64) -> io::Result<bool> {
    let mut b = [0u8];
    f.seek(SeekFrom::Start(pos - 1))?;
    f.read_exact(&mut b)?;
    Ok(b[0] == b'\n')
}

/// Una sesión donde buscar: su id, su conversación y dónde guarda Claude las transcripciones.
pub struct Target {
    pub id: String,
    pub session_id: String,
    pub config_dir: PathBuf,
}

/// Un mensaje encontrado.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Found {
    /// La sesión de nido.
    pub session: String,
    /// La conversación de Claude.
    pub sid: String,
    /// El `at` del mensaje en la vista chat (para `chat?around=`).
    pub at: u64,
    /// El `id` del mensaje en la vista chat.
    pub id: String,
    /// Cuándo (ms desde 1970; 0 si no se sabe).
    pub ts: u64,
    pub role: &'static str,
    /// El trozo del mensaje.
    pub text: String,
    /// Las coincidencias en `text`, en caracteres Unicode: `[inicio, fin)`.
    pub hits: Vec<[usize; 2]>,
}

/// Los índices de todas las sesiones.
pub struct Indexes {
    all: Mutex<HashMap<String, Arc<Mutex<Index>>>>,
    max_text: usize,
}

impl Default for Indexes {
    fn default() -> Self {
        Self::with_max_text(MAX_TEXT)
    }
}

impl Indexes {
    /// Con otro tope de texto por conversación (para las pruebas).
    pub fn with_max_text(max_text: usize) -> Self {
        Indexes {
            all: Mutex::new(HashMap::new()),
            max_text,
        }
    }

    /// Busca `terms` en las conversaciones de `targets`, poniendo antes al día sus índices.
    /// Devuelve lo más reciente primero (como mucho `limit`) y si alguna conversación tenía
    /// más texto del que se guarda (lo más antiguo no se ha buscado). E/S síncrona.
    pub fn search(
        &self,
        targets: Vec<Target>,
        terms: &[String],
        limit: usize,
    ) -> (Vec<Found>, bool) {
        let entries: Vec<(Target, Arc<Mutex<Index>>)> = {
            let mut all = self.all.lock().expect("índices");
            // Los de las sesiones que ya no están, fuera.
            all.retain(|id, _| targets.iter().any(|t| &t.id == id));
            targets
                .into_iter()
                .map(|t| {
                    let e = all.entry(t.id.clone()).or_default().clone();
                    (t, e)
                })
                .collect()
        };

        // Al día y lo que casa en cada una, varias a la vez.
        let next = AtomicUsize::new(0);
        let found = Mutex::new(Vec::<(u64, u64, u64, usize)>::new());
        let truncated = std::sync::atomic::AtomicBool::new(false);
        let workers = std::thread::available_parallelism()
            .map_or(2, |n| n.get())
            .min(WORKERS)
            .min(entries.len().max(1));
        std::thread::scope(|s| {
            for _ in 0..workers {
                s.spawn(|| {
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        let Some((t, e)) = entries.get(i) else {
                            break;
                        };
                        let mut idx = e.lock().expect("índice");
                        if idx.session_id != t.session_id || idx.max_text != self.max_text {
                            *idx = Index::new(&t.session_id, self.max_text);
                        }
                        let path = match idx.path.clone().filter(|p| p.is_file()) {
                            Some(p) => Some(p),
                            None => transcript::find(&t.config_dir, &t.session_id),
                        };
                        let Some(path) = path else {
                            continue; // aún no ha escrito nada
                        };
                        if let Err(e) = idx.refresh(&path) {
                            eprintln!("nido: no se pudo leer {}: {e}", path.display());
                            continue;
                        }
                        if idx.truncated {
                            truncated.store(true, Ordering::Relaxed);
                        }
                        let hits = idx.find(terms, limit);
                        drop(idx);
                        found
                            .lock()
                            .expect("encontrados")
                            .extend(hits.into_iter().map(|(ts, at, seq)| (ts, at, seq, i)));
                    }
                });
            }
        });

        // Lo más reciente de todas, y de eso el trozo con las coincidencias.
        let mut found = found.into_inner().expect("encontrados");
        found.sort_unstable_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)).then(a.3.cmp(&b.3)));
        found.truncate(limit);
        let mut out = Vec::with_capacity(found.len());
        for (_, _, seq, i) in found {
            let (t, e) = &entries[i];
            let idx = e.lock().expect("índice");
            let Ok(n) = idx.docs.binary_search_by_key(&seq, |d| d.seq) else {
                continue; // se fue entretanto
            };
            let d = &idx.docs[n];
            let (text, hits) = snippet(&d.text, &hits(&d.text, terms));
            out.push(Found {
                session: t.id.clone(),
                sid: idx.session_id.clone(),
                at: d.at,
                id: d.id.clone(),
                ts: d.ts,
                role: if d.user { "user" } else { "assistant" },
                text,
                hits,
            });
        }
        (out, truncated.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Write;

    fn t(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| w.to_string()).collect()
    }

    #[test]
    fn normalizes_case_and_accents() {
        assert_eq!(normalize("Código ÁRBOL Ñandú"), "codigo arbol nandu");
        // Ya descompuesto (la tilde aparte): igual.
        assert_eq!(normalize("Co\u{301}digo"), "codigo");
        assert_eq!(normalize("ΣΊΣΥΦΟΣ σοφός"), "σισυφοσ σοφοσ");
        assert_eq!(normalize("Ça ü Ø"), "ca u ø");
        // Lo que no tiene mayúsculas ni acentos, tal cual.
        assert_eq!(normalize("🙂 日本 x"), "🙂 日本 x");
    }

    #[test]
    fn terms_of_a_query() {
        assert_eq!(terms("a"), None);
        assert_eq!(terms("  É  "), None);
        assert_eq!(terms(""), None);
        // Dos marcas combinantes solas: nada que buscar.
        assert_eq!(terms("\u{301}\u{301}"), None);
        assert_eq!(terms("ab"), Some(t(&["ab"])));
        assert_eq!(terms(" Código  ÉL el "), Some(t(&["codigo", "el"])));
        assert_eq!(terms("a b"), Some(t(&["a", "b"])));
    }

    #[test]
    fn hits_are_characters_of_the_original() {
        let q = |q: &str| terms(q).unwrap();
        assert_eq!(
            hits("El Código y el CODIGO", &q("codigo")),
            [[3, 9], [15, 21]]
        );
        // Varias palabras: todas, y lo que se solapa, junto.
        assert_eq!(hits("abcd xbc", &q("ab bc")), [[0, 3], [6, 8]]);
        assert_eq!(
            hits("Ñandú y ñandu", &q("NANDU y")),
            [[0, 5], [6, 7], [8, 13]]
        );
        // Emojis y otros caracteres de varios bytes: se cuentan como uno.
        assert_eq!(hits("🙂🙂 café 🙂 cafe", &q("cafe")), [[3, 7], [10, 14]]);
        // Texto ya descompuesto: la tilde aparte va con su letra.
        assert_eq!(hits("co\u{301}digo!", &q("código")), [[0, 7]]);
        // Un carácter que se descompone en varios (hangul): entero.
        assert_eq!(hits("x 한국 y", &q("한국")), [[2, 4]]);
        assert_eq!(hits("x 한국 y", &q("x 국")), [[0, 1], [3, 4]]);
        assert!(hits("nada", &q("otra")).is_empty());
    }

    #[test]
    fn snippets_around_the_first_hit() {
        // Corto: entero, con los saltos de línea como espacios.
        let (s, h) = snippet("hola\nmundo", &[[5, 10]]);
        assert_eq!((s.as_str(), h), ("hola mundo", vec![[5, 10]]));
        // Largo: «…» delante (que corre las posiciones), unos 160 alrededor y «…» detrás.
        let text = format!("{}clave{}", "á".repeat(300), "b".repeat(300));
        let (s, h) = snippet(&text, &[[300, 305], [600, 601]]);
        assert_eq!(s.chars().count(), 162);
        assert!(s.starts_with('…') && s.ends_with('…'));
        assert_eq!(h, [[51, 56]]);
        let c: Vec<char> = s.chars().collect();
        assert_eq!(c[51..56].iter().collect::<String>(), "clave");
        // Al final: los últimos 160, sin «…» detrás.
        let text = format!("{}fin", "x".repeat(400));
        let (s, h) = snippet(&text, &[[400, 403]]);
        assert_eq!(s.chars().count(), 161);
        assert!(s.starts_with('…') && s.ends_with("fin"));
        assert_eq!(h, [[158, 161]]);
        // Al principio: sin «…» delante.
        let text = format!("inicio{}", "x".repeat(400));
        let (s, h) = snippet(&text, &[[0, 6]]);
        assert!(s.starts_with("inicio") && s.ends_with('…'));
        assert_eq!(h, [[0, 6]]);
    }

    #[test]
    fn timestamps() {
        assert_eq!(ts_ms("1970-01-01T00:00:00.000Z"), 0);
        assert_eq!(ts_ms("2026-11-12T10:20:30.123Z"), 1_794_478_830_123);
        assert_eq!(ts_ms("2026-11-12T10:20:30Z"), 1_794_478_830_000);
        assert_eq!(ts_ms("2026-11-12T11:20:30.1+01:00"), 1_794_478_830_100);
        assert_eq!(ts_ms("2000-02-29T00:00:00Z"), 951_782_400_000);
        assert_eq!(ts_ms("ayer"), 0);
        assert_eq!(ts_ms("t1"), 0);
    }

    fn user(uuid: &str, text: &str, ts: &str) -> String {
        format!(
            "{}\n",
            json!({"type":"user","uuid":uuid,"timestamp":ts,"message":{"content":text}})
        )
    }

    fn reply(uuid: &str, text: &str, ts: &str) -> String {
        format!(
            "{}\n",
            json!({"type":"assistant","uuid":uuid,"timestamp":ts,"message":{"model":"claude-opus-5-5","content":[{"type":"text","text":text}]}})
        )
    }

    fn tool_output(text: &str) -> String {
        format!(
            "{}\n",
            json!({"type":"user","uuid":"r","message":{"content":[{"type":"tool_result","tool_use_id":"t","content":text}]}})
        )
    }

    /// Dos sesiones de Claude en `config`; `a` y `b` son sus transcripciones.
    struct Fixture {
        _dir: tempfile::TempDir,
        config: PathBuf,
        a: PathBuf,
        b: PathBuf,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("claude");
        let project = config.join("projects").join("-w");
        std::fs::create_dir_all(&project).unwrap();
        Fixture {
            a: project.join("conv-a.jsonl"),
            b: project.join("conv-b.jsonl"),
            config,
            _dir: dir,
        }
    }

    fn target(f: &Fixture, id: &str, sid: &str) -> Target {
        Target {
            id: id.into(),
            session_id: sid.into(),
            config_dir: f.config.clone(),
        }
    }

    fn append(path: &Path, text: &str) {
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        f.write_all(text.as_bytes()).unwrap();
    }

    fn texts(found: &[Found]) -> Vec<String> {
        found.iter().map(|f| f.text.clone()).collect()
    }

    #[test]
    fn finds_what_was_said_newest_first() {
        let f = fixture();
        std::fs::write(
            &f.a,
            user("u1", "Arregla el código del login", "2026-11-12T10:00:00Z")
                + &tool_output("codigo en la salida de una herramienta")
                + &reply("a1", "Ya está el CÓDIGO arreglado", "2026-11-12T10:05:00Z"),
        )
        .unwrap();
        std::fs::write(
            &f.b,
            reply("b1", "Otro codigo, en otra sesión", "2026-11-12T10:02:00Z"),
        )
        .unwrap();
        let idx = Indexes::default();
        let targets = || vec![target(&f, "s1", "conv-a"), target(&f, "s2", "conv-b")];
        let (found, truncated) = idx.search(targets(), &terms("codigo").unwrap(), 50);
        assert!(!truncated);
        assert_eq!(
            texts(&found),
            [
                "Ya está el CÓDIGO arreglado",
                "Otro codigo, en otra sesión",
                "Arregla el código del login"
            ]
        );
        let first = &found[0];
        assert_eq!(
            (
                first.session.as_str(),
                first.sid.as_str(),
                first.role,
                first.id.as_str()
            ),
            ("s1", "conv-a", "assistant", "a1")
        );
        assert_eq!(first.ts, ts_ms("2026-11-12T10:05:00Z"));
        assert_eq!(first.hits, [[11, 17]]);
        // `at`: donde empieza su línea (la de la respuesta, tras las dos primeras).
        let len = |p: &Path| std::fs::metadata(p).unwrap().len();
        assert_eq!(
            first.at,
            len(&f.a)
                - reply("a1", "Ya está el CÓDIGO arreglado", "2026-11-12T10:05:00Z").len() as u64
        );
        assert_eq!(found[2].role, "user");
        assert_eq!(found[2].at, 0);
        // Varias palabras: todas en el mismo mensaje.
        let (found, _) = idx.search(targets(), &terms("login CODIGO").unwrap(), 50);
        assert_eq!(texts(&found), ["Arregla el código del login"]);
        assert_eq!(found[0].hits, [[11, 17], [22, 27]]);
        // `limit`: los más recientes.
        let (found, _) = idx.search(targets(), &terms("codigo").unwrap(), 2);
        assert_eq!(found.len(), 2);
        assert_eq!(found[1].session, "s2");
        // Lo de las herramientas no se busca.
        let (found, _) = idx.search(targets(), &terms("salida").unwrap(), 50);
        assert!(found.is_empty());
    }

    #[test]
    fn keeps_up_with_the_transcript() {
        let f = fixture();
        let idx = Indexes::default();
        let targets = || vec![target(&f, "s1", "conv-a")];
        let find = |q: &str| texts(&idx.search(targets(), &terms(q).unwrap(), 50).0);
        // Aún sin transcripción: nada (y no falla).
        assert!(find("hola").is_empty());
        std::fs::write(&f.a, user("u1", "hola uno", "2026-11-12T10:00:00Z")).unwrap();
        assert_eq!(find("hola"), ["hola uno"]);
        // Crece: solo se lee lo nuevo; lo que está a medio escribir, aún no.
        let next = reply("a1", "hola dos", "2026-11-12T10:01:00Z");
        append(&f.a, &next[..20]);
        assert_eq!(find("hola"), ["hola uno"]);
        append(&f.a, &next[20..]);
        assert_eq!(find("hola"), ["hola dos", "hola uno"]);
        assert_eq!(idx.all.lock().unwrap()["s1"].lock().unwrap().docs.len(), 2);
        // Se rehace la transcripción (más corta): desde el principio.
        std::fs::write(&f.a, user("u9", "hola rehecho", "2026-11-12T11:00:00Z")).unwrap();
        assert_eq!(find("hola"), ["hola rehecho"]);
        // Se rehace con algo igual de largo o más: también se nota (lo leído ya no acaba en una
        // línea).
        std::fs::write(
            &f.a,
            user(
                "u8",
                "hola otra vez, más largo que antes",
                "2026-11-12T12:00:00Z",
            ),
        )
        .unwrap();
        assert_eq!(find("hola"), ["hola otra vez, más largo que antes"]);
        // Otra conversación en la sesión (`/clear`): la nueva, y nada de la anterior.
        std::fs::write(&f.b, user("n1", "hola nueva", "2026-11-12T13:00:00Z")).unwrap();
        let (found, _) = idx.search(
            vec![target(&f, "s1", "conv-b")],
            &terms("hola").unwrap(),
            50,
        );
        assert_eq!(texts(&found), ["hola nueva"]);
        assert_eq!(found[0].sid, "conv-b");
        // Las sesiones que ya no están, fuera del índice.
        idx.search(vec![], &terms("hola").unwrap(), 50);
        assert!(idx.all.lock().unwrap().is_empty());
    }

    #[test]
    fn keeps_only_the_last_text() {
        let f = fixture();
        let idx = Indexes::with_max_text(100);
        let mut text = String::new();
        for i in 0..10 {
            text += &user(
                &format!("u{i}"),
                &format!("mensaje {i} {}", "x".repeat(20)),
                "",
            );
        }
        std::fs::write(&f.a, &text).unwrap();
        let (found, truncated) = idx.search(
            vec![target(&f, "s1", "conv-a")],
            &terms("mensaje").unwrap(),
            50,
        );
        assert!(truncated);
        // 30 bytes cada uno: caben 3, los últimos.
        let ids: Vec<&str> = found.iter().map(|f| f.id.as_str()).collect();
        assert_eq!(ids, ["u9", "u8", "u7"]);
        // Sin fecha: al final, y entre ellos, lo de más abajo primero.
        assert!(found.iter().all(|f| f.ts == 0));
    }

    #[test]
    fn many_sessions_at_once() {
        let f = fixture();
        let project = f.a.parent().unwrap().to_owned();
        let mut targets = Vec::new();
        for i in 0..12 {
            let sid = format!("conv-{i}");
            std::fs::write(
                project.join(format!("{sid}.jsonl")),
                user(
                    "u",
                    &format!("aguja {i}"),
                    &format!("2026-11-12T10:{i:02}:00Z"),
                ),
            )
            .unwrap();
            targets.push(target(&f, &format!("s{i}"), &sid));
        }
        let idx = Indexes::default();
        let (found, _) = idx.search(targets, &terms("aguja").unwrap(), 5);
        assert_eq!(
            texts(&found),
            ["aguja 11", "aguja 10", "aguja 9", "aguja 8", "aguja 7"]
        );
    }
}
