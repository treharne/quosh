//! Transport-agnostic Quosh client state machine.
//!
//! Owns the protocol conversation, input sequencing, prediction state, and the
//! display frame. It performs no I/O and never reads a clock: an adapter (the
//! CLI's tokio loop, a browser event loop) feeds it events plus monotonic
//! milliseconds, and drains its outbound control bytes.
//!
//! The same state machine is used by the native CLI and by the browser client,
//! so session, reconnect, and prediction-feed behaviour cannot drift.

use blit_remote::FrameState;
use quosh_predict::{DisplayPreference, Predictor};
use quosh_proto::{
    FrameFeed, Hello, HelloOk, MSG_ERROR, MSG_EXIT, MSG_HELLO_OK, MSG_INPUT_ACK, MSG_PONG,
    MSG_SCREEN, OUTAGE_BANNER_SECS, PROTOCOL_VERSION, Screen, decode_error, decode_exit,
    decode_u64, encode_ack_state, encode_hangup, encode_input, encode_ping, encode_resize,
    split_frame,
};
use std::time::Duration;

/// Input messages larger than this are not predicted (bulk paste).
pub const PASTE_BYTES: usize = 100;
/// Stop consuming new input once this much is unacknowledged.
pub const UNACKED_CAP: usize = 256 * 1024;

const PING_INTERVAL_MS: u64 = 1000;

fn scaled_ms(srtt_ms: Option<f64>, factor: f64) -> u64 {
    srtt_ms
        .map(|s| (factor * s).clamp(0.0, 20_000.0) as u64)
        .unwrap_or(0)
}

/// Re-probe an outstanding ping after this long without a pong.
pub fn ping_retry_ms(srtt_ms: Option<f64>) -> u64 {
    2_000.max(scaled_ms(srtt_ms, 2.0))
}

/// Declare the path dead when nothing has arrived for this long.
pub fn link_dead_ms(srtt_ms: Option<f64>) -> u64 {
    8_000.max(scaled_ms(srtt_ms, 4.0))
}

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// A protocol violation or server rejection; retrying cannot help.
    #[error("{0}")]
    Fatal(String),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tick {
    Alive,
    /// Nothing received for [`link_dead_ms`]; the adapter should reconnect
    /// (classifying by [`Client::hello_ok`]).
    LinkDead,
}

pub struct Client {
    session_id: [u8; 16],
    token: [u8; 32],
    cols: u16,
    rows: u16,
    never: bool,

    // Connection/protocol.
    connected: bool,
    hello_ok: bool,
    feed: FrameFeed,
    sent_seq: u64,
    pending_ack: Option<u64>,
    recv_buf: Vec<u8>,
    seq: u64,
    unacked: Vec<(u64, Vec<u8>)>,

    // Liveness/timing (monotonic ms supplied by the adapter).
    last_rx_ms: Option<u64>,
    last_ping_ms: u64,
    ping_sent_ms: Option<u64>,
    srtt_ms: Option<f64>,

    // Display.
    predictor: Predictor,
    confirmed: Option<Screen>,
    display: Option<FrameState>,
    outage: Option<Duration>,

    hungup: bool,
    exit_code: i32,
}

impl Client {
    pub fn new(
        session_id: [u8; 16],
        token: [u8; 32],
        cols: u16,
        rows: u16,
        predict_never: bool,
    ) -> Self {
        let mut predictor = Predictor::new();
        predictor.set_display_preference(if predict_never {
            DisplayPreference::Never
        } else {
            DisplayPreference::Adaptive
        });
        Self {
            session_id,
            token,
            cols,
            rows,
            never: predict_never,
            connected: false,
            hello_ok: false,
            feed: FrameFeed::default(),
            sent_seq: 0,
            pending_ack: None,
            recv_buf: Vec::new(),
            seq: 0,
            unacked: Vec::new(),
            last_rx_ms: None,
            last_ping_ms: 0,
            ping_sent_ms: None,
            srtt_ms: None,
            predictor,
            confirmed: None,
            display: None,
            outage: None,
            hungup: false,
            exit_code: 0,
        }
    }

