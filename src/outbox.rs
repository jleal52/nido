//! Lo que las sesiones mandan al usuario con el MCP de nido: textos para copiar (`clipboard`) y
//! copias de ficheros (`send_file`), en `<state>/outbox/<sesión>/` (0700; ficheros 0600) con su
//! índice (`items.json`). La web y la app los piden por el canal cifrado al pulsar el botón; el
//! push solo lleva el título.
//!
//! Las copias se borran a los 7 días o al cerrar la sesión, y entre todas no pasan de 256 MB por
//! equipo: al pasarlo, fuera las más antiguas. Lo borrado sigue en el índice, caducado.

use std::io::{self, Read};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::registry::NoteKind;

/// Lo más que ocupa un fichero.
pub const MAX_FILE: u64 = 64 * 1024 * 1024;
/// Lo más que ocupa un texto.
pub const MAX_TEXT: usize = 64 * 1024;
/// Lo que dura una copia (ms).
pub const KEEP_MS: u64 = 7 * 24 * 3600 * 1000;
/// Lo más que ocupan todas las copias de este equipo.
pub const CAP: u64 = 256 * 1024 * 1024;
/// Los envíos que se recuerdan por sesión.
const MAX_ITEMS: usize = 50;
/// Lo más largo de un título (caracteres).
const MAX_TITLE: usize = 80;

/// Un envío, como lo ven la web y la app (`GET /api/v1/sessions/{id}/items`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Item {
    pub id: String,
    /// `clipboard` o `file`.
    pub kind: NoteKind,
    pub title: String,
    /// El nombre del fichero (sin carpeta).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub size: u64,
    /// Cuándo (ms).
    pub at: u64,
    /// Ya no está (7 días, el tope o se cerró la sesión).
    #[serde(default)]
    pub expired: bool,
}

/// Por qué no se pudo mandar un fichero. Los textos, en español (ver `i18n::TABLE`).
#[derive(Debug)]
pub enum SendError {
    NotFound(PathBuf),
    Unreadable(PathBuf, io::Error),
    IsDir(PathBuf),
    /// Una tubería, un dispositivo…: solo ficheros normales.
    NotAFile(PathBuf),
    /// Está en la carpeta de secretos de nido.
    Secret(PathBuf),
    TooBig(PathBuf, u64),
    Io(io::Error),
}

impl std::fmt::Display for SendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SendError::NotFound(p) => write!(f, "no existe el fichero {}", p.display()),
            SendError::Unreadable(p, e) => write!(f, "no se puede leer {}: {e}", p.display()),
            SendError::IsDir(p) => write!(f, "{} es una carpeta", p.display()),
            SendError::NotAFile(p) => write!(f, "{} no es un fichero normal", p.display()),
            SendError::Secret(p) => write!(
                f,
                "{} está en la carpeta de secretos de nido: no se manda",
                p.display()
            ),
            SendError::TooBig(p, n) => {
                write!(
                    f,
                    "{} pasa de 64 MB ({:.1} MB)",
                    p.display(),
                    *n as f64 / 1048576.0
                )
            }
            SendError::Io(e) => write!(f, "no se pudo guardar la copia: {e}"),
        }
    }
}

/// Caracteres de formato de Unicode (categoría `Cf`): no se ven, pero cambian lo que se ve (el
/// sentido del texto: `informe\u{202E}fdp.exe` se lee «informeexe.pdf») o lo esconden.
fn is_format(c: char) -> bool {
    matches!(
        c,
        '\u{AD}'
            | '\u{600}'..='\u{605}'
            | '\u{61C}'
            | '\u{6DD}'
            | '\u{70F}'
            | '\u{890}'..='\u{891}'
            | '\u{8E2}'
            | '\u{180E}'
            | '\u{200B}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{206F}'
            | '\u{FEFF}'
            | '\u{FFF9}'..='\u{FFFB}'
            | '\u{110BD}'
            | '\u{110CD}'
            | '\u{13430}'..='\u{1343F}'
            | '\u{1BCA0}'..='\u{1BCA3}'
            | '\u{1D173}'..='\u{1D17A}'
            | '\u{E0001}'
            | '\u{E0020}'..='\u{E007F}'
    )
}

