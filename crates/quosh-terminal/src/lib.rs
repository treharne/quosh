//! Quosh-owned adapter over unmodified `alacritty_terminal`.
//! Captures `PtyWrite` (DSR/DA) and converts the grid to `FrameState`.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::Flags as CellFlags;
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::vte::ansi::{Color, NamedColor, Processor};
use blit_remote::{CELL_SIZE, FrameState};

#[derive(Clone)]
struct Proxy {
    pty_write: Arc<Mutex<Vec<u8>>>,
    title: Arc<Mutex<String>>,
}

impl EventListener for Proxy {
    fn send_event(&self, event: Event) {
        match event {
            Event::PtyWrite(s) => self
                .pty_write
                .lock()
                .unwrap()
                .extend_from_slice(s.as_bytes()),
            Event::Title(t) => *self.title.lock().unwrap() = t,
            Event::ResetTitle => self.title.lock().unwrap().clear(),
            Event::ClipboardStore(_, _) => {}
            _ => {}
        }
    }
}

struct TermDims {
    cols: usize,
    rows: usize,
}

impl Dimensions for TermDims {
    fn total_lines(&self) -> usize {
        self.rows
    }
    fn screen_lines(&self) -> usize {
        self.rows
    }
    fn columns(&self) -> usize {
        self.cols
    }
}

pub struct Emulator {
    term: Term<Proxy>,
    processor: Processor,
    proxy: Proxy,
    rows: u16,
    cols: u16,
}

impl Emulator {
    pub fn new(rows: u16, cols: u16, scrollback: usize) -> Self {
        let proxy = Proxy {
            pty_write: Arc::new(Mutex::new(Vec::new())),
            title: Arc::new(Mutex::new(String::new())),
        };
        let dims = TermDims {
            cols: cols as usize,
            rows: rows as usize,
        };
        let config = Config {
            scrolling_history: scrollback,
            ..Config::default()
        };
        let term = Term::new(config, &dims, proxy.clone());
        Self {
            term,
            processor: Processor::default(),
            proxy,
            rows,
            cols,
        }
    }

    pub fn process(&mut self, data: &[u8]) {
        self.processor.advance(&mut self.term, data);
    }

    /// When `Some`, the owner must call [`flush_sync`] at or after this instant.
    pub fn sync_deadline(&self) -> Option<std::time::Instant> {
        self.processor.sync_timeout().sync_timeout()
    }

    pub fn flush_sync(&mut self) {
        self.processor.stop_sync(&mut self.term);
    }

    pub fn take_pty_write(&mut self) -> Vec<u8> {
        std::mem::take(&mut *self.proxy.pty_write.lock().unwrap())
    }

    pub fn resize(&mut self, rows: u16, cols: u16) {
        self.rows = rows;
        self.cols = cols;
        self.term.resize(TermDims {
            cols: cols as usize,
            rows: rows as usize,
        });
    }

    pub fn snapshot(&mut self) -> FrameState {
        let rows = self.rows as usize;
        let cols = self.cols as usize;
        let mut cells = vec![0u8; rows * cols * CELL_SIZE];
        let mut overflow = BTreeMap::new();
        let grid = self.term.grid();
        for row in 0..rows {
            let grid_row = &grid[Line(row as i32)];
            for col in 0..cols {
                let cell = &grid_row[Column(col)];
                let flat = row * cols + col;
                encode_cell(
                    cell,
                    &mut cells[flat * CELL_SIZE..][..CELL_SIZE],
                    flat,
                    &mut overflow,
                );
            }
        }
        let cursor = grid.cursor.point;
        let cursor_row = (cursor.line.0 as u16).min(self.rows.saturating_sub(1));
        let cursor_col = (cursor.column.0 as u16).min(self.cols.saturating_sub(1));
        let title = self.proxy.title.lock().unwrap().clone();
        let mut frame = FrameState::from_parts(
            self.rows,
            self.cols,
            cursor_row,
            cursor_col,
            self.pack_mode(),
            title,
            cells,
        );
        *frame.overflow_mut() = overflow;
        frame
    }

    fn pack_mode(&self) -> u16 {
        let m = self.term.mode();
        let mut mode = 0u16;
        if m.contains(TermMode::SHOW_CURSOR) {
            mode |= 1;
        }
        if m.contains(TermMode::BRACKETED_PASTE) {
            mode |= 1 << 3;
        }
        mode
    }
}