    pub fn cols(&self) -> u16 {
        self.cols
    }

    pub fn rows(&self) -> u16 {
        self.rows
    }

    pub fn hello_ok(&self) -> bool {
        self.hello_ok
    }

    pub fn is_hungup(&self) -> bool {
        self.hungup
    }

    pub fn exit_code(&self) -> i32 {
        self.exit_code
    }

    pub fn outbound(&self) -> &[u8] {
        self.feed.rest()
    }

    pub fn writing(&self) -> bool {
        self.feed.writing()
    }

    pub fn advance_outbound(&mut self, n: usize) {
        self.feed.advance(n);
    }

    /// Move pending acks and unsent input into the outbound buffer. No-op while
    /// detached or after a hangup.
    pub fn pump(&mut self) {
        if !self.connected || self.hungup {
            return;
        }
        if let Some(v) = self.pending_ack
            && self.feed.push_ctrl(encode_ack_state(v))
        {
            self.pending_ack = None;
        }
        if self.feed.writing() {
            return;
        }
        if let Some((seq, data)) = self.unacked.iter().find(|(s, _)| *s > self.sent_seq)
            && self.feed.push_ctrl(encode_input(*seq, data))
        {
            self.sent_seq = *seq;
        }
    }

    pub fn can_accept_input(&self) -> bool {
        self.unacked_bytes() < UNACKED_CAP
    }

    pub fn unacked_bytes(&self) -> usize {
        self.unacked.iter().map(|(_, d)| d.len()).sum()
    }

    /// Start a fresh transport: replay unacked input and send a Hello. Keeps
    /// `unacked`, so input queued while detached is retransmitted in order.
    pub fn begin_connection(&mut self, now_ms: u64) {
        self.connected = true;
        self.hello_ok = false;
        self.recv_buf.clear();
        self.feed = FrameFeed::default();
        self.sent_seq = 0;
        self.pending_ack = None;
        self.ping_sent_ms = None;
        self.last_ping_ms = now_ms;
        self.last_rx_ms = Some(now_ms);
        self.outage = None;
        let _ = self.feed.push_ctrl(
            Hello {
                protocol: PROTOCOL_VERSION,
                session_id: self.session_id,
                token: self.token,
                cols: self.cols,
                rows: self.rows,
            }
            .encode(),
        );
    }

    pub fn end_connection(&mut self) {
        self.connected = false;
    }

    /// Drop predictions and the display frame, keeping the confirmed screen.
    /// Used after a transport loss and before a reconnect.
    pub fn reset(&mut self) {
        self.predictor.reset();
        self.display = None;
    }

    pub fn recv_control(&mut self, bytes: &[u8], now_ms: u64) -> Result<(), ClientError> {
        if !bytes.is_empty() {
            self.last_rx_ms = Some(now_ms);
        }
        self.recv_buf.extend_from_slice(bytes);
        while let Some((typ, payload)) = split_frame(&mut self.recv_buf)
            .map_err(|e| ClientError::Fatal(format!("protocol error: {e}")))?
        {
            self.handle_frame(typ, &payload, now_ms)?;
        }
        Ok(())
    }

