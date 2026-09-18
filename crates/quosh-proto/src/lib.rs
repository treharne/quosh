//! Slice-1 framing: control messages, versioned screen payloads, helper JSON.

use blit_remote::{CELL_SIZE, FrameState, MAX_CELL_COUNT};
use lz4_flex::{compress_prepend_size, decompress_size_prepended};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::io::{self, Cursor, Read, Write};

/// Bytes waiting to go out on a framed stream. Never cancel a write after a
/// prefix has been sent: resume from `off` or drop the connection.
#[derive(Default)]
pub struct OutBuf {
    buf: Vec<u8>,
    off: usize,
}

impl OutBuf {
    pub const MAX_UNREAD: usize = 256 * 1024;

    pub fn queue(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// Queue only if the unread budget allows. Hangup/exit should use [`queue`].
    pub fn try_queue(&mut self, bytes: &[u8]) -> bool {
        if self.unread().saturating_add(bytes.len()) > Self::MAX_UNREAD {
            return false;
        }
        self.queue(bytes);
        true
    }

    pub fn rest(&self) -> &[u8] {
        if self.off >= self.buf.len() {
            &[]
        } else {
            &self.buf[self.off..]
        }
    }

    pub fn is_empty(&self) -> bool {
        self.off >= self.buf.len()
    }

    pub fn unread(&self) -> usize {
        self.buf.len().saturating_sub(self.off)
    }

    pub fn advance(&mut self, n: usize) {
        self.off += n;
        if self.off >= self.buf.len() {
            self.buf.clear();
            self.off = 0;
            return;
        }
        self.buf.drain(..self.off);
        self.off = 0;
    }
}

/// One in-flight frame (any size) plus coalesced extras. A 400 KiB screen can
/// stream; additional screens replace `screen` instead of enqueueing copies.
/// Queued control frames are byte-capped; the active `out` frame is not.
#[derive(Default)]
pub struct FrameFeed {
    out: OutBuf,
    screen: Option<Vec<u8>>,
    ctrl: VecDeque<Vec<u8>>,
    ctrl_bytes: usize,
    fin: Option<Vec<u8>>,
}

impl FrameFeed {
    pub const CTRL_CAP: usize = 64 * 1024;

    pub fn pump(&mut self) {
        if !self.out.is_empty() {
            return;
        }
        if let Some(f) = self.ctrl.pop_front() {
            self.ctrl_bytes = self.ctrl_bytes.saturating_sub(f.len());
            self.out.queue(&f);
            return;
        }
        if let Some(f) = self.screen.take() {
            self.out.queue(&f);
            return;
        }
        if let Some(f) = self.fin.take() {
            self.out.queue(&f);
        }
    }

    pub fn push_screen(&mut self, frame: Vec<u8>) {
        self.screen = Some(frame);
        self.pump();
    }

    /// Queue a control frame. Succeeds for any size when nothing is in-flight
    /// (the active frame has no size cap). Otherwise the queued-ctrl budget
    /// applies. Returns false if the extra would exceed [`CTRL_CAP`].
    pub fn push_ctrl(&mut self, frame: Vec<u8>) -> bool {
        self.pump();
        if self.out.is_empty() && self.ctrl.is_empty() {
            self.out.queue(&frame);
            return true;
        }
        if self.ctrl_bytes.saturating_add(frame.len()) > Self::CTRL_CAP {
            return false;
        }
        self.ctrl_bytes += frame.len();
        self.ctrl.push_back(frame);
        true
    }

    pub fn push_fin(&mut self, frame: Vec<u8>) {
        self.fin = Some(frame);
        self.ctrl.clear();
        self.ctrl_bytes = 0;
        self.pump();
    }

    pub fn rest(&self) -> &[u8] {
        self.out.rest()
    }

    pub fn is_empty(&self) -> bool {
        self.out.is_empty() && self.screen.is_none() && self.ctrl.is_empty() && self.fin.is_none()
    }

    pub fn writing(&self) -> bool {
        !self.out.is_empty()
    }