/// Un título: una línea sin caracteres de control ni de formato (los tabuladores, espacios),
/// hasta 80 caracteres.
fn clean_title(s: &str) -> Option<String> {
    let s: String = s
        .chars()
        .filter(|&c| !is_format(c))
        .map(|c| if c == '\t' { ' ' } else { c })
        .collect();
    let t = crate::server::alert_text(&s)?;
    let first = t.lines().map(str::trim).find(|l| !l.is_empty())?;
    Some(
        first
            .chars()
            .take(MAX_TITLE)
            .collect::<String>()
            .trim_end()
            .to_owned(),
    )
}

fn title_of(title: Option<&str>, fallback: &str) -> String {
    title
        .and_then(clean_title)
        .or_else(|| clean_title(fallback))
        .unwrap_or_else(|| "texto".to_owned())
}

/// El tipo con el que se sirve un fichero: solo los que no ejecutan nada al abrirlos en el
/// navegador; los demás, como bytes (se descargan).
pub fn content_type(name: &str) -> &'static str {
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase());
    match ext.as_deref() {
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("pdf") => "application/pdf",
        Some("txt" | "log" | "md" | "csv" | "json" | "yaml" | "yml" | "toml") => {
            "text/plain; charset=utf-8"
        }
        Some("mp4") => "video/mp4",
        Some("mp3") => "audio/mpeg",
        _ => "application/octet-stream",
    }
}

/// `Content-Disposition` para descargar `name`: en ASCII seguro y, entero, en UTF-8.
pub fn disposition(name: &str) -> String {
    let ascii = crate::upload::safe_name(Some(name), None);
    let mut pct = String::new();
    for b in name.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            pct.push(b as char);
        } else {
            pct.push_str(&format!("%{b:02X}"));
        }
    }
    format!("attachment; filename=\"{ascii}\"; filename*=UTF-8''{pct}")
}

/// La bandeja de salida de este equipo, en `<state>/outbox`.
pub struct Outbox {
    dir: PathBuf,
    /// La carpeta de los secretos (`secrets.rs`): de ahí no se manda nada.
    secrets: PathBuf,
    /// El tope (las pruebas lo bajan).
    cap: std::sync::atomic::AtomicU64,
    /// Una escritura del índice a la vez.
    lock: std::sync::Mutex<()>,
}