    fn handle_frame(&mut self, typ: u8, payload: &[u8], now_ms: u64) -> Result<(), ClientError> {
        match typ {
            MSG_HELLO_OK => {
                let hello = HelloOk::decode(payload)
                    .map_err(|e| ClientError::Fatal(format!("invalid HelloOk: {e}")))?;
                if hello.protocol != PROTOCOL_VERSION {
                    return Err(ClientError::Fatal(format!(
                        "server protocol {} != client {PROTOCOL_VERSION}; update the other side",
                        hello.protocol
                    )));
                }
                self.hello_ok = true;
            }
            MSG_SCREEN => {
                if let Ok(s) = Screen::decode_compressed(payload) {
                    self.apply_screen(s, now_ms);
                }
            }
            MSG_INPUT_ACK => {
                let ack = decode_u64(payload)
                    .map_err(|e| ClientError::Fatal(format!("protocol error: {e}")))?;
                self.unacked.retain(|(s, _)| *s > ack);
            }
            MSG_PONG => {
                if let Some(t) = self.ping_sent_ms.take() {
                    let sample = now_ms.saturating_sub(t) as f64;
                    let srtt = self
                        .srtt_ms
                        .map(|s| s * 0.75 + sample * 0.25)
                        .unwrap_or(sample);
                    self.srtt_ms = Some(srtt);
                    let interval = ((srtt / 2.0).ceil() as u32).clamp(20, 250);
                    self.predictor.set_send_interval(interval);
                }
                self.last_rx_ms = Some(now_ms);
                self.outage = None;
            }
            MSG_EXIT => {
                let st = decode_exit(payload)
                    .map_err(|e| ClientError::Fatal(format!("protocol error: {e}")))?;
                self.exit_code = st;
                self.hungup = true;
            }
            MSG_ERROR => {
                let (c, m) = decode_error(payload)
                    .map_err(|e| ClientError::Fatal(format!("protocol error: {e}")))?;
                return Err(ClientError::Fatal(format!("server error {c}: {m}")));
            }
            _ => {}
        }
        Ok(())
    }

    pub fn recv_datagram(&mut self, bytes: &[u8], now_ms: u64) {
        self.last_rx_ms = Some(now_ms);
        if let Ok(s) = Screen::decode_compressed(bytes) {
            self.apply_screen(s, now_ms);
        }
    }

    fn apply_screen(&mut self, incoming: Screen, now_ms: u64) {
        let Some(applied) = Screen::apply_newer(self.confirmed.as_ref(), incoming) else {
            return;
        };
        self.predictor.set_local_frame_late_acked(applied.echo_ack);
        self.predictor.cull(&applied.frame, now_ms);
        self.pending_ack = Some(applied.version);
        self.confirmed = Some(applied);
        self.outage = None;
        self.refresh_display();
    }

    /// Feed one input message. The caller checks [`can_accept_input`] first.
    pub fn queue_input(&mut self, bytes: Vec<u8>, now_ms: u64) {
        self.seq += 1;
        self.feed_predict(self.seq, &bytes, now_ms);
        self.unacked.push((self.seq, bytes));
    }

    fn feed_predict(&mut self, seq: u64, bytes: &[u8], now_ms: u64) {
        if bytes.len() > PASTE_BYTES {
            // Bulk input is not predicted; drop any stale overlay immediately.
            self.predictor.reset();
            self.refresh_display();
            return;
        }
        let Some(screen) = self.confirmed.as_ref() else {
            return;
        };
        // The predictor expires a prediction at `local_frame_sent + 1`, so
        // message `seq` is fed as `seq - 1` to expire at `seq`, matching the
        // server's echo checkpoint exactly.
        self.predictor.set_local_frame_sent(seq.saturating_sub(1));
        for &b in bytes {
            let basis = self.display.as_ref().unwrap_or(&screen.frame);
            self.predictor.new_user_byte(b, basis, now_ms);
        }
        self.refresh_display();
    }

    pub fn set_size(&mut self, cols: u16, rows: u16) {
        self.cols = cols;
        self.rows = rows;
        if self.connected && !self.hungup {
            let _ = self.feed.push_ctrl(encode_resize(cols, rows));
        }
    }

    /// Ask the server to hang up now; the adapter flushes the remaining
    /// outbound bytes and ends the session.
    pub fn request_hangup(&mut self) {
        self.feed.push_fin(encode_hangup());
        self.hungup = true;
    }

