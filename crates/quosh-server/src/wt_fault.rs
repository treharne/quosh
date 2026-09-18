//! In-process WebTransport client/server fault tests.
//! Uses the production CLI client stack (`web-transport-quinn`) against
//! `handle_incoming`, with a UDP proxy that can delay, drop, or blackhole.

use crate::helper::Registry;
use crate::session::{Session, StubSession, spawn_pair};
use crate::transport::handle_incoming;
use nix::unistd::Uid;
use quosh_proto::{
    Hello, MSG_ERROR, MSG_EXIT, MSG_HELLO_OK, MSG_PONG, PROTOCOL_VERSION, WT_PATH, encode_input,
    encode_ping, encode_resize, split_frame,
};
use rand::RngCore;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use tokio::net::UdpSocket;
use tokio::task::JoinHandle;
use web_transport_quinn::{RecvStream, SendStream, Session as WtSession};
use wtransport::{Endpoint, Identity, ServerConfig};

static NEXT: AtomicU64 = AtomicU64::new(1);

fn install_crypto() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

/// Userspace UDP relay in front of the WT server. QUIC still ACKs if we only
/// stop reading streams; dropping datagrams here is a real path blackhole.
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
                        forward(&sock, pkt, c, delay).await;
                    } else {
                        *client.lock().await = Some(from);
                        forward(&sock, pkt, backend, delay).await;
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

async fn forward(sock: &Arc<UdpSocket>, pkt: Vec<u8>, dest: SocketAddr, delay: Duration) {
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

struct Harness {
    sessions: Registry,
    hash: [u8; 32],
    proxy: UdpProxy,
    dir: PathBuf,
    accept: JoinHandle<()>,
    transports: Arc<AtomicUsize>,
}

impl Harness {
    async fn start() -> Self {
        install_crypto();
        let dir = std::env::temp_dir().join(format!(
            "quosh-wt-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("tls dir");
        let tls = crate::cert::load_or_generate(&dir).expect("tls");
        let identity = Identity::load_pemfiles(&tls.cert_pem, &tls.key_pem)
            .await
            .expect("identity");
        let config = ServerConfig::builder()
            .with_bind_address("127.0.0.1:0".parse().unwrap())
            .with_identity(identity)
            .build();
        let endpoint = Endpoint::server(config).expect("endpoint");
        let backend = endpoint.local_addr().expect("local_addr");
        let proxy = UdpProxy::spawn(backend).await;
        let sessions: Registry = Arc::new(tokio::sync::Mutex::new(Default::default()));
        let s2 = sessions.clone();
        let transports = Arc::new(AtomicUsize::new(0));
        let t2 = transports.clone();
        let accept = tokio::spawn(async move {
            loop {
                let incoming = endpoint.accept().await;
                let sessions = s2.clone();
                let transports = t2.clone();
                tokio::spawn(async move {
                    transports.fetch_add(1, Ordering::SeqCst);
                    let _ = handle_incoming(incoming, sessions).await;
                    transports.fetch_sub(1, Ordering::SeqCst);
                });
            }
        });
        Self {
            sessions,
            hash: tls.sha256,
            proxy,
            dir,
            accept,
            transports,
        }
    }

    async fn connect(&self) -> WtSession {
        let url = url::Url::parse(&format!(
            "https://127.0.0.1:{}{WT_PATH}",
            self.proxy.addr.port()
        ))
        .unwrap();
        let client = web_transport_quinn::ClientBuilder::new()
            .with_server_certificate_hashes(vec![self.hash.to_vec()])
            .expect("wt client");
        client.connect(url).await.expect("wt connect")
    }

    async fn insert_live(&self) -> Arc<Session> {
        let uid = Uid::current().as_raw();
        let mut id = [0u8; 16];
        let mut token = [0u8; 32];
        rand::rng().fill_bytes(&mut id);
        rand::rng().fill_bytes(&mut token);
        let (sess, owner) = spawn_pair(id, token, uid, 80, 24).expect("spawn");
        self.sessions.lock().await.insert(sess.id, sess.clone());
        sess.start(self.sessions.clone(), owner);
        sess
    }

    async fn insert_stub(&self) -> Arc<Session> {
        let mut id = [0u8; 16];
        rand::rng().fill_bytes(&mut id);
        let stub = StubSession::new(id, Uid::current().as_raw());
        self.sessions
            .lock()
            .await
            .insert(stub.sess.id, stub.sess.clone());
        stub.sess.clone()
    }

    async fn insert_exited(&self, status: i32) -> Arc<Session> {
        let sess = self.insert_stub().await;
        sess.publish_exit(status);
        sess
    }

    async fn contains(&self, id: [u8; 16]) -> bool {
        self.sessions.lock().await.contains_key(&id)
    }

    fn live_transports(&self) -> usize {
        self.transports.load(Ordering::SeqCst)
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.accept.abort();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

struct Ctrl {
    send: SendStream,
    recv: RecvStream,
    buf: Vec<u8>,
    _wt: WtSession,
}

impl Ctrl {
    async fn attach(h: &Harness, sess: &Session, cols: u16, rows: u16) -> Self {
        let wt = h.connect().await;
        let (mut send, recv) = wt.open_bi().await.expect("open_bi");
        send.write_all(
            &Hello {
                protocol: PROTOCOL_VERSION,
                session_id: sess.id,
                token: sess.token,
                cols,
                rows,
            }
            .encode(),
        )
        .await
        .expect("hello");
        Self {
            send,
            recv,
            buf: Vec::new(),
            _wt: wt,
        }
    }

    async fn next_frame(&mut self) -> (u8, Vec<u8>) {
        let mut tmp = [0u8; 4096];
        loop {
            if let Some(f) = split_frame(&mut self.buf).expect("split") {
                return f;
            }
            let n = tokio::time::timeout(Duration::from_secs(2), self.recv.read(&mut tmp))
                .await
                .expect("read timeout")
                .expect("read")
                .unwrap_or(0);
            assert!(n > 0, "eof on control stream");
            self.buf.extend_from_slice(&tmp[..n]);
        }
    }

    async fn expect(&mut self, want: u8) -> Vec<u8> {
        loop {
            let (typ, payload) = self.next_frame().await;
            if typ == want {
                return payload;
            }
            if typ == MSG_ERROR {
                panic!("server error while waiting for {want}");
            }
        }
    }

    async fn write(&mut self, bytes: &[u8]) {
        self.send.write_all(bytes).await.expect("write");
    }
}

async fn wait_until<F, Fut>(limit: Duration, mut pred: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    tokio::time::timeout(limit, async {
        loop {
            if pred().await {
                return;
            }
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
    })
    .await
    .expect("condition not met in time");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wt_disconnect_during_partial_frame_then_reconnect() {
    let h = Harness::start().await;
    let sess = h.insert_live().await;
    let id = sess.id;
    {
        let mut c = Ctrl::attach(&h, &sess, 80, 24).await;
        let _ = c.expect(MSG_HELLO_OK).await;
        c.write(&encode_ping()).await;
        let _ = c.expect(MSG_PONG).await;
        wait_until(Duration::from_secs(2), || {
            let sess = sess.clone();
            async move { sess.test_control_leftover().is_empty() }
        })
        .await;
        let frame = encode_input(1, b"hello\n");
        let prefix = frame[..2].to_vec();
        c.write(&prefix).await;
        wait_until(Duration::from_secs(2), || {
            let sess = sess.clone();
            let prefix = prefix.clone();
            async move { sess.test_control_leftover() == prefix }
        })
        .await;
        drop(c);
    }
    assert!(
        h.contains(id).await,
        "session must survive a partial frame drop"
    );

    let mut c = Ctrl::attach(&h, &sess, 80, 24).await;
    let _ = c.expect(MSG_HELLO_OK).await;
    c.write(&encode_input(1, b"x")).await;
    wait_until(Duration::from_secs(2), || {
        let sess = sess.clone();
        async move { sess.accepted_seq() >= 1 }
    })
    .await;
    sess.request_hangup();
    wait_until(Duration::from_secs(5), || async { !h.contains(id).await }).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wt_reconnect_while_output_and_resize_active() {
    let h = Harness::start().await;
    let sess = h.insert_live().await;
    let id = sess.id;
    let epoch0 = sess.current_epoch();
    let epoch1;
    {
        let mut c = Ctrl::attach(&h, &sess, 80, 24).await;
        let _ = c.expect(MSG_HELLO_OK).await;
        epoch1 = sess.current_epoch();
        assert!(epoch1 > epoch0, "first attach must claim a transport");
        assert_eq!(sess.id, id);
        c.write(&encode_resize(100, 30)).await;
        c.write(&encode_input(1, b"echo QUOSH_WT\n")).await;
        wait_until(Duration::from_secs(2), || {
            let sess = sess.clone();
            async move { sess.size().await == (100, 30) && sess.accepted_seq() >= 1 }
        })
        .await;
        drop(c);
    }

    let mut c = Ctrl::attach(&h, &sess, 120, 40).await;
    let _ = c.expect(MSG_HELLO_OK).await;
    let epoch2 = sess.current_epoch();
    assert!(
        epoch2 > epoch1,
        "reconnect must replace the transport epoch, not reuse it; {epoch1} -> {epoch2}"
    );
    assert_eq!(sess.id, id);
    wait_until(Duration::from_secs(2), || {
        let sess = sess.clone();
        async move { sess.size().await == (120, 40) }
    })
    .await;
    // Replay seq 1 (already accepted) then seq 2. Ordering must stay 1 then 2.
    c.write(&encode_input(1, b"echo QUOSH_WT\n")).await;
    c.write(&encode_input(2, b"x")).await;
    wait_until(Duration::from_secs(2), || {
        let sess = sess.clone();
        async move { sess.accepted_seq() >= 2 }
    })
    .await;
    assert_eq!(
        sess.accepted_seq(),
        2,
        "replayed seq 1 must not be accepted twice"
    );
    sess.request_hangup();
    wait_until(Duration::from_secs(5), || async { !h.contains(id).await }).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wt_shell_exit_during_attachment() {
    let h = Harness::start().await;
    let sess = h.insert_exited(7).await;
    let mut c = Ctrl::attach(&h, &sess, 80, 24).await;
    let payload = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let (typ, payload) = c.next_frame().await;
            match typ {
                MSG_EXIT => return payload,
                MSG_ERROR => panic!("server error instead of exit"),
                MSG_HELLO_OK => {}
                _ => {}
            }
        }
    })
    .await
    .expect("timed out waiting for Exit; transport likely stuck on changed()");
    let st = i32::from_le_bytes(payload.try_into().expect("exit payload"));
    assert_eq!(st, 7);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wt_blackholed_connection_during_hangup() {
    let h = Harness::start().await;
    let sess = h.insert_stub().await;
    let mut c = Ctrl::attach(&h, &sess, 80, 24).await;
    let _ = c.expect(MSG_HELLO_OK).await;
    wait_until(Duration::from_secs(2), || async {
        h.live_transports() == 1
    })
    .await;

    // Stop ACKs, then queue more than a typical QUIC stream window so
    // send.write actually blocks. A tiny Exit frame would otherwise sit in
    // the send buffer and flush would return immediately.
    h.proxy.blackhole();
    sess.publish_screen_blob(vec![7u8; 8 * 1024 * 1024]);
    tokio::time::sleep(Duration::from_millis(150)).await;
    sess.publish_exit(3);
    let start = Instant::now();
    wait_until(Duration::from_millis(800), || async {
        h.live_transports() == 0
    })
    .await;
    let elapsed = start.elapsed();
    assert!(
        elapsed >= Duration::from_millis(250),
        "teardown returned in {elapsed:?}; writes did not block (deadline unused)"
    );
    assert!(
        elapsed <= Duration::from_millis(800),
        "transport teardown exceeded flush deadline: {elapsed:?}"
    );
    drop(c);
}

async fn run_wt_lossy_soak(default_secs: u64) {
    let h = Harness::start().await;
    h.proxy.set_delay(Duration::from_millis(40));
    h.proxy.set_drop_pct(15);
    let sess = h.insert_live().await;
    let id = sess.id;
    let mut c = Ctrl::attach(&h, &sess, 80, 24).await;
    let _ = c.expect(MSG_HELLO_OK).await;
    let soak = std::env::var("QUOSH_SOAK_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default_secs);
    let deadline = Instant::now() + Duration::from_secs(soak);
    let mut seq = 1u64;
    while Instant::now() < deadline {
        c.write(&encode_input(seq, b"x")).await;
        seq += 1;
        tokio::time::sleep(Duration::from_millis(80)).await;
    }
    wait_until(Duration::from_secs(10), || {
        let sess = sess.clone();
        async move { sess.accepted_seq() >= 1 }
    })
    .await;
    h.proxy.set_drop_pct(0);
    h.proxy.set_delay(Duration::ZERO);
    c.write(&encode_input(seq, b"y")).await;
    wait_until(Duration::from_secs(10), || {
        let want = seq;
        let sess = sess.clone();
        async move { sess.accepted_seq() >= want }
    })
    .await;
    sess.request_hangup();
    wait_until(Duration::from_secs(5), || async { !h.contains(id).await }).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wt_lossy_link_still_accepts_input() {
    run_wt_lossy_soak(5).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "opt-in soak; 5s smoke is wt_lossy_link_still_accepts_input"]
async fn wt_lossy_link_soak_long() {
    run_wt_lossy_soak(60).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wt_rejects_wrong_protocol_version() {
    let h = Harness::start().await;
    let sess = h.insert_stub().await;
    let wt = h.connect().await;
    let (mut send, mut recv) = wt.open_bi().await.expect("open_bi");
    send.write_all(
        &Hello {
            protocol: PROTOCOL_VERSION + 1,
            session_id: sess.id,
            token: sess.token,
            cols: 80,
            rows: 24,
        }
        .encode(),
    )
    .await
    .expect("hello");

    let mut buf = Vec::new();
    let mut tmp = [0u8; 1024];
    loop {
        if let Some((typ, payload)) = split_frame(&mut buf).expect("split") {
            assert_eq!(typ, MSG_ERROR, "expected a protocol error frame");
            let (code, msg) = quosh_proto::decode_error(&payload).expect("error");
            assert_eq!(code, 4);
            assert!(msg.contains("protocol"), "message was {msg:?}");
            return;
        }
        let n = tokio::time::timeout(Duration::from_secs(2), recv.read(&mut tmp))
            .await
            .expect("timeout waiting for error")
            .expect("read")
            .unwrap_or(0);
        assert!(n > 0, "eof before error frame");
        buf.extend_from_slice(&tmp[..n]);
    }
}
