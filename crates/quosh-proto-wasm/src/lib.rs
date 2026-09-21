//! Browser (`wasm-bindgen`) bindings for `quosh-proto`.
//!
//! Exposes the pieces a browser transport needs: the auth-handshake codecs, a
//! frame splitter for the control stream, and screen decoding. The terminal
//! rendering itself is left to the host (the reference PWA decodes the raw
//! 12-byte cells; see `blit_remote::CELL_SIZE`).
//!
//! All `u64` wire values cross the boundary as `f64` (JavaScript numbers),
//! which is exact well past any plausible sequence number.

use wasm_bindgen::prelude::*;

fn js_err<E: std::fmt::Display>(e: E) -> JsValue {
    JsValue::from_str(&e.to_string())
}

fn arr16(b: &[u8]) -> Result<[u8; 16], JsValue> {
    b.try_into().map_err(|_| JsValue::from_str("expected 16 bytes"))
}

fn arr32(b: &[u8]) -> Result<[u8; 32], JsValue> {
    b.try_into().map_err(|_| JsValue::from_str("expected 32 bytes"))
}

/// One decoded screen frame.
#[wasm_bindgen]
pub struct ScreenView {
    version: u64,
    echo_ack: u64,
    frame: blit_remote::FrameState,
}

#[wasm_bindgen]
impl ScreenView {
    #[wasm_bindgen(getter)]
    pub fn version(&self) -> f64 {
        self.version as f64
    }

    #[wasm_bindgen(getter)]
    pub fn echo_ack(&self) -> f64 {
        self.echo_ack as f64
    }

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

    /// Raw cells, `rows * cols * 12` bytes (`CELL_SIZE`).
    #[wasm_bindgen(getter)]
    pub fn cells(&self) -> Vec<u8> {
        self.frame.cells().to_vec()
    }
}

/// Decode a QS2 control-frame screen payload.
#[wasm_bindgen(js_name = decodeScreen)]
pub fn decode_screen(bytes: &[u8]) -> Result<ScreenView, JsValue> {
    let s = quosh_proto::Screen::decode_compressed(bytes).map_err(js_err)?;
    Ok(ScreenView {
        version: s.version,
        echo_ack: s.echo_ack,
        frame: s.frame,
    })
}

/// One complete framed control message.
#[wasm_bindgen]
pub struct FrameView {
    typ: u8,
    payload: Vec<u8>,
}

#[wasm_bindgen]
impl FrameView {
    #[wasm_bindgen(getter)]
    pub fn typ(&self) -> u8 {
        self.typ
    }

    #[wasm_bindgen(getter)]
    pub fn payload(&self) -> Vec<u8> {
        self.payload.clone()
    }
}

/// Accumulates control-stream bytes and yields complete frames. Used by the
/// auth handshake, which runs in JavaScript before the Rust client takes over.
#[derive(Default)]
#[wasm_bindgen]
pub struct FrameBuffer {
    buf: Vec<u8>,
}

#[wasm_bindgen]
impl FrameBuffer {
    #[wasm_bindgen(constructor)]
    pub fn new() -> FrameBuffer {
        FrameBuffer::default()
    }

    pub fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// Consume the next complete frame, or return `undefined` if more bytes are
    /// needed.
    pub fn take(&mut self) -> Result<Option<FrameView>, JsValue> {
        match quosh_proto::split_frame(&mut self.buf).map_err(js_err)? {
            Some((typ, payload)) => Ok(Some(FrameView { typ, payload })),
            None => Ok(None),
        }
    }
}

/// Outbound queue with datagram/reliable screen handling (used by the client).
#[derive(Default)]
#[wasm_bindgen]
pub struct FrameFeed {
    inner: quosh_proto::FrameFeed,
}

#[wasm_bindgen]
impl FrameFeed {
    #[wasm_bindgen(constructor)]
    pub fn new() -> FrameFeed {
        FrameFeed::default()
    }

    pub fn push_ctrl(&mut self, bytes: &[u8]) {
        self.inner.push_ctrl(bytes.to_vec());
    }

    pub fn push_screen(&mut self, bytes: &[u8]) {
        self.inner.push_screen(bytes.to_vec());
    }

    pub fn writing(&self) -> bool {
        self.inner.writing()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// Pending outbound bytes.
    pub fn rest(&self) -> Vec<u8> {
        self.inner.rest().to_vec()
    }

    pub fn advance(&mut self, n: usize) {
        self.inner.advance(n);
    }
}

// -- Auth handshake codecs ---------------------------------------------------

#[wasm_bindgen(js_name = encodeAuthHello)]
pub fn encode_auth_hello(
    auth_token: &[u8],
    session_id: &[u8],
    session_token: &[u8],
    cols: u16,
    rows: u16,
) -> Result<Vec<u8>, JsValue> {
    Ok(quosh_proto::AuthHello {
        auth_token: arr32(auth_token)?,
        session_id: arr16(session_id)?,
        session_token: arr32(session_token)?,
        cols,
        rows,
    }
    .encode())
}

#[wasm_bindgen]
pub struct ChallengeView {
    challenge: Vec<u8>,
    rp_id: String,
    hashes: Vec<String>,
}

#[wasm_bindgen]
impl ChallengeView {
    #[wasm_bindgen(getter)]
    pub fn challenge(&self) -> Vec<u8> {
        self.challenge.clone()
    }

