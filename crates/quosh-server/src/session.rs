use crate::helper::Registry;
use crate::pty::{dup_cloexec, open_cloexec};
use crate::term::Emulator;
use anyhow::{Context, Result, bail};
use libc::{TIOCSCTTY, TIOCSWINSZ, winsize};
use nix::sys::socket::{getsockopt, sockopt::PeerCredentials};
use nix::unistd::{Gid, Uid, User, getgrouplist};
use quosh_proto::{MAX_INPUT_BYTES, Screen, validate_dims};
use std::ffi::CString;
use std::io::{Read, Write};
use std::os::fd::{AsFd, AsRawFd};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::io::unix::AsyncFd;
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, Notify, mpsc, watch};

const CMD_Q: usize = 32;
/// Pause PTY reads and new commands at this pending-write size.
const MAX_PENDING: usize = 1024 * 1024;
/// Kill the session if a single generated burst still exceeds this.
const PENDING_HARD_MAX: usize = MAX_PENDING + 64 * 1024;

/// Append terminal-generated (or input) bytes. False means the hard cap was
/// hit; the owner must terminate rather than grow without bound.
fn append_generated(pending: &mut Vec<u8>, bytes: &[u8]) -> bool {
    if pending.len().saturating_add(bytes.len()) > PENDING_HARD_MAX {
        return false;
    }
    pending.extend_from_slice(bytes);
    true
}

pub enum QueueResult {
    Queued,
    Busy,
}

enum OwnerMsg {
    Input { seq: u64, data: Vec<u8> },
    Resize { cols: u16, rows: u16 },
    Ack(u64),
}

pub struct Latest {
    pub version: u64,
    pub blob: Vec<u8>,
}

pub struct Session {
    pub id: [u8; 16],
    pub token: [u8; 32],
    pub uid: u32,
    pub last_detach: Mutex<Option<Instant>>,
    hangup: Notify,
    shutdown: AtomicBool,
    epoch: AtomicU64,
    epoch_watch: watch::Sender<u64>,
    last_acked: AtomicU64,
    latest: watch::Sender<Option<Arc<Latest>>>,
    size: Mutex<(u16, u16)>,
    cmds: mpsc::Sender<OwnerMsg>,
    exit: watch::Sender<Option<i32>>,
    accepted: watch::Sender<u64>,
    #[cfg(test)]
    control_leftover: std::sync::Mutex<Vec<u8>>,
}

impl Session {
    pub fn start(self: &Arc<Self>, registry: Registry, owner: Owner) {
        tokio::spawn(async move { owner.run(registry).await });
    }

    pub fn subscribe_latest(&self) -> watch::Receiver<Option<Arc<Latest>>> {
        self.latest.subscribe()
    }

    pub fn subscribe_exit(&self) -> watch::Receiver<Option<i32>> {
        self.exit.subscribe()
    }

    pub fn exit_code(&self) -> Option<i32> {
        *self.exit.borrow()
    }

    #[cfg(test)]
    pub(crate) fn publish_exit(&self, st: i32) {
        let _ = self.exit.send_replace(Some(st));
    }

    #[cfg(test)]
    pub(crate) fn publish_screen_blob(&self, blob: Vec<u8>) {
        let version = self.version() + 1;
        let _ = self.latest.send_replace(Some(Arc::new(Latest { version, blob })));
    }

    #[cfg(test)]
    pub(crate) fn note_control_leftover(&self, leftover: &[u8]) {
        *self.control_leftover.lock().unwrap() = leftover.to_vec();
    }

    #[cfg(test)]
    pub(crate) fn test_control_leftover(&self) -> Vec<u8> {
        self.control_leftover.lock().unwrap().clone()
    }

    pub fn subscribe_epoch(&self) -> watch::Receiver<u64> {
        self.epoch_watch.subscribe()
    }

    pub fn subscribe_accepted(&self) -> watch::Receiver<u64> {
        self.accepted.subscribe()
    }

    pub fn current_epoch(&self) -> u64 {
        self.epoch.load(Ordering::SeqCst)
    }

    pub async fn claim_transport(&self) -> u64 {
        let next = self.epoch.fetch_add(1, Ordering::SeqCst) + 1;
        let _ = self.epoch_watch.send_replace(next);
        *self.last_detach.lock().await = None;
        next
    }

    pub fn accepted_seq(&self) -> u64 {
        *self.accepted.borrow()
    }

    pub async fn detach_if_epoch(&self, epoch: u64) {
        if self.current_epoch() == epoch {
            *self.last_detach.lock().await = Some(Instant::now());
        }
    }