    pub fn advance(&mut self, n: usize) {
        self.out.advance(n);
        self.pump();
    }
}

pub const PROTOCOL_VERSION: u16 = 1;
pub const WT_PATH: &str = "/quosh";
pub const DEFAULT_PORT: u16 = 443;
pub const CONNECT_PREFIX: &str = "QUOSH CONNECT";
pub const IDLE_SECS: u64 = 7 * 24 * 60 * 60;
pub const OUTAGE_BANNER_SECS: u64 = 3;
pub const MAX_FRAME: usize = 16 * 1024 * 1024;
/// Cap on LZ4-advertised uncompressed screen bytes (before we allocate).
pub const MAX_SCREEN_RAW: usize = 8 * 1024 * 1024;
pub const MAX_INPUT_BYTES: usize = 32 * 1024;
pub const SCREEN_MAGIC: &[u8; 4] = b"QS2\0";
pub const MODE_CURSOR_VISIBLE: u16 = 1;
pub const MODE_BRACKETED_PASTE: u16 = 1 << 3;
/// Upper bound on a single axis. Together with [`MAX_CELL_COUNT`] this keeps
/// a helper-socket client from asking the root daemon for a 65535² grid.
pub const MAX_COLS: u16 = 512;
pub const MAX_ROWS: u16 = 512;
pub const MIN_COLS: u16 = 2;
pub const MIN_ROWS: u16 = 2;

pub const MSG_HELLO: u8 = 1;
pub const MSG_HELLO_OK: u8 = 2;
pub const MSG_INPUT: u8 = 3;
pub const MSG_RESIZE: u8 = 4;
pub const MSG_HANGUP: u8 = 5;
pub const MSG_ACK_STATE: u8 = 6;
pub const MSG_INPUT_ACK: u8 = 7;
pub const MSG_SCREEN: u8 = 8;
pub const MSG_EXIT: u8 = 9;
pub const MSG_ERROR: u8 = 10;
pub const MSG_PING: u8 = 11;
pub const MSG_PONG: u8 = 12;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("truncated")]
    Truncated,
    #[error("invalid magic")]
    Magic,
    #[error("invalid frame")]
    Frame,
    #[error("lz4")]
    Lz4,
    #[error("too large")]
    TooLarge,
    #[error("io: {0}")]
    Io(#[from] io::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

pub fn encode_frame(typ: u8, payload: &[u8]) -> Vec<u8> {
    let n = 1 + payload.len();
    let mut out = Vec::with_capacity(4 + n);
    out.extend_from_slice(&(n as u32).to_le_bytes());
    out.push(typ);
    out.extend_from_slice(payload);
    out
}

/// After four bytes the length is known. Zero and `> MAX_FRAME` are errors
/// even if the body has not arrived; do not treat them as incomplete.
pub fn peek_len(buf: &[u8]) -> Result<Option<usize>> {
    if buf.len() < 4 {
        return Ok(None);
    }
    let n = u32::from_le_bytes(buf[0..4].try_into().unwrap()) as usize;
    if n == 0 || n > MAX_FRAME {
        return Err(Error::TooLarge);
    }
    Ok(Some(n))
}

/// Read one length-prefixed frame from an async-friendly buffer of bytes.
pub fn split_frame(buf: &mut Vec<u8>) -> Result<Option<(u8, Vec<u8>)>> {
    let Some(n) = peek_len(buf)? else {
        return Ok(None);
    };
    if buf.len() < 4 + n {
        return Ok(None);
    }
    let mut body = buf.drain(..4 + n).skip(4).collect::<Vec<_>>();
    let typ = *body.first().ok_or(Error::Truncated)?;
    body.remove(0);
    Ok(Some((typ, body)))
}

pub struct Hello {
    pub session_id: [u8; 16],
    pub token: [u8; 32],
    pub cols: u16,
    pub rows: u16,
}

impl Hello {
    pub fn encode(&self) -> Vec<u8> {
        let mut p = Vec::with_capacity(52);
        p.extend_from_slice(&self.session_id);
        p.extend_from_slice(&self.token);
        p.extend_from_slice(&self.cols.to_le_bytes());
        p.extend_from_slice(&self.rows.to_le_bytes());
        encode_frame(MSG_HELLO, &p)
    }

    pub fn decode(p: &[u8]) -> Result<Self> {
        if p.len() != 52 {
            return Err(Error::Frame);
        }
        Ok(Self {
            session_id: p[0..16].try_into().unwrap(),
            token: p[16..48].try_into().unwrap(),
            cols: u16::from_le_bytes(p[48..50].try_into().unwrap()),
            rows: u16::from_le_bytes(p[50..52].try_into().unwrap()),
        })
    }
}

pub struct HelloOk {
    pub session_id: [u8; 16],
    pub version: u64,
    pub cols: u16,
    pub rows: u16,
}

impl HelloOk {
    pub fn encode(&self) -> Vec<u8> {
        let mut p = Vec::with_capacity(28);
        p.extend_from_slice(&self.session_id);
        p.extend_from_slice(&self.version.to_le_bytes());
        p.extend_from_slice(&self.cols.to_le_bytes());
        p.extend_from_slice(&self.rows.to_le_bytes());
        encode_frame(MSG_HELLO_OK, &p)
    }

    pub fn decode(p: &[u8]) -> Result<Self> {
        if p.len() != 28 {
            return Err(Error::Frame);
        }
        Ok(Self {
            session_id: p[0..16].try_into().unwrap(),
            version: u64::from_le_bytes(p[16..24].try_into().unwrap()),
            cols: u16::from_le_bytes(p[24..26].try_into().unwrap()),
            rows: u16::from_le_bytes(p[26..28].try_into().unwrap()),
        })
    }
}

pub fn encode_input(seq: u64, data: &[u8]) -> Vec<u8> {
    let mut p = Vec::with_capacity(8 + data.len());
    p.extend_from_slice(&seq.to_le_bytes());
    p.extend_from_slice(data);
    encode_frame(MSG_INPUT, &p)
}

pub fn decode_input(p: &[u8]) -> Result<(u64, Vec<u8>)> {
    if p.len() < 8 {
        return Err(Error::Truncated);
    }
    Ok((
        u64::from_le_bytes(p[0..8].try_into().unwrap()),
        p[8..].to_vec(),
    ))
}

pub fn encode_resize(cols: u16, rows: u16) -> Vec<u8> {
    let mut p = [0u8; 4];
    p[0..2].copy_from_slice(&cols.to_le_bytes());
    p[2..4].copy_from_slice(&rows.to_le_bytes());
    encode_frame(MSG_RESIZE, &p)
}

pub fn decode_resize(p: &[u8]) -> Result<(u16, u16)> {
    if p.len() != 4 {
        return Err(Error::Frame);
    }
    Ok((
        u16::from_le_bytes(p[0..2].try_into().unwrap()),
        u16::from_le_bytes(p[2..4].try_into().unwrap()),
    ))
}

pub fn encode_hangup() -> Vec<u8> {
    encode_frame(MSG_HANGUP, &[])
}

pub fn encode_ping() -> Vec<u8> {
    encode_frame(MSG_PING, &[])
}

pub fn encode_pong() -> Vec<u8> {
    encode_frame(MSG_PONG, &[])
}

/// Reject dimensions that would blow up the emulator. `0`×`0` is not a
/// default here — callers substitute 80×24 before validating if they want.
pub fn validate_dims(cols: u16, rows: u16) -> std::result::Result<(u16, u16), &'static str> {
    if cols < MIN_COLS || rows < MIN_ROWS {
        return Err("terminal too small");
    }
    if cols > MAX_COLS || rows > MAX_ROWS {
        return Err("terminal too large");
    }
    let cells = (cols as usize).saturating_mul(rows as usize);
    if cells > MAX_CELL_COUNT {
        return Err("terminal too large");
    }
    Ok((cols, rows))
}

