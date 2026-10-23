//! Emulador de terminal de una sesión: la pantalla actual (para repintar al conectar) y el
//! historial propio (las filas que salen por arriba).

use std::num::NonZeroU16;

use vt100::capture::{BasicFormattedCaptureRange, BasicFormattedCaptureState, RowContents};

use crate::history::{History, Marker};

/// Recibe de vt100 cada fila que sale por arriba de la pantalla.
pub struct Capture {
    pub history: History,
    state: BasicFormattedCaptureState,
    /// La línea en curso: una fila partida por el ancho sigue en la siguiente.
    pending: String,
}

impl vt100::Callbacks for Capture {
    fn on_scroll(&mut self, row: RowContents<'_>, alternate_screen: bool) {
        // La pantalla alternativa (vim, less…) no es historial.
        if alternate_screen {
            return;
        }
        let _ = row.write_formatted_basic(&mut self.pending, &mut self.state);
        // Cada '\n' cierra una línea; lo que queda detrás sigue abierto.
        while let Some(i) = self.pending.find('\n') {
            let rest = self.pending.split_off(i + 1);
            let mut line = std::mem::replace(&mut self.pending, rest);
            line.pop();
            self.history.push_line(line);
        }
    }
}

pub struct Emu {
    parser: vt100::Parser<Capture>,
    /// Los últimos bytes del trozo anterior, por si un `ESC[3J` llega partido en dos.
    tail: Vec<u8>,
}

fn nz(v: u16) -> NonZeroU16 {
    NonZeroU16::new(v.max(1)).expect("max(1) nunca es cero")
}

/// `ESC[3J` y su variante `ESC[?3J`: el programa pide borrar el historial. Solo cuentan las que
/// terminan después de `old` (los bytes que ya se miraron la vez anterior).
fn find_clear_history(buf: &[u8], old: usize) -> bool {
    const SEQS: [&[u8]; 2] = [b"\x1b[3J", b"\x1b[?3J"];
    SEQS.iter().any(|s| {
        buf.windows(s.len())
            .enumerate()
            .any(|(i, w)| w == *s && i + s.len() > old)
    })
}

impl Emu {
    pub fn new(cols: u16, rows: u16, history: History) -> Self {
        let capture = Capture {
            history,
            state: BasicFormattedCaptureState::new(),
            pending: String::new(),
        };
        // Sin historial de vt100: el único es el nuestro, que no borra nadie.
        let parser = vt100::Parser::new_with_callbacks(nz(rows), nz(cols), 0, capture);
        Self {
            parser,
            tail: Vec::new(),
        }
    }