    pub fn ack_state(&self, version: u64) {
        self.last_acked.fetch_max(version, Ordering::SeqCst);
    }

    pub fn last_acked(&self) -> u64 {
        self.last_acked.load(Ordering::SeqCst)
    }

    pub fn latest(&self) -> Option<Arc<Latest>> {
        self.latest.borrow().clone()
    }

    pub fn try_input(&self, seq: u64, data: Vec<u8>) -> Result<QueueResult> {
        if data.is_empty() {
            bail!("empty input");
        }
        if data.len() > MAX_INPUT_BYTES {
            bail!("input too large");
        }
        match self.cmds.try_send(OwnerMsg::Input { seq, data }) {
            Ok(()) => Ok(QueueResult::Queued),
            Err(mpsc::error::TrySendError::Full(_)) => Ok(QueueResult::Busy),
            Err(mpsc::error::TrySendError::Closed(_)) => bail!("session closed"),
        }
    }

    pub fn try_resize(&self, cols: u16, rows: u16) -> Result<QueueResult> {
        validate_dims(cols, rows).map_err(anyhow::Error::msg)?;
        match self.cmds.try_send(OwnerMsg::Resize { cols, rows }) {
            Ok(()) => Ok(QueueResult::Queued),
            Err(mpsc::error::TrySendError::Full(_)) => Ok(QueueResult::Busy),
            Err(mpsc::error::TrySendError::Closed(_)) => bail!("session closed"),
        }
    }

    pub fn try_ack(&self, version: u64) {
        let _ = self.cmds.try_send(OwnerMsg::Ack(version));
        self.ack_state(version);
    }

    pub fn request_hangup(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
        // notify_one stores a permit if the owner is not waiting, so a
        // shutdown between the flag check and notified() still wakes it.
        self.hangup.notify_one();
    }

    pub fn is_shutdown(&self) -> bool {
        self.shutdown.load(Ordering::SeqCst)
    }

    pub async fn size(&self) -> (u16, u16) {
        *self.size.lock().await
    }

    pub fn version(&self) -> u64 {
        self.latest
            .borrow()
            .as_ref()
            .map(|s| s.version)
            .unwrap_or(0)
    }

    pub async fn detached_secs(&self) -> Option<u64> {
        self.last_detach.lock().await.map(|t| t.elapsed().as_secs())
    }
}

pub struct Owner {
    sess: Arc<Session>,
    cmd_rx: mpsc::Receiver<OwnerMsg>,
    child: Child,
    master: std::fs::File,
    driver: Emulator,
    last_seq: u64,
    version: u64,
}

