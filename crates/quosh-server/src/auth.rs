//! The browser authentication handshake.
//!
//! A browser replaces the CLI's first `MSG_HELLO` with `MSG_AUTH_HELLO`. If it
//! already holds a live session token the server rotates it and answers
//! immediately; otherwise the server sends a fresh `MSG_CHALLENGE` and the
//! browser replies with a passkey assertion (`MSG_ASSERT`) or an enrolment
//! registration (`MSG_ENROLL`). Either way the server ends with `MSG_AUTH_OK`
//! carrying a rotated session token, the uid, and the Quosh session to use.
//!
//! The client then sends the ordinary `MSG_HELLO` with that session, so the
//! rest of the transport is unchanged. The uid is only ever taken from the
//! stored registration or the nonce, never from the network.

use crate::devices::Registration;
use crate::helper::{Daemon, unix_name};
use crate::webauthn;
use anyhow::{Context, Result, anyhow, bail};
use quosh_proto::{
    Assert, AuthFail, AuthHello, AuthOk, Challenge, Enroll, MSG_ASSERT, MSG_ENROLL, split_frame,
};
use rand::RngCore;
use std::time::{SystemTime, UNIX_EPOCH};
use wtransport::{RecvStream, SendStream};

/// The authenticated uid, for the caller to check the `Hello` session against.
pub struct Authed {
    pub uid: u32,
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Read the next framed message, pulling more bytes as needed.
async fn next_frame(recv: &mut RecvStream, buf: &mut Vec<u8>) -> Result<(u8, Vec<u8>)> {
    loop {
        if let Some(f) = split_frame(buf)? {
            return Ok(f);
        }
        let mut tmp = [0u8; 4096];
        let n = recv.read(&mut tmp).await?.context("eof during auth")?;
        if n == 0 {
            bail!("eof during auth");
        }
        buf.extend_from_slice(&tmp[..n]);
    }
}

/// Report an auth failure to the client before the connection is dropped.
async fn send_fail(send: &mut SendStream, reason: &str) -> anyhow::Error {
    if let Ok(frame) = (AuthFail {
        reason: reason.to_string(),
    })
    .encode()
    {
        let _ = send.write_all(&frame).await;
    }
    // Bounded: a peer that has stopped reading must not wedge the task.
    let _ = tokio::time::timeout(std::time::Duration::from_millis(150), send.finish()).await;
    anyhow!("auth: {reason}")
}

async fn send_ok(
    send: &mut SendStream,
    auth_token: [u8; 32],
    uid: u32,
    session_id: [u8; 16],
    session_token: [u8; 32],
    hashes: Vec<[u8; 32]>,
) -> Result<()> {
    let frame = AuthOk {
        auth_token,
        uid,
        session_id,
        session_token,
        hashes,
    }
    .encode()?;
    send.write_all(&frame).await.context("write auth ok")?;
    Ok(())
}

/// Resolve the Quosh session named in the auth hello: resume it if it belongs
/// to `uid`, otherwise create a new one.
async fn resolve_session(
    daemon: &Daemon,
    uid: u32,
    hello: &AuthHello,
    cols: u16,
    rows: u16,
) -> Result<([u8; 16], [u8; 32])> {
    if hello.session_id == [0u8; 16] {
        return daemon
            .spawn_session(uid, cols, rows)
            .await
            .map_err(|e| anyhow!("{e}"));
    }
    let g = daemon.sessions.lock().await;
    match g.get(&hello.session_id) {
        Some(s) if s.uid == uid && s.token == hello.session_token => {
            Ok((hello.session_id, hello.session_token))
        }
        _ => bail!("session does not belong to authenticated user"),
    }
}

async fn do_enroll(
    daemon: &Daemon,
    reply: &[u8],
    challenge: &[u8; 32],
    now: i64,
) -> Result<(u32, String)> {
    let e = Enroll::decode(reply).map_err(|e| anyhow!("enroll frame: {e}"))?;
    let uid = daemon
        .nonces
        .consume(&e.nonce, now)
        .ok_or_else(|| anyhow!("invalid or expired enrolment nonce"))?;
    let reg = webauthn::verify_registration(
        &daemon.rp_id,
        &daemon.origin,
        challenge,
        &e.client_data_json,
        &e.attestation_object,
    )?;
    let credential_id = hex::encode(&reg.credential_id);
    let user_name = match unix_name(uid) {
        Some(name) => format!("{name}@{}", daemon.origin),
        None => format!("uid-{uid}@{}", daemon.origin),
    };
    daemon
        .devices
        .register(Registration {
            uid,
            credential_id: credential_id.clone(),
            public_key: hex::encode(&reg.public_key),
            rp_id: daemon.rp_id.clone(),
            origin: daemon.origin.clone(),
            user_name,
            sign_count: reg.sign_count,
            created: now,
            last_used: Some(now),
        })
        .map_err(|e| anyhow!("{e:#}"))?;
    Ok((uid, credential_id))
}

async fn do_assert(
    daemon: &Daemon,
    reply: &[u8],
    challenge: &[u8; 32],
    now: i64,
) -> Result<(u32, String)> {
    let a = Assert::decode(reply).map_err(|e| anyhow!("assert frame: {e}"))?;
    let credential_id = hex::encode(&a.credential_id);
    let reg = daemon
        .devices
        .find(&credential_id)
        .ok_or_else(|| anyhow!("unknown credential"))?;
    let public_key = hex::decode(&reg.public_key).context("stored public key")?;
    let sign_count = webauthn::verify_assertion(
        &daemon.rp_id,
        &daemon.origin,
        challenge,
        &a.client_data_json,
        &a.authenticator_data,
        &a.signature,
        &public_key,
    )?;
    daemon
        .devices
        .touched(&credential_id, sign_count, now)
        .map_err(|e| anyhow!("{e:#}"))?;
    Ok((reg.uid, credential_id))
}

/// Run the handshake after the first `MSG_AUTH_HELLO` frame has been read.
pub async fn run(
    send: &mut SendStream,
    recv: &mut RecvStream,
    buf: &mut Vec<u8>,
    daemon: &Daemon,
    payload: &[u8],
) -> Result<Authed> {
    let hello = AuthHello::decode(payload).map_err(|e| anyhow!("auth hello: {e}"))?;
    let now = now();
    let cols = if hello.cols == 0 { 80 } else { hello.cols };
    let rows = if hello.rows == 0 { 24 } else { hello.rows };

    // Fast path: a live session token.
    if hello.auth_token != [0u8; 32]
        && let Some(uid) = daemon.devices.validate_token(&hello.auth_token, now)
    {
        let auth_token = daemon
            .devices
            .rotate_token(Some(&hello.auth_token), uid, None, now)
            .map_err(|e| anyhow!("{e:#}"))?;
        let hashes = daemon
            .chain
            .forward_hashes()
            .map_err(|e| anyhow!("{e:#}"))?;
        let (session_id, session_token) = match resolve_session(daemon, uid, &hello, cols, rows).await
        {
            Ok(v) => v,
            Err(e) => return Err(send_fail(send, &format!("{e:#}")).await),
        };
        send_ok(send, auth_token, uid, session_id, session_token, hashes).await?;
        return Ok(Authed { uid });
    }

    // Otherwise run a ceremony with a fresh challenge.
    let mut challenge = [0u8; 32];
    rand::rng().fill_bytes(&mut challenge);
    let hashes = daemon
        .chain
        .forward_hashes()
        .map_err(|e| anyhow!("{e:#}"))?;
    let frame = Challenge {
        challenge,
        rp_id: daemon.rp_id.clone(),
        hashes,
    }
    .encode()?;
    send.write_all(&frame).await.context("write challenge")?;

    let (typ, reply) = match next_frame(recv, buf).await {
        Ok(v) => v,
        Err(e) => return Err(send_fail(send, &format!("{e:#}")).await),
    };
    let outcome = match typ {
        MSG_ENROLL => do_enroll(daemon, &reply, &challenge, now).await,
        MSG_ASSERT => do_assert(daemon, &reply, &challenge, now).await,
        _ => Err(anyhow!("expected MSG_ASSERT or MSG_ENROLL")),
    };
    let (uid, credential_id) = match outcome {
        Ok(v) => v,
        Err(e) => return Err(send_fail(send, &format!("{e:#}")).await),
    };

    let auth_token = daemon
        .devices
        .issue_token(uid, Some(&credential_id), now)
        .map_err(|e| anyhow!("{e:#}"))?;
    let hashes = daemon
        .chain
        .forward_hashes()
        .map_err(|e| anyhow!("{e:#}"))?;
    let (session_id, session_token) = match resolve_session(daemon, uid, &hello, cols, rows).await {
        Ok(v) => v,
        Err(e) => return Err(send_fail(send, &format!("{e:#}")).await),
    };
    send_ok(send, auth_token, uid, session_id, session_token, hashes).await?;
    Ok(Authed { uid })
}