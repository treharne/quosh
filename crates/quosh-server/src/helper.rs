use crate::cert::CertChain;
use crate::devices::DeviceStore;
use crate::enroll::NonceStore;
use crate::session::{Session, spawn_pair};
use anyhow::Result;
use quosh_proto::{
    DeviceInfo, HelperRequest, HelperResponse, IDLE_SECS, IdleInfo, SessionInfo, validate_dims,
};
use rand::RngCore;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Mutex;
use tracing::{info, warn};

pub type Registry = Arc<Mutex<HashMap<[u8; 16], Arc<Session>>>>;

const MAX_HELPER_BYTES: usize = 4096;
const MAX_SESSIONS: usize = 128;
const MAX_PER_UID: usize = 32;

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[derive(Clone)]
pub struct Daemon {
    pub sessions: Registry,
    pub chain: Arc<CertChain>,
    pub devices: Arc<DeviceStore>,
    pub nonces: Arc<NonceStore>,
    pub port: u16,
    /// WebAuthn Relying Party ID.
    pub rp_id: String,
    /// Expected WebAuthn origin.
    pub origin: String,
}

impl Daemon {
    pub async fn serve_unix(self: Arc<Self>, listener: UnixListener) {
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    let d = self.clone();
                    tokio::spawn(async move {
                        if let Err(e) = d.handle_helper(stream).await {
                            warn!("helper: {e:#}");
                        }
                    });
                }
                Err(e) => warn!("unix accept: {e}"),
            }
        }
    }

    async fn handle_helper(&self, stream: UnixStream) -> Result<()> {
        let std = stream.into_std()?;
        let uid = crate::session::peer_uid(&std)?;
        std.set_nonblocking(true)?;
        let mut stream = UnixStream::from_std(std)?;
        let line = read_request_line(&mut stream, MAX_HELPER_BYTES, Duration::from_secs(2)).await?;
        if line.is_empty() {
            return Ok(());
        }
        let req: HelperRequest = serde_json::from_slice(&line)?;
        let resp = self.dispatch(uid, req).await;
        let mut out = serde_json::to_string(&resp)?;
        out.push('\n');
        stream.write_all(out.as_bytes()).await?;
        Ok(())
    }

    async fn dispatch(&self, uid: u32, req: HelperRequest) -> HelperResponse {
        let cert = match self.chain.current_hash() {
            Ok(h) => hex::encode(h),
            Err(e) => return fail(format!("certificate: {e:#}")),
        };
        let base = || HelperResponse {
            ok: true,
            port: Some(self.port),
            cert_sha256: Some(cert.clone()),
            ..Default::default()
        };
        match req.op.as_str() {
            "idle" => {
                let mut r = base();
                r.idle = self.collect_idle(uid).await;
                r
            }
            "ping" => base(),
            "create" => self.create(uid, req, base()).await,
            "enroll-nonce" => self.enroll_nonce(uid, base()),
            "devices" => self.list_devices(uid, base()),
            "revoke" => self.revoke(uid, &req, base()),
            other => fail(format!("unknown op {other}")),
        }
    }

    /// Mint a one-time enrolment nonce bound to the SSH-authenticated uid.
    fn enroll_nonce(&self, uid: u32, mut resp: HelperResponse) -> HelperResponse {
        let nonce = self.nonces.issue(uid, now());
        resp.nonce = Some(hex::encode(nonce));
        resp.user = unix_name(uid);
        resp
    }

    fn list_devices(&self, uid: u32, mut resp: HelperResponse) -> HelperResponse {
        resp.devices = self
            .devices
            .registrations(uid)
            .into_iter()
            .map(|r| DeviceInfo {
                credential: r.credential_id,
                user: r.user_name,
                created: r.created,
                last_used: r.last_used,
            })
            .collect();
        resp.sessions = self
            .devices
            .tokens(uid)
            .into_iter()
            .map(|(token, last_seen)| SessionInfo { token, last_seen })
            .collect();
        resp
    }

    fn revoke(&self, uid: u32, req: &HelperRequest, mut resp: HelperResponse) -> HelperResponse {
        let result = if req.all {
            self.devices.revoke_all(uid).map(|n| (n, "registrations"))
        } else if req.sessions {
            self.devices.revoke_sessions(uid).map(|n| (n, "sessions"))
        } else if let Some(prefix) = &req.credential {
            match resolve_credential(&self.devices, uid, prefix) {
                Ok(cred) => self
                    .devices
                    .revoke_credential(uid, &cred)
                    .map(|ok| (ok as usize, "credential")),
                Err(e) => return fail(e),
            }
        } else {
            return fail("revoke needs a credential, --all, or --sessions");
        };
        match result {
            Ok((n, what)) => {
                info!("revoked {n} {what} for uid {uid}");
                resp.revoked = Some(n);
                resp
            }
            Err(e) => fail(format!("revoke: {e:#}")),
        }
    }

    async fn collect_idle(&self, uid: u32) -> Vec<IdleInfo> {
        let g = self.sessions.lock().await;
        let mut idle = Vec::new();
        for (id, s) in g.iter() {
            if s.uid != uid {
                continue;
            }
            if let Some(secs) = s.detached_secs().await
                && secs >= IDLE_SECS
            {
                idle.push(IdleInfo {
                    id: hex::encode(id),
                    idle_secs: secs,
                });
            }
        }
        idle
    }

    /// Create a session subject to the global and per-uid limits. Shared by
    /// the SSH helper and the browser auth handshake.
    pub(crate) async fn spawn_session(
        &self,
        uid: u32,
        cols: u16,
        rows: u16,
    ) -> std::result::Result<([u8; 16], [u8; 32]), String> {
        let (cols, rows) = validate_dims(cols, rows).map_err(str::to_string)?;
        let mut id = [0u8; 16];
        let mut token = [0u8; 32];
        rand::rng().fill_bytes(&mut id);
        rand::rng().fill_bytes(&mut token);
        let mut g = self.sessions.lock().await;
        if g.len() >= MAX_SESSIONS {
            return Err("too many sessions".into());
        }
        if g.values().filter(|s| s.uid == uid).count() >= MAX_PER_UID {
            return Err("too many sessions for user".into());
        }
        match spawn_pair(id, token, uid, cols, rows) {
            Ok((sess, owner)) => {
                g.insert(id, sess.clone());
                drop(g);
                sess.start(self.sessions.clone(), owner);
                Ok((id, token))
            }
            Err(e) => Err(format!("{e:#}")),
        }
    }

    async fn create(
        &self,
        uid: u32,
        req: HelperRequest,
        mut resp: HelperResponse,
    ) -> HelperResponse {
        let cols = if req.cols == 0 { 80 } else { req.cols };
        let rows = if req.rows == 0 { 24 } else { req.rows };
        // Collect (and optionally kill) idle sessions first, so a user at the
        // limit can replace one by reconnecting.
        let mut idle = Vec::new();
        {
            let mut g = self.sessions.lock().await;
            let mut drop_ids = Vec::new();
            for (sid, s) in g.iter() {
                if s.uid != uid {
                    continue;
                }
                if let Some(secs) = s.detached_secs().await
                    && secs >= IDLE_SECS
                {
                    idle.push(IdleInfo {
                        id: hex::encode(sid),
                        idle_secs: secs,
                    });
                    if req.kill_idle {
                        drop_ids.push(*sid);
                    }
                }
            }
            for sid in drop_ids {
                if let Some(s) = g.remove(&sid) {
                    s.request_hangup();
                    info!("killed idle session {}", hex::encode(sid));
                }
            }
        }
        match self.spawn_session(uid, cols, rows).await {
            Ok((id, token)) => {
                info!("session {} uid {uid} {}x{}", hex::encode(id), cols, rows);
                resp.session_id = Some(hex::encode(id));
                resp.token = Some(hex::encode(token));
                resp.idle = idle;
                resp
            }
            Err(e) => {
                let mut r = fail(e);
                r.idle = idle;
                r
            }
        }
    }
}