impl Owner {
    async fn run(self, registry: Registry) {
        let Owner {
            sess,
            mut cmd_rx,
            mut child,
            master,
            mut driver,
            mut last_seq,
            mut version,
        } = self;
        let pid = child.id();
        let afd = match AsyncFd::new(master) {
            Ok(f) => f,
            Err(e) => {
                tracing::error!("asyncfd: {e}");
                registry.lock().await.remove(&sess.id);
                return;
            }
        };
        let mut buf = [0u8; 8192];
        let mut child_done: Option<i32> = None;
        let mut pending = Vec::new();
        let mut woff = 0usize;
        loop {
            if sess.is_shutdown() {
                break;
            }
            if woff > 0 {
                pending.drain(..woff);
                woff = 0;
            }
            let unread = pending.len();
            let want_write = unread > 0;
            let accept_more = unread < MAX_PENDING;
            let sync_at = driver.sync_deadline().map(tokio::time::Instant::from_std);
            tokio::select! {
                _ = sess.hangup.notified() => {
                    break;
                }
                st = child.wait() => {
                    child_done = Some(st.ok().and_then(|s| s.code()).unwrap_or(1));
                    break;
                }
                // Always drain PTY output. Gating reads on a full input buffer
                // deadlocks a child that writes before reading more stdin.
                // Generated DSR/DA replies still hit append_generated's hard cap.
                r = afd.readable() => {
                    let n = {
                        let mut g = match r {
                            Ok(g) => g,
                            Err(_) => break,
                        };
                        match g.try_io(|inner| inner.get_ref().read(&mut buf)) {
                            Ok(Ok(n)) => n,
                            Ok(Err(_)) => break,
                            Err(_would_block) => continue,
                        }
                    };
                    if n == 0 {
                        break;
                    }
                    driver.process(&buf[..n]);
                    if !append_generated(&mut pending, &driver.take_pty_write()) {
                        tracing::error!("pty-generated writes exceeded pending budget");
                        break;
                    }
                    publish(&sess, &mut driver, &mut version);
                }
                r = afd.writable(), if want_write => {
                    let mut g = match r {
                        Ok(g) => g,
                        Err(_) => break,
                    };
                    match g.try_io(|inner| inner.get_ref().write(&pending[woff..])) {
                        Ok(Ok(0)) | Ok(Err(_)) => break,
                        Ok(Ok(n)) => {
                            woff += n;
                            pending.drain(..woff);
                            woff = 0;
                        }
                        Err(_would_block) => {}
                    }
                }
                msg = cmd_rx.recv(), if accept_more => {
                    match msg {
                        None => break,
                        Some(OwnerMsg::Input { seq, data }) => {
                            let next = if last_seq == 0 { 1 } else { last_seq + 1 };
                            if seq == next {
                                if !append_generated(&mut pending, &data) {
                                    tracing::error!("input exceeded pending budget");
                                    break;
                                }
                                last_seq = seq;
                                let _ = sess.accepted.send_replace(last_seq);
                            }
                        }
                        Some(OwnerMsg::Resize { cols: c, rows: r }) => {
                            if validate_dims(c, r).is_ok() {
                                driver.resize(r, c);
                                *sess.size.lock().await = (c, r);
                                let ws = winsize {
                                    ws_row: r,
                                    ws_col: c,
                                    ws_xpixel: 0,
                                    ws_ypixel: 0,
                                };
                                let _ = unsafe {
                                    libc::ioctl(afd.get_ref().as_raw_fd(), TIOCSWINSZ, &ws)
                                };
                                publish(&sess, &mut driver, &mut version);
                            }
                        }
                        Some(OwnerMsg::Ack(v)) => {
                            sess.ack_state(v);
                        }
                    }
                }
                _ = async {
                    match sync_at {
                        Some(t) => tokio::time::sleep_until(t).await,
                        None => std::future::pending::<()>().await,
                    }
                } => {
                    driver.flush_sync();
                    if !append_generated(&mut pending, &driver.take_pty_write()) {
                        tracing::error!("pty-generated writes exceeded pending budget");
                        break;
                    }
                    publish(&sess, &mut driver, &mut version);
                }
            }
        }
        if child_done.is_none() {
            child_done = Some(reap_child(&mut child, pid).await);
        }
        drain_pty(&afd, &mut driver, &mut buf, &mut pending);
        publish(&sess, &mut driver, &mut version);
        let _ = sess.exit.send_replace(Some(child_done.unwrap_or(1)));
        registry.lock().await.remove(&sess.id);
    }
}

