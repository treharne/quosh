//! Clean-room Rust port of Mosh's `PredictionEngine`
//! (`reference/mosh/src/frontend/terminaloverlay.{h,cc}`).
//!
//! Local, reversible echo prediction against a confirmed `FrameState`. The
//! predictor never owns authoritative state: callers pass the confirmed frame
//! to [`Predictor::cull`] and get a speculative copy back from
//! [`Predictor::apply`]. The wire protocol only needs to supply an input-frame
//! echo acknowledgement (Mosh's `late_ack`); see `MSG_ECHO_ACK`.
//!
//! Slice 2 ships adaptive display only. `Always`/`Never` exist so tests and
//! the oracle harness can pin behaviour.
//!
//! Behavioural provenance: Mosh commit `decd9b705eb81626f694335b8d5940538beb06da`,
//! GPLv3. This is a reimplementation, not a copy; the differential oracle under
//! `tools/mosh-oracle` checks it against the original.

use blit_remote::{CELL_SIZE, FrameState};
use unicode_width::UnicodeWidthChar;
use vte::{Params, Perform};

/// Predicted input is displayed once a prediction in the same epoch is
/// confirmed; passwords and other echo-off input are never confirmed, so they
/// are never shown. This epoch gate is what makes the engine "conservative".
const SRTT_TRIGGER_LOW: u32 = 20;
const SRTT_TRIGGER_HIGH: u32 = 30;
const FLAG_TRIGGER_LOW: u32 = 50;
const FLAG_TRIGGER_HIGH: u32 = 80;
const GLITCH_THRESHOLD: u64 = 250;
const GLITCH_REPAIR_COUNT: u32 = 10;
const GLITCH_REPAIR_MININTERVAL: u64 = 150;
const GLITCH_FLAG_THRESHOLD: u64 = 5000;
/// Sentinel for "no frame/time"; matches Mosh's `uint64_t(-1)`.
const NONE: u64 = u64::MAX;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DisplayPreference {
    Adaptive,
    Always,
    Never,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Validity {
    Pending,
    Correct,
    CorrectNoCredit,
    IncorrectOrExpired,
    Inactive,
}

#[derive(Clone, Copy)]
struct Base {
    expiration_frame: u64,
    col: u16,
    active: bool,
    tentative_until_epoch: u64,
    prediction_time: u64,
}

impl Base {
    fn new(col: u16, tentative: u64) -> Self {
        Self {
            expiration_frame: 0,
            col,
            active: false,
            tentative_until_epoch: tentative,
            prediction_time: NONE,
        }
    }
    fn tentative(&self, confirmed_epoch: u64) -> bool {
        self.tentative_until_epoch > confirmed_epoch
    }
    fn reset(&mut self) {
        self.expiration_frame = NONE;
        self.tentative_until_epoch = NONE;
        self.active = false;
        self.prediction_time = NONE;
    }
    fn expire(&mut self, expiration_frame: u64, now: u64) {
        self.expiration_frame = expiration_frame;
        self.prediction_time = now;
    }
}

#[derive(Clone)]
struct CellOverlay {
    base: Base,
    replacement: [u8; CELL_SIZE],
    unknown: bool,
    /// What the predicted cell replaced. Correct predictions that merely match
    /// the original contents earn no credit (Mosh's `CorrectNoCredit`).
    original_contents: Vec<String>,
}

impl CellOverlay {
    fn reset(&mut self) {
        self.unknown = false;
        self.original_contents.clear();
        self.base.reset();
    }
    fn reset_with_orig(&mut self) {
        if !self.base.active || self.unknown {
            self.reset();
            return;
        }
        self.original_contents.push(cell_text(&self.replacement));
        self.base.reset();
    }
    fn validity(&self, fb: &FrameState, row: u16, late_ack: u64) -> Validity {
        if !self.base.active {
            return Validity::Inactive;
        }
        if row >= fb.rows() || self.base.col >= fb.cols() {
            return Validity::IncorrectOrExpired;
        }
        if late_ack < self.base.expiration_frame {
            return Validity::Pending;
        }
        if self.unknown {
            return Validity::CorrectNoCredit;
        }
        let replacement = cell_text(&self.replacement);
        if is_blank(&replacement) {
            // Too easy for a blank to trigger falsely.
            return Validity::CorrectNoCredit;
        }
        let current = fb.cell_content(row, self.base.col);
        if contents_match(current, &replacement) {
            if self
                .original_contents
                .iter()
                .any(|o| contents_match(o, &replacement))
            {
                Validity::CorrectNoCredit
            } else {
                Validity::Correct
            }
        } else {
            Validity::IncorrectOrExpired
        }
    }
    fn apply(&self, fb: &mut FrameState, confirmed_epoch: u64, row: u16, mut flag: bool) {
        if !self.base.active || row >= fb.rows() || self.base.col >= fb.cols() {
            return;
        }
        if self.base.tentative(confirmed_epoch) {
            return;
        }
        let replacement = cell_text(&self.replacement);
        if is_blank(&replacement) && is_blank(fb.cell_content(row, self.base.col)) {
            flag = false;
        }
        if self.unknown {
            if flag && self.base.col != fb.cols() - 1 {
                set_underline(fb, row, self.base.col);
            }
            return;
        }
        if cell_bytes(fb, row, self.base.col) != self.replacement {
            set_cell(fb, row, self.base.col, &self.replacement);
            if flag {
                set_underline(fb, row, self.base.col);
            }
        }
    }
}

#[derive(Clone)]
struct CursorOverlay {
    base: Base,
    row: u16,
    col: u16,
}

impl CursorOverlay {
    fn validity(&self, fb: &FrameState, late_ack: u64) -> Validity {
        if !self.base.active {
            return Validity::Inactive;
        }
        if self.row >= fb.rows() || self.col >= fb.cols() {
            return Validity::IncorrectOrExpired;
        }
        if late_ack >= self.base.expiration_frame {
            if fb.cursor_col() == self.col && fb.cursor_row() == self.row {
                Validity::Correct
            } else {
                Validity::IncorrectOrExpired
            }
        } else {
            Validity::Pending
        }
    }
    fn apply(&self, fb: &mut FrameState, confirmed_epoch: u64) {
        if !self.base.active || self.base.tentative(confirmed_epoch) {
            return;
        }
        if self.row < fb.rows() && self.col < fb.cols() {
            fb.set_cursor(self.row, self.col);
        }
    }
}

#[derive(Clone)]
struct Row {
    row_num: u16,
    cells: Vec<CellOverlay>,
}

pub struct Predictor {
    last_byte: u8,
    parser: vte::Parser,
    overlays: Vec<Row>,
    cursors: Vec<CursorOverlay>,
    local_frame_sent: u64,
    local_frame_acked: u64,
    local_frame_late_acked: u64,
    prediction_epoch: u64,
    confirmed_epoch: u64,
    flagging: bool,
    srtt_trigger: bool,
    glitch_trigger: u32,
    last_quick_confirmation: u64,
    send_interval: u32,
    last_height: u16,
    last_width: u16,
    display_preference: DisplayPreference,
    predict_overwrite: bool,
}

impl Default for Predictor {
    fn default() -> Self {
        Self::new()
    }
}

impl Predictor {
    pub fn new() -> Self {
        Self {
            last_byte: 0,
            parser: vte::Parser::new(),
            overlays: Vec::new(),
            cursors: Vec::new(),
            local_frame_sent: 0,
            local_frame_acked: 0,
            local_frame_late_acked: 0,
            prediction_epoch: 1,
            confirmed_epoch: 0,
            flagging: false,
            srtt_trigger: false,
            glitch_trigger: 0,
            last_quick_confirmation: 0,
            send_interval: 250,
            last_height: 0,
            last_width: 0,
            display_preference: DisplayPreference::Adaptive,
            predict_overwrite: false,
        }
    }

    pub fn set_display_preference(&mut self, pref: DisplayPreference) {
        self.display_preference = pref;
    }

    pub fn set_local_frame_sent(&mut self, seq: u64) {
        self.local_frame_sent = seq;
    }

    pub fn set_local_frame_acked(&mut self, seq: u64) {
        self.local_frame_acked = seq;
    }

    /// The highest input frame the server's authoritative state reflects.
    pub fn set_local_frame_late_acked(&mut self, seq: u64) {
        self.local_frame_late_acked = seq;
    }

    /// Mosh's `send_interval`: `clamp(ceil(SRTT/2), 20, 250)` ms, supplied by
    /// the network layer.
    pub fn set_send_interval(&mut self, ms: u32) {
        self.send_interval = ms;
    }

    pub fn set_predict_overwrite(&mut self, overwrite: bool) {
        self.predict_overwrite = overwrite;
    }

    pub fn reset(&mut self) {
        self.cursors.clear();
        self.overlays.clear();
        self.become_tentative();
    }

    pub fn active(&self) -> bool {
        if !self.cursors.is_empty() {
            return true;
        }
        self.overlays
            .iter()
            .any(|r| r.cells.iter().any(|c| c.base.active))
    }

    /// Feed one user keystroke. `display` is the frame currently on screen
    /// (confirmed plus previous predictions), as in Mosh.
    pub fn new_user_byte(&mut self, byte: u8, display: &FrameState, now: u64) {
        if self.display_preference == DisplayPreference::Never {
            return;
        }
        self.cull(display, now);

        // Mosh translates application-mode cursor keys (SS3) to CSI before
        // parsing so the arrow predictor still fires.
        let mut b = byte;
        if self.last_byte == 0x1b && b == b'O' {
            b = b'[';
        }
        self.last_byte = b;

        if b == 0x7f {
            self.handle_print('\x7f', display, now);
            return;
        }

        let mut collector = Collector { acts: Vec::new() };
        self.parser.advance(&mut collector, &[b]);
        for act in collector.acts {
            match act {
                Act::Print(ch) => self.handle_print(ch, display, now),
                Act::Execute(0x0d) => {
                    self.become_tentative();
                    self.newline_carriage_return(display, now);
                }
                Act::Csi('C') => {
                    self.init_cursor(display);
                    if self.cursor().col < display.cols().saturating_sub(1) {
                        let exp = self.local_frame_sent + 1;
                        let c = self.cursors.last_mut().unwrap();
                        c.col += 1;
                        c.base.expire(exp, now);
                    }
                }
                Act::Csi('D') => {
                    self.init_cursor(display);
                    if self.cursor().col > 0 {
                        let exp = self.local_frame_sent + 1;
                        let c = self.cursors.last_mut().unwrap();
                        c.col -= 1;
                        c.base.expire(exp, now);
                    }
                }
                Act::Execute(_) | Act::Esc | Act::Csi(_) => self.become_tentative(),
            }
        }
    }

    fn handle_print(&mut self, ch: char, display: &FrameState, now: u64) {
        self.init_cursor(display);
        if ch == '\x7f' {
            self.backspace(display, now);
            return;
        }
        if ch < '\x20' || UnicodeWidthChar::width(ch) != Some(1) {
            self.become_tentative();
            return;
        }
        self.predict_char(ch, display, now);
    }

    fn become_tentative(&mut self) {
        self.prediction_epoch = self.prediction_epoch.wrapping_add(1);
    }

    fn cursor(&self) -> &CursorOverlay {
        self.cursors.last().expect("cursor")
    }

    fn init_cursor(&mut self, fb: &FrameState) {
        let (exp, epoch) = (self.local_frame_sent + 1, self.prediction_epoch);
        if self.cursors.is_empty() {
            self.cursors.push(CursorOverlay {
                base: Base {
                    expiration_frame: exp,
                    col: fb.cursor_col(),
                    active: true,
                    tentative_until_epoch: epoch,
                    prediction_time: NONE,
                },
                row: fb.cursor_row(),
                col: fb.cursor_col(),
            });
        } else if self.cursor().base.tentative_until_epoch != epoch {
            let (row, col) = (self.cursor().row, self.cursor().col);
            self.cursors.push(CursorOverlay {
                base: Base {
                    expiration_frame: exp,
                    col,
                    active: true,
                    tentative_until_epoch: epoch,
                    prediction_time: NONE,
                },
                row,
                col,
            });
        }
    }

    fn backspace(&mut self, display: &FrameState, now: u64) {
        let (row_num, col0) = (self.cursor().row, self.cursor().col);
        let width = display.cols();
        let (exp, epoch) = (self.local_frame_sent + 1, self.prediction_epoch);
        let overwrite = self.predict_overwrite;
        let row = get_or_make_row(&mut self.overlays, row_num, width, epoch);
        if col0 == 0 {
            return;
        }
        let col = col0 - 1;
        {
            let c = self.cursors.last_mut().unwrap();
            c.col = col;
            c.base.expire(exp, now);
        }
        if overwrite {
            let actual = cell_bytes(display, row_num, col);
            let cell = &mut row.cells[col as usize];
            cell.reset_with_orig();
            cell.base.active = true;
            cell.base.tentative_until_epoch = epoch;
            cell.base.expire(exp, now);
            cell.original_contents.push(cell_text(&actual));
            cell.replacement = make_blank_cell(&actual);
            return;
        }
        for i in col..width {
            let iu = i as usize;
            let next = if i + 2 < width {
                let n = &row.cells[iu + 1];
                Some((
                    n.base.active,
                    n.unknown,
                    n.replacement,
                    cell_bytes(display, row_num, i + 1),
                ))
            } else {
                None
            };
            let actual = cell_bytes(display, row_num, i);
            let cell = &mut row.cells[iu];
            cell.reset_with_orig();
            cell.base.active = true;
            cell.base.tentative_until_epoch = epoch;
            cell.base.expire(exp, now);
            cell.original_contents.push(cell_text(&actual));
            match next {
                Some((active, unknown, repl, next_actual)) => {
                    if active {
                        if unknown {
                            cell.unknown = true;
                        } else {
                            cell.unknown = false;
                            cell.replacement = repl;
                        }
                    } else {
                        cell.unknown = false;
                        cell.replacement = next_actual;
                    }
                }
                None => cell.unknown = true,
            }
        }
    }

    fn predict_char(&mut self, ch: char, display: &FrameState, now: u64) {
        let (row_num, col) = (self.cursor().row, self.cursor().col);
        if row_num >= display.rows() || col >= display.cols() {
            return;
        }
        let width = display.cols();
        let (exp, old_epoch) = (self.local_frame_sent + 1, self.prediction_epoch);
        let at_last_col = col + 1 >= width;
        if at_last_col {
            self.become_tentative();
        }
        let epoch = self.prediction_epoch;
        let overwrite = self.predict_overwrite;
        let row = get_or_make_row(&mut self.overlays, row_num, width, old_epoch);

        let rightmost = if overwrite { col } else { width - 1 };
        let mut i = rightmost;
        loop {
            if i <= col {
                break;
            }
            let iu = i as usize;
            let prev = if i == width - 1 {
                None
            } else {
                let p = &row.cells[iu - 1];
                Some((
                    p.base.active,
                    p.unknown,
                    p.replacement,
                    cell_bytes(display, row_num, i - 1),
                ))
            };
            let actual = cell_bytes(display, row_num, i);
            let cell = &mut row.cells[iu];
            cell.reset_with_orig();
            cell.base.active = true;
            cell.base.tentative_until_epoch = epoch;
            cell.base.expire(exp, now);
            cell.original_contents.push(cell_text(&actual));
            if i == width - 1 {
                cell.unknown = true;
            } else {
                let (active, unknown, repl, prev_actual) = prev.unwrap();
                if active {
                    if unknown {
                        cell.unknown = true;
                    } else {
                        cell.unknown = false;
                        cell.replacement = repl;
                    }
                } else {
                    cell.unknown = false;
                    cell.replacement = prev_actual;
                }
            }
            i -= 1;
        }

        // Base rendition: current pen (proxied by the confirmed cursor cell),
        // overridden by the cell to the left when there is one.
        let base_cell = cell_bytes(display, row_num, col);
        let mut replacement = base_cell;
        if col > 0 {
            let prev = &row.cells[col as usize - 1];
            replacement = if prev.base.active && !prev.unknown {
                prev.replacement
            } else {
                cell_bytes(display, row_num, col - 1)
            };
        }
        let actual = cell_bytes(display, row_num, col);
        let cell = &mut row.cells[col as usize];
        cell.reset_with_orig();
        cell.base.active = true;
        cell.base.tentative_until_epoch = epoch;
        cell.base.expire(exp, now);
        cell.replacement = make_char_cell(&replacement, ch);
        cell.original_contents.push(cell_text(&actual));

        let c = self.cursors.last_mut().unwrap();
        c.base.expire(exp, now);
        if col < width - 1 {
            c.col += 1;
        } else {
            self.become_tentative();
            self.newline_carriage_return(display, now);
        }
    }

    fn newline_carriage_return(&mut self, display: &FrameState, now: u64) {
        self.init_cursor(display);
        let height = display.rows();
        let width = display.cols();
        let (exp, epoch) = (self.local_frame_sent + 1, self.prediction_epoch);
        let row_num = {
            let c = self.cursors.last_mut().unwrap();
            c.col = 0;
            c.row
        };
        if row_num == height - 1 {
            let row = get_or_make_row(&mut self.overlays, row_num, width, epoch);
            for cell in row.cells.iter_mut() {
                cell.base.active = true;
                cell.base.tentative_until_epoch = epoch;
                cell.base.expire(exp, now);
                cell.replacement = make_blank_cell(&cell.replacement);
            }
        } else {
            self.cursors.last_mut().unwrap().row += 1;
        }
    }

    fn kill_epoch(&mut self, rows: &mut [Row], epoch: u64, fb: &FrameState) {
        let old = epoch.saturating_sub(1);
        self.cursors.retain(|c| !c.base.tentative(old));
        let (exp, prediction_epoch) = (self.local_frame_sent + 1, self.prediction_epoch);
        self.cursors.push(CursorOverlay {
            base: Base {
                expiration_frame: exp,
                col: fb.cursor_col(),
                active: true,
                tentative_until_epoch: prediction_epoch,
                prediction_time: NONE,
            },
            row: fb.cursor_row(),
            col: fb.cursor_col(),
        });
        for r in rows.iter_mut() {
            for cell in r.cells.iter_mut() {
                if cell.base.tentative(old) {
                    cell.reset();
                }
            }
        }
        self.become_tentative();
    }

    /// Reconcile predictions against a newly confirmed frame, then let the
    /// caller [`apply`](Self::apply) what survives.
    pub fn cull(&mut self, fb: &FrameState, now: u64) {
        if self.display_preference == DisplayPreference::Never {
            return;
        }

        if self.last_height != fb.rows() || self.last_width != fb.cols() {
            self.last_height = fb.rows();
            self.last_width = fb.cols();
            self.reset();
        }

        // SRTT trigger hysteresis.
        if self.send_interval > SRTT_TRIGGER_HIGH {
            self.srtt_trigger = true;
        } else if self.srtt_trigger && self.send_interval <= SRTT_TRIGGER_LOW && !self.active() {
            self.srtt_trigger = false;
        }

        // Underline (flagging) hysteresis.
        if self.send_interval > FLAG_TRIGGER_HIGH {
            self.flagging = true;
        } else if self.send_interval <= FLAG_TRIGGER_LOW {
            self.flagging = false;
        }
        if self.glitch_trigger > GLITCH_REPAIR_COUNT {
            self.flagging = true;
        }

        let mut rows = std::mem::take(&mut self.overlays);
        rows.retain(|r| r.row_num < fb.rows());
        for ri in 0..rows.len() {
            let row_num = rows[ri].row_num;
            let ncols = rows[ri].cells.len();
            let mut ci = 0;
            while ci < ncols {
                let validity =
                    rows[ri].cells[ci].validity(fb, row_num, self.local_frame_late_acked);
                match validity {
                    Validity::IncorrectOrExpired => {
                        let cell = &rows[ri].cells[ci];
                        if cell.base.tentative(self.confirmed_epoch) {
                            let epoch = cell.base.tentative_until_epoch;
                            self.kill_epoch(&mut rows, epoch, fb);
                        } else {
                            self.reset();
                            return;
                        }
                    }
                    Validity::Correct => {
                        let te = rows[ri].cells[ci].base.tentative_until_epoch;
                        if te > self.confirmed_epoch {
                            self.confirmed_epoch = te;
                        }
                        let pt = rows[ri].cells[ci].base.prediction_time;
                        if now.saturating_sub(pt) < GLITCH_THRESHOLD
                            && self.glitch_trigger > 0
                            && now.saturating_sub(self.last_quick_confirmation)
                                >= GLITCH_REPAIR_MININTERVAL
                        {
                            self.glitch_trigger -= 1;
                            self.last_quick_confirmation = now;
                        }
                        let actual = cell_bytes(fb, row_num, rows[ri].cells[ci].base.col);
                        for k in ci..ncols {
                            copy_renditions(&mut rows[ri].cells[k].replacement, &actual);
                        }
                        rows[ri].cells[ci].reset();
                    }
                    Validity::CorrectNoCredit => rows[ri].cells[ci].reset(),
                    Validity::Pending => {
                        let pt = rows[ri].cells[ci].base.prediction_time;
                        let pending = now.saturating_sub(pt);
                        if pending >= GLITCH_FLAG_THRESHOLD {
                            self.glitch_trigger = GLITCH_REPAIR_COUNT * 2;
                        } else if pending >= GLITCH_THRESHOLD
                            && self.glitch_trigger < GLITCH_REPAIR_COUNT
                        {
                            self.glitch_trigger = GLITCH_REPAIR_COUNT;
                        }
                    }
                    Validity::Inactive => {}
                }
                ci += 1;
            }
        }
        self.overlays = rows;

        if let Some(back) = self.cursors.last()
            && back.validity(fb, self.local_frame_late_acked) == Validity::IncorrectOrExpired
        {
            self.reset();
            return;
        }
        self.cursors.retain(|c| {
            c.base.active && c.validity(fb, self.local_frame_late_acked) == Validity::Pending
        });
    }

    /// Apply surviving predictions to `frame` (normally a clone of confirmed).
    pub fn apply(&self, frame: &mut FrameState) {
        if self.display_preference == DisplayPreference::Never {
            return;
        }
        if !(self.srtt_trigger
            || self.glitch_trigger > 0
            || self.display_preference == DisplayPreference::Always)
        {
            return;
        }
        for c in &self.cursors {
            c.apply(frame, self.confirmed_epoch);
        }
        for r in &self.overlays {
            for cell in &r.cells {
                cell.apply(frame, self.confirmed_epoch, r.row_num, self.flagging);
            }
        }
    }
}

enum Act {
    Print(char),
    Execute(u8),
    Csi(char),
    Esc,
}

struct Collector {
    acts: Vec<Act>,
}

impl Perform for Collector {
    fn print(&mut self, c: char) {
        self.acts.push(Act::Print(c));
    }
    fn execute(&mut self, byte: u8) {
        self.acts.push(Act::Execute(byte));
    }
    fn csi_dispatch(&mut self, _p: &Params, _i: &[u8], _ignore: bool, action: char) {
        self.acts.push(Act::Csi(action));
    }
    fn esc_dispatch(&mut self, _i: &[u8], _ignore: bool, _byte: u8) {
        self.acts.push(Act::Esc);
    }
}

fn get_or_make_row(overlays: &mut Vec<Row>, row_num: u16, num_cols: u16, epoch: u64) -> &mut Row {
    if let Some(pos) = overlays.iter().position(|r| r.row_num == row_num) {
        return &mut overlays[pos];
    }
    let cells = (0..num_cols)
        .map(|i| CellOverlay {
            base: Base::new(i, epoch),
            replacement: [0u8; CELL_SIZE],
            unknown: false,
            original_contents: Vec::new(),
        })
        .collect();
    overlays.push(Row { row_num, cells });
    overlays.last_mut().unwrap()
}

#[inline]
fn index(cols: u16, row: u16, col: u16) -> usize {
    (row as usize * cols as usize + col as usize) * CELL_SIZE
}

fn cell_bytes(fb: &FrameState, row: u16, col: u16) -> [u8; CELL_SIZE] {
    if row >= fb.rows() || col >= fb.cols() {
        return [0u8; CELL_SIZE];
    }
    let i = index(fb.cols(), row, col);
    fb.cells()[i..i + CELL_SIZE].try_into().unwrap()
}

fn set_cell(fb: &mut FrameState, row: u16, col: u16, cell: &[u8; CELL_SIZE]) {
    let cols = fb.cols();
    let i = index(cols, row, col);
    fb.cells_mut()[i..i + CELL_SIZE].copy_from_slice(cell);
}

fn set_underline(fb: &mut FrameState, row: u16, col: u16) {
    let cols = fb.cols();
    let i = index(cols, row, col);
    fb.cells_mut()[i] |= 1 << 7;
}

/// Decode the 12-byte blit-remote cell to its text content, including the
/// "blank is a space" convention used by `FrameState::cell_content`.
fn cell_text(cell: &[u8; CELL_SIZE]) -> String {
    let f1 = cell[1];
    if f1 & 4 != 0 {
        return String::new();
    }
    let len = ((f1 >> 3) & 7) as usize;
    if len == 0 {
        return " ".to_string();
    }
    if len >= 7 {
        return String::new();
    }
    String::from_utf8_lossy(&cell[8..8 + len]).into_owned()
}

fn is_blank(s: &str) -> bool {
    s.is_empty() || s == " " || s == "\u{a0}"
}

fn contents_match(a: &str, b: &str) -> bool {
    (is_blank(a) && is_blank(b)) || a == b
}

/// Build a width-1 predicted cell: content `ch`, style copied from `src`.
fn make_char_cell(src: &[u8; CELL_SIZE], ch: char) -> [u8; CELL_SIZE] {
    let mut cell = *src;
    let len = ch.len_utf8().min(4);
    cell[1] = (src[1] & 0b0000_0001) | ((len as u8) << 3);
    cell[8..CELL_SIZE].fill(0);
    let mut tmp = [0u8; 4];
    let s = ch.encode_utf8(&mut tmp);
    cell[8..8 + len].copy_from_slice(&s.as_bytes()[..len]);
    cell
}

/// Blank the cell, keeping its style (Mosh `Cell::clear`).
fn make_blank_cell(src: &[u8; CELL_SIZE]) -> [u8; CELL_SIZE] {
    let mut cell = *src;
    cell[1] &= 0b0000_0001;
    cell[8..CELL_SIZE].fill(0);
    cell
}

/// Copy the rendition bits (blit `f0` plus inverse) without disturbing content.
fn copy_renditions(dst: &mut [u8; CELL_SIZE], src: &[u8; CELL_SIZE]) {
    dst[0] = src[0];
    dst[1] = (dst[1] & 0b1111_1110) | (src[1] & 1);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_frame(rows: &[&str], cursor_row: u16, cursor_col: u16) -> FrameState {
        let width = rows.iter().map(|r| r.chars().count()).max().unwrap_or(0) as u16;
        text_frame_w(
            rows,
            cursor_row,
            cursor_col,
            width.max(cursor_col + 2).max(24),
        )
    }

    fn text_frame_w(rows: &[&str], cursor_row: u16, cursor_col: u16, cols_n: u16) -> FrameState {
        let rows_n = rows.len() as u16;
        let mut cells = vec![0u8; rows_n as usize * cols_n as usize * CELL_SIZE];
        for (r, line) in rows.iter().enumerate() {
            for (c, ch) in line.chars().enumerate() {
                if ch == ' ' {
                    continue;
                }
                let mut cell = [0u8; CELL_SIZE];
                let len = ch.len_utf8();
                cell[1] = (len as u8) << 3;
                let mut tmp = [0u8; 4];
                let s = ch.encode_utf8(&mut tmp);
                cell[8..8 + len].copy_from_slice(&s.as_bytes()[..len]);
                let i = (r * cols_n as usize + c) * CELL_SIZE;
                cells[i..i + CELL_SIZE].copy_from_slice(&cell);
            }
        }
        FrameState::from_parts(rows_n, cols_n, cursor_row, cursor_col, 1, "", cells)
    }

    fn shown(pred: &Predictor, confirmed: &FrameState) -> FrameState {
        let mut f = confirmed.clone();
        pred.apply(&mut f);
        f
    }

    fn row_text(frame: &FrameState, row: u16) -> String {
        let s: String = (0..frame.cols())
            .map(|c| frame.cell_content(row, c))
            .collect();
        s.trim_end().to_string()
    }

    fn underlined(pred: &Predictor, confirmed: &FrameState, row: u16, col: u16) -> bool {
        let f = shown(pred, confirmed);
        let i = index(f.cols(), row, col);
        f.cells()[i] & (1 << 7) != 0
    }

    #[test]
    fn first_prediction_is_hidden_until_confirmed() {
        let confirmed = text_frame(&["user@host:~$ "], 0, 13);
        let mut pred = Predictor::new();
        pred.set_display_preference(DisplayPreference::Always);
        pred.set_send_interval(250);

        pred.set_local_frame_sent(0);
        pred.new_user_byte(b'a', &confirmed, 0);
        assert_eq!(
            row_text(&shown(&pred, &confirmed), 0),
            "user@host:~$",
            "tentative prediction must not be displayed"
        );

        // Server echoes and acknowledges the first input frame.
        let echoed = text_frame(&["user@host:~$ a"], 0, 14);
        pred.set_local_frame_late_acked(1);
        pred.cull(&echoed, 1);
        assert_eq!(row_text(&shown(&pred, &echoed), 0), "user@host:~$ a");

        // The next character is displayable immediately and underlined.
        pred.set_local_frame_sent(1);
        pred.new_user_byte(b'b', &shown(&pred, &echoed), 2);
        assert_eq!(row_text(&shown(&pred, &echoed), 0), "user@host:~$ ab");
        assert!(
            underlined(&pred, &echoed, 0, 14),
            "predicted cell should be underlined while unconfirmed"
        );
    }

    #[test]
    fn echo_off_input_is_never_displayed() {
        let confirmed = text_frame(&["Password: "], 0, 10);
        let mut pred = Predictor::new();
        pred.set_display_preference(DisplayPreference::Always);
        pred.set_send_interval(250);

        pred.set_local_frame_sent(0);
        pred.new_user_byte(b's', &confirmed, 0);
        pred.set_local_frame_late_acked(1); // server accepted but echoed nothing
        pred.cull(&confirmed, 1);
        assert_eq!(
            row_text(&shown(&pred, &confirmed), 0),
            "Password:",
            "an accepted but unconfirmed prediction must stay hidden"
        );
    }

    #[test]
    fn adaptive_hides_predictions_on_fast_links() {
        let confirmed = text_frame(&["$ "], 0, 2);
        let mut pred = Predictor::new();
        pred.set_display_preference(DisplayPreference::Adaptive);
        pred.set_send_interval(20);
        pred.set_local_frame_sent(0);
        pred.new_user_byte(b'x', &confirmed, 0);
        assert_eq!(row_text(&shown(&pred, &confirmed), 0), "$");
    }
}
