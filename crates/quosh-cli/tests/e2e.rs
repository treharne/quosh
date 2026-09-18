//! PTY-driven CLI session tests and a bounded lossy-link soak.
//! Spawns the real `quosh` binary against an in-process `quosh-server`.

use quosh_server::helper::Daemon;
use quosh_server::pty::open_cloexec;
use quosh_server::transport::handle_incoming;
use rand::RngCore;
use std::fs::File;
use std::io::{Read, Write};
use std::net::SocketAddr;
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::net::{UdpSocket, UnixListener};
use tokio::process::Command;
use tokio::task::JoinHandle;
use wtransport::{Endpoint, Identity, ServerConfig};

fn install_crypto() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

struct UdpProxy {
    addr: SocketAddr,
    blackhole: Arc<AtomicBool>,
    delay_ms: Arc<AtomicU64>,
    drop_pct: Arc<AtomicU32>,
    task: JoinHandle<()>,
}

impl UdpProxy {
    async fn spawn(backend: SocketAddr) -> Self {
        let sock = Arc::new(UdpSocket::bind("127.0.0.1:0").await.expect("proxy bind"));
        let addr = sock.local_addr().expect("proxy addr");
        let blackhole = Arc::new(AtomicBool::new(false));
        let delay_ms = Arc::new(AtomicU64::new(0));
        let drop_pct = Arc::new(AtomicU32::new(0));
        let client = Arc::new(tokio::sync::Mutex::new(None::<SocketAddr>));
        let task = tokio::spawn({
            let sock = sock.clone();
            let blackhole = blackhole.clone();
            let delay_ms = delay_ms.clone();
            let drop_pct = drop_pct.clone();
            let client = client.clone();
            async move {
                let mut buf = vec![0u8; 64 * 1024];
                loop {
                    let Ok((n, from)) = sock.recv_from(&mut buf).await else {
                        break;
                    };
                    if blackhole.load(Ordering::SeqCst) {
                        continue;
                    }
                    let pct = drop_pct.load(Ordering::SeqCst);
                    if pct > 0 && (rand::rng().next_u32() % 100) < pct {
                        continue;
                    }
                    let pkt = buf[..n].to_vec();
                    let delay = Duration::from_millis(delay_ms.load(Ordering::SeqCst));
                    let from_backend = from.ip() == backend.ip() && from.port() == backend.port();
                    if from_backend {
                        let Some(c) = *client.lock().await else {
                            continue;
                        };
                        send_maybe_delay(&sock, pkt, c, delay).await;
                    } else {
                        *client.lock().await = Some(from);
                        send_maybe_delay(&sock, pkt, backend, delay).await;
                    }
                }
            }
        });
        Self {
            addr,
            blackhole,
            delay_ms,
            drop_pct,
            task,
        }
    }

    fn blackhole(&self) {
        self.blackhole.store(true, Ordering::SeqCst);
    }

    fn blackhole_off(&self) {
        self.blackhole.store(false, Ordering::SeqCst);
    }

    fn set_delay(&self, d: Duration) {
        self.delay_ms.store(d.as_millis() as u64, Ordering::SeqCst);
    }

    fn set_drop_pct(&self, pct: u32) {
        self.drop_pct.store(pct.min(100), Ordering::SeqCst);
    }
}

