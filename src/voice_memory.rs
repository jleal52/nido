//! La memoria de la voz: las notas que apunta el agente («recuerda que…»), el resumen (o la
//! bitácora, si el resumen no llega) de las últimas llamadas. Se guarda en `voice-memory.json`, en
//! la carpeta de estado, y no sale del equipo. (El handle para retomar una conversación con
//! Gemini no está aquí: lo guarda el hub.)
//!
//! El fichero es la memoria: cada operación lo lee, lo cambia y lo escribe entero (en un temporal
//! que se renombra), todo con el cerrojo tomado. Así, borrar el fichero la borra también con nido
//! en marcha. Uno que falte es una memoria vacía; uno que no se entienda se aparta como
//! `voice-memory.json.bad` (o `.bad.<ms>`, si ya hay uno) y la memoria empieza vacía; si no se
//! puede apartar, es un error, para que nada lo pise. Lo que se lee pasa por las mismas limpiezas
//! y límites que lo que llega por la API.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::{Mutex, PoisonError};

pub const MAX_NOTES: usize = 50;
pub const MAX_NOTE: usize = 300;
pub const MAX_CALLS: usize = 20;
pub const MAX_CALL_TEXT: usize = 4000;
pub const MAX_CALL_ID: usize = 64;
/// El id más alto de una nota: 2^53 − 1, el entero más grande que JavaScript y Dart (en la web)
/// guardan exacto.
pub const MAX_NOTE_ID: u64 = (1 << 53) - 1;

const FILE: &str = "voice-memory.json";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Note {
    pub id: u64,
    pub text: String,
    /// ms desde 1970.
    pub at: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CallKind {
    /// El que hace Gemini al colgar.
    #[serde(rename = "resumen")]
    Resumen,
    /// La de respaldo: lo que se dijo y se hizo, apuntado por la web o la app durante la llamada.
    #[serde(rename = "bitácora", alias = "bitacora")]
    Bitacora,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Call {
    pub id: String,
    /// ms desde 1970: la última vez que se guardó.
    pub at: u64,
    pub kind: CallKind,
    pub text: String,
}

/// Lo que se enseña de la memoria.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    pub notes: Vec<Note>,
    pub calls: Vec<Call>,
}

/// El fichero.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct Data {
    notes: Vec<Note>,
    calls: Vec<Call>,
    /// El id de la próxima nota: los de las borradas no se repiten.
    next_note: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemoryError {
    EmptyNote,
    /// Ya hay [`MAX_NOTES`] (la API lo da como 409).
    Full,
    EmptyText,
    BadCallId,
    Read(String),
    Save(String),
}

impl fmt::Display for MemoryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MemoryError::EmptyNote => f.write_str("la nota está vacía"),
            MemoryError::Full => write!(f, "hay {MAX_NOTES} notas: olvida alguna"),
            MemoryError::EmptyText => f.write_str("el texto está vacío"),
            MemoryError::BadCallId => write!(
                f,
                "id de llamada no válido: de 1 a {MAX_CALL_ID} caracteres entre A-Z, a-z, 0-9, '_' y '-'"
            ),
            MemoryError::Read(e) => write!(f, "no se pudo leer la memoria: {e}"),
            MemoryError::Save(e) => write!(f, "no se pudo guardar la memoria: {e}"),
        }
    }
}

impl std::error::Error for MemoryError {}

/// Que falte el fichero o su carpeta no es un error: la memoria está vacía.
fn missing(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
    )
}

/// Sin espacios a los lados y como mucho `max` caracteres (no bytes: no parte una letra).
fn clip(s: &str, max: usize) -> String {
    let s = s.trim();
    match s.char_indices().nth(max) {
        Some((at, _)) => s[..at].trim_end().to_string(),
        None => s.to_string(),
    }
}

/// Los últimos `max` caracteres de `s`, desde el principio de una línea: si el corte cae a
/// media línea, lo que queda de ella se tira (si hay otra detrás). Sin espacios al principio.
fn tail(s: &str, max: usize) -> &str {
    let skip = s.chars().count().saturating_sub(max);
    if skip == 0 {
        return s;
    }
    let at = s.char_indices().nth(skip).map_or(s.len(), |(at, _)| at);
    let mut rest = &s[at..];
    if !s[..at].ends_with('\n')
        && let Some(nl) = rest.find('\n')
    {
        rest = &rest[nl + 1..];
    }
    rest.trim_start()
}

/// Una nota es una línea: los saltos de línea y los tabuladores pasan a espacios, y el resto de
/// caracteres de control (C0 y C1, como en el pegado de `prompt.rs`) se quitan. Recortada a
/// [`MAX_NOTE`].
fn note_text(text: &str) -> String {
    let line: String = text
        .replace("\r\n", " ")
        .chars()
        .filter_map(|c| {
            if c.is_control() || matches!(c, '\u{2028}' | '\u{2029}') {
                c.is_whitespace().then_some(' ')
            } else {
                Some(c)
            }
        })
        .collect();
    clip(&line, MAX_NOTE)
}

/// El texto de una llamada (la bitácora tiene varias líneas): sin caracteres de control salvo
/// los saltos de línea, que quedan como `\n`, y los demás espacios (tabuladores, U+0085), que
/// pasan a espacios. Si pasa de [`MAX_CALL_TEXT`], el final: en la bitácora, lo último es lo que
/// importa.
fn call_text(text: &str) -> String {
    let clean: String = text
        .replace("\r\n", "\n")
        .chars()
        .filter_map(|c| match c {
            '\n' | '\r' | '\u{2028}' | '\u{2029}' => Some('\n'),
            c if c.is_control() => c.is_whitespace().then_some(' '),
            c => Some(c),
        })
        .collect();
    tail(clean.trim(), MAX_CALL_TEXT).to_string()
}

