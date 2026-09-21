//! Browser (`wasm-bindgen`) bindings for `quosh-client`.
//!
//! The browser owns the transport (`WebTransport`) and the clock
//! (`performance.now()`); this wrapper owns the protocol state machine. Feed it
//! received bytes and the current time, drain `outbound()`, and read `display()`
//! to paint. The auth handshake happens in JavaScript before a `Client` is
//! constructed (see `quosh-proto-wasm`).

use wasm_bindgen::prelude::*;

fn arr16(b: &[u8]) -> Result<[u8; 16], JsValue> {
    b.try_into().map_err(|_| JsValue::from_str("expected 16 bytes"))
}

fn arr32(b: &[u8]) -> Result<[u8; 32], JsValue> {
    b.try_into().map_err(|_| JsValue::from_str("expected 32 bytes"))
}

/// A snapshot of a display frame.
#[wasm_bindgen]
pub struct FrameView {
    frame: blit_remote::FrameState,
}

#[wasm_bindgen]
impl FrameView {
    #[wasm_bindgen(getter)]
    pub fn rows(&self) -> u16 {
        self.frame.rows()
    }

    #[wasm_bindgen(getter)]
    pub fn cols(&self) -> u16 {
        self.frame.cols()
    }

    #[wasm_bindgen(getter)]
    pub fn cursor_row(&self) -> u16 {
        self.frame.cursor_row()
    }

    #[wasm_bindgen(getter)]
    pub fn cursor_col(&self) -> u16 {
        self.frame.cursor_col()
    }

    #[wasm_bindgen(getter)]
    pub fn mode(&self) -> u16 {
        self.frame.mode()
    }

    #[wasm_bindgen(getter)]
    pub fn title(&self) -> String {
        self.frame.title().to_string()
    }

    /// Raw cells, `rows * cols * 12` bytes.
    #[wasm_bindgen(getter)]
    pub fn cells(&self) -> Vec<u8> {
        self.frame.cells().to_vec()
    }

    /// Resolved text for one cell (overflow-table aware).
    pub fn cell_content(&self, row: u16, col: u16) -> String {
        self.frame.cell_content(row, col).to_string()
    }
}

/// The transport-agnostic client state machine.
#[wasm_bindgen]
pub struct Client {
    inner: quosh_client::Client,
}

#[wasm_bindgen]
impl Client {
    #[wasm_bindgen(constructor)]
    pub fn new(
        session_id: &[u8],
        token: &[u8],
        cols: u16,
        rows: u16,
        predict_never: bool,
    ) -> Result<Client, JsValue> {
        Ok(Client {
            inner: quosh_client::Client::new(
                arr16(session_id)?,
                arr32(token)?,
                cols,
                rows,
                predict_never,
            ),
        })
    }

    /// Start a connection: replay any unacked input and queue `Hello`.
    pub fn begin_connection(&mut self, now_ms: f64) {
        self.inner.begin_connection(now_ms as u64);
    }

    pub fn end_connection(&mut self) {
        self.inner.end_connection();
    }

    /// Forget predictions and the last frame, e.g. after a reconnect.
    pub fn reset(&mut self) {
        self.inner.reset();
    }

    pub fn recv_control(&mut self, bytes: &[u8], now_ms: f64) -> Result<(), JsValue> {
        self.inner
            .recv_control(bytes, now_ms as u64)
            .map_err(|e| JsValue::from_str(&e.to_string()))
    }

    pub fn recv_datagram(&mut self, bytes: &[u8], now_ms: f64) {
        self.inner.recv_datagram(bytes, now_ms as u64);
    }

    pub fn queue_input(&mut self, bytes: &[u8], now_ms: f64) {
        self.inner.queue_input(bytes.to_vec(), now_ms as u64);
    }

    pub fn set_size(&mut self, cols: u16, rows: u16) {
        self.inner.set_size(cols, rows);
    }

    pub fn request_hangup(&mut self) {
        self.inner.request_hangup();
    }

    /// Advance timers. Returns `true` while the link is alive.
    pub fn tick(&mut self, now_ms: f64) -> bool {
        self.inner.tick(now_ms as u64) == quosh_client::Tick::Alive
    }

    /// Fill the outbound queue (acks and unsent input).
    pub fn pump(&mut self) {
        self.inner.pump();
    }

    #[wasm_bindgen(getter)]
    pub fn outbound(&self) -> Vec<u8> {
        self.inner.outbound().to_vec()
    }

    pub fn advance_outbound(&mut self, n: usize) {
        self.inner.advance_outbound(n);
    }

    #[wasm_bindgen(getter)]
    pub fn writing(&self) -> bool {
        self.inner.writing()
    }

    #[wasm_bindgen(getter)]
    pub fn can_accept_input(&self) -> bool {
        self.inner.can_accept_input()
    }

    #[wasm_bindgen(getter)]
    pub fn hello_ok(&self) -> bool {
        self.inner.hello_ok()
    }

    #[wasm_bindgen(getter)]
    pub fn is_hungup(&self) -> bool {
        self.inner.is_hungup()
    }

    #[wasm_bindgen(getter)]
    pub fn exit_code(&self) -> i32 {
        self.inner.exit_code()
    }

    #[wasm_bindgen(getter)]
    pub fn cols(&self) -> u16 {
        self.inner.cols()
    }

    #[wasm_bindgen(getter)]
    pub fn rows(&self) -> u16 {
        self.inner.rows()
    }

    #[wasm_bindgen(getter)]
    pub fn mode(&self) -> u16 {
        self.inner.mode()
    }

    /// Seconds the link has been silent, or `undefined` when healthy.
    #[wasm_bindgen(getter)]
    pub fn outage_secs(&self) -> Option<f64> {
        self.inner.outage().map(|d| d.as_secs_f64())
    }

    /// Suggested delay until the next `tick()`, or `undefined` to wait for I/O.
    #[wasm_bindgen(getter)]
    pub fn tick_delay_ms(&self) -> Option<f64> {
        self.inner.tick_delay_ms().map(|d| d as f64)
    }

    /// The frame to paint (confirmed state plus prediction overlays).
    pub fn display(&self) -> Option<FrameView> {
        self.inner.display().map(|f| FrameView { frame: f.clone() })
    }

    /// The confirmed frame, without overlays.
    pub fn confirmed(&self) -> Option<FrameView> {
        self.inner.confirmed().map(|f| FrameView { frame: f.clone() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outbound_starts_with_hello() {
        let mut c = Client::new(&[0; 16], &[0; 32], 80, 24, false).unwrap();
        c.begin_connection(0.0);
        c.pump();
        let out = c.outbound();
        let (typ, _) = quosh_proto::split_frame(&mut out.to_vec()).unwrap().unwrap();
        assert_eq!(typ, quosh_proto::MSG_HELLO);
    }
}