impl Drop for UdpProxy {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn send_maybe_delay(sock: &Arc<UdpSocket>, pkt: Vec<u8>, dest: SocketAddr, delay: Duration) {
    if delay.is_zero() {
        let _ = sock.send_to(&pkt, dest).await;
        return;
    }
    let sock = sock.clone();
    tokio::spawn(async move {
        tokio::time::sleep(delay).await;
        let _ = sock.send_to(&pkt, dest).await;
    });
}

struct Stack {
    sessions: quosh_server::helper::Registry,
    socket: PathBuf,
    proxy: UdpProxy,
    dir: PathBuf,
    fake_ssh: PathBuf,
    accept: JoinHandle<()>,
    helper: JoinHandle<()>,
}

impl Stack {
    async fn start() -> Self {
        install_crypto();
        let dir = std::env::temp_dir().join(format!(
            "quosh-cli-e2e-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        let tls = quosh_server::cert::load_or_generate(&dir.join("tls")).expect("tls");
        let identity = Identity::load_pemfiles(&tls.cert_pem, &tls.key_pem)
            .await
            .expect("identity");
        let config = ServerConfig::builder()
            .with_bind_address("127.0.0.1:0".parse().unwrap())
            .with_identity(identity)
            .build();
        let endpoint = Endpoint::server(config).expect("endpoint");
        let backend = endpoint.local_addr().expect("addr");
        let proxy = UdpProxy::spawn(backend).await;

        let socket = dir.join("quosh.sock");
        let _ = std::fs::remove_file(&socket);
        let listener = UnixListener::bind(&socket).expect("unix bind");
        let _ = std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o666));

        let sessions: quosh_server::helper::Registry =
            Arc::new(tokio::sync::Mutex::new(Default::default()));
        let daemon = Arc::new(Daemon {
            sessions: sessions.clone(),
            cert_sha256: tls.sha256,
            port: proxy.addr.port(),
        });
        let helper = tokio::spawn({
            let d = daemon.clone();
            async move {
                d.serve_unix(listener).await;
            }
        });
        let s2 = sessions.clone();
        let accept = tokio::spawn(async move {
            loop {
                let incoming = endpoint.accept().await;
                let sessions = s2.clone();
                tokio::spawn(async move {
                    let _ = handle_incoming(incoming, sessions).await;
                });
            }
        });

        let fake_ssh = dir.join("fake-ssh");
        std::fs::write(
            &fake_ssh,
            "#!/bin/bash\n\
             set -e\n\
             args=()\n\
             seen=0\n\
             for a in \"$@\"; do\n\
               if [ \"$a\" = \"create-session\" ]; then seen=1; fi\n\
               if [ \"$seen\" = 1 ]; then args+=(\"$a\"); fi\n\
             done\n\
             if [ \"$seen\" != 1 ]; then echo 'fake-ssh: no create-session' >&2; exit 1; fi\n\
             exec \"$QUOSH_TEST_BIN\" --socket \"$QUOSH_TEST_SOCK\" \"${args[@]}\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&fake_ssh, std::fs::Permissions::from_mode(0o755)).unwrap();

        Self {
            sessions,
            socket,
            proxy,
            dir,
            fake_ssh,
            accept,
            helper,
        }
    }
}

impl Drop for Stack {
    fn drop(&mut self) {
        self.accept.abort();
        self.helper.abort();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

struct CliPty {
    child: tokio::process::Child,
    master: File,
    slave: File,
    collected: Vec<u8>,
}

impl CliPty {
    fn spawn(stack: &Stack) -> Self {
        Self::spawn_args(stack, &[])
    }

    fn spawn_args(stack: &Stack, extra: &[&str]) -> Self {
        let bin = PathBuf::from(env!("CARGO_BIN_EXE_quosh"));
        let pty = open_cloexec(80, 24).expect("pty");
        let slave_fd = pty.slave.as_raw_fd();
        unsafe {
            let mut t = std::mem::zeroed();
            assert_eq!(libc::tcgetattr(slave_fd, &mut t), 0);
            t.c_lflag |= libc::ICANON | libc::ISIG | libc::ECHO;
            assert_eq!(libc::tcsetattr(slave_fd, libc::TCSANOW, &t), 0);
            let fd = pty.master.as_raw_fd();
            let flags = libc::fcntl(fd, libc::F_GETFL);
            libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
        let mut cmd = Command::new(&bin);
        cmd.arg(format!("--ssh={}", stack.fake_ssh.display()));
        for a in extra {
            cmd.arg(a);
        }
        cmd.arg("quoshtest@127.0.0.1")
            .env("QUOSH_TEST_BIN", &bin)
            .env("QUOSH_TEST_SOCK", &stack.socket)
            .env("TERM", "xterm-256color")
            .stdin(Stdio::from(pty.slave.try_clone().unwrap()))
            .stdout(Stdio::from(pty.slave.try_clone().unwrap()))
            .stderr(Stdio::from(pty.slave.try_clone().unwrap()))
            .kill_on_drop(true);
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                let _ = libc::ioctl(0, libc::TIOCSCTTY as _, 0);
                Ok(())
            });
        }
        let child = cmd.spawn().expect("spawn quosh");
        Self {
            child,
            master: pty.master,
            slave: pty.slave,
            collected: Vec::new(),
        }
    }

    fn write(&mut self, bytes: &[u8]) {
        let mut off = 0;
        while off < bytes.len() {
            match self.master.write(&bytes[off..]) {
                Ok(0) => break,
                Ok(n) => off += n,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5))
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => panic!("pty write: {e}"),
            }
        }
    }