fn drain_pty(
    afd: &AsyncFd<std::fs::File>,
    driver: &mut Emulator,
    buf: &mut [u8],
    pending: &mut Vec<u8>,
) {
    for _ in 0..8 {
        match afd.get_ref().read(buf) {
            Ok(0) => break,
            Ok(n) => {
                driver.process(&buf[..n]);
                if !append_generated(pending, &driver.take_pty_write()) {
                    break;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(_) => break,
        }
    }
}

fn publish(sess: &Session, driver: &mut Emulator, version: &mut u64) {
    *version += 1;
    let frame = driver.snapshot();
    let screen = Screen {
        version: *version,
        frame,
    };
    let Ok(blob) = screen.encode_compressed() else {
        return;
    };
    let latest = Arc::new(Latest {
        version: *version,
        blob,
    });
    let _ = sess.latest.send_replace(Some(latest));
}

fn kill_pgid(pid: Option<u32>, sig: i32) {
    if let Some(pid) = pid {
        unsafe {
            libc::kill(-(pid as i32), sig);
        }
    }
}

async fn reap_child(child: &mut Child, pid: Option<u32>) -> i32 {
    kill_pgid(pid, libc::SIGTERM);
    match tokio::time::timeout(Duration::from_secs(2), child.wait()).await {
        Ok(Ok(st)) => st.code().unwrap_or(1),
        _ => {
            kill_pgid(pid, libc::SIGKILL);
            match tokio::time::timeout(Duration::from_millis(400), child.wait()).await {
                Ok(Ok(st)) => st.code().unwrap_or(1),
                _ => 1,
            }
        }
    }
}

pub fn peer_uid(sock: impl AsFd) -> Result<u32> {
    let creds = getsockopt(&sock, PeerCredentials).context("SO_PEERCRED")?;
    Ok(creds.uid())
}

/// Build the session and its owner together. The caller inserts into the
/// registry then `start`s the owner so the reaper can remove that entry.
pub fn spawn_pair(
    id: [u8; 16],
    token: [u8; 32],
    uid: u32,
    cols: u16,
    rows: u16,
) -> Result<(Arc<Session>, Owner)> {
    let (cols, rows) = validate_dims(cols, rows).map_err(anyhow::Error::msg)?;
    let user = User::from_uid(Uid::from_raw(uid))
        .context("user lookup")?
        .with_context(|| format!("uid {uid} has no passwd entry"))?;
    let gid = user.gid.as_raw();
    let shell = user.shell.clone();
    let home = user.dir.clone();
    let name = user.name.clone();
    let name_c = CString::new(name.clone())?;
    let groups = getgrouplist(&name_c, Gid::from_raw(gid)).context("getgrouplist")?;
    let gids: Vec<libc::gid_t> = groups.iter().map(|g| g.as_raw()).collect();

    let pty = open_cloexec(cols, rows)?;
    let slave_raw = pty.slave.as_raw_fd();
    let stdin = Stdio::from(dup_cloexec(slave_raw)?);
    let stdout = Stdio::from(dup_cloexec(slave_raw)?);
    let stderr = Stdio::from(dup_cloexec(slave_raw)?);
    drop(pty.slave);

    let shell_name = std::path::Path::new(&shell)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("sh")
        .to_string();
    let argv0 = format!("-{shell_name}");
    let home_str = home.to_string_lossy().into_owned();

    let mut cmd = Command::new(&shell);
    cmd.arg0(&argv0)
        .stdin(stdin)
        .stdout(stdout)
        .stderr(stderr)
        .current_dir(&home)
        .env_clear()
        .env("USER", &name)
        .env("LOGNAME", &name)
        .env("HOME", &home_str)
        .env("SHELL", &shell)
        .env("TERM", "xterm-256color")
        .env(
            "PATH",
            "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
        )
        .env("LANG", "C.UTF-8")
        .kill_on_drop(true);

    let gids_c = gids;
    let euid = Uid::current().as_raw();
    unsafe {
        cmd.pre_exec(move || {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if euid == 0 {
                if libc::setgroups(gids_c.len(), gids_c.as_ptr()) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::setgid(gid) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::setuid(uid) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
            } else if uid != euid {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "not root; cannot spawn another uid",
                ));
            }
            let _ = libc::ioctl(0, TIOCSCTTY as _, 0);
            Ok(())
        });
    }

    let child = cmd.spawn().context("spawn login shell")?;
    let master = pty.master;
    let fd = master.as_raw_fd();
    let fl = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if fl >= 0 {
        unsafe { libc::fcntl(fd, libc::F_SETFL, fl | libc::O_NONBLOCK) };
    }

    let (cmds, cmd_rx) = mpsc::channel(CMD_Q);
    let (epoch_watch, _) = watch::channel(0u64);
    let (exit, _) = watch::channel(None);
    let (latest, _) = watch::channel(None);
    let (accepted, _) = watch::channel(0u64);

    let sess = Arc::new(Session {
        id,
        token,
        uid,
        last_detach: Mutex::new(Some(Instant::now())),
        hangup: Notify::new(),
        shutdown: AtomicBool::new(false),
        epoch: AtomicU64::new(0),
        epoch_watch,
        last_acked: AtomicU64::new(0),
        latest,
        size: Mutex::new((cols, rows)),
        cmds,
        exit,
        accepted,
        #[cfg(test)]
        control_leftover: std::sync::Mutex::new(Vec::new()),
    });
    let owner = Owner {
        sess: sess.clone(),
        cmd_rx,
        child,
        master,
        driver: Emulator::new(rows, cols, 2000),
        last_seq: 0,
        version: 0,
    };
    Ok((sess, owner))
}

#[cfg(test)]
pub struct StubSession {
    pub sess: Arc<Session>,
    _rx: mpsc::Receiver<OwnerMsg>,
}

#[cfg(test)]
impl StubSession {
    pub fn new(id: [u8; 16], uid: u32) -> Self {
        Self::with_detach(id, uid, Some(Instant::now()))
    }

    pub fn detached_for(id: [u8; 16], uid: u32, idle: Duration) -> Self {
        Self::with_detach(id, uid, Instant::now().checked_sub(idle))
    }

