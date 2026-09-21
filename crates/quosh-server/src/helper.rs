use crate::cert::CertChain;
use crate::session::{Session, spawn_pair};
use anyhow::Result;
use quosh_proto::{HelperRequest, HelperResponse, IDLE_SECS, IdleInfo, validate_dims};
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

#[derive(Clone)]
pub struct Daemon {
    pub sessions: Registry,
    pub chain: Arc<CertChain>,
    pub port: u16,
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
        match req.op.as_str() {
            "idle" => self.list_idle(uid, cert).await,
            "ping" => HelperResponse {
                ok: true,
                error: None,
                session_id: None,
                token: None,
                port: Some(self.port),
                cert_sha256: Some(cert),
                idle: vec![],
            },
            "create" => self.create(uid, req, cert).await,
            other => fail(format!("unknown op {other}")),
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

    async fn list_idle(&self, uid: u32, cert: String) -> HelperResponse {
        HelperResponse {
            ok: true,
            error: None,
            session_id: None,
            token: None,
            port: Some(self.port),
            cert_sha256: Some(cert),
            idle: self.collect_idle(uid).await,
        }
    }

    async fn create(&self, uid: u32, req: HelperRequest, cert: String) -> HelperResponse {
        let cols = if req.cols == 0 { 80 } else { req.cols };
        let rows = if req.rows == 0 { 24 } else { req.rows };
        if let Err(e) = validate_dims(cols, rows) {
            return fail(e);
        }
        let mut idle = Vec::new();
        let mut id = [0u8; 16];
        let mut token = [0u8; 32];
        rand::rng().fill_bytes(&mut id);
        rand::rng().fill_bytes(&mut token);

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
        if g.len() >= MAX_SESSIONS {
            return fail("too many sessions");
        }
        if g.values().filter(|s| s.uid == uid).count() >= MAX_PER_UID {
            return fail("too many sessions for user");
        }
        match spawn_pair(id, token, uid, cols, rows) {
            Ok((sess, owner)) => {
                g.insert(id, sess.clone());
                drop(g);
                sess.start(self.sessions.clone(), owner);
                info!("session {} uid {uid} {}x{}", hex::encode(id), cols, rows);
                HelperResponse {
                    ok: true,
                    error: None,
                    session_id: Some(hex::encode(id)),
                    token: Some(hex::encode(token)),
                    port: Some(self.port),
                    cert_sha256: Some(cert),
                    idle,
                }
            }
            Err(e) => {
                let mut r = fail(format!("{e:#}"));
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
        session_id: None,
        token: None,
        port: None,
        cert_sha256: None,
        idle: vec![],
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
            port: 7,
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
        };
        let d1 = daemon.clone();
        let d2 = daemon.clone();
        let r1 = req.clone();
        let r2 = req;
        let (a, b) = tokio::join!(
            d1.create(uid, r1, String::new()),
            d2.create(uid, r2, String::new())
        );
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
                },
                String::new(),
            )
            .await;
        assert!(resp.ok, "idle cleanup should free a slot: {:?}", resp.error);
        hangup_live(&daemon).await;
        drop(holds);
    }
}
