//! Browser (`wasm-bindgen`) bindings for `quosh-predict`.
//!
//! The predictor is transport-free; this wrapper lets a browser apply local
//! echo prediction to the same cell format the native client uses. `quosh-predict`
//! itself stays WASM-free.

use blit_remote::{CELL_SIZE, FrameState};
use wasm_bindgen::prelude::*;

/// A terminal frame in the shared `blit-remote` cell format.
#[wasm_bindgen]
pub struct Frame {
    inner: FrameState,
}

#[wasm_bindgen]
impl Frame {
    #[wasm_bindgen(constructor)]
    pub fn new(
        rows: u16,
        cols: u16,
        cursor_row: u16,
        cursor_col: u16,
        mode: u16,
        title: String,
        cells: &[u8],
    ) -> Result<Frame, JsValue> {
        let expected = rows as usize * cols as usize * CELL_SIZE;
        if cells.len() != expected {
            return Err(JsValue::from_str(&format!(
                "cells must be {expected} bytes ({}x{}x{CELL_SIZE}), got {}",
                rows,
                cols,
                cells.len()
            )));
        }
        Ok(Frame {
            inner: FrameState::from_parts(
                rows,
                cols,
                cursor_row,
                cursor_col,
                mode,
                &title,
                cells.to_vec(),
            ),
        })
    }

    #[wasm_bindgen(getter)]
    pub fn rows(&self) -> u16 {
        self.inner.rows()
    }

    #[wasm_bindgen(getter)]
    pub fn cols(&self) -> u16 {
        self.inner.cols()
    }

    #[wasm_bindgen(getter)]
    pub fn cursor_row(&self) -> u16 {
        self.inner.cursor_row()
    }

    #[wasm_bindgen(getter)]
    pub fn cursor_col(&self) -> u16 {
        self.inner.cursor_col()
    }

    #[wasm_bindgen(getter)]
    pub fn mode(&self) -> u16 {
        self.inner.mode()
    }

    #[wasm_bindgen(getter)]
    pub fn title(&self) -> String {
        self.inner.title().to_string()
    }

    #[wasm_bindgen(getter)]
    pub fn cells(&self) -> Vec<u8> {
        self.inner.cells().to_vec()
    }

    /// An independent copy, for keeping a confirmed frame while the predictor
    /// mutates another.
    pub fn clone_frame(&self) -> Frame {
        Frame {
            inner: self.inner.clone(),
        }
    }
}

impl Clone for Frame {
    fn clone(&self) -> Self {
        self.clone_frame()
    }
}

/// The Mosh-style local echo predictor.
#[wasm_bindgen]
pub struct Predictor {
    inner: quosh_predict::Predictor,
}

impl Default for Predictor {
    fn default() -> Self {
        Self {
            inner: quosh_predict::Predictor::new(),
        }
    }
}

#[wasm_bindgen]
impl Predictor {
    #[wasm_bindgen(constructor)]
    pub fn new() -> Predictor {
        Predictor::default()
    }

    /// `adaptive = true` (default) predicts cautiously on slow links;
    /// `false` never predicts.
    pub fn set_display_preference(&mut self, adaptive: bool) {
        self.inner
            .set_display_preference(if adaptive {
                quosh_predict::DisplayPreference::Adaptive
            } else {
                quosh_predict::DisplayPreference::Never
            });
    }

    pub fn set_local_frame_sent(&mut self, seq: f64) {
        self.inner.set_local_frame_sent(seq as u64);
    }

    pub fn set_local_frame_acked(&mut self, seq: f64) {
        self.inner.set_local_frame_acked(seq as u64);
    }

    pub fn set_local_frame_late_acked(&mut self, seq: f64) {
        self.inner.set_local_frame_late_acked(seq as u64);
    }

    pub fn set_send_interval(&mut self, ms: u32) {
        self.inner.set_send_interval(ms);
    }

    pub fn set_predict_overwrite(&mut self, overwrite: bool) {
        self.inner.set_predict_overwrite(overwrite);
    }

    pub fn reset(&mut self) {
        self.inner.reset();
    }

    #[wasm_bindgen(getter)]
    pub fn active(&self) -> bool {
        self.inner.active()
    }

    /// Record a locally-typed byte, predicting against the display frame.
    pub fn new_user_byte(&mut self, byte: u8, display: &Frame, now_ms: f64) {
        self.inner.new_user_byte(byte, &display.inner, now_ms as u64);
    }

    /// Drop predictions that were not confirmed in time.
    pub fn cull(&mut self, confirmed: &Frame, now_ms: f64) {
        self.inner.cull(&confirmed.inner, now_ms as u64);
    }

    /// Overlay predictions onto `frame` (which must be the confirmed frame).
    pub fn apply(&self, frame: &mut Frame) {
        self.inner.apply(&mut frame.inner);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(text: &str, cursor_col: u16) -> Frame {
        let cols = 24u16;
        let rows = 1u16;
        let mut cells = vec![0u8; rows as usize * cols as usize * CELL_SIZE];
        for (i, ch) in text.chars().enumerate() {
            let mut tmp = [0u8; 4];
            let s = ch.encode_utf8(&mut tmp);
            let cell = i * CELL_SIZE;
            cells[cell + 1] = (s.len() as u8) << 3;
            cells[cell + 8..cell + 8 + s.len()].copy_from_slice(s.as_bytes());
        }
        Frame::new(rows, cols, 0, cursor_col, 1, String::new(), &cells)
            .map_err(|_| "bad frame")
            .unwrap()
    }

    #[test]
    fn predictor_binding_matches_native_sequence() {
        let mut p = Predictor::new();
        p.set_display_preference(true); // adaptive
        p.set_send_interval(250);

        let confirmed = line("prompt> ", 8);
        p.set_local_frame_sent(0.0);
        let mut display = confirmed.clone_frame();
        p.new_user_byte(b'a', &display, 0.0);
        p.apply(&mut display);
        // The first, tentative prediction is not shown.
        assert_eq!(display.cells()[8 * CELL_SIZE + 8], 0);

        // The server echoes and acknowledges the input.
        let echoed = line("prompt> a", 9);
        p.set_local_frame_late_acked(1.0);
        p.cull(&echoed, 1.0);
        let mut shown = echoed.clone_frame();
        p.apply(&mut shown);
        assert_eq!(shown.cells()[8 * CELL_SIZE + 8], b'a');

        // The next character is displayable immediately.
        p.set_local_frame_sent(1.0);
        let mut shown2 = echoed.clone_frame();
        p.new_user_byte(b'b', &shown2, 2.0);
        p.apply(&mut shown2);
        assert_eq!(shown2.cells()[9 * CELL_SIZE + 8], b'b');
    }
}