    fn with_detach(id: [u8; 16], uid: u32, last_detach: Option<Instant>) -> Self {
        let (cmds, cmd_rx) = mpsc::channel(CMD_Q);
        let (epoch_watch, _) = watch::channel(0u64);
        let (exit, _) = watch::channel(None);
        let (latest, _) = watch::channel(None);
        let (accepted, _) = watch::channel(0u64);
        let sess = Arc::new(Session {
            id,
            token: [0; 32],
            uid,
            last_detach: Mutex::new(last_detach),
            hangup: Notify::new(),
            shutdown: AtomicBool::new(false),
            epoch: AtomicU64::new(0),
            epoch_watch,
            last_acked: AtomicU64::new(0),
            latest,
            size: Mutex::new((80, 24)),
            cmds,
            exit,
            accepted,
            control_leftover: std::sync::Mutex::new(Vec::new()),
        });
        Self {
            sess,
            _rx: cmd_rx,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn send_replace_retains_without_subscriber() {
        let (tx, rx) = watch::channel(None::<u64>);
        drop(rx);
        let _ = tx.send(Some(1));
        let (tx2, rx2) = watch::channel(None::<u64>);
        drop(rx2);
        tx2.send_replace(Some(2));
        assert_eq!(*tx2.borrow(), Some(2));
        let _ = tx;
    }

    #[tokio::test]
    async fn notify_one_permit_survives_before_waiter() {
        let n = tokio::sync::Notify::new();
        n.notify_one();
        n.notified().await;
    }

    #[test]
    fn try_input_returns_busy_when_cmd_queue_full() {
        let stub = StubSession::new([1; 16], 1000);
        for i in 0..CMD_Q {
            assert!(matches!(
                stub.sess.try_input(i as u64 + 1, vec![1]),
                Ok(QueueResult::Queued)
            ));
        }
        assert!(matches!(
            stub.sess.try_input(99, vec![1]),
            Ok(QueueResult::Busy)
        ));
    }

    #[test]
    fn try_input_rejects_empty_and_oversized() {
        let stub = StubSession::new([3; 16], 1000);
        assert!(stub.sess.try_input(1, vec![]).is_err());
        assert!(stub.sess.try_input(1, vec![0; MAX_INPUT_BYTES + 1]).is_err());
    }

    #[test]
    fn generated_writes_fail_when_exceeding_hard_cap() {
        let mut pending = vec![0u8; PENDING_HARD_MAX];
        assert!(!append_generated(&mut pending, &[1]));
        let mut pending = vec![0u8; PENDING_HARD_MAX - 8];
        assert!(append_generated(&mut pending, &[1; 8]));
        assert!(!append_generated(&mut pending, &[1]));
    }

    #[test]
    fn dsr_burst_is_subject_to_pending_hard_cap() {
        let mut em = Emulator::new(24, 80, 100);
        em.process(&b"\x1b[6n".repeat(200));
        let replies = em.take_pty_write();
        assert!(!replies.is_empty());
        let mut pending = vec![0u8; PENDING_HARD_MAX - 1];
        assert!(
            !append_generated(&mut pending, &replies),
            "generated DSR replies must not grow pending past the hard cap"
        );
    }

    async fn wait_gone(registry: &Registry, id: [u8; 16]) {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if !registry.lock().await.contains_key(&id) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("session was not reaped");
    }

    #[tokio::test]
    async fn hangup_wakes_idle_owner() {
        let uid = Uid::current().as_raw();
        let id = [0x11; 16];
        let (sess, owner) = spawn_pair(id, [0x22; 32], uid, 80, 24).expect("spawn");
        let registry: Registry = Arc::new(Mutex::new(HashMap::new()));
        registry.lock().await.insert(id, sess.clone());
        sess.start(registry.clone(), owner);
        tokio::time::sleep(Duration::from_millis(80)).await;
        sess.request_hangup();
        wait_gone(&registry, id).await;
    }

    #[tokio::test]
    async fn reconnect_ignores_replayed_seq_and_accepts_next() {
        let uid = Uid::current().as_raw();
        let id = [0x12; 16];
        let (sess, owner) = spawn_pair(id, [0x33; 32], uid, 80, 24).expect("spawn");
        let registry: Registry = Arc::new(Mutex::new(HashMap::new()));
        registry.lock().await.insert(id, sess.clone());
        sess.start(registry.clone(), owner);

        let e1 = sess.claim_transport().await;
        assert!(matches!(
            sess.try_input(1, b"echo\n".to_vec()),
            Ok(QueueResult::Queued)
        ));
        let mut acc = sess.subscribe_accepted();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if sess.accepted_seq() >= 1 {
                    break;
                }
                let _ = acc.changed().await;
            }
        })
        .await
        .expect("owner did not accept seq 1");