    pub fn tick(&mut self, now_ms: u64) -> Tick {
        if self.predictor.active() {
            if let Some(screen) = self.confirmed.as_ref() {
                self.predictor.cull(&screen.frame, now_ms);
            }
            self.refresh_display();
        }
        self.pump();
        if self.connected && !self.hungup {
            self.maybe_ping(now_ms);
            if let Some(last) = self.last_rx_ms
                && now_ms.saturating_sub(last) >= link_dead_ms(self.srtt_ms)
            {
                return Tick::LinkDead;
            }
        }
        if let Some(last) = self.last_rx_ms {
            let elapsed = now_ms.saturating_sub(last);
            if elapsed > OUTAGE_BANNER_SECS * 1000 {
                self.outage = Some(Duration::from_millis(elapsed));
            }
        }
        Tick::Alive
    }

    fn maybe_ping(&mut self, now_ms: u64) {
        let due = now_ms.saturating_sub(self.last_ping_ms) >= PING_INTERVAL_MS;
        let retry = self
            .ping_sent_ms
            .map(|t| now_ms.saturating_sub(t) >= ping_retry_ms(self.srtt_ms))
            .unwrap_or(true);
        if due && retry && self.feed.push_ctrl(encode_ping()) {
            self.last_ping_ms = now_ms;
            self.ping_sent_ms = Some(now_ms);
        }
    }

    fn refresh_display(&mut self) {
        let Some(screen) = self.confirmed.as_ref() else {
            self.display = None;
            return;
        };
        let mut frame = screen.frame.clone();
        self.predictor.apply(&mut frame);
        self.display = Some(frame);
    }

    /// The confirmed frame with predictions applied, or `None` before the
    /// first screen. Adapters paint this.
    pub fn display(&self) -> Option<&FrameState> {
        self.display.as_ref()
    }

    pub fn confirmed(&self) -> Option<&FrameState> {
        self.confirmed.as_ref().map(|s| &s.frame)
    }

    pub fn mode(&self) -> u16 {
        self.confirmed.as_ref().map(|s| s.frame.mode()).unwrap_or(0)
    }

    /// `Some(elapsed)` while the outage banner should be shown.
    pub fn outage(&self) -> Option<Duration> {
        self.outage
    }

    /// How long until the adapter should call [`tick`] again. `None` means no
    /// reconciliation is pending; the adapter still needs a heartbeat.
    pub fn tick_delay_ms(&self) -> Option<u64> {
        if self.predictor.active() {
            Some(50)
        } else {
            None
        }
    }