fn fail(msg: impl Into<String>) -> HelperResponse {
    HelperResponse {
        ok: false,
        error: Some(msg.into()),
        ..Default::default()
    }
}

/// Work out the display name `<unix_user>@<server>` uses. Only the user part
/// comes from the server; the browser knows the address.
pub(crate) fn unix_name(uid: u32) -> Option<String> {
    nix::unistd::User::from_uid(nix::unistd::Uid::from_raw(uid))
        .ok()
        .flatten()
        .map(|u| u.name)
}

/// Resolve a full credential id or a unique prefix (matching what `quosh
/// devices` shows).
fn resolve_credential(
    devices: &DeviceStore,
    uid: u32,
    prefix: &str,
) -> std::result::Result<String, String> {
    let matches: Vec<_> = devices
        .registrations(uid)
        .into_iter()
        .filter(|r| r.credential_id.starts_with(prefix))
        .collect();
    match matches.len() {
        1 => Ok(matches.into_iter().next().unwrap().credential_id),
        0 => Err(format!("no credential matching {prefix}")),
        n => Err(format!("{n} credentials match {prefix}; be more specific")),
    }
}

async fn read_request_line(
    stream: &mut UnixStream,
    max: usize,
    limit: Duration,
) -> Result<Vec<u8>> {
    let deadline = tokio::time::Instant::now() + limit;
    let mut acc = Vec::new();
    let mut tmp = [0u8; 256];
    loop {
        if let Some(i) = acc.iter().position(|b| *b == b'\n') {
            acc.truncate(i);
            return Ok(acc);
        }
        if acc.len() >= max {
            anyhow::bail!("helper request too large");
        }
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            anyhow::bail!("helper request timeout");
        }
        let n = tokio::time::timeout(left, stream.read(&mut tmp)).await??;
        if n == 0 {
            return Ok(acc);
        }
        let take = n.min(max.saturating_sub(acc.len()));
        acc.extend_from_slice(&tmp[..take]);
        if take < n && !acc.contains(&b'\n') {
            anyhow::bail!("helper request too large");
        }
    }
}

