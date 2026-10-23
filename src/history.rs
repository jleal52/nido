//! Historial de una sesión: las líneas que salen por arriba de la pantalla, con sus colores.
//!
//! Es nuestro, no del programa: un `ESC[3J` (con el que Claude Code borra el historial al
//! repintarse) solo deja una marca. Se guarda en disco para leerlo tras un reinicio.

use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, BufWriter, Seek, Write};
use std::path::{Path, PathBuf};

/// Las marcas empiezan por este carácter, que no aparece en la salida de un terminal.
const MARK: char = '\u{1}';

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Marker {
    /// El programa mandó borrar el historial (`ESC[3J`), normalmente para repintarlo entero.
    Reset,
    /// La sesión volvió a arrancar (tras parar o reiniciar el equipo).
    Restart,
}

impl Marker {
    fn tag(self) -> &'static str {
        match self {
            Marker::Reset => "reset",
            Marker::Restart => "restart",
        }
    }
}

pub struct History {
    lines: VecDeque<String>,
    bytes: usize,
    max_bytes: usize,
    file: Option<(PathBuf, BufWriter<File>, u64)>,
}

impl History {
    pub fn in_memory(max_bytes: usize) -> Self {
        Self {
            lines: VecDeque::new(),
            bytes: 0,
            max_bytes,
            file: None,
        }
    }

    /// Carga la cola del fichero (si existe), pone una marca de rearranque si había algo y
    /// sigue escribiendo en él.
    pub fn open(path: &Path, max_bytes: usize) -> io::Result<Self> {
        let mut h = Self::in_memory(max_bytes);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        if let Ok(f) = File::open(path) {
            for line in BufReader::new(f).lines() {
                h.push_mem(line?);
            }
        }
        let had_content = !h.lines.is_empty();
        let mut opts = OpenOptions::new();
        opts.create(true).append(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
        let file = opts.open(path)?;
        let len = file.metadata()?.len();
        h.file = Some((path.to_owned(), BufWriter::new(file), len));
        h.compact_file()?;
        if had_content {
            h.mark(Marker::Restart);
        }
        Ok(h)
    }

    /// Solo leer (la sesión está parada): sin marca de rearranque ni fichero abierto.
    pub fn open_read_only(path: &Path) -> io::Result<Self> {
        let mut h = Self::in_memory(8 * 1024 * 1024);
        for line in BufReader::new(File::open(path)?).lines() {
            h.push_mem(line?);
        }
        Ok(h)
    }

    fn push_mem(&mut self, line: String) {
        self.bytes += line.len() + 1;
        self.lines.push_back(line);
        while self.bytes > self.max_bytes {
            match self.lines.pop_front() {
                Some(old) => self.bytes -= old.len() + 1,
                None => break,
            }
        }
    }

    pub fn push_line(&mut self, line: String) {
        if let Some((_, w, len)) = &mut self.file {
            // Si el disco falla, el historial en memoria sigue: no se tumba la sesión por eso.
            if writeln!(w, "{line}").is_ok() {
                *len += line.len() as u64 + 1;
            }
        }
        self.push_mem(line);
        if self
            .file
            .as_ref()
            .is_some_and(|(_, _, len)| *len > 2 * self.max_bytes as u64)
        {
            let _ = self.compact_file();
        }
    }

    pub fn mark(&mut self, marker: Marker) {
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        self.push_line(format!("{MARK}{} {secs}", marker.tag()));
    }

    /// Reescribe el fichero solo con lo que hay en memoria (la cola) cuando ha crecido mucho.
    fn compact_file(&mut self) -> io::Result<()> {
        let Some((path, w, len)) = &mut self.file else {
            return Ok(());
        };
        if *len <= 2 * self.max_bytes as u64 {
            return Ok(());
        }
        w.flush()?;
        let tmp = path.with_extension("tmp");
        {
            let mut out = BufWriter::new(File::create(&tmp)?);
            for l in &self.lines {
                writeln!(out, "{l}")?;
            }
            out.flush()?;
        }
        std::fs::rename(&tmp, &*path)?;
        let mut file = OpenOptions::new().append(true).open(&*path)?;
        let new_len = file.seek(io::SeekFrom::End(0))?;
        *w = BufWriter::new(file);
        *len = new_len;
        Ok(())
    }

    pub fn flush(&mut self) -> io::Result<()> {
        match &mut self.file {
            Some((_, w, _)) => w.flush(),
            None => Ok(()),
        }
    }

    /// El historial como texto con colores, una línea por fila. Con `all = false`, solo desde la
    /// última marca (lo que el programa considera su historial actual). Las marcas salen como
    /// una línea `\u{1}reset <epoch>` / `\u{1}restart <epoch>` que la web dibuja como separador.
    pub fn text(&self, all: bool) -> String {
        let start = if all {
            0
        } else {
            self.lines
                .iter()
                .rposition(|l| l.starts_with(MARK))
                .map_or(0, |i| i + 1)
        };
        let mut out = String::new();
        for l in self.lines.iter().skip(start) {
            out.push_str(l);
            out.push('\n');
        }
        out
    }

    pub fn len(&self) -> usize {
        self.lines.len()
    }

    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }
}

impl Drop for History {
    fn drop(&mut self) {
        let _ = self.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trims_from_the_front() {
        let mut h = History::in_memory(20);
        for i in 0..10 {
            h.push_line(format!("linea{i}"));
        }
        // 7 bytes por línea con su salto: caben 2.
        assert_eq!(h.text(true), "linea8\nlinea9\n");
    }

    #[test]
    fn markers_split_current_from_older() {
        let mut h = History::in_memory(1 << 20);
        h.push_line("vieja".into());
        h.mark(Marker::Reset);
        h.push_line("nueva".into());
        assert_eq!(h.text(false), "nueva\n");
        let all = h.text(true);
        assert!(
            all.starts_with("vieja\n\u{1}reset ") && all.ends_with("\nnueva\n"),
            "{all:?}"
        );
    }

    #[test]
    fn persists_and_marks_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s/history.ans");
        {
            let mut h = History::open(&path, 1 << 20).unwrap();
            h.push_line("\x1b[32mantes\x1b[m".into());
        }
        let mut h = History::open(&path, 1 << 20).unwrap();
        h.push_line("despues".into());
        assert_eq!(h.text(false), "despues\n");
        let all = h.text(true);
        assert!(
            all.starts_with("\x1b[32mantes\x1b[m\n\u{1}restart "),
            "{all:?}"
        );
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn compacts_the_file_when_it_grows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("h.ans");
        let mut h = History::open(&path, 100).unwrap();
        for i in 0..200 {
            h.push_line(format!("linea {i:03}"));
        }
        h.flush().unwrap();
        let size = std::fs::metadata(&path).unwrap().len();
        assert!(size <= 200 + 10, "el fichero no se compactó: {size} bytes");
        let reloaded = History::open(&path, 100).unwrap();
        assert!(reloaded.text(true).contains("linea 199"));
    }
}