    pub fn predict_never(&self) -> bool {
        self.never
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use blit_remote::CELL_SIZE;
    use quosh_proto::{MSG_INPUT_ACK, encode_frame};

    fn text_frame(rows: &[&str], cursor_row: u16, cursor_col: u16) -> FrameState {
        let cols = rows.iter().map(|r| r.chars().count()).max().unwrap_or(0) as u16;
        let cols = cols.max(cursor_col + 2).max(24);
        let rows_n = rows.len() as u16;
        let mut cells = vec![0u8; rows_n as usize * cols as usize * CELL_SIZE];
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
                let i = (r * cols as usize + c) * CELL_SIZE;
                cells[i..i + CELL_SIZE].copy_from_slice(&cell);
            }
        }
        FrameState::from_parts(rows_n, cols, cursor_row, cursor_col, 1, "", cells)
    }

    fn screen(version: u64, echo_ack: u64, frame: FrameState) -> Vec<u8> {
        let s = Screen {
            version,
            echo_ack,
            frame,
        };
        encode_frame(MSG_SCREEN, &s.encode_compressed().unwrap())
    }

    fn hello_ok(protocol: u16) -> Vec<u8> {
        HelloOk {
            protocol,
            session_id: [0; 16],
            version: 1,
            cols: 80,
            rows: 24,
        }
        .encode()
    }

    fn drain(c: &mut Client) -> Vec<u8> {
        c.pump();
        let out = c.outbound().to_vec();
        c.advance_outbound(out.len());
        out
    }

    #[test]
    fn input_is_sequenced_then_acked() {
        let mut c = Client::new([0; 16], [0; 32], 80, 24, false);
        c.begin_connection(0);
        assert!(!drain(&mut c).is_empty(), "Hello should be queued");
        c.queue_input(b"ab".to_vec(), 0);
        assert_eq!(drain(&mut c), encode_input(1, b"ab"));
        c.queue_input(b"c".to_vec(), 1);
        assert_eq!(drain(&mut c), encode_input(2, b"c"));
        assert_eq!(c.unacked_bytes(), 3);
        c.recv_control(&encode_frame(MSG_INPUT_ACK, &2u64.to_le_bytes()), 2)
            .unwrap();
        assert_eq!(c.unacked_bytes(), 0);
    }

    #[test]
    fn reconnect_replays_unacked_input() {
        let mut c = Client::new([0; 16], [0; 32], 80, 24, false);
        c.begin_connection(0);
        drain(&mut c);
        c.queue_input(b"x".to_vec(), 0);
        drain(&mut c);
        c.end_connection();
        c.begin_connection(100);
        assert!(!drain(&mut c).is_empty(), "Hello re-sent");
        assert_eq!(drain(&mut c), encode_input(1, b"x"));
    }

    #[test]
    fn version_mismatch_is_fatal() {
        let mut c = Client::new([0; 16], [0; 32], 80, 24, false);
        c.begin_connection(0);
        let err = c
            .recv_control(&hello_ok(PROTOCOL_VERSION + 1), 0)
            .unwrap_err();
        assert!(matches!(err, ClientError::Fatal(_)));
    }

    #[test]
    fn silence_trips_the_liveness_deadline() {
        let mut c = Client::new([0; 16], [0; 32], 80, 24, false);
        c.begin_connection(0);
        assert_eq!(c.tick(1_000), Tick::Alive);
        assert_eq!(c.tick(link_dead_ms(None) - 1), Tick::Alive);
        assert_eq!(c.tick(link_dead_ms(None)), Tick::LinkDead);
        assert!(!c.hello_ok());
    }

    #[test]
    fn screen_applies_and_queues_ack_and_mode() {
        let mut c = Client::new([0; 16], [0; 32], 80, 24, false);
        c.begin_connection(0);
        drain(&mut c);
        c.recv_control(&hello_ok(PROTOCOL_VERSION), 0).unwrap();
        assert!(c.display().is_none());
        c.recv_control(&screen(1, 0, text_frame(&["hi"], 0, 2)), 0)
            .unwrap();
        assert!(c.display().is_some());
        assert_eq!(c.mode(), 1);
        assert_eq!(drain(&mut c), encode_ack_state(1));
        // A stale version is ignored and queues no further ack.
        c.recv_control(&screen(1, 0, text_frame(&["no"], 0, 2)), 0)
            .unwrap();
        assert!(drain(&mut c).is_empty());
    }

    #[test]
    fn bulk_input_is_not_predicted() {
        let mut c = Client::new([0; 16], [0; 32], 80, 24, false);
        c.begin_connection(0);
        drain(&mut c);
        c.recv_control(&hello_ok(PROTOCOL_VERSION), 0).unwrap();
        c.recv_control(&screen(1, 0, text_frame(&["hi"], 0, 2)), 0)
            .unwrap();
        drain(&mut c);
        c.queue_input(vec![b'a'; PASTE_BYTES + 1], 0);
        // No prediction: the display stays the confirmed frame.
        assert_eq!(c.display().unwrap().cursor_col(), 2);
    }

    #[test]
    fn banner_follows_silence() {
        let mut c = Client::new([0; 16], [0; 32], 80, 24, false);
        c.begin_connection(0);
        assert!(c.outage().is_none());
        c.tick(OUTAGE_BANNER_SECS * 1000 + 1);
        assert!(c.outage().is_some());
    }
}