    pub fn process(&mut self, bytes: &[u8]) {
        let mut probe = std::mem::take(&mut self.tail);
        let old = probe.len();
        probe.extend_from_slice(bytes);
        if find_clear_history(&probe, old) {
            self.parser.callbacks_mut().history.mark(Marker::Reset);
        }
        let keep = probe.len().min(4);
        self.tail = probe.split_off(probe.len() - keep);
        self.parser.process(bytes);
    }

    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.parser.set_size(nz(rows), nz(cols));
    }

    pub fn size(&self) -> (u16, u16) {
        let (rows, cols) = self.parser.screen().size();
        (cols.get(), rows.get())
    }

    /// Lo que hay que mandar a un terminal recién conectado para que vea lo mismo: borrar,
    /// pintar la pantalla y poner los modos (cursor, pegado entre corchetes, ratón…).
    pub fn snapshot(&self) -> Vec<u8> {
        let mut out = b"\x1b[H\x1b[2J\x1b[m".to_vec();
        out.extend_from_slice(self.parser.screen().state_formatted().as_bytes());
        out
    }

    /// Historial más la pantalla actual, como texto con colores (una línea por fila).
    pub fn history_text(&self, all: bool) -> String {
        let cap = self.parser.callbacks();
        let mut out = cap.history.text(all);
        if !cap.pending.is_empty() {
            out.push_str(&cap.pending);
            out.push('\n');
        }
        let screen = self.parser.screen();
        // En la pantalla alternativa la "pantalla" es la del programa (vim…): también cuenta.
        let _ =
            screen.write_contents_formatted_basic(&mut out, BasicFormattedCaptureRange::Visible);
        out.truncate(out.trim_end().len());
        out
    }

    /// Al terminar la sesión: lo que queda en pantalla también pasa al historial, para verlo
    /// tras volver a arrancarla.
    pub fn finish(&mut self) {
        let mut screen_text = String::new();
        let _ = self
            .parser
            .screen()
            .write_contents_formatted_basic(&mut screen_text, BasicFormattedCaptureRange::Visible);
        let cap = self.parser.callbacks_mut();
        let pending = std::mem::take(&mut cap.pending);
        if !pending.is_empty() {
            cap.history.push_line(pending);
        }
        for line in screen_text.trim_end().lines() {
            cap.history.push_line(line.to_owned());
        }
        let _ = cap.history.flush();
    }

    /// Solo la pantalla actual, con colores.
    pub fn screen_text(&self) -> String {
        let mut out = String::new();
        let _ = self
            .parser
            .screen()
            .write_contents_formatted_basic(&mut out, BasicFormattedCaptureRange::Visible);
        out.truncate(out.trim_end().len());
        out
    }

    pub fn history_mut(&mut self) -> &mut History {
        &mut self.parser.callbacks_mut().history
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn emu() -> Emu {
        Emu::new(40, 10, History::in_memory(1 << 20))
    }

    fn plain(s: &str) -> String {
        // Quita las secuencias SGR para comparar texto.
        let mut out = String::new();
        let mut it = s.chars().peekable();
        while let Some(c) = it.next() {
            if c == '\x1b' {
                for n in it.by_ref() {
                    if n.is_ascii_alphabetic() {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    }

    #[test]
    fn lines_that_scroll_off_go_to_history_with_colour() {
        let mut e = emu();
        for i in 0..100 {
            e.process(format!("\x1b[32mlinea {i}\x1b[m\r\n").as_bytes());
        }
        // 10 filas de pantalla: 91 han salido por arriba (la última sigue abierta por si
        // continuaba en la siguiente fila); el resto está en pantalla.
        assert_eq!(e.parser.callbacks().history.len(), 90);
        let text = e.history_text(true);
        assert!(text.contains("\x1b[32m"), "sin color: {text:?}");
        let expected: Vec<String> = (0..100).map(|i| format!("linea {i}")).collect();
        assert_eq!(plain(&text).lines().collect::<Vec<_>>(), expected);
    }

    #[test]
    fn wrapped_rows_are_one_line() {
        let mut e = emu();
        let long = "x".repeat(100);
        e.process(format!("{long}\r\n").as_bytes());
        for _ in 0..12 {
            e.process(b"\r\n");
        }
        let p = plain(&e.history_text(true));
        assert!(
            p.lines().any(|l| l == long),
            "la línea partida no se unió: {p:?}"
        );
    }

    #[test]
    fn clear_history_only_marks_even_if_split() {
        let mut e = emu();
        for i in 0..30 {
            e.process(format!("antes {i}\r\n").as_bytes());
        }
        // ESC[3J partido entre dos trozos.
        e.process(b"\x1b[2J\x1b[");
        e.process(b"3J\x1b[H");
        for i in 0..30 {
            e.process(format!("despues {i}\r\n").as_bytes());
        }
        let all = plain(&e.history_text(true));
        assert!(
            all.contains("antes 0") && all.contains("despues 29"),
            "{all:?}"
        );
        assert!(all.contains("\u{1}reset "), "falta la marca: {all:?}");
        let current = plain(&e.history_text(false));
        assert!(
            !current.contains("antes 0") && current.contains("despues 0"),
            "{current:?}"
        );
    }

    #[test]
    fn clear_history_at_the_end_of_a_chunk_counts_once() {
        let mut e = emu();
        e.process(b"a\x1b[3J");
        e.process(b"b");
        let all = e.history_text(true);
        assert_eq!(all.matches("\u{1}reset").count(), 1, "{all:?}");
    }

    #[test]
    fn finish_keeps_the_screen() {
        let mut e = emu();
        e.process(b"ultima pantalla\r\n");
        e.finish();
        assert!(
            e.parser
                .callbacks()
                .history
                .text(true)
                .contains("ultima pantalla")
        );
    }

    #[test]
    fn alternate_screen_is_not_history() {
        let mut e = emu();
        e.process(b"\x1b[?1049h");
        for i in 0..30 {
            e.process(format!("vim {i}\r\n").as_bytes());
        }
        e.process(b"\x1b[?1049l");
        assert_eq!(e.parser.callbacks().history.len(), 0);
    }

    #[test]
    fn snapshot_repaints_the_screen() {
        let mut e = emu();
        e.process(b"hola \x1b[1mnegrita\x1b[m");
        let mut other = vt100::Parser::new(nz(10), nz(40), 0);
        other.process(&e.snapshot());
        assert_eq!(other.screen().contents(), e.parser.screen().contents());
        assert_eq!(
            other.screen().cursor_position(),
            e.parser.screen().cursor_position()
        );
    }
}