fn check_call_id(id: &str) -> Result<(), MemoryError> {
    let ok = (1..=MAX_CALL_ID).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'));
    if ok {
        Ok(())
    } else {
        Err(MemoryError::BadCallId)
    }
}

/// Se queda con los de `v` que digan `keep` (uno por elemento, en orden).
fn retain_marked<T>(v: &mut Vec<T>, keep: &[bool]) {
    let mut keep = keep.iter();
    v.retain(|_| keep.next().copied().unwrap_or(true));
}

/// Lo que se lee del fichero, con las mismas limpiezas y límites que lo que llega por la API:
/// uno tocado a mano no saca por `get` textos largos, caracteres de control, ids de nota que la
/// web no guarda exactos ni ids repetidos (de una nota, la última; de una llamada, la más nueva y,
/// a igual hora, la última). De lo que sobre, se quedan las notas últimas y las llamadas más
/// nuevas.
fn tidy(mut data: Data) -> Data {
    data.notes.retain_mut(|n| {
        n.text = note_text(&n.text);
        !n.text.is_empty() && n.id <= MAX_NOTE_ID
    });
    let mut seen = HashSet::new();
    let mut keep: Vec<bool> = data.notes.iter().rev().map(|n| seen.insert(n.id)).collect();
    keep.reverse();
    retain_marked(&mut data.notes, &keep);
    let extra = data.notes.len().saturating_sub(MAX_NOTES);
    data.notes.drain(..extra);
    data.calls.retain_mut(|c| {
        c.text = call_text(&c.text);
        !c.text.is_empty() && check_call_id(&c.id).is_ok()
    });
    let mut newest: HashMap<&str, usize> = HashMap::new();
    for (i, c) in data.calls.iter().enumerate() {
        let best = newest.entry(c.id.as_str()).or_insert(i);
        if c.at >= data.calls[*best].at {
            *best = i;
        }
    }
    let mut keep = vec![false; data.calls.len()];
    for i in newest.into_values() {
        keep[i] = true;
    }
    retain_marked(&mut data.calls, &keep);
    while data.calls.len() > MAX_CALLS {
        let Some((oldest, _)) = data.calls.iter().enumerate().min_by_key(|(_, c)| c.at) else {
            break;
        };
        data.calls.remove(oldest);
    }
    data
}

/// La memoria de la voz de una carpeta de estado. Tiene que haber una sola por carpeta (la de
/// `AppState`, en un `Arc`): el cerrojo es de la instancia, así que dos (o dos procesos con la
/// misma carpeta) podrían pisarse los cambios, como los planes (`plan.rs`).
pub struct VoiceMemory {
    path: PathBuf,
    now: Box<dyn Fn() -> u64 + Send + Sync>,
    /// Pone en fila las operaciones.
    lock: Mutex<()>,
}

impl VoiceMemory {
    pub fn new(state_dir: PathBuf) -> Self {
        Self::with_clock(state_dir, crate::oauth::now_ms)
    }

    /// Con otro reloj (ms desde 1970), para las pruebas.
    pub fn with_clock(state_dir: PathBuf, now: impl Fn() -> u64 + Send + Sync + 'static) -> Self {
        VoiceMemory {
            path: state_dir.join(FILE),
            now: Box::new(now),
            lock: Mutex::new(()),
        }
    }

    /// Lee el fichero (con el cerrojo tomado). Si falta, la memoria está vacía; si no se
    /// entiende, se aparta como `voice-memory.json.bad` (o `.bad.<ms>`, si ya hay uno: no se
    /// pisa) para mirarlo, y está vacía. Si no se puede leer (permisos, un fallo del disco) o no
    /// se puede apartar, es el error: así no se pisa.
    fn load(&self) -> Result<Data, String> {
        let bytes = match std::fs::read(&self.path) {
            Ok(b) => Some(b),
            Err(e) if missing(&e) => None,
            Err(e) => return Err(e.to_string()),
        };
        let parsed = bytes.map(|b| serde_json::from_slice::<Data>(&b));
        let mut data = match parsed.unwrap_or_else(|| Ok(Data::default())) {
            Ok(d) => tidy(d),
            Err(e) => {
                let aside = self.set_aside();
                let name = aside.0.file_name().unwrap_or_default().to_string_lossy();
                if let Err(re) = aside.1 {
                    return Err(format!(
                        "no se entiende ({e}) y no se pudo apartar como {name}: {re}"
                    ));
                }
                eprintln!(
                    "nido: la memoria de la voz no se entiende ({e}): apartada como {name}; se \
                     empieza vacía"
                );
                Data::default()
            }
        };
        // Un fichero sin `next_note`, o con uno que se quedó atrás: los ids no se repiten. Uno que
        // pasa de los ids que hay (tocado a mano) dejaría la memoria llena para siempre: vuelve a
        // seguir al más alto.
        let after_max = data
            .notes
            .iter()
            .map(|n| n.id.saturating_add(1))
            .max()
            .unwrap_or(1);
        if data.next_note > MAX_NOTE_ID + 1 {
            data.next_note = after_max;
        }
        data.next_note = data.next_note.max(after_max).max(1);
        Ok(data)
    }

    /// Aparta el fichero que no se entiende: como `voice-memory.json.bad` o, si ya hay uno,
    /// `voice-memory.json.bad.<ms>`. Si ese también está (o no se puede mirar), no se pisa: error.
    fn set_aside(&self) -> (PathBuf, io::Result<()>) {
        let taken = |p: &PathBuf| !matches!(std::fs::symlink_metadata(p), Err(e) if missing(&e));
        let mut bad = self.path.with_extension("json.bad");
        if taken(&bad) {
            bad = self
                .path
                .with_extension(format!("json.bad.{}", (self.now)()));
        }
        let moved = if taken(&bad) {
            Err(io::Error::from(io::ErrorKind::AlreadyExists))
        } else {
            std::fs::rename(&self.path, &bad)
        };
        (bad, moved)
    }

