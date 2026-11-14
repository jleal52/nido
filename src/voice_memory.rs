//! La memoria de la voz: las notas que apunta el agente («recuerda que…»), el resumen (o la
//! bitácora, si el resumen no llega) de las últimas llamadas y el handle para retomar la última
//! conversación. Se guarda en `voice-memory.json`, en la carpeta de estado, y no sale del equipo.
//!
//! El fichero es la memoria: cada operación lo lee, lo cambia y lo escribe entero (en un temporal
//! que se renombra), todo con el cerrojo tomado. Así, borrar el fichero la borra también con nido
//! en marcha. Uno que falte o no se entienda es una memoria vacía, y el primer cambio lo
//! reescribe.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::{Mutex, PoisonError};

pub const MAX_NOTES: usize = 50;
pub const MAX_NOTE: usize = 300;
pub const MAX_CALLS: usize = 20;
pub const MAX_CALL_TEXT: usize = 4000;
pub const MAX_CALL_ID: usize = 64;
/// En bytes.
pub const MAX_HANDLE: usize = 4096;

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
    #[serde(rename = "bitácora")]
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Resume {
    pub handle: String,
    /// ms desde 1970.
    pub at: u64,
}

/// Lo que se enseña de la memoria (`resume` sale como `null` si no hay).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    pub notes: Vec<Note>,
    pub calls: Vec<Call>,
    pub resume: Option<Resume>,
}