pub async fn destroy(sessions: &Registry, id: [u8; 16]) {
    if let Some(s) = sessions.lock().await.remove(&id) {
        s.request_hangup();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::StubSession;
    use nix::unistd::Uid;
    use tokio::net::UnixStream as TokioUnix;

    fn test_daemon() -> Daemon {
        let dir = std::env::temp_dir().join(format!(
            "quosh-helper-{}-{}",
            std::process::id(),
            rand::random::<u32>()
        ));
        let chain = CertChain::load(&dir, 1).expect("chain");
        Daemon {
            sessions: Arc::new(Mutex::new(Default::default())),
            chain,
            devices: DeviceStore::load(&dir.join("devices.json")).expect("devices"),
            nonces: NonceStore::new(),
            port: 7,
            rp_id: "quosh.jtcs.dev".into(),
            origin: "https://quosh.jtcs.dev".into(),
        }
    }

    fn ok_resp() -> HelperResponse {
        HelperResponse {
            ok: true,
            port: Some(7),
            ..Default::default()
        }
    }

    async fn hangup_live(daemon: &Daemon) {
        let ids: Vec<_> = daemon.sessions.lock().await.keys().copied().collect();
        for id in ids {
            destroy(&daemon.sessions, id).await;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    #[tokio::test]
    async fn helper_line_accumulates_fragments() {
        let (a, mut b) = TokioUnix::pair().unwrap();
        let mut a = a;
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            let _ = b.write_all(b"{\"op\":\"ping\"").await;
            tokio::time::sleep(Duration::from_millis(20)).await;
            let _ = b.write_all(b"}\n").await;
        });
        let line = read_request_line(&mut a, 4096, Duration::from_secs(2))
            .await
            .unwrap();
        assert_eq!(line, br#"{"op":"ping"}"#);
    }

    #[tokio::test]
    async fn enroll_nonce_and_device_ops() {
        use crate::devices::Registration;
        let uid = Uid::current().as_raw();
        let daemon = test_daemon();

        let r = daemon
            .dispatch(
                uid,
                HelperRequest {
                    op: "enroll-nonce".into(),
                    ..Default::default()
                },
            )
            .await;
        assert!(r.ok);
        let nonce: [u8; 16] = hex::decode(r.nonce.unwrap()).unwrap().try_into().unwrap();
        assert_eq!(daemon.nonces.consume(&nonce, now()), Some(uid));
        // Single use.
        assert_eq!(daemon.nonces.consume(&nonce, now()), None);

        daemon
            .devices
            .register(Registration {
                uid,
                credential_id: "abcdef0123456789".into(),
                public_key: "04".into(),
                rp_id: daemon.rp_id.clone(),
                origin: daemon.origin.clone(),
                user_name: format!("{uid}@host"),
                sign_count: 0,
                created: 1,
                last_used: None,
            })
            .unwrap();
        let r = daemon
            .dispatch(
                uid,
                HelperRequest {
                    op: "devices".into(),
                    ..Default::default()
                },
            )
            .await;
        assert_eq!(r.devices.len(), 1);
        assert_eq!(r.devices[0].credential, "abcdef0123456789");

        // Revoke by the short prefix shown in `quosh devices`.
        let r = daemon
            .dispatch(
                uid,
                HelperRequest {
                    op: "revoke".into(),
                    credential: Some("abcdef".into()),
                    ..Default::default()
                },
            )
            .await;
        assert_eq!(r.revoked, Some(1));
        assert!(
            daemon
                .dispatch(
                    uid,
                    HelperRequest {
                        op: "devices".into(),
                        ..Default::default()
                    }
                )
                .await
                .devices
                .is_empty()
        );
    }

    #[tokio::test]
    async fn concurrent_create_respects_per_uid_limit() {
        let uid = Uid::current().as_raw();
        let daemon = test_daemon();
        let mut holds = Vec::new();
        {
            let mut g = daemon.sessions.lock().await;
            for i in 0..MAX_PER_UID - 1 {
                let mut id = [0u8; 16];
                id[0] = i as u8;
                let stub = StubSession::new(id, uid);
                g.insert(id, stub.sess.clone());
                holds.push(stub);
            }
        }
        let req = HelperRequest {
            op: "create".into(),
            cols: 80,
            rows: 24,
            kill_idle: false,
            ..Default::default()
        };
        let d1 = daemon.clone();
        let d2 = daemon.clone();
        let r1 = req.clone();
        let r2 = req;
        let (a, b) = tokio::join!(d1.create(uid, r1, ok_resp()), d2.create(uid, r2, ok_resp()));
        let oks = [a.ok, b.ok].into_iter().filter(|x| *x).count();
        assert_eq!(
            oks, 1,
            "exactly one of two concurrent creates should succeed"
        );
        let n = daemon
            .sessions
            .lock()
            .await
            .values()
            .filter(|s| s.uid == uid)
            .count();
        assert_eq!(n, MAX_PER_UID);
        hangup_live(&daemon).await;
        drop(holds);
    }

    #[tokio::test]
    async fn create_kills_idle_before_rejecting_at_limit() {
        let uid = Uid::current().as_raw();
        let daemon = test_daemon();
        let mut holds = Vec::new();
        {
            let mut g = daemon.sessions.lock().await;
            for i in 0..MAX_PER_UID {
                let mut id = [0u8; 16];
                id[0] = i as u8;
                let stub = StubSession::detached_for(id, uid, Duration::from_secs(IDLE_SECS + 60));
                g.insert(id, stub.sess.clone());
                holds.push(stub);
            }
        }
        let resp = daemon
            .create(
                uid,
                HelperRequest {
                    op: "create".into(),
                    cols: 80,
                    rows: 24,
                    kill_idle: true,
                    ..Default::default()
                },
                ok_resp(),
            )
            .await;
        assert!(resp.ok, "idle cleanup should free a slot: {:?}", resp.error);
        hangup_live(&daemon).await;
        drop(holds);
    }
}