    fn pump(&mut self) {
        let mut tmp = [0u8; 4096];
        loop {
            match self.master.read(&mut tmp) {
                Ok(0) => break,
                Ok(n) => self.collected.extend_from_slice(&tmp[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => panic!("pty read: {e}"),
            }
        }
    }

    fn output(&self) -> String {
        String::from_utf8_lossy(&self.collected).into_owned()
    }

    async fn wait_for(&mut self, needle: &str, limit: Duration) {
        tokio::time::timeout(limit, async {
            loop {
                self.pump();
                if self.output().contains(needle) {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(30)).await;
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!(
                "timed out waiting for {needle:?} in CLI output:\n{}",
                self.output()
            )
        });
    }

    /// Pump for at most `limit`; true if `needle` appeared.
    async fn wait_for_within(&mut self, needle: &str, limit: Duration) -> bool {
        let deadline = Instant::now() + limit;
        loop {
            self.pump();
            if self.output().contains(needle) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// Pump until `pred` accepts the collected output, or `limit` elapses.
    async fn wait_until(&mut self, limit: Duration, mut pred: impl FnMut(&str) -> bool) -> bool {
        let deadline = Instant::now() + limit;
        loop {
            self.pump();
            if pred(&self.output()) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    fn icanon(&self) -> bool {
        unsafe {
            let mut t = std::mem::zeroed();
            assert_eq!(libc::tcgetattr(self.slave.as_raw_fd(), &mut t), 0);
            t.c_lflag & libc::ICANON != 0
        }
    }
}

async fn wait_session(stack: &Stack, limit: Duration) -> [u8; 16] {
    tokio::time::timeout(limit, async {
        loop {
            {
                let g = stack.sessions.lock().await;
                if let Some(id) = g.keys().next().copied() {
                    return id;
                }
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    })
    .await
    .expect("CLI did not create a session")
}

async fn wait_attached(stack: &Stack, id: [u8; 16], limit: Duration) {
    tokio::time::timeout(limit, async {
        loop {
            {
                let g = stack.sessions.lock().await;
                if let Some(s) = g.get(&id)
                    && s.current_epoch() > 0
                {
                    return;
                }
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    })
    .await
    .expect("CLI did not attach WebTransport");
}

/// The most recent full repaint, which `paint_frame` starts with a clear+home.
fn last_repaint(output: &str) -> &str {
    output.rsplit("\x1b[?25l\x1b[H\x1b[2J").next().unwrap_or("")
}

/// Whether `needle` in the latest repaint is drawn underlined. Adaptive
/// prediction underlines speculative cells, so this distinguishes a prediction
/// from the authoritative echo that replaces it.
fn underlined_in_last_repaint(output: &str, needle: char) -> bool {
    let repaint = last_repaint(output);
    let Some(pos) = repaint.rfind(needle) else {
        return false;
    };
    let before = &repaint[..pos];
    let Some(start) = before.rfind("\x1b[") else {
        return false;
    };
    let seq = &before[start..];
    seq.contains(";4") || seq.starts_with("\x1b[4")
}

fn remote_printf(left: &str, right: &str) -> (String, String) {
    let cmd = format!("printf '%s%s\\n' {left} {right}\r");
    let marker = format!("{left}{right}");
    assert!(
        !cmd.contains(&marker),
        "marker {marker:?} must not appear in the typed command {cmd:?}"
    );
    (cmd, marker)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_pty_echo_exit_restores_tty() {
    let stack = Stack::start().await;
    let mut cli = CliPty::spawn(&stack);
    assert!(cli.icanon(), "slave should start in cooked mode");
    let id = wait_session(&stack, Duration::from_secs(8)).await;
    wait_attached(&stack, id, Duration::from_secs(8)).await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    let (cmd, marker) = remote_printf("CLI_E2E", "_OK");
    cli.write(cmd.as_bytes());
    cli.wait_for(&marker, Duration::from_secs(8)).await;
    cli.write(b"exit\r");
    let st = tokio::time::timeout(Duration::from_secs(8), cli.child.wait())
        .await
        .expect("CLI did not exit")
        .expect("wait");
    assert_eq!(st.code(), Some(0), "expected clean remote exit, got {st:?}");
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(cli.icanon(), "RawMode drop must restore ICANON on the PTY");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_pty_replacement_transport_preserves_order() {
    let stack = Stack::start().await;
    let mut cli = CliPty::spawn(&stack);
    let id = wait_session(&stack, Duration::from_secs(8)).await;
    wait_attached(&stack, id, Duration::from_secs(8)).await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    let (cmd_a, mark_a) = remote_printf("ORD", "_A");
    cli.write(cmd_a.as_bytes());
    cli.wait_for(&mark_a, Duration::from_secs(8)).await;

    let sess = stack
        .sessions
        .lock()
        .await
        .get(&id)
        .cloned()
        .expect("session");
    assert_eq!(sess.id, id);
    let epoch1 = sess.current_epoch();
    let kicked = sess.claim_transport().await;
    assert!(kicked > epoch1);
    tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            if sess.current_epoch() > kicked {
                break;
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    })
    .await
    .expect("CLI did not reconnect with a new transport epoch");
    assert_eq!(sess.id, id, "reconnect must keep the same session");

    let (cmd_b, mark_b) = remote_printf("ORD", "_B");
    cli.write(cmd_b.as_bytes());
    cli.wait_for(&mark_b, Duration::from_secs(10)).await;
    let out = cli.output();
    let ia = out.find(&mark_a).expect("ORD_A missing after reconnect");
    let ib = out.find(&mark_b).expect("ORD_B missing");
    assert!(
        ia < ib,
        "input order lost across replacement transport: {out:?}"
    );
    cli.write(b"exit\r");
    let _ = tokio::time::timeout(Duration::from_secs(8), cli.child.wait()).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_pty_hangup_while_blackholed() {
    let stack = Stack::start().await;
    let mut cli = CliPty::spawn(&stack);
    let id = wait_session(&stack, Duration::from_secs(8)).await;
    wait_attached(&stack, id, Duration::from_secs(8)).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    stack.proxy.blackhole();
    let pid = cli.child.id().expect("pid");
    unsafe {
        libc::kill(pid as i32, libc::SIGINT);
    }
    let st = tokio::time::timeout(Duration::from_secs(2), cli.child.wait())
        .await
        .expect("CLI hangup must finish within the local flush deadline while blackholed");
    assert!(st.is_ok(), "wait failed: {st:?}");
}

async fn run_lossy_soak(default_secs: u64) {
    let stack = Stack::start().await;
    stack.proxy.set_delay(Duration::from_millis(40));
    stack.proxy.set_drop_pct(12);
    let mut cli = CliPty::spawn(&stack);
    let id = wait_session(&stack, Duration::from_secs(10)).await;
    wait_attached(&stack, id, Duration::from_secs(10)).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let soak = std::env::var("QUOSH_SOAK_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default_secs);
    let deadline = Instant::now() + Duration::from_secs(soak);
    let mut n = 0u32;
    while Instant::now() < deadline {
        n += 1;
        let (cmd, mark) = remote_printf("SK", &format!("_{n}"));
        cli.write(cmd.as_bytes());
        cli.wait_for(&mark, Duration::from_secs(15)).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert!(n >= 1, "soak produced no successful remote results");
    stack.proxy.set_drop_pct(0);
    stack.proxy.set_delay(Duration::ZERO);
    cli.write(b"exit\r");
    let _ = tokio::time::timeout(Duration::from_secs(8), cli.child.wait()).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_lossy_link_soak() {
    run_lossy_soak(5).await;
}

/// Sustained lossy-link soak. Default 60s; override with QUOSH_SOAK_SECS.
/// `cargo test -p quosh --test e2e -- --ignored --nocapture`
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "opt-in soak; 5s smoke is cli_lossy_link_soak"]
async fn cli_lossy_link_soak_long() {
    run_lossy_soak(60).await;
}

/// Warm the epoch with one echoed character, then slow the link and check that
/// adaptive prediction draws the next character before its authoritative echo.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_predicts_before_the_echo_arrives() {
    let stack = Stack::start().await;
    let mut cli = CliPty::spawn(&stack);
    let id = wait_session(&stack, Duration::from_secs(8)).await;
    wait_attached(&stack, id, Duration::from_secs(8)).await;
    tokio::time::sleep(Duration::from_millis(400)).await;

    // A first character is tentative until the server confirms it. The PTY
    // echo does that, which makes the epoch confident for later keystrokes.
    cli.write(b"w");
    cli.wait_for("w", Duration::from_secs(8)).await;

    // 400 ms each way. The RTT estimate needs a ping/pong cycle to catch up.
    stack.proxy.set_delay(Duration::from_millis(400));
    tokio::time::sleep(Duration::from_millis(2500)).await;

    cli.write(b"Z");
    let predicted = cli.wait_for_within("Z", Duration::from_millis(300)).await;
    assert!(
        predicted,
        "adaptive prediction must draw 'Z' well before the ~800 ms echo; output:\n{}",
        cli.output()
    );
    assert!(
        underlined_in_last_repaint(&cli.output(), 'Z'),
        "the early 'Z' must be a speculative (underlined) prediction"
    );

    // Independently prove the authoritative echo arrived: it repaints the same
    // cell without the prediction underline.
    let confirmed = cli
        .wait_until(Duration::from_secs(6), |o| {
            last_repaint(o).contains('Z') && !underlined_in_last_repaint(o, 'Z')
        })
        .await;
    assert!(
        confirmed,
        "the authoritative echo must repaint 'Z' without the prediction underline; output:\n{}",
        cli.output()
    );
    stack.proxy.set_delay(Duration::ZERO);
}

/// Control for the test above: `--predict=never` must not draw locally.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_predict_never_does_not_draw_locally() {
    let stack = Stack::start().await;
    let mut cli = CliPty::spawn_args(&stack, &["--predict=never"]);
    let id = wait_session(&stack, Duration::from_secs(8)).await;
    wait_attached(&stack, id, Duration::from_secs(8)).await;
    tokio::time::sleep(Duration::from_millis(400)).await;

    stack.proxy.set_delay(Duration::from_millis(400));
    tokio::time::sleep(Duration::from_millis(2500)).await;

    cli.write(b"Z");
    let early = cli.wait_for_within("Z", Duration::from_millis(300)).await;
    assert!(
        !early,
        "--predict=never must wait for the remote echo; output:\n{}",
        cli.output()
    );
    cli.wait_for("Z", Duration::from_secs(6)).await;
    stack.proxy.set_delay(Duration::ZERO);
}

/// Integration check for the conservative epoch gate: after Enter the next
/// epoch is unconfirmed, so a silent application (here `stty -echo`) must not
/// have its keystrokes drawn at all. The oracle covers the behavioural cases;
/// this proves the CLI actually wires `--predict=adaptive` to the predictor.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_silent_input_is_not_predicted() {
    let stack = Stack::start().await;
    let mut cli = CliPty::spawn(&stack);
    let id = wait_session(&stack, Duration::from_secs(8)).await;
    wait_attached(&stack, id, Duration::from_secs(8)).await;
    tokio::time::sleep(Duration::from_millis(400)).await;

    // Enter starts a new, unconfirmed epoch. The remote command disables echo
    // for a few seconds, so nothing in that epoch can be confirmed.
    cli.write(b"stty -echo; sleep 3; stty echo\r");
    cli.wait_for("sleep 3", Duration::from_secs(8)).await;
    tokio::time::sleep(Duration::from_millis(800)).await;

    cli.write(b"Q");
    let drawn = cli.wait_for_within("Q", Duration::from_millis(800)).await;
    assert!(
        !drawn,
        "a silent application must never have its input drawn; output:\n{}",
        cli.output()
    );

    // Let echo come back and tear the session down.
    tokio::time::sleep(Duration::from_millis(2500)).await;
    cli.write(b"\r");
    cli.write(b"exit\r");
    let _ = tokio::time::timeout(Duration::from_secs(8), cli.child.wait()).await;
}

/// Bulk input must clear any existing prediction immediately, not leave the
/// overlay on screen until some later frame happens to repaint.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_bulk_input_clears_predictions() {
    let stack = Stack::start().await;
    let mut cli = CliPty::spawn(&stack);
    let id = wait_session(&stack, Duration::from_secs(8)).await;
    wait_attached(&stack, id, Duration::from_secs(8)).await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    cli.write(b"w");
    cli.wait_for("w", Duration::from_secs(8)).await;
    stack.proxy.set_delay(Duration::from_millis(400));
    tokio::time::sleep(Duration::from_millis(2500)).await;

    cli.write(b"Z");
    assert!(
        cli.wait_until(Duration::from_millis(300), |o| {
            underlined_in_last_repaint(o, 'Z')
        })
        .await,
        "expected a speculative 'Z' first; output:\n{}",
        cli.output()
    );

    // Well over the 100-byte threshold: the CLI resets and repaints at once.
    cli.write(&vec![b'a'; 512]);
    let cleared = cli
        .wait_until(Duration::from_millis(400), |o| {
            !last_repaint(o).contains('Z')
        })
        .await;
    assert!(
        cleared,
        "bulk input must clear the pending prediction immediately; output:\n{}",
        cli.output()
    );
    stack.proxy.set_delay(Duration::ZERO);
    let _ = tokio::time::timeout(Duration::from_secs(8), cli.child.wait()).await;
}

/// `Ctrl-^ .` must quit the client cleanly even though raw mode swallows
/// Ctrl+C as remote input.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_quit_sequence_exits() {
    let stack = Stack::start().await;
    let mut cli = CliPty::spawn(&stack);
    let id = wait_session(&stack, Duration::from_secs(8)).await;
    wait_attached(&stack, id, Duration::from_secs(8)).await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    cli.write(&[0x1e, b'.']);
    let st = tokio::time::timeout(Duration::from_secs(8), cli.child.wait())
        .await
        .expect("client did not quit on Ctrl-^ .")
        .expect("wait");
    assert_eq!(st.code(), Some(0), "clean quit expected, got {st:?}");
    assert!(cli.icanon(), "RawMode drop must restore ICANON");
}

/// The outage banner must stay on screen, not flash once per second and be
/// wiped by the next predictor tick. An active prediction keeps ticks running
/// during the blackhole.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_outage_banner_persists_across_ticks() {
    let stack = Stack::start().await;
    let mut cli = CliPty::spawn(&stack);
    let id = wait_session(&stack, Duration::from_secs(8)).await;
    wait_attached(&stack, id, Duration::from_secs(8)).await;
    tokio::time::sleep(Duration::from_millis(400)).await;

    // Warm the epoch so the next keystroke produces an active prediction.
    cli.write(b"w");
    cli.wait_for("w", Duration::from_secs(8)).await;
    stack.proxy.set_delay(Duration::from_millis(400));
    tokio::time::sleep(Duration::from_millis(2500)).await;
    cli.write(b"Z");
    cli.wait_for_within("Z", Duration::from_millis(400)).await;

    // Cut the link. The prediction stays pending, so 50 ms ticks keep running
    // between the 1 s banner updates.
    stack.proxy.blackhole();
    let shown = cli
        .wait_until(Duration::from_secs(8), |o| {
            last_repaint(o).contains("seconds without network")
        })
        .await;
    assert!(
        shown,
        "outage banner never appeared; output:\n{}",
        cli.output()
    );

    // Several ticks later the banner must still be the most recent thing drawn.
    tokio::time::sleep(Duration::from_millis(700)).await;
    cli.pump();
    assert!(
        last_repaint(&cli.output()).contains("seconds without network"),
        "banner was wiped by a predictor tick; output tail:\n{}",
        last_repaint(&cli.output())
    );

    stack.proxy.blackhole_off();
    stack.proxy.set_delay(Duration::ZERO);
    let _ = tokio::time::timeout(Duration::from_secs(8), cli.child.wait()).await;
}
/// A hard path change (new IP, VPN) must not wait for QUIC's ~30 s idle
/// timeout. Blackhole for longer than the client's liveness deadline but
/// shorter than the QUIC idle timeout: the client must drop the stale
/// connection and reattach, rather than let the old one quietly recover.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_detects_dead_path_before_quic_idle_timeout() {
    let stack = Stack::start().await;
    let mut cli = CliPty::spawn(&stack);
    let id = wait_session(&stack, Duration::from_secs(8)).await;
    wait_attached(&stack, id, Duration::from_secs(8)).await;
    tokio::time::sleep(Duration::from_millis(400)).await;

    let sess = stack
        .sessions
        .lock()
        .await
        .get(&id)
        .cloned()
        .expect("session");
    let epoch = sess.current_epoch();

    stack.proxy.blackhole();
    // Keep draining the PTY: the client writes the outage banner
    // synchronously, and a full PTY buffer would stall its event loop.
    let deadline = Instant::now() + Duration::from_secs(12);
    while Instant::now() < deadline {
        cli.pump();
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let banner = cli.output().contains("without network");
    stack.proxy.blackhole_off();
    assert!(banner, "client never showed the outage banner");

    let mut reconnected = false;
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        cli.pump();
        if sess.current_epoch() > epoch {
            reconnected = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        reconnected,
        "client kept a stale connection past its liveness deadline; out:\n{}",
        cli.output()
    );

    let (cmd, mark) = remote_printf("LIVE", "_OK");
    cli.write(cmd.as_bytes());
    cli.wait_for(&mark, Duration::from_secs(8)).await;
    cli.write(b"exit\r");
    let _ = tokio::time::timeout(Duration::from_secs(8), cli.child.wait()).await;
}