        let e2 = sess.claim_transport().await;
        assert_ne!(e1, e2);
        assert_eq!(sess.accepted_seq(), 1);
        // Replayed seq 1 is queued but the owner must not advance past 1.
        assert!(matches!(
            sess.try_input(1, b"echo\n".to_vec()),
            Ok(QueueResult::Queued)
        ));
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert_eq!(sess.accepted_seq(), 1);
        assert!(matches!(
            sess.try_input(2, b"x".to_vec()),
            Ok(QueueResult::Queued)
        ));
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if sess.accepted_seq() >= 2 {
                    break;
                }
                let _ = acc.changed().await;
            }
        })
        .await
        .expect("owner did not accept seq 2 after replay");

        sess.request_hangup();
        wait_gone(&registry, id).await;
    }

    #[tokio::test]
    async fn owner_exit_is_visible_to_late_subscriber() {
        let uid = Uid::current().as_raw();
        let id = [0xe2; 16];
        let (sess, owner) = spawn_pair(id, [0x44; 32], uid, 80, 24).expect("spawn");
        let registry: Registry = Arc::new(Mutex::new(HashMap::new()));
        registry.lock().await.insert(id, sess.clone());
        sess.start(registry.clone(), owner);
        sess.request_hangup();
        wait_gone(&registry, id).await;
        let mut rx = sess.subscribe_exit();
        assert!(
            rx.borrow().is_some(),
            "handler that subscribes after the shell exits must still see the status"
        );
        let missed = tokio::time::timeout(Duration::from_millis(40), rx.changed()).await;
        assert!(
            missed.is_err(),
            "changed() must not be required to observe a pre-existing exit"
        );
    }

    #[tokio::test]
    async fn full_input_must_not_stop_output_drain() {
        let stub = StubSession::new([0x71; 16], Uid::current().as_raw());
        let sess = stub.sess;
        let pty = crate::pty::open_cloexec(80, 24).unwrap();
        unsafe {
            let mut t = std::mem::zeroed();
            assert_eq!(libc::tcgetattr(pty.slave.as_raw_fd(), &mut t), 0);
            libc::cfmakeraw(&mut t);
            assert_eq!(libc::tcsetattr(pty.slave.as_raw_fd(), libc::TCSANOW, &t), 0);
            let fd = pty.master.as_raw_fd();
            let flags = libc::fcntl(fd, libc::F_GETFL);
            assert_eq!(libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK), 0);
        }
        let total = 40 * MAX_INPUT_BYTES;
        let script = format!(
            "import os,time\n\
             time.sleep(0.2)\n\
             x=b'X'*65536\n\
             while x:\n\
              n=os.write(1,x);x=x[n:]\n\
             left={total}\n\
             while left:\n\
              b=os.read(0,min(left,65536));left-=len(b)\n"
        );
        let mut command = Command::new("python3");
        command
            .arg("-c")
            .arg(script)
            .stdin(Stdio::from(pty.slave.try_clone().unwrap()))
            .stdout(Stdio::from(pty.slave.try_clone().unwrap()))
            .stderr(Stdio::from(pty.slave.try_clone().unwrap()))
            .kill_on_drop(true);
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command.spawn().unwrap();
        drop(pty.slave);
        let owner = Owner {
            sess: sess.clone(),
            cmd_rx: stub._rx,
            child,
            master: pty.master,
            driver: Emulator::new(24, 80, 0),
            last_seq: 0,
            version: 0,
        };
        let registry: Registry = Arc::new(Mutex::new(Default::default()));
        registry.lock().await.insert(sess.id, sess.clone());
        let done = tokio::spawn(owner.run(registry));
        for seq in 1..=40 {
            loop {
                match sess.try_input(seq, vec![b'x'; MAX_INPUT_BYTES]).unwrap() {
                    QueueResult::Queued => break,
                    QueueResult::Busy => tokio::time::sleep(Duration::from_millis(1)).await,
                }
            }
        }
        let completed = tokio::time::timeout(Duration::from_secs(2), async {
            while !done.is_finished() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .is_ok();
        let accepted = sess.accepted_seq();
        sess.request_hangup();
        tokio::time::timeout(Duration::from_secs(5), done)
            .await
            .unwrap()
            .unwrap();
        assert!(
            completed,
            "PTY made no progress with full input buffer; accepted seq={accepted}, child blocked writing output before reading input"
        );
    }
}