pub fn encode_ack_state(version: u64) -> Vec<u8> {
    encode_frame(MSG_ACK_STATE, &version.to_le_bytes())
}

pub fn decode_u64(p: &[u8]) -> Result<u64> {
    if p.len() != 8 {
        return Err(Error::Frame);
    }
    Ok(u64::from_le_bytes(p.try_into().unwrap()))
}

pub fn encode_input_ack(seq: u64) -> Vec<u8> {
    encode_frame(MSG_INPUT_ACK, &seq.to_le_bytes())
}

pub fn encode_exit(status: i32) -> Vec<u8> {
    encode_frame(MSG_EXIT, &status.to_le_bytes())
}

pub fn decode_exit(p: &[u8]) -> Result<i32> {
    if p.len() != 4 {
        return Err(Error::Frame);
    }
    Ok(i32::from_le_bytes(p.try_into().unwrap()))
}

pub fn encode_error(code: u16, msg: &str) -> Vec<u8> {
    let b = msg.as_bytes();
    let mut p = Vec::with_capacity(4 + b.len());
    p.extend_from_slice(&code.to_le_bytes());
    p.extend_from_slice(&(b.len() as u16).to_le_bytes());
    p.extend_from_slice(b);
    encode_frame(MSG_ERROR, &p)
}