    /// Escribe en un temporal y lo renombra: nunca queda una memoria a medias.
    fn write(&self, data: &Data) -> io::Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = self.path.with_extension("json.tmp");
        let body = serde_json::to_vec_pretty(data).map_err(io::Error::other)?;
        let written = (|| {
            let mut opts = std::fs::OpenOptions::new();
            opts.write(true).create(true).truncate(true);
            let mut f = crate::sys::private(&mut opts).open(&tmp)?;
            f.write_all(&body)?;
            f.sync_all()?;
            std::fs::rename(&tmp, &self.path)
        })();
        if written.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        written
    }

    /// Lee, cambia con `f` (que dice qué devolver y si cambió algo) y, si cambió, escribe. Todo
    /// con el cerrojo tomado.
    fn change<T>(
        &self,
        f: impl FnOnce(&mut Data, u64) -> Result<(T, bool), MemoryError>,
    ) -> Result<T, MemoryError> {
        let _one = self.lock.lock().unwrap_or_else(PoisonError::into_inner);
        let mut data = self.load().map_err(MemoryError::Read)?;
        let (out, changed) = f(&mut data, (self.now)())?;
        if changed {
            self.write(&data)
                .map_err(|e| MemoryError::Save(e.to_string()))?;
        }
        Ok(out)
    }

    /// Lo que hay (`Read` si el fichero no se puede leer).
    pub fn get(&self) -> Result<Snapshot, MemoryError> {
        let _one = self.lock.lock().unwrap_or_else(PoisonError::into_inner);
        let data = self.load().map_err(MemoryError::Read)?;
        Ok(Snapshot {
            notes: data.notes,
            calls: data.calls,
        })
    }

    /// Apunta una nota: el id que le toca. Si ya no quedan ids exactos para la web (se gastó
    /// [`MAX_NOTE_ID`], solo con un fichero tocado a mano), `Full`.
    pub fn add_note(&self, text: &str) -> Result<u64, MemoryError> {
        let text = note_text(text);
        if text.is_empty() {
            return Err(MemoryError::EmptyNote);
        }
        self.change(|data, now| {
            if data.notes.len() >= MAX_NOTES {
                return Err(MemoryError::Full);
            }
            let id = data.next_note;
            if id > MAX_NOTE_ID {
                return Err(MemoryError::Full);
            }
            data.next_note = id + 1;
            data.notes.push(Note { id, text, at: now });
            Ok((id, true))
        })
    }

    /// Borra una nota: `false` si no había ninguna con ese id.
    pub fn delete_note(&self, id: u64) -> Result<bool, MemoryError> {
        self.change(|data, _| {
            let before = data.notes.len();
            data.notes.retain(|n| n.id != id);
            let gone = data.notes.len() < before;
            Ok((gone, gone))
        })
    }

    /// Guarda el resumen o la bitácora de la llamada `id`: sustituye el que hubiera con ese id
    /// (y pasa a ser la más nueva), salvo una bitácora sobre un resumen, que no se guarda (es la
    /// de cada 30 s, que llega tarde). Si pasan de [`MAX_CALLS`], se tira la más vieja.
    pub fn put_call(&self, id: &str, kind: CallKind, text: &str) -> Result<(), MemoryError> {
        check_call_id(id)?;
        let text = call_text(text);
        if text.is_empty() {
            return Err(MemoryError::EmptyText);
        }
        self.change(|data, now| {
            if let Some(i) = data.calls.iter().position(|c| c.id == id) {
                if data.calls[i].kind == CallKind::Resumen && kind == CallKind::Bitacora {
                    return Ok(((), false));
                }
                data.calls.remove(i);
            }
            data.calls.push(Call {
                id: id.to_string(),
                at: now,
                kind,
                text,
            });
            while data.calls.len() > MAX_CALLS {
                // La más vieja por la hora, sin contar la que se acaba de guardar (por si el
                // reloj fue hacia atrás); a igual hora, la primera.
                let rest = &data.calls[..data.calls.len() - 1];
                let Some((oldest, _)) = rest.iter().enumerate().min_by_key(|(_, c)| c.at) else {
                    break;
                };
                data.calls.remove(oldest);
            }
            Ok(((), true))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Una memoria en `state` con un reloj que se pone a mano.
    fn mem(state: &Path) -> (VoiceMemory, Arc<AtomicU64>) {
        let clock = Arc::new(AtomicU64::new(1000));
        let c = clock.clone();
        let m = VoiceMemory::with_clock(state.to_path_buf(), move || c.load(Ordering::SeqCst));
        (m, clock)
    }

    fn set(clock: &AtomicU64, at: u64) {
        clock.store(at, Ordering::SeqCst);
    }

    /// Lo que hay en la carpeta de estado (sin temporales a medias).
    fn files(state: &Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(state)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    fn texts(s: &Snapshot) -> Vec<&str> {
        s.notes.iter().map(|n| n.text.as_str()).collect()
    }

    #[test]
    fn empty_without_file() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        let (m, _) = mem(&state);
        assert_eq!(m.get().unwrap(), Snapshot::default());
        // Leer no crea nada.
        assert!(!state.exists());
    }

    #[test]
    fn notes_add_delete_and_persist() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        let (m, clock) = mem(&state);
        assert_eq!(m.add_note("la reunión es el jueves"), Ok(1));
        set(&clock, 2000);
        assert_eq!(m.add_note("el proyecto nuevo va en ~/obra"), Ok(2));
        let s = m.get().unwrap();
        assert_eq!(
            s.notes,
            vec![
                Note {
                    id: 1,
                    text: "la reunión es el jueves".into(),
                    at: 1000
                },
                Note {
                    id: 2,
                    text: "el proyecto nuevo va en ~/obra".into(),
                    at: 2000
                },
            ]
        );

        assert_eq!(m.delete_note(1), Ok(true));
        assert_eq!(m.delete_note(1), Ok(false));
        assert_eq!(m.delete_note(99), Ok(false));
        // El id de una borrada no se repite.
        assert_eq!(m.add_note("otra"), Ok(3));

        // Como tras reiniciar.
        let (again, _) = mem(&state);
        assert_eq!(
            texts(&again.get().unwrap()),
            vec!["el proyecto nuevo va en ~/obra", "otra"]
        );
        assert_eq!(again.add_note("y otra"), Ok(4));
        assert_eq!(m.get().unwrap().notes.len(), 3);
    }

    #[test]
    fn note_text_is_one_clean_line() {
        let dir = tempfile::tempdir().unwrap();
        let (m, _) = mem(dir.path());
        let id = m
            .add_note("  hola\r\nmundo\nya\tvale\u{1b}[201~\u{9b}x\u{7f}\u{85}fin  ")
            .unwrap();
        let s = m.get().unwrap();
        let n = s.notes.iter().find(|n| n.id == id).unwrap();
        assert_eq!(n.text, "hola mundo ya vale[201~x fin");

        for empty in ["", "   ", "\u{1b}\u{7}", " \n\r\t ", "\u{9b}\u{0}"] {
            assert_eq!(m.add_note(empty), Err(MemoryError::EmptyNote), "{empty:?}");
        }
        assert_eq!(MemoryError::EmptyNote.to_string(), "la nota está vacía");
        assert_eq!(m.get().unwrap().notes.len(), 1);
    }

    #[test]
    fn note_is_clipped_to_300_chars() {
        let dir = tempfile::tempdir().unwrap();
        let (m, _) = mem(dir.path());
        // Por caracteres, no por bytes.
        m.add_note(&"ñ".repeat(301)).unwrap();
        m.add_note(&"ñ".repeat(300)).unwrap();
        // Lo que queda al cortar, sin espacios al final.
        m.add_note(&format!("{} b", "a".repeat(299))).unwrap();
        let s = m.get().unwrap();
        assert_eq!(s.notes[0].text, "ñ".repeat(300));
        assert_eq!(s.notes[1].text, "ñ".repeat(300));
        assert_eq!(s.notes[2].text, "a".repeat(299));
    }

    #[test]
    fn at_most_50_notes() {
        let dir = tempfile::tempdir().unwrap();
        let (m, _) = mem(dir.path());
        for i in 0..MAX_NOTES {
            m.add_note(&format!("nota {i}")).unwrap();
        }
        assert_eq!(m.add_note("una más"), Err(MemoryError::Full));
        assert_eq!(MemoryError::Full.to_string(), "hay 50 notas: olvida alguna");
        assert_eq!(m.get().unwrap().notes.len(), MAX_NOTES);
        // Vacía sigue siendo vacía, aunque esté llena.
        assert_eq!(m.add_note(" "), Err(MemoryError::EmptyNote));

        assert_eq!(m.delete_note(10), Ok(true));
        assert_eq!(m.add_note("una más"), Ok(MAX_NOTES as u64 + 1));
        assert_eq!(m.add_note("y otra"), Err(MemoryError::Full));
    }

    #[test]
    fn call_id_is_checked() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        let (m, _) = mem(&state);
        let long = "a".repeat(MAX_CALL_ID + 1);
        for bad in ["", long.as_str(), "a.b", "á", "a b", "a/b", "../x", "a\n"] {
            assert_eq!(
                m.put_call(bad, CallKind::Bitacora, "texto"),
                Err(MemoryError::BadCallId),
                "{bad:?}"
            );
        }
        assert!(!state.exists());
        assert_eq!(
            MemoryError::BadCallId.to_string(),
            "id de llamada no válido: de 1 a 64 caracteres entre A-Z, a-z, 0-9, '_' y '-'"
        );

        let max = "Z".repeat(MAX_CALL_ID);
        m.put_call(&max, CallKind::Resumen, "uno").unwrap();
        m.put_call("aZ09_-", CallKind::Resumen, "dos").unwrap();
        let ids: Vec<String> = m.get().unwrap().calls.into_iter().map(|c| c.id).collect();
        assert_eq!(ids, vec![max, "aZ09_-".to_string()]);
    }

    #[test]
    fn call_text_keeps_lines_and_the_end() {
        let dir = tempfile::tempdir().unwrap();
        let (m, clock) = mem(dir.path());
        m.put_call(
            "c1",
            CallKind::Bitacora,
            "  10:00 hola\r\n10:01 escribe\rya\u{1b}[201~\t\u{9b}\u{85}fin\n\n  ",
        )
        .unwrap();
        // El tabulador y U+0085, espacios; el resto de caracteres de control, fuera.
        assert_eq!(
            m.get().unwrap().calls[0].text,
            "10:00 hola\n10:01 escribe\nya[201~  fin"
        );
        assert_eq!(m.get().unwrap().calls[0].at, 1000);

        // Si pasa de 4000 caracteres, se queda el final (por caracteres, no por bytes).
        set(&clock, 2000);
        let head = "principio\n".repeat(50);
        let end = "ñ".repeat(MAX_CALL_TEXT - 4) + "\nfin";
        m.put_call("c2", CallKind::Bitacora, &format!("{head}{end}"))
            .unwrap();
        let c = m
            .get()
            .unwrap()
            .calls
            .into_iter()
            .find(|c| c.id == "c2")
            .unwrap();
        assert_eq!(c.text.chars().count(), MAX_CALL_TEXT);
        assert_eq!(c.text, end);
        // Si el corte cae a media línea, desde la siguiente.
        let line = "y".repeat(MAX_CALL_TEXT - 10);
        m.put_call(
            "c5",
            CallKind::Bitacora,
            &format!("{}\n   {line}", "x".repeat(100)),
        )
        .unwrap();
        let c = m
            .get()
            .unwrap()
            .calls
            .into_iter()
            .find(|c| c.id == "c5")
            .unwrap();
        assert_eq!(c.text, line);
        // Sin otra línea detrás, lo que quede de esa.
        m.put_call("c6", CallKind::Bitacora, &"z".repeat(MAX_CALL_TEXT + 7))
            .unwrap();
        let c = m
            .get()
            .unwrap()
            .calls
            .into_iter()
            .find(|c| c.id == "c6")
            .unwrap();
        assert_eq!(c.text, "z".repeat(MAX_CALL_TEXT));
        // Justo 4000, entero.
        let exact = "x".repeat(MAX_CALL_TEXT);
        m.put_call("c3", CallKind::Resumen, &exact).unwrap();
        let c = m
            .get()
            .unwrap()
            .calls
            .into_iter()
            .find(|c| c.id == "c3")
            .unwrap();
        assert_eq!(c.text, exact);

        for empty in ["", "  \n ", "\u{1b}\u{7}"] {
            assert_eq!(
                m.put_call("c4", CallKind::Resumen, empty),
                Err(MemoryError::EmptyText),
                "{empty:?}"
            );
        }
        assert_eq!(m.get().unwrap().calls.len(), 5);
    }

    #[test]
    fn a_log_does_not_replace_a_summary() {
        let dir = tempfile::tempdir().unwrap();
        let (m, clock) = mem(dir.path());
        m.put_call("c1", CallKind::Resumen, "el resumen").unwrap();
        let file = dir.path().join(FILE);
        let before = std::fs::read(&file).unwrap();

        // La bitácora que llega tarde (el guardado de cada 30 s) no lo pisa ni lo cambia de hora.
        set(&clock, 2000);
        assert_eq!(m.put_call("c1", CallKind::Bitacora, "la bitácora"), Ok(()));
        assert_eq!(
            m.get().unwrap().calls,
            vec![Call {
                id: "c1".into(),
                at: 1000,
                kind: CallKind::Resumen,
                text: "el resumen".into()
            }]
        );
        assert_eq!(std::fs::read(&file).unwrap(), before);
    }

    #[test]
    fn a_summary_replaces_the_log() {
        let dir = tempfile::tempdir().unwrap();
        let (m, clock) = mem(dir.path());
        m.put_call("c1", CallKind::Bitacora, "10:00 hola").unwrap();
        m.put_call("c2", CallKind::Bitacora, "otra llamada")
            .unwrap();
        // La bitácora se va guardando con el mismo id.
        set(&clock, 2000);
        m.put_call("c1", CallKind::Bitacora, "10:00 hola\n10:01 adiós")
            .unwrap();
        set(&clock, 3000);
        m.put_call("c1", CallKind::Resumen, "se habló de todo")
            .unwrap();
        let calls = m.get().unwrap().calls;
        assert_eq!(calls.len(), 2);
        let c1 = calls.iter().find(|c| c.id == "c1").unwrap();
        assert_eq!(
            *c1,
            Call {
                id: "c1".into(),
                at: 3000,
                kind: CallKind::Resumen,
                text: "se habló de todo".into()
            }
        );
        // Un resumen sí sustituye a otro.
        set(&clock, 4000);
        m.put_call("c1", CallKind::Resumen, "mejor resumen")
            .unwrap();
        let calls = m.get().unwrap().calls;
        assert_eq!(calls.len(), 2);
        let c1 = calls.iter().find(|c| c.id == "c1").unwrap();
        assert_eq!((c1.at, c1.text.as_str()), (4000, "mejor resumen"));
        let c2 = calls.iter().find(|c| c.id == "c2").unwrap();
        assert_eq!((c2.at, c2.kind), (1000, CallKind::Bitacora));
    }

    #[test]
    fn at_most_20_calls_dropping_the_oldest() {
        let dir = tempfile::tempdir().unwrap();
        let (m, clock) = mem(dir.path());
        for i in 0..MAX_CALLS as u64 {
            set(&clock, 1000 + i);
            m.put_call(&format!("c{i}"), CallKind::Resumen, "r")
                .unwrap();
        }
        // Sustituir una que ya está no tira ninguna (y la pone al día).
        set(&clock, 2000);
        m.put_call("c0", CallKind::Resumen, "r2").unwrap();
        let calls = m.get().unwrap().calls;
        assert_eq!(calls.len(), MAX_CALLS);
        assert!(calls.iter().any(|c| c.id == "c0" && c.at == 2000));

        // La 21: se va la más vieja por la hora (c1), no la primera que entró (c0).
        set(&clock, 3000);
        m.put_call("nueva", CallKind::Bitacora, "b").unwrap();
        let calls = m.get().unwrap().calls;
        assert_eq!(calls.len(), MAX_CALLS);
        assert!(!calls.iter().any(|c| c.id == "c1"));
        assert!(calls.iter().any(|c| c.id == "c0"));
        assert!(calls.iter().any(|c| c.id == "nueva"));

        // Aunque el reloj vaya hacia atrás, la que se acaba de guardar se queda.
        set(&clock, 1);
        m.put_call("atrasada", CallKind::Resumen, "r").unwrap();
        let calls = m.get().unwrap().calls;
        assert_eq!(calls.len(), MAX_CALLS);
        assert!(calls.iter().any(|c| c.id == "atrasada"));
        assert!(!calls.iter().any(|c| c.id == "c2"));
    }

    #[test]
    fn a_broken_file_is_an_empty_memory() {
        for broken in [
            "{ esto no es memoria",
            "",
            "{\"notes\": 5}",
            "[1]",
            "\u{0}\u{1}",
        ] {
            let dir = tempfile::tempdir().unwrap();
            let file = dir.path().join(FILE);
            std::fs::write(&file, broken).unwrap();
            let (m, _) = mem(dir.path());
            assert_eq!(m.get().unwrap(), Snapshot::default(), "{broken:?}");
            // Apartado, tal cual, para mirarlo.
            let bad = dir.path().join(format!("{FILE}.bad"));
            assert_eq!(std::fs::read_to_string(&bad).unwrap(), broken);
            assert!(!file.exists());
            // El primer cambio lo reescribe, entero y bien.
            assert_eq!(m.add_note("nueva"), Ok(1));
            let data: Data = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
            assert_eq!(data.notes.len(), 1);
            let (again, _) = mem(dir.path());
            assert_eq!(texts(&again.get().unwrap()), vec!["nueva"]);
        }
    }

    /// Si ya hay un `.bad` (de otra vez), no se pisa: el nuevo va con la hora.
    #[test]
    fn an_older_bad_file_is_not_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join(FILE);
        let bad = dir.path().join(format!("{FILE}.bad"));
        std::fs::write(&bad, "el de antes").unwrap();
        std::fs::write(&file, "{ roto").unwrap();
        let (m, _) = mem(dir.path());
        assert_eq!(m.get().unwrap(), Snapshot::default());
        assert_eq!(std::fs::read_to_string(&bad).unwrap(), "el de antes");
        let stamped = dir.path().join(format!("{FILE}.bad.1000"));
        assert_eq!(std::fs::read_to_string(&stamped).unwrap(), "{ roto");
        assert!(!file.exists());
        assert_eq!(m.add_note("nueva"), Ok(1));
    }

    /// Uno que no se entiende y no se puede apartar es un error (500 en la API), no una memoria
    /// vacía: si no, el primer cambio lo pisaría sin que nadie lo hubiera mirado.
    #[test]
    fn a_broken_file_that_cannot_be_set_aside_is_an_error() {
        for stamped_is_dir in [true, false] {
            let dir = tempfile::tempdir().unwrap();
            let file = dir.path().join(FILE);
            let bad = dir.path().join(format!("{FILE}.bad"));
            let stamped = dir.path().join(format!("{FILE}.bad.1000"));
            std::fs::write(&file, "{ roto").unwrap();
            std::fs::write(&bad, "el de antes").unwrap();
            // Donde iría ya hay algo (una carpeta, o un fichero que no se pisa).
            if stamped_is_dir {
                std::fs::create_dir(&stamped).unwrap();
            } else {
                std::fs::write(&stamped, "el otro").unwrap();
            }
            let (m, _) = mem(dir.path());
            for e in [
                m.get().unwrap_err(),
                m.add_note("una").unwrap_err(),
                m.delete_note(1).unwrap_err(),
                m.put_call("c1", CallKind::Resumen, "r").unwrap_err(),
            ] {
                assert!(matches!(e, MemoryError::Read(_)), "{e:?}");
                let en = crate::i18n::english(&e.to_string());
                assert!(
                    en.starts_with("could not read the memory: it can't be understood ("),
                    "{en}"
                );
                assert!(
                    en.contains(") and could not be set aside as voice-memory.json.bad.1000: "),
                    "{en}"
                );
            }
            assert_eq!(std::fs::read_to_string(&file).unwrap(), "{ roto");
            assert_eq!(std::fs::read_to_string(&bad).unwrap(), "el de antes");
            if stamped_is_dir {
                assert_eq!(std::fs::read_dir(&stamped).unwrap().count(), 0);
            } else {
                assert_eq!(std::fs::read_to_string(&stamped).unwrap(), "el otro");
            }
        }
    }

    /// Con ids repetidos (un fichero tocado a mano): de cada nota, la última; de cada llamada, la
    /// más nueva (a igual hora, la última). Se mira después de limpiar, y antes de los límites.
    #[test]
    fn repeated_ids_are_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join(FILE);
        let raw = serde_json::json!({
            "notes": [
                {"id": 1, "text": "uno", "at": 1},
                {"id": 2, "text": "dos", "at": 2},
                {"id": 1, "text": "uno bis", "at": 3},
                {"id": 3, "text": " ", "at": 4},
                {"id": 3, "text": "tres", "at": 5},
                {"id": 2, "text": "\u{7}", "at": 6},
            ],
            "calls": [
                {"id": "c1", "at": 5, "kind": "resumen", "text": "nueva"},
                {"id": "c1", "at": 3, "kind": "resumen", "text": "vieja"},
                {"id": "c2", "at": 4, "kind": "resumen", "text": "a"},
                {"id": "c2", "at": 4, "kind": "bitacora", "text": "b"},
                {"id": "c3", "at": 1, "kind": "resumen", "text": "x"},
                {"id": "c3", "at": 9, "kind": "resumen", "text": ""},
            ],
        });
        std::fs::write(&file, raw.to_string()).unwrap();
        let (m, _) = mem(dir.path());
        let s = m.get().unwrap();
        let notes: Vec<(u64, &str)> = s.notes.iter().map(|n| (n.id, n.text.as_str())).collect();
        assert_eq!(notes, vec![(2, "dos"), (1, "uno bis"), (3, "tres")]);
        let calls: Vec<(&str, u64, &str)> = s
            .calls
            .iter()
            .map(|c| (c.id.as_str(), c.at, c.text.as_str()))
            .collect();
        assert_eq!(
            calls,
            vec![("c1", 5, "nueva"), ("c2", 4, "b"), ("c3", 1, "x")]
        );

        // Los repetidos no se comen el sitio de los demás.
        let notes: Vec<serde_json::Value> = (0..2 * MAX_NOTES)
            .map(|i| serde_json::json!({"id": i % MAX_NOTES + 1, "text": format!("n{i}"), "at": i}))
            .collect();
        let calls: Vec<serde_json::Value> = (0..2 * MAX_CALLS)
            .map(|i| serde_json::json!({"id": format!("c{}", i % MAX_CALLS), "at": i, "kind": "resumen", "text": "r"}))
            .collect();
        std::fs::write(
            &file,
            serde_json::json!({"notes": notes, "calls": calls}).to_string(),
        )
        .unwrap();
        let s = m.get().unwrap();
        assert_eq!(s.notes.len(), MAX_NOTES);
        assert!(
            s.notes
                .iter()
                .all(|n| n.text == format!("n{}", n.id - 1 + MAX_NOTES as u64))
        );
        assert_eq!(s.calls.len(), MAX_CALLS);
        assert!(s.calls.iter().all(|c| c.at >= MAX_CALLS as u64));
    }

    #[test]
    fn a_partial_file_keeps_what_it_has() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join(FILE);
        // Sin `next_note` (o con uno que se quedó atrás): el id sigue al más alto.
        std::fs::write(&file, r#"{"notes": [{"id": 7, "text": "siete", "at": 1}]}"#).unwrap();
        let (m, _) = mem(dir.path());
        assert_eq!(texts(&m.get().unwrap()), vec!["siete"]);
        assert_eq!(m.get().unwrap().calls, vec![]);
        assert_eq!(m.add_note("ocho"), Ok(8));
        std::fs::write(
            &file,
            r#"{"notes": [{"id": 7, "text": "siete", "at": 1}], "next_note": 3}"#,
        )
        .unwrap();
        assert_eq!(m.add_note("ocho"), Ok(8));
    }

    /// Los ids de las notas no pasan de 2^53 − 1 (la web los guarda exactos), ni con un fichero
    /// tocado a mano; con ids enormes, nada salta.
    #[test]
    fn note_ids_stay_exact_for_the_web() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join(FILE);
        let last = MAX_NOTE_ID - 1;
        std::fs::write(
            &file,
            format!(r#"{{"notes": [{{"id": {last}, "text": "x", "at": 1}}]}}"#),
        )
        .unwrap();
        let (m, _) = mem(dir.path());
        assert_eq!(m.add_note("y"), Ok(MAX_NOTE_ID));
        assert_eq!(m.add_note("z"), Err(MemoryError::Full));
        assert_eq!(m.get().unwrap().notes.len(), 2);
        // Gastado el último id de verdad, sigue así tras reiniciar.
        let (again, _) = mem(dir.path());
        assert_eq!(again.add_note("z"), Err(MemoryError::Full));

        let max = u64::MAX;
        std::fs::write(
            &file,
            format!(r#"{{"notes": [{{"id": {max}, "text": "x", "at": 1}}], "next_note": {max}}}"#),
        )
        .unwrap();
        // La de un id que la web no guarda exacto no sale, y un `next_note` imposible (tocado a
        // mano) no la deja llena para siempre: vuelve a seguir al más alto que queda.
        assert_eq!(m.get().unwrap().notes, vec![]);
        assert_eq!(m.add_note("y"), Ok(1));
        let raw: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
        assert_eq!(raw["next_note"], 2);
        let past = MAX_NOTE_ID + 2;
        std::fs::write(
            &file,
            format!(r#"{{"notes": [{{"id": 5, "text": "x", "at": 1}}], "next_note": {past}}}"#),
        )
        .unwrap();
        assert_eq!(m.add_note("y"), Ok(6));
    }

    /// Un fichero tocado a mano pasa por lo mismo que lo que llega por la API.
    #[test]
    fn what_is_read_is_cleaned_and_capped() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join(FILE);
        let mut notes: Vec<serde_json::Value> = (1..=60u64)
            .map(|i| serde_json::json!({"id": i, "text": format!("nota {i}"), "at": i}))
            .collect();
        notes[59]["text"] = format!("a\u{1b}[2J\nb{}", "c".repeat(400)).into();
        notes.push(serde_json::json!({"id": 61, "text": " \u{7} ", "at": 61}));
        let mut calls: Vec<serde_json::Value> = (1..=25u64)
            .map(|i| serde_json::json!({"id": format!("c{i}"), "at": i, "kind": "resumen", "text": "r"}))
            .collect();
        calls[24]["text"] = format!("{}\u{9b}fin", "x".repeat(5000)).into();
        calls.push(serde_json::json!({"id": "../mal", "at": 99, "kind": "bitacora", "text": "b"}));
        calls
            .push(serde_json::json!({"id": "vacía", "at": 99, "kind": "resumen", "text": "\u{0}"}));
        // Con algo que ya no se guarda (el handle para retomar, ahora en el hub): se ignora.
        let raw = serde_json::json!({
            "notes": notes, "calls": calls,
            "resume": {"handle": "h", "at": 1},
        });
        std::fs::write(&file, raw.to_string()).unwrap();
        let (m, _) = mem(dir.path());
        let s = m.get().unwrap();
        // Las 50 últimas, limpias y recortadas.
        assert_eq!(s.notes.len(), MAX_NOTES);
        assert_eq!(s.notes[0].id, 11);
        let last = &s.notes[MAX_NOTES - 1];
        assert_eq!(last.id, 60);
        assert_eq!(last.text, format!("a[2J b{}", "c".repeat(MAX_NOTE - 6)));
        // Las 20 más nuevas, con su texto recortado por el final.
        assert_eq!(s.calls.len(), MAX_CALLS);
        assert_eq!(s.calls[0].id, "c6");
        let newest = &s.calls[MAX_CALLS - 1];
        assert_eq!(newest.id, "c25");
        assert_eq!(newest.text.chars().count(), MAX_CALL_TEXT);
        assert!(newest.text.ends_with("xfin"));
        assert!(serde_json::to_value(&s).unwrap().get("resume").is_none());
        // Y el primer cambio lo reescribe sin ello.
        m.delete_note(11).unwrap();
        let raw: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
        assert!(raw.get("resume").is_none(), "{raw}");
    }

    #[test]
    fn the_unaccented_kind_is_accepted_too() {
        let kind: CallKind = serde_json::from_str("\"bitacora\"").unwrap();
        assert_eq!(kind, CallKind::Bitacora);
        assert_eq!(serde_json::to_string(&kind).unwrap(), "\"bitácora\"");
    }

    #[test]
    fn a_failed_save_keeps_the_old_file() {
        let dir = tempfile::tempdir().unwrap();
        let (m, _) = mem(dir.path());
        m.add_note("una").unwrap();
        let before = std::fs::read(dir.path().join(FILE)).unwrap();
        // El temporal no se puede crear: el cambio falla y el fichero bueno sigue intacto.
        std::fs::create_dir(dir.path().join(format!("{FILE}.tmp"))).unwrap();
        let e = m.add_note("dos").unwrap_err();
        assert!(matches!(e, MemoryError::Save(_)), "{e:?}");
        assert_eq!(std::fs::read(dir.path().join(FILE)).unwrap(), before);
        assert_eq!(texts(&m.get().unwrap()), vec!["una"]);
    }

    #[test]
    fn deleting_the_file_forgets_everything() {
        let dir = tempfile::tempdir().unwrap();
        let (m, _) = mem(dir.path());
        m.add_note("una").unwrap();
        m.put_call("c1", CallKind::Resumen, "r").unwrap();
        std::fs::remove_file(dir.path().join(FILE)).unwrap();
        assert_eq!(m.get().unwrap(), Snapshot::default());
        assert_eq!(m.add_note("otra"), Ok(1));
        assert_eq!(texts(&m.get().unwrap()), vec!["otra"]);
    }

    #[test]
    fn json_shape() {
        let dir = tempfile::tempdir().unwrap();
        let (m, _) = mem(dir.path());
        assert_eq!(
            serde_json::to_value(m.get().unwrap()).unwrap(),
            serde_json::json!({"notes": [], "calls": []})
        );
        m.add_note("n").unwrap();
        m.put_call("c1", CallKind::Bitacora, "b").unwrap();
        m.put_call("c2", CallKind::Resumen, "r").unwrap();
        assert_eq!(
            serde_json::to_value(m.get().unwrap()).unwrap(),
            serde_json::json!({
                "notes": [{"id": 1, "text": "n", "at": 1000}],
                "calls": [
                    {"id": "c1", "at": 1000, "kind": "bitácora", "text": "b"},
                    {"id": "c2", "at": 1000, "kind": "resumen", "text": "r"},
                ],
            })
        );
        let raw: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.path().join(FILE)).unwrap()).unwrap();
        assert_eq!(raw["next_note"], 2);
        assert_eq!(raw["calls"][0]["kind"], "bitácora");
    }

    #[test]
    fn writes_leave_only_the_whole_file() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        let (m, _) = mem(&state);
        let only = vec![FILE.to_string()];
        m.add_note("una").unwrap();
        assert_eq!(files(&state), only);
        m.delete_note(1).unwrap();
        assert_eq!(files(&state), only);
        m.put_call("c1", CallKind::Bitacora, "b").unwrap();
        assert_eq!(files(&state), only);
        m.put_call("c1", CallKind::Resumen, "r").unwrap();
        assert_eq!(files(&state), only);
        let _ = m.add_note("");
        let _ = m.put_call("x.y", CallKind::Resumen, "r");
        assert_eq!(files(&state), only);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(state.join(FILE))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn a_failed_write_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        let (m, _) = mem(&state);
        // La carpeta de estado es un fichero: no se puede escribir dentro.
        std::fs::write(&state, "no soy una carpeta").unwrap();
        assert_eq!(m.get().unwrap(), Snapshot::default());
        for e in [
            m.add_note("una").unwrap_err(),
            m.put_call("c1", CallKind::Resumen, "r").unwrap_err(),
        ] {
            assert!(matches!(e, MemoryError::Save(_)), "{e:?}");
            assert!(
                e.to_string().starts_with("no se pudo guardar la memoria: "),
                "{e}"
            );
        }
        // Lo que no cambia nada no escribe (ni falla).
        assert_eq!(m.delete_note(1), Ok(false));
        assert_eq!(
            std::fs::read_to_string(&state).unwrap(),
            "no soy una carpeta"
        );
    }

    #[test]
    fn an_unreadable_file_is_not_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        // El fichero es una carpeta: leerlo falla (no es que falte ni que esté roto).
        std::fs::create_dir(dir.path().join(FILE)).unwrap();
        let (m, _) = mem(dir.path());
        let e = m.get().unwrap_err();
        assert!(matches!(e, MemoryError::Read(_)), "{e:?}");
        let e = m.add_note("una").unwrap_err();
        assert!(matches!(e, MemoryError::Read(_)), "{e:?}");
        assert!(
            e.to_string().starts_with("no se pudo leer la memoria: "),
            "{e}"
        );
        assert!(dir.path().join(FILE).is_dir());
    }

    /// Lo que devuelve la API sale en inglés si la petición no prefiere español.
    #[test]
    fn errors_are_translated() {
        for e in [
            MemoryError::EmptyNote,
            MemoryError::Full,
            MemoryError::EmptyText,
            MemoryError::BadCallId,
            MemoryError::Read("e".into()),
            MemoryError::Save("e".into()),
        ] {
            let es = e.to_string();
            assert_ne!(crate::i18n::english(&es), es, "sin traducir: {es}");
        }
    }
}