/// El fichero.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct Data {
    notes: Vec<Note>,
    calls: Vec<Call>,
    resume: Option<Resume>,
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
    BadHandle,
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
            MemoryError::BadHandle => write!(
                f,
                "handle no válido: de 1 a {MAX_HANDLE} bytes, sin caracteres de control"
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

/// Los últimos `max` caracteres de `s`.
fn tail(s: &str, max: usize) -> &str {
    let skip = s.chars().count().saturating_sub(max);
    match s.char_indices().nth(skip) {
        Some((at, _)) => &s[at..],
        None => "",
    }
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
/// los saltos de línea, que quedan como `\n`. Si pasa de [`MAX_CALL_TEXT`], el final: en la
/// bitácora, lo último es lo que importa.
fn call_text(text: &str) -> String {
    let clean: String = text
        .replace("\r\n", "\n")
        .chars()
        .filter_map(|c| match c {
            '\n' | '\r' | '\u{2028}' | '\u{2029}' => Some('\n'),
            c if c.is_control() => None,
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

/// El handle no se limpia: o vale tal cual o no vale.
fn check_handle(handle: &str) -> Result<(), MemoryError> {
    if (1..=MAX_HANDLE).contains(&handle.len()) && !handle.chars().any(char::is_control) {
        Ok(())
    } else {
        Err(MemoryError::BadHandle)
    }
}

pub struct VoiceMemory {
    path: PathBuf,
    now: Box<dyn Fn() -> u64 + Send + Sync>,
    /// Pone en fila las operaciones. Dentro, si ya se avisó de que el fichero no se entiende
    /// (para no repetirlo en cada lectura).
    lock: Mutex<bool>,
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
            lock: Mutex::new(false),
        }
    }

    /// Lee el fichero (con el cerrojo tomado). Si falta, o no se entiende, la memoria está vacía;
    /// si no se puede leer (permisos, un fallo del disco), es el error: así no se pisa.
    fn load(&self, warned: &mut bool) -> Result<Data, String> {
        let bytes = match std::fs::read(&self.path) {
            Ok(b) => Some(b),
            Err(e) if missing(&e) => None,
            Err(e) => return Err(e.to_string()),
        };
        let parsed = bytes.map(|b| serde_json::from_slice::<Data>(&b));
        let mut data = match parsed.unwrap_or_else(|| Ok(Data::default())) {
            Ok(d) => {
                *warned = false;
                d
            }
            Err(e) => {
                if !*warned {
                    *warned = true;
                    eprintln!(
                        "nido: la memoria de la voz no se entiende ({e}): se empieza vacía y se \
                         reescribirá al primer cambio"
                    );
                }
                Data::default()
            }
        };
        // Un fichero sin `next_note`, o con uno que se quedó atrás: los ids no se repiten.
        let after_max = data
            .notes
            .iter()
            .map(|n| n.id.saturating_add(1))
            .max()
            .unwrap_or(1);
        data.next_note = data.next_note.max(after_max).max(1);
        Ok(data)
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
            #[cfg(unix)]
            std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
            let mut f = opts.open(&tmp)?;
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
        let mut warned = self.lock.lock().unwrap_or_else(PoisonError::into_inner);
        let mut data = self.load(&mut warned).map_err(MemoryError::Read)?;
        let (out, changed) = f(&mut data, (self.now)())?;
        if changed {
            self.write(&data)
                .map_err(|e| MemoryError::Save(e.to_string()))?;
        }
        Ok(out)
    }

    /// Lo que hay. Si el fichero no se puede leer, vacía (y se dice en el registro).
    pub fn get(&self) -> Snapshot {
        let mut warned = self.lock.lock().unwrap_or_else(PoisonError::into_inner);
        let data = self.load(&mut warned).unwrap_or_else(|e| {
            eprintln!("nido: la memoria de la voz no se pudo leer ({e})");
            Data::default()
        });
        Snapshot {
            notes: data.notes,
            calls: data.calls,
            resume: data.resume,
        }
    }

    /// Apunta una nota: el id que le toca.
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
            data.next_note = id.saturating_add(1);
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

    /// Guarda el handle para retomar la conversación de Gemini, con la hora.
    pub fn set_resume(&self, handle: &str) -> Result<(), MemoryError> {
        check_handle(handle)?;
        self.change(|data, now| {
            data.resume = Some(Resume {
                handle: handle.to_string(),
                at: now,
            });
            Ok(((), true))
        })
    }

    /// Lo quita (que no hubiera no es error).
    pub fn clear_resume(&self) -> Result<(), MemoryError> {
        self.change(|data, _| {
            let had = data.resume.take().is_some();
            Ok(((), had))
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
        assert_eq!(m.get(), Snapshot::default());
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
        let s = m.get();
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
            texts(&again.get()),
            vec!["el proyecto nuevo va en ~/obra", "otra"]
        );
        assert_eq!(again.add_note("y otra"), Ok(4));
        assert_eq!(m.get().notes.len(), 3);
    }

    #[test]
    fn note_text_is_one_clean_line() {
        let dir = tempfile::tempdir().unwrap();
        let (m, _) = mem(dir.path());
        let id = m
            .add_note("  hola\r\nmundo\nya\tvale\u{1b}[201~\u{9b}x\u{7f}\u{85}fin  ")
            .unwrap();
        let s = m.get();
        let n = s.notes.iter().find(|n| n.id == id).unwrap();
        assert_eq!(n.text, "hola mundo ya vale[201~x fin");

        for empty in ["", "   ", "\u{1b}\u{7}", " \n\r\t ", "\u{9b}\u{0}"] {
            assert_eq!(m.add_note(empty), Err(MemoryError::EmptyNote), "{empty:?}");
        }
        assert_eq!(MemoryError::EmptyNote.to_string(), "la nota está vacía");
        assert_eq!(m.get().notes.len(), 1);
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
        let s = m.get();
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
        assert_eq!(m.get().notes.len(), MAX_NOTES);
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
        let ids: Vec<String> = m.get().calls.into_iter().map(|c| c.id).collect();
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
        assert_eq!(
            m.get().calls[0].text,
            "10:00 hola\n10:01 escribe\nya[201~fin"
        );
        assert_eq!(m.get().calls[0].at, 1000);

        // Si pasa de 4000 caracteres, se queda el final (por caracteres, no por bytes).
        set(&clock, 2000);
        let head = "principio\n".repeat(50);
        let end = "ñ".repeat(MAX_CALL_TEXT - 4) + "\nfin";
        m.put_call("c2", CallKind::Bitacora, &format!("{head}{end}"))
            .unwrap();
        let c = m.get().calls.into_iter().find(|c| c.id == "c2").unwrap();
        assert_eq!(c.text.chars().count(), MAX_CALL_TEXT);
        assert_eq!(c.text, end);
        // Justo 4000, entero.
        let exact = "x".repeat(MAX_CALL_TEXT);
        m.put_call("c3", CallKind::Resumen, &exact).unwrap();
        let c = m.get().calls.into_iter().find(|c| c.id == "c3").unwrap();
        assert_eq!(c.text, exact);

        for empty in ["", "  \n ", "\u{1b}\u{7}"] {
            assert_eq!(
                m.put_call("c4", CallKind::Resumen, empty),
                Err(MemoryError::EmptyText),
                "{empty:?}"
            );
        }
        assert_eq!(m.get().calls.len(), 3);
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
            m.get().calls,
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
        let calls = m.get().calls;
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
        let calls = m.get().calls;
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
        let calls = m.get().calls;
        assert_eq!(calls.len(), MAX_CALLS);
        assert!(calls.iter().any(|c| c.id == "c0" && c.at == 2000));

        // La 21: se va la más vieja por la hora (c1), no la primera que entró (c0).
        set(&clock, 3000);
        m.put_call("nueva", CallKind::Bitacora, "b").unwrap();
        let calls = m.get().calls;
        assert_eq!(calls.len(), MAX_CALLS);
        assert!(!calls.iter().any(|c| c.id == "c1"));
        assert!(calls.iter().any(|c| c.id == "c0"));
        assert!(calls.iter().any(|c| c.id == "nueva"));

        // Aunque el reloj vaya hacia atrás, la que se acaba de guardar se queda.
        set(&clock, 1);
        m.put_call("atrasada", CallKind::Resumen, "r").unwrap();
        let calls = m.get().calls;
        assert_eq!(calls.len(), MAX_CALLS);
        assert!(calls.iter().any(|c| c.id == "atrasada"));
        assert!(!calls.iter().any(|c| c.id == "c2"));
    }

    #[test]
    fn resume_handle() {
        let dir = tempfile::tempdir().unwrap();
        let (m, clock) = mem(dir.path());
        set(&clock, 5000);
        m.set_resume("abc").unwrap();
        assert_eq!(
            m.get().resume,
            Some(Resume {
                handle: "abc".into(),
                at: 5000
            })
        );
        // Hasta 4096 bytes (no caracteres).
        m.set_resume(&"a".repeat(MAX_HANDLE)).unwrap();
        m.set_resume(&"ñ".repeat(MAX_HANDLE / 2)).unwrap();
        assert_eq!(m.get().resume.unwrap().handle, "ñ".repeat(MAX_HANDLE / 2));
        let too_long = ["a".repeat(MAX_HANDLE + 1), "ñ".repeat(MAX_HANDLE / 2) + "a"];
        for bad in too_long
            .iter()
            .map(String::as_str)
            .chain(["", "ab\u{7}", "ab\n", "a\u{9b}b"])
        {
            assert_eq!(m.set_resume(bad), Err(MemoryError::BadHandle), "{bad:?}");
        }
        assert_eq!(
            MemoryError::BadHandle.to_string(),
            "handle no válido: de 1 a 4096 bytes, sin caracteres de control"
        );
        assert_eq!(m.get().resume.unwrap().handle, "ñ".repeat(MAX_HANDLE / 2));

        set(&clock, 6000);
        m.set_resume("otro").unwrap();
        let (again, _) = mem(dir.path());
        assert_eq!(
            again.get().resume,
            Some(Resume {
                handle: "otro".into(),
                at: 6000
            })
        );
        m.clear_resume().unwrap();
        assert_eq!(again.get().resume, None);
        m.clear_resume().unwrap();
    }

    #[test]
    fn a_broken_file_is_an_empty_memory() {
        for broken in [
            "{ esto no es memoria",
            "",
            "{\"notes\": 5}",
            "[]",
            "\u{0}\u{1}",
        ] {
            let dir = tempfile::tempdir().unwrap();
            let file = dir.path().join(FILE);
            std::fs::write(&file, broken).unwrap();
            let (m, _) = mem(dir.path());
            assert_eq!(m.get(), Snapshot::default(), "{broken:?}");
            // El primer cambio lo reescribe, entero y bien.
            assert_eq!(m.add_note("nueva"), Ok(1));
            let data: Data = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
            assert_eq!(data.notes.len(), 1);
            let (again, _) = mem(dir.path());
            assert_eq!(texts(&again.get()), vec!["nueva"]);
        }
    }

    #[test]
    fn a_partial_file_keeps_what_it_has() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join(FILE);
        // Sin `next_note` (o con uno que se quedó atrás): el id sigue al más alto.
        std::fs::write(&file, r#"{"notes": [{"id": 7, "text": "siete", "at": 1}]}"#).unwrap();
        let (m, _) = mem(dir.path());
        assert_eq!(texts(&m.get()), vec!["siete"]);
        assert_eq!(m.get().calls, vec![]);
        assert_eq!(m.get().resume, None);
        assert_eq!(m.add_note("ocho"), Ok(8));
        std::fs::write(
            &file,
            r#"{"notes": [{"id": 7, "text": "siete", "at": 1}], "next_note": 3}"#,
        )
        .unwrap();
        assert_eq!(m.add_note("ocho"), Ok(8));
    }

    /// Un fichero tocado a mano con ids enormes no hace saltar nada.
    #[test]
    fn huge_ids_do_not_panic() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join(FILE);
        let max = u64::MAX;
        std::fs::write(
            &file,
            format!(r#"{{"notes": [{{"id": {max}, "text": "x", "at": 1}}], "next_note": {max}}}"#),
        )
        .unwrap();
        let (m, _) = mem(dir.path());
        assert_eq!(m.add_note("y"), Ok(max));
        assert_eq!(m.add_note("z"), Ok(max));
        assert_eq!(m.get().notes.len(), 3);
    }

    #[test]
    fn deleting_the_file_forgets_everything() {
        let dir = tempfile::tempdir().unwrap();
        let (m, _) = mem(dir.path());
        m.add_note("una").unwrap();
        m.put_call("c1", CallKind::Resumen, "r").unwrap();
        m.set_resume("h").unwrap();
        std::fs::remove_file(dir.path().join(FILE)).unwrap();
        assert_eq!(m.get(), Snapshot::default());
        assert_eq!(m.add_note("otra"), Ok(1));
        assert_eq!(texts(&m.get()), vec!["otra"]);
    }

    #[test]
    fn json_shape() {
        let dir = tempfile::tempdir().unwrap();
        let (m, _) = mem(dir.path());
        assert_eq!(
            serde_json::to_value(m.get()).unwrap(),
            serde_json::json!({"notes": [], "calls": [], "resume": null})
        );
        m.add_note("n").unwrap();
        m.put_call("c1", CallKind::Bitacora, "b").unwrap();
        m.put_call("c2", CallKind::Resumen, "r").unwrap();
        m.set_resume("h").unwrap();
        assert_eq!(
            serde_json::to_value(m.get()).unwrap(),
            serde_json::json!({
                "notes": [{"id": 1, "text": "n", "at": 1000}],
                "calls": [
                    {"id": "c1", "at": 1000, "kind": "bitácora", "text": "b"},
                    {"id": "c2", "at": 1000, "kind": "resumen", "text": "r"},
                ],
                "resume": {"handle": "h", "at": 1000},
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
        m.set_resume("h").unwrap();
        assert_eq!(files(&state), only);
        m.clear_resume().unwrap();
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
        assert_eq!(m.get(), Snapshot::default());
        for e in [
            m.add_note("una").unwrap_err(),
            m.put_call("c1", CallKind::Resumen, "r").unwrap_err(),
            m.set_resume("h").unwrap_err(),
        ] {
            assert!(matches!(e, MemoryError::Save(_)), "{e:?}");
            assert!(
                e.to_string().starts_with("no se pudo guardar la memoria: "),
                "{e}"
            );
        }
        // Lo que no cambia nada no escribe (ni falla).
        assert_eq!(m.delete_note(1), Ok(false));
        assert_eq!(m.clear_resume(), Ok(()));
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
        assert_eq!(m.get(), Snapshot::default());
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
            MemoryError::BadHandle,
            MemoryError::Read("e".into()),
            MemoryError::Save("e".into()),
        ] {
            let es = e.to_string();
            assert_ne!(crate::i18n::english(&es), es, "sin traducir: {es}");
        }
    }
}