pub fn decode_error(p: &[u8]) -> Result<(u16, String)> {
    if p.len() < 4 {
        return Err(Error::Truncated);
    }
    let code = u16::from_le_bytes(p[0..2].try_into().unwrap());
    let n = u16::from_le_bytes(p[2..4].try_into().unwrap()) as usize;
    if p.len() < 4 + n {
        return Err(Error::Truncated);
    }
    Ok((code, String::from_utf8_lossy(&p[4..4 + n]).into_owned()))
}

/// Versioned screen. `version == 0` is never applied.
///
/// `echo_ack` is the input sequence this screen may be reconciled against. It
/// travels with the frame so an acknowledgement is never applied to a screen
/// that does not reflect it (see `docs/12-prediction.md`).
#[derive(Clone, Debug)]
pub struct Screen {
    pub version: u64,
    pub echo_ack: u64,
    pub frame: FrameState,
}

impl Screen {
    pub fn encode_compressed(&self) -> Result<Vec<u8>> {
        let raw = self.encode_raw()?;
        Ok(compress_prepend_size(&raw))
    }

    pub fn decode_compressed(data: &[u8]) -> Result<Self> {
        if data.len() < 4 {
            return Err(Error::Truncated);
        }
        let claimed = u32::from_le_bytes(data[0..4].try_into().unwrap()) as usize;
        if claimed > MAX_SCREEN_RAW {
            return Err(Error::TooLarge);
        }
        let raw = decompress_size_prepended(data).map_err(|_| Error::Lz4)?;
        if raw.len() > MAX_SCREEN_RAW {
            return Err(Error::TooLarge);
        }
        Self::decode_raw(&raw)
    }

    /// Apply last-state-wins: keep `incoming` iff its version is newer.
    pub fn apply_newer(current: Option<&Self>, incoming: Self) -> Option<Self> {
        match current {
            None if incoming.version > 0 => Some(incoming),
            Some(c) if incoming.version > c.version => Some(incoming),
            _ => None,
        }
    }

    fn encode_raw(&self) -> Result<Vec<u8>> {
        let mut w = Vec::new();
        w.write_all(SCREEN_MAGIC)?;
        w.write_all(&self.version.to_le_bytes())?;
        w.write_all(&self.echo_ack.to_le_bytes())?;
        w.write_all(&self.frame.rows().to_le_bytes())?;
        w.write_all(&self.frame.cols().to_le_bytes())?;
        w.write_all(&self.frame.cursor_row().to_le_bytes())?;
        w.write_all(&self.frame.cursor_col().to_le_bytes())?;
        w.write_all(&self.frame.mode().to_le_bytes())?;
        let title = self.frame.title().as_bytes();
        let tlen = u16::try_from(title.len()).unwrap_or(u16::MAX);
        w.write_all(&tlen.to_le_bytes())?;
        w.write_all(&title[..tlen as usize])?;
        let cells = self.frame.cells();
        w.write_all(&(cells.len() as u32).to_le_bytes())?;
        w.write_all(cells)?;
        let ovf = self.frame.overflow();
        w.write_all(&(ovf.len() as u32).to_le_bytes())?;
        for (idx, s) in ovf {
            w.write_all(&(*idx as u32).to_le_bytes())?;
            let b = s.as_bytes();
            let n = u16::try_from(b.len()).unwrap_or(u16::MAX);
            w.write_all(&n.to_le_bytes())?;
            w.write_all(&b[..n as usize])?;
        }
        Ok(w)
    }