    #[wasm_bindgen(getter)]
    pub fn rp_id(&self) -> String {
        self.rp_id.clone()
    }

    /// Forward certificate hashes, hex.
    #[wasm_bindgen(getter)]
    pub fn hashes(&self) -> Vec<String> {
        self.hashes.clone()
    }
}

#[wasm_bindgen(js_name = decodeChallenge)]
pub fn decode_challenge(bytes: &[u8]) -> Result<ChallengeView, JsValue> {
    let c = quosh_proto::Challenge::decode(bytes).map_err(js_err)?;
    Ok(ChallengeView {
        challenge: c.challenge.to_vec(),
        rp_id: c.rp_id,
        hashes: c.hashes.iter().map(hex::encode).collect(),
    })
}

#[wasm_bindgen(js_name = encodeEnroll)]
pub fn encode_enroll(
    nonce: &[u8],
    client_data_json: &[u8],
    attestation_object: &[u8],
) -> Result<Vec<u8>, JsValue> {
    quosh_proto::Enroll {
        nonce: arr16(nonce)?,
        client_data_json: client_data_json.to_vec(),
        attestation_object: attestation_object.to_vec(),
    }
    .encode()
    .map_err(js_err)
}

#[wasm_bindgen(js_name = encodeAssert)]
pub fn encode_assert(
    credential_id: &[u8],
    authenticator_data: &[u8],
    client_data_json: &[u8],
    signature: &[u8],
) -> Result<Vec<u8>, JsValue> {
    quosh_proto::Assert {
        credential_id: credential_id.to_vec(),
        authenticator_data: authenticator_data.to_vec(),
        client_data_json: client_data_json.to_vec(),
        signature: signature.to_vec(),
    }
    .encode()
    .map_err(js_err)
}

#[wasm_bindgen]
pub struct AuthOkView {
    auth_token: Vec<u8>,
    uid: u32,
    session_id: Vec<u8>,
    session_token: Vec<u8>,
    hashes: Vec<String>,
}

#[wasm_bindgen]
impl AuthOkView {
    #[wasm_bindgen(getter)]
    pub fn auth_token(&self) -> Vec<u8> {
        self.auth_token.clone()
    }

    #[wasm_bindgen(getter)]
    pub fn uid(&self) -> u32 {
        self.uid
    }

    #[wasm_bindgen(getter)]
    pub fn session_id(&self) -> Vec<u8> {
        self.session_id.clone()
    }

    #[wasm_bindgen(getter)]
    pub fn session_token(&self) -> Vec<u8> {
        self.session_token.clone()
    }

    /// Forward certificate hashes, hex.
    #[wasm_bindgen(getter)]
    pub fn hashes(&self) -> Vec<String> {
        self.hashes.clone()
    }
}

#[wasm_bindgen(js_name = decodeAuthOk)]
pub fn decode_auth_ok(bytes: &[u8]) -> Result<AuthOkView, JsValue> {
    let a = quosh_proto::AuthOk::decode(bytes).map_err(js_err)?;
    Ok(AuthOkView {
        auth_token: a.auth_token.to_vec(),
        uid: a.uid,
        session_id: a.session_id.to_vec(),
        session_token: a.session_token.to_vec(),
        hashes: a.hashes.iter().map(hex::encode).collect(),
    })
}

#[wasm_bindgen(js_name = decodeAuthFail)]
pub fn decode_auth_fail(bytes: &[u8]) -> Result<String, JsValue> {
    Ok(quosh_proto::AuthFail::decode(bytes)
        .map_err(js_err)?
        .reason)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_codecs_round_trip() {
        let hello = encode_auth_hello(&[1; 32], &[2; 16], &[3; 32], 80, 24).unwrap();
        let (typ, payload) = quosh_proto::split_frame(&mut hello.clone()).unwrap().unwrap();
        assert_eq!(typ, quosh_proto::MSG_AUTH_HELLO);
        assert_eq!(quosh_proto::AuthHello::decode(&payload).unwrap().cols, 80);

        let ch = quosh_proto::Challenge {
            challenge: [4; 32],
            rp_id: "quosh.jtcs.dev".into(),
            hashes: vec![[5; 32]],
        }
        .encode()
        .unwrap();
        let (_, payload) = quosh_proto::split_frame(&mut ch.clone()).unwrap().unwrap();
        let view = decode_challenge(&payload).unwrap();
        assert_eq!(view.rp_id, "quosh.jtcs.dev");
        assert_eq!(view.challenge, vec![4; 32]);
        assert_eq!(view.hashes, vec![hex::encode([5; 32])]);
    }

    #[test]
    fn frame_buffer_yields_frames() {
        let mut fb = FrameBuffer::new();
        let a = quosh_proto::encode_frame(quosh_proto::MSG_PING, &[]);
        fb.push(&a[..2]);
        assert!(fb.take().unwrap().is_none());
        fb.push(&a[2..]);
        let f = fb.take().unwrap().unwrap();
        assert_eq!(f.typ(), quosh_proto::MSG_PING);
        assert!(fb.take().unwrap().is_none());
    }
}