fn encode_cell(
    cell: &alacritty_terminal::term::cell::Cell,
    buf: &mut [u8],
    flat: usize,
    overflow: &mut BTreeMap<usize, String>,
) {
    let mut f0 = 0u8;
    match &cell.fg {
        Color::Named(NamedColor::Foreground) => {}
        Color::Named(n) => {
            f0 |= 1;
            buf[2] = *n as u8;
        }
        Color::Indexed(i) => {
            f0 |= 1;
            buf[2] = *i;
        }
        Color::Spec(rgb) => {
            f0 |= 2;
            buf[2] = rgb.r;
            buf[3] = rgb.g;
            buf[4] = rgb.b;
        }
    }
    match &cell.bg {
        Color::Named(NamedColor::Background) => {}
        Color::Named(n) => {
            f0 |= 1 << 2;
            buf[5] = *n as u8;
        }
        Color::Indexed(i) => {
            f0 |= 1 << 2;
            buf[5] = *i;
        }
        Color::Spec(rgb) => {
            f0 |= 2 << 2;
            buf[5] = rgb.r;
            buf[6] = rgb.g;
            buf[7] = rgb.b;
        }
    }
    let flags = cell.flags;
    if flags.contains(CellFlags::BOLD) {
        f0 |= 1 << 4;
    }
    if flags.contains(CellFlags::DIM) {
        f0 |= 1 << 5;
    }
    if flags.contains(CellFlags::ITALIC) {
        f0 |= 1 << 6;
    }
    if flags.intersects(
        CellFlags::UNDERLINE
            | CellFlags::DOUBLE_UNDERLINE
            | CellFlags::UNDERCURL
            | CellFlags::DOTTED_UNDERLINE
            | CellFlags::DASHED_UNDERLINE,
    ) {
        f0 |= 1 << 7;
    }
    buf[0] = f0;
    let mut f1 = 0u8;
    if flags.contains(CellFlags::INVERSE) {
        f1 |= 1;
    }
    if flags.contains(CellFlags::WIDE_CHAR) {
        f1 |= 1 << 1;
    }
    if flags.contains(CellFlags::WIDE_CHAR_SPACER) {
        f1 |= 1 << 2;
    }
    let c = cell.c;
    if c <= '\x7f' && c > ' ' && cell.extra.is_none() {
        f1 |= 1 << 3;
        buf[8] = c as u8;
    } else if c != '\0' && c != ' ' {
        let mut tmp = [0u8; 4];
        let s = c.encode_utf8(&mut tmp);
        let mut full = s.to_string();
        if let Some(zw) = cell.zerowidth() {
            for &z in zw {
                full.push(z);
            }
        }
        let bytes = full.as_bytes();
        if bytes.len() <= 4 {
            f1 |= (bytes.len() as u8) << 3;
            buf[8..8 + bytes.len()].copy_from_slice(bytes);
        } else {
            f1 |= 7 << 3;
            overflow.insert(flat, full);
        }
    }
    buf[1] = f1;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dsr_cursor_query_writes_back() {
        let mut em = Emulator::new(24, 80, 100);
        em.process(b"\x1b[6n");
        let reply = em.take_pty_write();
        let s = String::from_utf8_lossy(&reply);
        assert!(
            s.contains('R') && s.contains('\u{1b}'),
            "expected DSR reply, got {s:?}"
        );
    }

    #[test]
    fn synchronized_update_flushes_after_timeout() {
        let mut em = Emulator::new(24, 80, 100);
        em.process(b"\x1b[?2026hSYNC_TEXT");
        let before = em.snapshot();
        let hidden = before.cell_content(0, 0) != "S";
        assert!(
            hidden || em.sync_deadline().is_some(),
            "BSU should buffer or set a deadline"
        );
        if let Some(d) = em.sync_deadline() {
            let now = std::time::Instant::now();
            if d > now {
                std::thread::sleep(
                    d.saturating_duration_since(now) + std::time::Duration::from_millis(5),
                );
            }
        }
        em.flush_sync();
        let after = em.snapshot();
        let mut found = String::new();
        for c in 0..after.cols() {
            found.push_str(after.cell_content(0, c));
        }
        assert!(
            found.contains("SYNC_TEXT"),
            "expected flushed text, got {found:?}"
        );
    }
}