    fn decode_raw(data: &[u8]) -> Result<Self> {
        let mut r = Cursor::new(data);
        let mut magic = [0u8; 4];
        r.read_exact(&mut magic).map_err(|_| Error::Truncated)?;
        if &magic != SCREEN_MAGIC {
            return Err(Error::Magic);
        }
        let mut u64b = [0u8; 8];
        r.read_exact(&mut u64b).map_err(|_| Error::Truncated)?;
        let version = u64::from_le_bytes(u64b);
        r.read_exact(&mut u64b).map_err(|_| Error::Truncated)?;
        let echo_ack = u64::from_le_bytes(u64b);
        let mut u16b = [0u8; 2];
        let mut read_u16 = |r: &mut Cursor<&[u8]>| -> Result<u16> {
            r.read_exact(&mut u16b).map_err(|_| Error::Truncated)?;
            Ok(u16::from_le_bytes(u16b))
        };
        let rows = read_u16(&mut r)?;
        let cols = read_u16(&mut r)?;
        let cursor_row = read_u16(&mut r)?;
        let cursor_col = read_u16(&mut r)?;
        let mode = read_u16(&mut r)?;
        if (rows as usize).saturating_mul(cols as usize) > MAX_CELL_COUNT {
            return Err(Error::TooLarge);
        }
        if rows > MAX_ROWS || cols > MAX_COLS {
            return Err(Error::TooLarge);
        }
        if cursor_row > rows || cursor_col > cols {
            return Err(Error::Frame);
        }
        let tlen = read_u16(&mut r)? as usize;
        let mut title = vec![0u8; tlen];
        r.read_exact(&mut title).map_err(|_| Error::Truncated)?;
        let mut u32b = [0u8; 4];
        r.read_exact(&mut u32b).map_err(|_| Error::Truncated)?;
        let clen = u32::from_le_bytes(u32b) as usize;
        let expect = rows as usize * cols as usize * CELL_SIZE;
        if clen != expect {
            return Err(Error::Frame);
        }
        let mut cells = vec![0u8; clen];
        r.read_exact(&mut cells).map_err(|_| Error::Truncated)?;
        let title = String::from_utf8_lossy(&title).into_owned();
        let mut frame =
            FrameState::from_parts(rows, cols, cursor_row, cursor_col, mode, title, cells);
        r.read_exact(&mut u32b).map_err(|_| Error::Truncated)?;
        let ovf_count = u32::from_le_bytes(u32b) as usize;
        let cell_n = rows as usize * cols as usize;
        if ovf_count > cell_n {
            return Err(Error::TooLarge);
        }
        for _ in 0..ovf_count {
            r.read_exact(&mut u32b).map_err(|_| Error::Truncated)?;
            let idx = u32::from_le_bytes(u32b) as usize;
            if idx >= cell_n {
                return Err(Error::Frame);
            }
            r.read_exact(&mut u16b).map_err(|_| Error::Truncated)?;
            let n = u16::from_le_bytes(u16b) as usize;
            let mut s = vec![0u8; n];
            r.read_exact(&mut s).map_err(|_| Error::Truncated)?;
            frame
                .overflow_mut()
                .insert(idx, String::from_utf8_lossy(&s).into_owned());
        }
        if r.position() as usize != data.len() {
            return Err(Error::Frame);
        }
        Ok(Self {
            version,
            echo_ack,
            frame,
        })
    }
}

/// Full-grid ANSI dump. Unlike blit `get_ansi_text`, trailing spaces are kept
/// so background-colored blank cells survive a clear+repaint.
pub fn frame_ansi(frame: &FrameState) -> Vec<u8> {
    let mut out = Vec::new();
    let rows = frame.rows();
    let cols = frame.cols();
    let cells = frame.cells();
    let mut cur = Style::default();
    for row in 0..rows {
        if row > 0 {
            out.extend_from_slice(b"\r\n");
        }
        for col in 0..cols {
            let style = style_at(cells, cols, row, col);
            if style != cur {
                push_sgr(&mut out, &style);
                cur = style;
            }
            let text = frame.cell_content(row, col);
            if text.is_empty() {
                continue;
            }
            out.extend_from_slice(text.as_bytes());
        }
    }
    if cur != Style::default() {
        out.extend_from_slice(b"\x1b[0m");
    }
    out
}

#[derive(Clone, Copy, PartialEq, Eq, Default)]
struct Style {
    fg: u8, // 0 default, 1 indexed, 2 rgb
    bg: u8,
    fg_v: [u8; 3],
    bg_v: [u8; 3],
    bold: bool,
    dim: bool,
    italic: bool,
    underline: bool,
    inverse: bool,
}