/// Abre `path` para leerlo. En Unix sin esperar: una tubería sin nadie al otro lado no deja a
/// nido colgado (y luego se rechaza: no es un fichero normal).
fn open_read(path: &Path) -> io::Result<std::fs::File> {
    let mut opts = std::fs::OpenOptions::new();
    opts.read(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::custom_flags(&mut opts, libc::O_NONBLOCK);
    opts.open(path)
}

fn valid_session(id: &str) -> bool {
    crate::mcp::session_from(Some(id.to_owned())).is_some()
}

impl Outbox {
    pub fn new(dir: PathBuf, secrets: PathBuf) -> Self {
        Self {
            dir,
            secrets,
            cap: CAP.into(),
            lock: std::sync::Mutex::new(()),
        }
    }

    /// Para las pruebas.
    pub fn set_cap(&self, bytes: u64) {
        self.cap.store(bytes, std::sync::atomic::Ordering::SeqCst);
    }

    fn session_dir(&self, session: &str) -> PathBuf {
        self.dir.join(session)
    }

    fn index(&self, session: &str) -> PathBuf {
        self.session_dir(session).join("items.json")
    }

    pub fn list(&self, session: &str) -> Vec<Item> {
        self.try_list(session).unwrap_or_default()
    }

    /// El índice de la sesión; `None` si está pero no se puede leer.
    fn try_list(&self, session: &str) -> Option<Vec<Item>> {
        if !valid_session(session) {
            return Some(Vec::new());
        }
        match std::fs::read(self.index(session)) {
            Ok(b) => serde_json::from_slice(&b).ok(),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Some(Vec::new()),
            Err(_) => None,
        }
    }

    fn save_list(&self, session: &str, list: &[Item]) -> io::Result<()> {
        let json = serde_json::to_vec(list).map_err(io::Error::other)?;
        crate::secrets::write_private(&self.index(session), &json)
    }

    /// La carpeta de la sesión, creada y solo para el usuario.
    fn ready(&self, session: &str) -> io::Result<PathBuf> {
        if !valid_session(session) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "sesión no válida",
            ));
        }
        let dir = self.session_dir(session);
        crate::sys::create_private_dir_all(&dir)?;
        crate::sys::private_dir(&self.dir)?;
        crate::sys::private_dir(&dir)?;
        Ok(dir)
    }

    /// Apunta `item` (con su contenido ya en la carpeta), deja los 50 últimos y aplica el tope.
    fn push(&self, session: &str, item: Item) -> io::Result<Item> {
        {
            let _one = self.lock.lock().expect("bandeja de salida");
            let mut list = self.list(session);
            list.push(item.clone());
            let extra = list.len().saturating_sub(MAX_ITEMS);
            for gone in list.drain(..extra) {
                let _ = std::fs::remove_file(self.session_dir(session).join(&gone.id));
            }
            self.save_list(session, &list)?;
        }
        // Coge el cerrojo él: no anidar.
        self.enforce_cap();
        Ok(item)
    }

    pub fn add_text(
        &self,
        session: &str,
        text: &str,
        title: Option<&str>,
        now: u64,
    ) -> io::Result<Item> {
        let dir = self.ready(session)?;
        let id = crate::registry::new_id();
        crate::secrets::write_private(&dir.join(&id), text.as_bytes())?;
        self.push(
            session,
            Item {
                id,
                kind: NoteKind::Clipboard,
                title: title_of(title, text),
                name: None,
                size: text.len() as u64,
                at: now,
                expired: false,
            },
        )
    }

    pub fn add_file(
        &self,
        session: &str,
        path: &Path,
        title: Option<&str>,
        now: u64,
    ) -> Result<Item, SendError> {
        // Dónde está de verdad (sin enlaces ni `..`): de la carpeta de secretos, nada.
        let real = match std::fs::canonicalize(path) {
            Ok(p) => p,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Err(SendError::NotFound(path.to_owned()));
            }
            Err(e) => return Err(SendError::Unreadable(path.to_owned(), e)),
        };
        if self.in_secrets(&real) {
            return Err(SendError::Secret(path.to_owned()));
        }
        // Primero se abre y luego se mira qué es lo abierto: entre mirar y abrir no puede
        // cambiar.
        let src = match open_read(&real) {
            Ok(f) => f,
            // En Windows una carpeta no se abre como un fichero.
            Err(_) if real.is_dir() => return Err(SendError::IsDir(path.to_owned())),
            Err(e) => return Err(SendError::Unreadable(path.to_owned(), e)),
        };
        let meta = src
            .metadata()
            .map_err(|e| SendError::Unreadable(path.to_owned(), e))?;
        if meta.is_dir() {
            return Err(SendError::IsDir(path.to_owned()));
        }
        if !meta.is_file() {
            return Err(SendError::NotAFile(path.to_owned()));
        }
        if meta.len() > MAX_FILE {
            return Err(SendError::TooBig(path.to_owned(), meta.len()));
        }
        let dir = self.ready(session).map_err(SendError::Io)?;
        let id = crate::registry::new_id();
        let dst = dir.join(&id);
        let mut out = crate::sys::private(std::fs::OpenOptions::new().write(true).create_new(true))
            .open(&dst)
            .map_err(SendError::Io)?;
        // Por si crece mientras se copia: como mucho un byte de más, para saberlo.
        let copied = io::copy(&mut src.take(MAX_FILE + 1), &mut out);
        let n = match copied {
            Ok(n) if n > MAX_FILE => {
                let _ = std::fs::remove_file(&dst);
                return Err(SendError::TooBig(path.to_owned(), n));
            }
            Ok(n) => n,
            Err(e) => {
                let _ = std::fs::remove_file(&dst);
                return Err(SendError::Unreadable(path.to_owned(), e));
            }
        };
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "fichero".to_owned());
        self.push(
            session,
            Item {
                id,
                kind: NoteKind::File,
                title: title_of(title, &name),
                name: Some(name),
                size: n,
                at: now,
                expired: false,
            },
        )
        .map_err(SendError::Io)
    }

    /// `real` (ya sin enlaces) está en la carpeta de secretos.
    fn in_secrets(&self, real: &Path) -> bool {
        real.starts_with(&self.secrets)
            || std::fs::canonicalize(&self.secrets).is_ok_and(|s| real.starts_with(s))
    }

    /// Un envío que aún está, con la ruta de su contenido.
    pub fn open(&self, session: &str, id: &str) -> Option<(Item, PathBuf)> {
        let item = self
            .list(session)
            .into_iter()
            .find(|i| i.id == id && !i.expired)?;
        let path = self.session_dir(session).join(&item.id);
        path.is_file().then_some((item, path))
    }

    /// La sesión se cerró: fuera todo lo suyo.
    pub fn forget(&self, session: &str) {
        if valid_session(session) {
            let _one = self.lock.lock().expect("bandeja de salida");
            let _ = std::fs::remove_dir_all(self.session_dir(session));
        }
    }

    /// Fuera lo de las sesiones que ya no existen (`live`) y lo de más de 7 días; luego el tope.
    /// Y lo que dejó una caída: copias que no llegaron al índice y temporales, de más de una hora
    /// (las de menos pueden ser una copia en curso).
    pub fn prune(&self, now: u64, live: impl Fn(&str) -> bool) {
        let Ok(dirs) = std::fs::read_dir(&self.dir) else {
            return;
        };
        let one = self.lock.lock().expect("bandeja de salida");
        for d in dirs.flatten() {
            let id = d.file_name().to_string_lossy().into_owned();
            if !live(&id) {
                let _ = std::fs::remove_dir_all(d.path());
                continue;
            }
            // Si el índice está pero no se puede leer, no se borra nada por él.
            let Some(mut list) = self.try_list(&id) else {
                continue;
            };
            let mut changed = false;
            for it in list
                .iter_mut()
                .filter(|i| !i.expired && now.saturating_sub(i.at) > KEEP_MS)
            {
                let _ = std::fs::remove_file(d.path().join(&it.id));
                it.expired = true;
                changed = true;
            }
            if changed {
                let _ = self.save_list(&id, &list);
            }
            for f in std::fs::read_dir(d.path()).into_iter().flatten().flatten() {
                let name = f.file_name().to_string_lossy().into_owned();
                let known = name == "items.json" || list.iter().any(|i| i.id == name && !i.expired);
                if !known && crate::secrets::older_than(&f.path(), now, crate::secrets::TMP_KEEP_MS)
                {
                    let _ = std::fs::remove_file(f.path());
                }
            }
        }
        // `enforce_cap` coge el cerrojo: no anidar.
        drop(one);
        self.enforce_cap();
    }

    /// Entre todas las sesiones, como mucho `cap` bytes: fuera las más antiguas.
    fn enforce_cap(&self) {
        let _one = self.lock.lock().expect("bandeja de salida");
        let cap = self.cap.load(std::sync::atomic::Ordering::SeqCst);
        let Ok(dirs) = std::fs::read_dir(&self.dir) else {
            return;
        };
        let mut all: Vec<(String, Item)> = dirs
            .flatten()
            .map(|d| d.file_name().to_string_lossy().into_owned())
            .flat_map(|s| self.list(&s).into_iter().map(move |i| (s.clone(), i)))
            .filter(|(_, i)| !i.expired)
            .collect();
        let mut total: u64 = all.iter().map(|(_, i)| i.size).sum();
        if total <= cap {
            return;
        }
        all.sort_by_key(|(_, i)| i.at);
        for (s, it) in all {
            if total <= cap {
                break;
            }
            let _ = std::fs::remove_file(self.session_dir(&s).join(&it.id));
            let mut list = self.list(&s);
            if let Some(x) = list.iter_mut().find(|x| x.id == it.id) {
                x.expired = true;
            }
            let _ = self.save_list(&s, &list);
            total -= it.size;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn outbox() -> (tempfile::TempDir, Outbox) {
        let tmp = tempfile::tempdir().unwrap();
        let o = Outbox::new(tmp.path().join("outbox"), tmp.path().join("secrets"));
        (tmp, o)
    }

    #[test]
    fn text_with_and_without_title() {
        let (_tmp, o) = outbox();
        let a = o
            .add_text("aaaa1111", "git push\nde verdad", None, 10)
            .unwrap();
        assert_eq!(
            (a.kind, a.title.as_str(), a.size),
            (NoteKind::Clipboard, "git push", 18)
        );
        let b = o
            .add_text("aaaa1111", "x", Some("  la orden \n"), 11)
            .unwrap();
        assert_eq!(b.title, "la orden");
        let (item, path) = o.open("aaaa1111", &a.id).unwrap();
        assert_eq!(item, a);
        assert_eq!(
            std::fs::read_to_string(path).unwrap(),
            "git push\nde verdad"
        );
        assert_eq!(o.list("aaaa1111"), [a, b]);
        assert!(o.list("bbbb2222").is_empty());
        assert!(o.open("bbbb2222", &o.list("aaaa1111")[0].id).is_none());
    }

    #[test]
    fn files_are_copied_up_to_64_mb() {
        let (tmp, o) = outbox();
        let src = tmp.path().join("informe.pdf");
        std::fs::write(&src, b"%PDF-1.7").unwrap();
        let it = o.add_file("aaaa1111", &src, None, 1).unwrap();
        assert_eq!(it.name.as_deref(), Some("informe.pdf"));
        assert_eq!((it.title.as_str(), it.size), ("informe.pdf", 8));
        // Es una copia: cambiar el original no la toca.
        std::fs::write(&src, b"otro").unwrap();
        let (_, copy) = o.open("aaaa1111", &it.id).unwrap();
        assert_eq!(std::fs::read(copy).unwrap(), b"%PDF-1.7");
        // Los errores del diseño.
        let none = tmp.path().join("no-existe");
        assert!(matches!(
            o.add_file("aaaa1111", &none, None, 1),
            Err(SendError::NotFound(_))
        ));
        assert!(matches!(
            o.add_file("aaaa1111", tmp.path(), None, 1),
            Err(SendError::IsDir(_))
        ));
        let big = tmp.path().join("grande.bin");
        std::fs::File::create(&big)
            .unwrap()
            .set_len(MAX_FILE + 1)
            .unwrap();
        match o.add_file("aaaa1111", &big, None, 1) {
            Err(e @ SendError::TooBig(_, n)) => {
                assert_eq!(n, MAX_FILE + 1);
                assert_eq!(
                    e.to_string(),
                    format!("{} pasa de 64 MB (64.0 MB)", big.display())
                );
            }
            other => panic!("{other:?}"),
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let locked = tmp.path().join("cerrado");
            std::fs::write(&locked, b"x").unwrap();
            std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
            // Como root se puede leer igual: solo si de verdad no se puede.
            if std::fs::File::open(&locked).is_err() {
                assert!(matches!(
                    o.add_file("aaaa1111", &locked, None, 1),
                    Err(SendError::Unreadable(..))
                ));
            }
        }
    }

    #[test]
    fn old_copies_expire_and_the_cap_drops_the_oldest() {
        let (tmp, o) = outbox();
        let src = tmp.path().join("a.txt");
        std::fs::write(&src, b"hola").unwrap();
        let viejo = o.add_file("aaaa1111", &src, None, 1_000).unwrap();
        let nuevo = o.add_file("aaaa1111", &src, None, 1_000 + KEEP_MS).unwrap();
        o.prune(1_000 + KEEP_MS + 1, |_| true);
        let list = o.list("aaaa1111");
        assert!(list.iter().find(|i| i.id == viejo.id).unwrap().expired);
        assert!(o.open("aaaa1111", &viejo.id).is_none());
        assert!(!list.iter().find(|i| i.id == nuevo.id).unwrap().expired);
        // Tope de 7 bytes: «hola» (4, el más antiguo vivo) y «abcd» (4) no caben juntos.
        o.set_cap(7);
        let tercero = o
            .add_text("bbbb2222", "abcd", None, 2_000 + KEEP_MS)
            .unwrap();
        assert!(
            o.open("aaaa1111", &nuevo.id).is_none(),
            "el más antiguo, fuera"
        );
        assert!(o.open("bbbb2222", &tercero.id).is_some());
        // Las sesiones que ya no existen, fuera enteras.
        o.prune(2_000 + KEEP_MS, |id| id != "bbbb2222");
        assert!(o.list("bbbb2222").is_empty());
        o.forget("aaaa1111");
        assert!(o.list("aaaa1111").is_empty());
    }

    #[test]
    fn index_keeps_the_last_50() {
        let (_tmp, o) = outbox();
        for i in 0..55 {
            o.add_text("aaaa1111", &format!("t{i}"), None, i).unwrap();
        }
        let list = o.list("aaaa1111");
        assert_eq!(list.len(), 50);
        assert_eq!(list[0].title, "t5");
    }

    #[test]
    fn content_types_never_run_anything() {
        assert_eq!(content_type("a.PNG"), "image/png");
        assert_eq!(content_type("a.pdf"), "application/pdf");
        assert_eq!(content_type("notas.md"), "text/plain; charset=utf-8");
        for risky in [
            "a.html",
            "a.htm",
            "a.svg",
            "a.xhtml",
            "a.js",
            "sin-extension",
        ] {
            assert_eq!(content_type(risky), "application/octet-stream", "{risky}");
        }
        assert_eq!(
            disposition("informe año.pdf"),
            "attachment; filename=\"informe_a_o.pdf\"; filename*=UTF-8''informe%20a%C3%B1o.pdf"
        );
        // Ni comillas ni saltos de línea: no se puede meter otra cabecera.
        assert_eq!(
            disposition("a\"\r\nX: y.pdf"),
            "attachment; filename=\"a___X__y.pdf\"; filename*=UTF-8''a%22%0D%0AX%3A%20y.pdf"
        );
    }

    #[test]
    fn titles_have_no_invisible_or_reversing_characters() {
        let (_tmp, o) = outbox();
        let t = |s: &str| o.add_text("aaaa1111", "x", Some(s), 1).unwrap().title;
        assert_eq!(t("informe\u{202E}fdp.exe"), "informefdp.exe");
        assert_eq!(t("\u{2066}a\u{2069}\u{200B}b\u{FEFF}c\u{00AD}"), "abc");
        assert_eq!(t("uno\tdos"), "uno dos");
        // Sin título: lo mismo con el nombre del fichero.
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("foto\u{202E}gnp.exe");
        std::fs::write(&src, b"x").unwrap();
        let it = o.add_file("aaaa1111", &src, None, 1).unwrap();
        assert_eq!(it.title, "fotognp.exe");
    }

    #[cfg(unix)]
    #[test]
    fn only_regular_files_are_sent_and_a_fifo_does_not_block() {
        let (tmp, o) = outbox();
        let fifo = tmp.path().join("tubo");
        let c = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        // Sin nadie al otro lado: abrirla sin `O_NONBLOCK` se quedaría esperando.
        let (tx, rx) = std::sync::mpsc::channel();
        let o2 = std::sync::Arc::new(o);
        let o3 = o2.clone();
        std::thread::spawn(move || {
            let _ = tx.send(o3.add_file("aaaa1111", &fifo, None, 1));
        });
        let got = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("se quedó esperando a la tubería");
        match got {
            Err(e @ SendError::NotAFile(_)) => assert!(e.to_string().contains("no es un fichero")),
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            o2.add_file("aaaa1111", Path::new("/dev/null"), None, 1),
            Err(SendError::NotAFile(_))
        ));
        assert!(o2.list("aaaa1111").is_empty());
    }

    #[test]
    fn nothing_from_the_secrets_folder() {
        let (tmp, o) = outbox();
        let secrets = crate::secrets::Store::new(tmp.path().join("secrets"));
        let p = secrets
            .save_team("gh", b"v", crate::secrets::Remember::Always, "api", 1)
            .unwrap();
        assert!(matches!(
            o.add_file("aaaa1111", &p, None, 1),
            Err(SendError::Secret(_))
        ));
        // Tampoco por otro camino: `..` o un enlace.
        let around = tmp.path().join("otra/../secrets/equipo/gh");
        std::fs::create_dir_all(tmp.path().join("otra")).unwrap();
        assert!(matches!(
            o.add_file("aaaa1111", &around, None, 1),
            Err(SendError::Secret(_))
        ));
        #[cfg(unix)]
        {
            let link = tmp.path().join("enlace");
            std::os::unix::fs::symlink(&p, &link).unwrap();
            match o.add_file("aaaa1111", &link, None, 1) {
                Err(e @ SendError::Secret(_)) => {
                    assert!(e.to_string().contains("secretos"), "{e}")
                }
                other => panic!("{other:?}"),
            }
        }
        assert!(o.list("aaaa1111").is_empty());
    }

    #[test]
    fn prune_drops_copies_a_crash_left_behind() {
        let (tmp, o) = outbox();
        let src = tmp.path().join("a.txt");
        std::fs::write(&src, b"hola").unwrap();
        let now = std::time::SystemTime::now();
        let now_ms = now
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let it = o.add_file("aaaa1111", &src, None, now_ms).unwrap();
        let dir = tmp.path().join("outbox/aaaa1111");
        // Una copia a medias (no llegó al índice) y un temporal del índice, viejos; y una copia
        // que se está haciendo ahora.
        let half = dir.join("0a1b2c3d");
        let tmp_index = dir.join(".items.json.0a1b2c3d~tmp");
        let now_copying = dir.join("4e5f6a7b");
        for p in [&half, &tmp_index, &now_copying] {
            std::fs::write(p, b"x").unwrap();
        }
        for p in [&half, &tmp_index, &dir.join(&it.id)] {
            std::fs::File::options()
                .write(true)
                .open(p)
                .unwrap()
                .set_modified(now - Duration::from_secs(2 * 3600))
                .unwrap();
        }
        o.prune(now_ms, |_| true);
        assert!(!half.exists() && !tmp_index.exists());
        assert!(now_copying.exists());
        assert!(o.open("aaaa1111", &it.id).is_some());
    }
}