fn style_at(cells: &[u8], cols: u16, row: u16, col: u16) -> Style {
    let idx = (row as usize * cols as usize + col as usize) * blit_remote::CELL_SIZE;
    if idx + blit_remote::CELL_SIZE > cells.len() {
        return Style::default();
    }
    let f0 = cells[idx];
    let f1 = cells[idx + 1];
    let fg_type = f0 & 3;
    let bg_type = (f0 >> 2) & 3;
    let mut s = Style {
        fg: fg_type,
        bg: bg_type,
        bold: (f0 >> 4) & 1 != 0,
        dim: (f0 >> 5) & 1 != 0,
        italic: (f0 >> 6) & 1 != 0,
        underline: (f0 >> 7) & 1 != 0,
        inverse: f1 & 1 != 0,
        ..Style::default()
    };
    s.fg_v = [cells[idx + 2], cells[idx + 3], cells[idx + 4]];
    s.bg_v = [cells[idx + 5], cells[idx + 6], cells[idx + 7]];
    s
}

fn push_sgr(out: &mut Vec<u8>, s: &Style) {
    out.extend_from_slice(b"\x1b[0");
    if s.bold {
        out.extend_from_slice(b";1");
    }
    if s.dim {
        out.extend_from_slice(b";2");
    }
    if s.italic {
        out.extend_from_slice(b";3");
    }
    if s.underline {
        out.extend_from_slice(b";4");
    }
    if s.inverse {
        out.extend_from_slice(b";7");
    }
    match s.fg {
        1 => {
            let n = s.fg_v[0];
            out.extend_from_slice(format!(";38;5;{n}").as_bytes());
        }
        2 => {
            out.extend_from_slice(
                format!(";38;2;{};{};{}", s.fg_v[0], s.fg_v[1], s.fg_v[2]).as_bytes(),
            );
        }
        _ => {}
    }
    match s.bg {
        1 => {
            let n = s.bg_v[0];
            out.extend_from_slice(format!(";48;5;{n}").as_bytes());
        }
        2 => {
            out.extend_from_slice(
                format!(";48;2;{};{};{}", s.bg_v[0], s.bg_v[1], s.bg_v[2]).as_bytes(),
            );
        }
        _ => {}
    }
    out.push(b'm');
}

pub type ConnectBits = (u16, [u8; 32], [u8; 16], [u8; 32]);

pub fn parse_connect_line(line: &str) -> Result<ConnectBits> {
    let line = line.trim();
    let rest = line
        .strip_prefix(CONNECT_PREFIX)
        .ok_or(Error::Frame)?
        .trim();
    let mut parts = rest.split_whitespace();
    let port: u16 = parts
        .next()
        .ok_or(Error::Frame)?
        .parse()
        .map_err(|_| Error::Frame)?;
    let hash = hex_32(parts.next().ok_or(Error::Frame)?)?;
    let sid = hex_16(parts.next().ok_or(Error::Frame)?)?;
    let tok = hex_32(parts.next().ok_or(Error::Frame)?)?;
    Ok((port, hash, sid, tok))
}

pub fn format_connect_line(
    port: u16,
    hash: &[u8; 32],
    session: &[u8; 16],
    token: &[u8; 32],
) -> String {
    format!(
        "{CONNECT_PREFIX} {port} {} {} {}",
        hex::encode(hash),
        hex::encode(session),
        hex::encode(token)
    )
}

fn hex_16(s: &str) -> Result<[u8; 16]> {
    let v = hex::decode(s).map_err(|_| Error::Frame)?;
    v.try_into().map_err(|_| Error::Frame)
}

fn hex_32(s: &str) -> Result<[u8; 32]> {
    let v = hex::decode(s).map_err(|_| Error::Frame)?;
    v.try_into().map_err(|_| Error::Frame)
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct HelperRequest {
    pub op: String,
    #[serde(default)]
    pub cols: u16,
    #[serde(default)]
    pub rows: u16,
    #[serde(default)]
    pub kill_idle: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct IdleInfo {
    pub id: String,
    pub idle_secs: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct HelperResponse {
    pub ok: bool,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub token: Option<String>,
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(default)]
    pub cert_sha256: Option<String>,
    #[serde(default)]
    pub idle: Vec<IdleInfo>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn screen_roundtrip_and_last_state_wins() {
        let frame = FrameState::from_parts(2, 2, 0, 1, 0, "t", vec![0u8; 2 * 2 * CELL_SIZE]);
        let s1 = Screen {
            version: 1,
            echo_ack: 0,
            frame: frame.clone(),
        };
        let bytes = s1.encode_compressed().unwrap();
        let back = Screen::decode_compressed(&bytes).unwrap();
        assert_eq!(back.version, 1);
        assert_eq!(back.echo_ack, 0);
        assert_eq!(back.frame.rows(), 2);
        assert_eq!(back.frame.title(), "t");

        let s1_ack = Screen {
            version: 1,
            echo_ack: 7,
            frame: frame.clone(),
        };
        let back = Screen::decode_compressed(&s1_ack.encode_compressed().unwrap()).unwrap();
        assert_eq!(back.echo_ack, 7);

        let s2 = Screen {
            version: 2,
            echo_ack: 0,
            frame: frame.clone(),
        };
        let s0 = Screen {
            version: 0,
            echo_ack: 0,
            frame: frame.clone(),
        };
        assert!(Screen::apply_newer(Some(&s2), s1.clone()).is_none());
        assert!(Screen::apply_newer(Some(&s1), s2).is_some());
        assert!(Screen::apply_newer(None, s0).is_none());
    }

    #[test]
    fn hello_and_connect_line() {
        let h = Hello {
            session_id: [1; 16],
            token: [2; 32],
            cols: 80,
            rows: 24,
        };
        let enc = h.encode();
        let mut buf = enc;
        let (typ, payload) = split_frame(&mut buf).unwrap().unwrap();
        assert_eq!(typ, MSG_HELLO);
        let d = Hello::decode(&payload).unwrap();
        assert_eq!(d.cols, 80);
        let line = format_connect_line(443, &[9; 32], &[1; 16], &[2; 32]);
        let (port, hash, sid, tok) = parse_connect_line(&line).unwrap();
        assert_eq!(port, 443);
        assert_eq!(hash, [9; 32]);
        assert_eq!(sid, [1; 16]);
        assert_eq!(tok, [2; 32]);
    }

    #[test]
    fn outbuf_resume_does_not_drop_prefix() {
        let mut o = OutBuf::default();
        o.queue(&[1, 2, 3, 4, 5]);
        o.advance(2);
        assert_eq!(o.rest(), &[3, 4, 5]);
        o.queue(&[6]);
        assert_eq!(o.rest(), &[3, 4, 5, 6]);
        o.advance(4);
        assert!(o.is_empty());
        o.queue(&[1, 2, 3, 4]);
        o.advance(1);
        assert_eq!(o.unread(), 3);
        assert_eq!(o.rest(), &[2, 3, 4]);
    }

    #[test]
    fn frame_feed_streams_large_screen_when_empty() {
        let mut f = FrameFeed::default();
        let big = vec![7u8; 300_000];
        f.push_screen(big.clone());
        assert_eq!(f.rest().len(), 300_000);
        f.advance(100_000);
        assert_eq!(f.rest().len(), 200_000);
        let big2 = vec![8u8; 300_000];
        f.push_screen(big2);
        f.advance(200_000);
        assert_eq!(f.rest().len(), 300_000);
        assert_eq!(f.rest()[0], 8);
    }

    #[test]
    fn frame_feed_accepts_screen_over_outbuf_max() {
        let n = 300usize * 300 * CELL_SIZE;
        let mut cells = vec![0u8; n];
        let mut x: u32 = 1;
        for b in cells.iter_mut() {
            x = x.wrapping_mul(1664525).wrapping_add(1013904223);
            *b = (x >> 24) as u8;
        }
        let frame = FrameState::from_parts(300, 300, 0, 0, 0, "", cells);
        let blob = Screen {
            version: 1,
            echo_ack: 0,
            frame,
        }
        .encode_compressed()
        .unwrap();
        assert!(
            blob.len() > OutBuf::MAX_UNREAD,
            "compressed screen should exceed OutBuf::MAX_UNREAD, got {}",
            blob.len()
        );
        let framed = encode_frame(MSG_SCREEN, &blob);
        let mut o = OutBuf::default();
        assert!(
            !o.try_queue(&framed),
            "try_queue must still reject oversized frames"
        );
        let mut f = FrameFeed::default();
        f.push_screen(framed.clone());
        assert_eq!(f.rest().len(), framed.len());
        f.advance(framed.len() / 2);
        assert_eq!(f.rest().len(), framed.len() - framed.len() / 2);
        f.advance(f.rest().len());
        assert!(f.is_empty());
    }

    #[test]
    fn frame_feed_sends_screen_before_fin() {
        let mut f = FrameFeed::default();
        f.push_screen(vec![1, 2, 3, 4]);
        f.push_fin(vec![9, 9]);
        assert_eq!(f.rest(), &[1, 2, 3, 4]);
        f.advance(4);
        assert_eq!(f.rest(), &[9, 9]);
    }

    #[test]
    fn frame_feed_ctrl_cap_does_not_block_active_frame() {
        let mut f = FrameFeed::default();
        let big = vec![3u8; FrameFeed::CTRL_CAP + 8];
        assert!(f.push_ctrl(big.clone()));
        assert_eq!(f.rest().len(), big.len());
        assert!(!f.push_ctrl(vec![1; FrameFeed::CTRL_CAP + 1]));
        assert!(f.push_ctrl(vec![4; 16]));
        f.advance(big.len());
        assert_eq!(f.rest(), &[4; 16]);
    }

    #[test]
    fn split_frame_partial() {
        let mut buf = encode_frame(MSG_HANGUP, &[]);
        let full = buf.clone();
        buf.truncate(2);
        assert!(split_frame(&mut buf).unwrap().is_none());
        buf = full;
        assert!(split_frame(&mut buf).unwrap().is_some());
        assert!(buf.is_empty());
    }

    #[test]
    fn peek_len_rejects_zero_and_oversize_before_body() {
        assert!(peek_len(&[]).unwrap().is_none());
        assert!(peek_len(&[1, 0, 0]).unwrap().is_none());
        assert!(matches!(
            peek_len(&0u32.to_le_bytes()),
            Err(Error::TooLarge)
        ));
        let over = (MAX_FRAME as u32 + 1).to_le_bytes();
        assert!(matches!(peek_len(&over), Err(Error::TooLarge)));
        assert_eq!(peek_len(&10u32.to_le_bytes()).unwrap(), Some(10));
        let mut buf = 0u32.to_le_bytes().to_vec();
        buf.push(MSG_INPUT);
        assert!(split_frame(&mut buf).is_err());
    }

    #[test]
    fn dims_rejected_when_huge_or_tiny() {
        assert!(validate_dims(80, 24).is_ok());
        assert!(validate_dims(1, 24).is_err());
        assert!(validate_dims(80, 1).is_err());
        assert!(validate_dims(MAX_COLS, MAX_ROWS).is_ok());
        assert!(validate_dims(MAX_COLS + 1, 24).is_err());
        assert!(validate_dims(u16::MAX, u16::MAX).is_err());
    }

    #[test]
    fn decode_rejects_lz4_bomb_and_trailing_bytes() {
        let mut bomb = (MAX_SCREEN_RAW as u32 + 1).to_le_bytes().to_vec();
        bomb.extend_from_slice(&[0, 1, 2, 3]);
        assert!(Screen::decode_compressed(&bomb).is_err());

        let frame = FrameState::from_parts(2, 2, 0, 0, 0, "", vec![0u8; 2 * 2 * CELL_SIZE]);
        let s = Screen {
            version: 1,
            echo_ack: 0,
            frame,
        };
        let mut raw = s.encode_raw().unwrap();
        raw.push(0);
        assert!(Screen::decode_raw(&raw).is_err());
    }

    #[test]
    fn frame_ansi_keeps_trailing_spaces() {
        let frame = FrameState::from_parts(1, 4, 0, 0, 0, "", vec![0u8; 4 * CELL_SIZE]);
        let ansi = frame_ansi(&frame);
        let spaces = ansi.iter().filter(|&&b| b == b' ').count();
        assert_eq!(spaces, 4);
    }
}
