use anyhow::{Context, Result, bail};
use blit_remote::FrameState;
use clap::{Parser, Subcommand};
use quosh_predict::{DisplayPreference, Predictor};
use quosh_proto::{
    CONNECT_PREFIX, DEFAULT_PORT, FrameFeed, Hello, HelloOk, HelperRequest, HelperResponse,
    MODE_BRACKETED_PASTE, MODE_CURSOR_VISIBLE, MSG_ERROR, MSG_EXIT, MSG_HELLO_OK, MSG_INPUT_ACK,
    MSG_PONG, MSG_SCREEN, OUTAGE_BANNER_SECS, PROTOCOL_VERSION, Screen, WT_PATH, decode_error,
    decode_exit, decode_u64, encode_ack_state, encode_hangup, encode_input, encode_ping,
    encode_resize, frame_ansi, parse_connect_line, split_frame,
};
use std::collections::VecDeque;
use std::fs::File;
use std::future::Future;
use std::io::{self, IsTerminal, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::path::PathBuf;
use std::pin::Pin;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::process::Command;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::mpsc;
use url::Url;

const SOCKET: &str = "/run/quosh/quosh.sock";
const REMOTE_QUOSH: &str = "/usr/local/bin/quosh";

#[derive(Parser, Debug)]
#[command(name = "quosh", about = "Mosh-style remote shell over WebTransport")]
struct Args {
    /// SSH command used to reach the server (default: ssh).
    #[arg(long, default_value = "ssh")]
    ssh: String,
    /// Local echo prediction: adaptive (default) or never.
    #[arg(long, value_enum, default_value_t = PredictMode::Adaptive)]
    predict: PredictMode,
    /// Unix socket (create-session helper only).
    #[arg(long, default_value = SOCKET, hide = true)]
    socket: PathBuf,
    #[command(subcommand)]
    cmd: Option<Cmd>,
    /// user@host (when not using a subcommand).
    target: Option<String>,
}

#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
enum PredictMode {
    Adaptive,
    Never,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Remote helper: create a session on the local daemon (invoked over SSH).
    CreateSession {
        #[arg(long)]
        cols: u16,
        #[arg(long)]
        rows: u16,
        #[arg(long)]
        kill_idle: bool,
        #[arg(long)]
        idle_only: bool,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    match args.cmd {
        Some(Cmd::CreateSession {
            cols,
            rows,
            kill_idle,
            idle_only,
        }) => helper(&args.socket, cols, rows, kill_idle, idle_only).await,
        None => {
            let Some(target) = args.target else {
                bail!("usage: quosh [--ssh=cmd] user@host");
            };
            client(&args.ssh, &target, args.predict).await
        }
    }
}

async fn helper(
    socket: &PathBuf,
    cols: u16,
    rows: u16,
    kill_idle: bool,
    idle_only: bool,
) -> Result<()> {
    let stream = match UnixStream::connect(socket).await {
        Ok(s) => s,
        Err(_) => {
            eprintln!(
                "quosh-server is not running (cannot connect to {}).\nStart it with: systemctl enable --now quosh-server",
                socket.display()
            );
            std::process::exit(2);
        }
    };
    let req = if idle_only {
        HelperRequest {
            op: "idle".into(),
            cols: 0,
            rows: 0,
            kill_idle: false,
        }
    } else {
        HelperRequest {
            op: "create".into(),
            cols,
            rows,
            kill_idle,
        }
    };
    let mut stream = stream;
    let line = serde_json::to_string(&req)? + "\n";
    stream.write_all(line.as_bytes()).await?;
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await?;
    let resp: HelperResponse = serde_json::from_slice(buf.trim_ascii())?;
    if !resp.ok {
        bail!("{}", resp.error.unwrap_or_else(|| "create failed".into()));
    }
    if idle_only {
        println!("{}", serde_json::to_string(&resp)?);
        return Ok(());
    }
    println!(
        "{}",
        quosh_proto::format_connect_line(
            resp.port.unwrap_or(DEFAULT_PORT),
            &hex32(resp.cert_sha256.as_deref())?,
            &hex16(resp.session_id.as_deref())?,
            &hex32(resp.token.as_deref())?,
        )
    );
    Ok(())
}

fn hex16(s: Option<&str>) -> Result<[u8; 16]> {
    let v = hex::decode(s.context("missing session_id")?)?;
    v.try_into().map_err(|_| anyhow::anyhow!("session_id"))
}
fn hex32(s: Option<&str>) -> Result<[u8; 32]> {
    let v = hex::decode(s.context("missing hex")?)?;
    v.try_into().map_err(|_| anyhow::anyhow!("hex32"))
}

async fn client(ssh: &str, target: &str, predict: PredictMode) -> Result<()> {
    let (cols, rows) = tty_size();
    let mut create_args = vec![
        "create-session".to_string(),
        "--cols".into(),
        cols.to_string(),
        "--rows".into(),
        rows.to_string(),
    ];
    if io::stdin().is_terminal()
        && let Ok(raw) = ssh_helper(
            ssh,
            target,
            &[
                "create-session",
                "--idle-only",
                "--cols",
                "1",
                "--rows",
                "1",
            ],
        )
        .await
        && let Ok(resp) = serde_json::from_str::<HelperResponse>(raw.trim())
        && !resp.idle.is_empty()
    {
        let n = resp.idle.len();
        eprint!(
            "There's {} idle connection{}. Kill {}? [y/N] ",
            n,
            if n == 1 { "" } else { "s" },
            if n == 1 { "it" } else { "them" }
        );
        let _ = io::stderr().flush();
        let mut ans = String::new();
        let _ = io::stdin().read_line(&mut ans);
        if matches!(ans.trim(), "y" | "Y" | "yes") {
            create_args.push("--kill-idle".into());
        }
    }
    let args_ref: Vec<&str> = create_args.iter().map(|s| s.as_str()).collect();
    let out = ssh_helper(ssh, target, &args_ref).await?;
    let line = out
        .lines()
        .rev()
        .find(|l| l.starts_with(CONNECT_PREFIX))
        .context("no QUOSH CONNECT line from server (is quosh-server running?)")?
        .to_string();
    let (port, hash, session_id, token) = parse_connect_line(&line)?;
    let host = ssh_hostname(ssh, target).await?;
    run_session(
        &host,
        port,
        hash,
        session_id,
        token,
        cols,
        rows,
        predict == PredictMode::Never,
    )
    .await
}

/// Use OpenSSH's evaluated `HostName` so `Host agents` / Tailscale aliases
/// match the SSH hop. Falling back to the name after `@` is wrong when that
/// alias only exists in `~/.ssh/config`.
async fn ssh_hostname(ssh: &str, target: &str) -> Result<String> {
    let fallback = target.rsplit('@').next().context("host")?.to_string();
    let mut parts: Vec<String> = ssh.split_whitespace().map(str::to_string).collect();
    if parts.is_empty() {
        return Ok(fallback);
    }
    parts.push("-G".into());
    parts.push(target.to_string());
    let mut cmd = Command::new(&parts[0]);
    cmd.args(&parts[1..]);
    let Ok(out) = cmd.output().await else {
        return Ok(fallback);
    };
    if !out.status.success() {
        return Ok(fallback);
    }
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let mut words = line.split_whitespace();
        if words.next() == Some("hostname")
            && let Some(h) = words.next()
            && !h.is_empty()
        {
            return Ok(h.to_string());
        }
    }
    Ok(fallback)
}

async fn ssh_helper(ssh: &str, target: &str, rest: &[&str]) -> Result<String> {
    let mut parts: Vec<String> = Vec::new();
    // Allow --ssh="ssh -i key -o StrictHostKeyChecking=accept-new"
    for p in ssh.split_whitespace() {
        parts.push(p.to_string());
    }
    parts.push(target.to_string());
    parts.push("--".into());
    parts.push(REMOTE_QUOSH.into());
    for r in rest {
        if !r.is_empty() {
            parts.push((*r).into());
        }
    }
    let mut cmd = Command::new(&parts[0]);
    cmd.args(&parts[1..]);
    let out = cmd.output().await.context("ssh")?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let stdout = String::from_utf8_lossy(&out.stdout);
        bail!("ssh helper failed: {err}{stdout}");
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

enum CtrlEvent {
    Eof,
    Winch,
    Interrupt,
    Hangup,
    Failed(String),
}

const UNACKED_CAP: usize = 256 * 1024;

/// Dup stdin for the input thread. Do not `F_SETFL O_NONBLOCK`: on a tty that
/// flag is OFD-wide and would make stdout paints fail with EAGAIN. macOS also
/// does not deliver keystrokes to a second `open("/dev/tty")`.
fn dup_stdin() -> io::Result<File> {
    let n = unsafe { libc::fcntl(libc::STDIN_FILENO, libc::F_DUPFD_CLOEXEC, 0) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(n) })
}

/// Mosh's escape key: `Ctrl-^`, and `Ctrl-^ .` quits the client. If the
/// escape key is followed by anything else, both bytes are forwarded so the
/// key can still be typed through.
const ESCAPE_KEY: u8 = 0x1e;
const QUIT_KEY: u8 = b'.';

/// Process one read. `escape` carries a pending `Ctrl-^` across reads. Returns
/// the bytes to forward and whether the quit sequence completed.
fn filter_input(bytes: &[u8], escape: &mut bool) -> (Vec<u8>, bool) {
    let mut out = Vec::with_capacity(bytes.len());
    let mut quit = false;
    for &b in bytes {
        if !*escape {
            if b == ESCAPE_KEY {
                *escape = true;
            } else {
                out.push(b);
            }
        } else {
            *escape = false;
            if b == QUIT_KEY {
                quit = true;
                break;
            }
            out.push(ESCAPE_KEY);
            if b != ESCAPE_KEY {
                out.push(b);
            }
        }
    }
    (out, quit)
}

/// Bounded local buffer for input the main loop has not consumed yet. The tty
/// reader must never block on the ordinary input channel: during an outage the
/// main loop stops draining it, and a blocked reader can no longer see
/// `Ctrl-^ .`. Oldest input is dropped past the cap, matching the bounded queue
/// the main loop already applies.
const TTY_PENDING_CAP: usize = 64 * 1024;

struct TtyWriter {
    bytes: mpsc::Sender<Vec<u8>>,
    ctrl: mpsc::Sender<CtrlEvent>,
    pending: VecDeque<Vec<u8>>,
    pending_bytes: usize,
}

impl TtyWriter {
    fn new(bytes: mpsc::Sender<Vec<u8>>, ctrl: mpsc::Sender<CtrlEvent>) -> Self {
        Self {
            bytes,
            ctrl,
            pending: VecDeque::new(),
            pending_bytes: 0,
        }
    }

    /// Quit is control, not ordinary input: put it on the control channel and
    /// try the preceding bytes best-effort, so a full input queue cannot
    /// strand it.
    fn quit(&mut self, out: Vec<u8>) {
        let _ = self.ctrl.blocking_send(CtrlEvent::Hangup);
        if !out.is_empty() {
            let _ = self.bytes.try_send(out);
        }
    }

    /// Hand ordinary input to the main loop without ever blocking. The reader
    /// keeps running (and stays able to see a later quit) even when the main
    /// loop has stopped consuming.
    fn send(&mut self, out: Vec<u8>) {
        if !out.is_empty() {
            self.pending_bytes += out.len();
            self.pending.push_back(out);
            while self.pending_bytes > TTY_PENDING_CAP {
                match self.pending.pop_front() {
                    Some(dropped) => self.pending_bytes -= dropped.len(),
                    None => break,
                }
            }
        }
        self.flush();
    }

    fn flush(&mut self) {
        while let Some(front) = self.pending.pop_front() {
            let n = front.len();
            match self.bytes.try_send(front) {
                Ok(()) => self.pending_bytes -= n,
                Err(mpsc::error::TrySendError::Full(front)) => {
                    self.pending.push_front(front);
                    break;
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    self.pending.clear();
                    self.pending_bytes = 0;
                    break;
                }
            }
        }
    }
}

fn spawn_tty_reader(bytes: mpsc::Sender<Vec<u8>>, ctrl: mpsc::Sender<CtrlEvent>, tty: File) {
    std::thread::Builder::new()
        .name("quosh-tty".into())
        .spawn(move || {
            let fd = tty.as_raw_fd();
            let mut buf = [0u8; 4096];
            let mut escape = false;
            let err_ctrl = ctrl.clone();
            let mut writer = TtyWriter::new(bytes, ctrl);
            loop {
                let mut pfd = libc::pollfd {
                    fd,
                    events: libc::POLLIN,
                    revents: 0,
                };
                let pr = unsafe { libc::poll(&mut pfd, 1, -1) };
                if pr < 0 {
                    let e = io::Error::last_os_error();
                    if e.kind() == io::ErrorKind::Interrupted {
                        continue;
                    }
                    let _ = err_ctrl.blocking_send(CtrlEvent::Failed(format!("tty poll: {e}")));
                    break;
                }
                if pfd.revents & (libc::POLLHUP | libc::POLLERR) != 0 {
                    let _ = err_ctrl.blocking_send(CtrlEvent::Eof);
                    break;
                }
                if pfd.revents & libc::POLLIN == 0 {
                    continue;
                }
                let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
                if n < 0 {
                    let e = io::Error::last_os_error();
                    if e.kind() == io::ErrorKind::Interrupted {
                        continue;
                    }
                    let _ = err_ctrl.blocking_send(CtrlEvent::Failed(format!("tty read: {e}")));
                    break;
                }
                if n == 0 {
                    let _ = err_ctrl.blocking_send(CtrlEvent::Eof);
                    break;
                }
                let (out, quit) = filter_input(&buf[..n as usize], &mut escape);
                if quit {
                    writer.quit(out);
                    break;
                }
                writer.send(out);
            }
        })
        .expect("spawn tty thread");
}

async fn signal_task(ctrl: mpsc::Sender<CtrlEvent>) {
    let mut sigwinch = match signal(SignalKind::window_change()) {
        Ok(s) => s,
        Err(e) => {
            let _ = ctrl.send(CtrlEvent::Failed(format!("sigwinch: {e}"))).await;
            return;
        }
    };
    let mut sigint = match signal(SignalKind::interrupt()) {
        Ok(s) => s,
        Err(e) => {
            let _ = ctrl.send(CtrlEvent::Failed(format!("sigint: {e}"))).await;
            return;
        }
    };
    let mut sighup = match signal(SignalKind::hangup()) {
        Ok(s) => s,
        Err(e) => {
            let _ = ctrl.send(CtrlEvent::Failed(format!("sighup: {e}"))).await;
            return;
        }
    };
    loop {
        tokio::select! {
            _ = sigwinch.recv() => {
                if ctrl.send(CtrlEvent::Winch).await.is_err() {
                    break;
                }
            }
            _ = sigint.recv() => {
                let _ = ctrl.send(CtrlEvent::Interrupt).await;
                break;
            }
            _ = sighup.recv() => {
                let _ = ctrl.send(CtrlEvent::Hangup).await;
                break;
            }
        }
    }
}

const PASTE_BYTES: usize = 100;

/// A condition that reconnecting cannot fix: server rejection, protocol
/// mismatch, or an unknown/expired session. `run_session` stops on these.
#[derive(Debug)]
struct FatalServer(String);

impl std::fmt::Display for FatalServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for FatalServer {}

fn fatal(msg: impl Into<String>) -> anyhow::Error {
    FatalServer(msg.into()).into()
}

/// A control stream that closes before `HelloOk` is a server-side rejection,
/// not a network drop we should retry.
fn handshake_closed() -> anyhow::Error {
    handshake_failed("connection closed before HelloOk")
}

/// Explicitly fatal: retrying cannot fix a server that will not complete the
/// handshake with a valid `HelloOk`.
fn handshake_failed(reason: impl std::fmt::Display) -> anyhow::Error {
    fatal(format!(
        "handshake with quosh-server failed: {reason} \
         (is quosh-server up to date with this client?)"
    ))
}

/// Owns the confirmed screen, the predictor, and the last displayed frame.
/// Every repaint starts from confirmed state and reapplies predictions.
struct Render {
    predictor: Predictor,
    screen: Option<Screen>,
    display: Option<FrameState>,
    /// When `Some`, the outage banner is drawn over the frame on every repaint.
    outage: Option<Duration>,
    /// The banner text currently on screen, so an unchanged one is not redrawn.
    shown_banner: Option<String>,
    start: Instant,
}

impl Render {
    fn new(never: bool) -> Self {
        let mut predictor = Predictor::new();
        predictor.set_display_preference(if never {
            DisplayPreference::Never
        } else {
            DisplayPreference::Adaptive
        });
        Self {
            predictor,
            screen: None,
            display: None,
            outage: None,
            shown_banner: None,
            start: Instant::now(),
        }
    }

    fn now(&self) -> u64 {
        self.start.elapsed().as_millis() as u64
    }

    /// Mosh's `send_interval`: `clamp(ceil(SRTT / 2), 20, 250)` ms.
    fn set_rtt_ms(&mut self, srtt_ms: f64) {
        let interval = ((srtt_ms / 2.0).ceil() as u32).clamp(20, 250);
        self.predictor.set_send_interval(interval);
    }

    fn reset(&mut self) {
        self.predictor.reset();
        self.display = None;
    }

    fn tick_delay(&self) -> Option<Duration> {
        if self.predictor.active() {
            Some(Duration::from_millis(50))
        } else {
            None
        }
    }

    /// Install a screen atomically: frame, echo checkpoint, cull, repaint.
    /// Returns `(version, mode)` when the screen was newer.
    fn apply_screen(&mut self, incoming: Screen) -> io::Result<Option<(u64, u16)>> {
        let Some(applied) = Screen::apply_newer(self.screen.as_ref(), incoming) else {
            return Ok(None);
        };
        let now = self.now();
        let version = applied.version;
        let mode = applied.frame.mode();
        self.predictor.set_local_frame_late_acked(applied.echo_ack);
        self.predictor.cull(&applied.frame, now);
        self.screen = Some(applied);
        // A frame arriving means connectivity is back; drop the banner.
        self.outage = None;
        self.repaint()?;
        Ok(Some((version, mode)))
    }

    /// Feed the bytes of one input message. All bytes share `seq` (the
    /// message's frame), and bulk reads are not predicted.
    ///
    /// The predictor expires a prediction at `local_frame_sent + 1` (Mosh's
    /// convention), so message `seq` is fed as `seq - 1` to expire at `seq`,
    /// matching the server's echo checkpoint exactly.
    fn predict(&mut self, seq: u64, bytes: &[u8]) -> io::Result<()> {
        if bytes.len() > PASTE_BYTES {
            // Bulk input is not predicted. Repaint immediately so any existing
            // overlay is removed instead of lingering until the next frame.
            self.predictor.reset();
            return self.repaint();
        }
        let Some(screen) = self.screen.as_ref() else {
            return Ok(());
        };
        self.predictor.set_local_frame_sent(seq.saturating_sub(1));
        let now = self.start.elapsed().as_millis() as u64;
        for &b in bytes {
            let basis = self.display.as_ref().unwrap_or(&screen.frame);
            self.predictor.new_user_byte(b, basis, now);
        }
        self.repaint()
    }

    /// Time-based reconciliation: a prediction pending on a quiet link still
    /// has to reach the glitch threshold and be redrawn.
    fn tick(&mut self) -> io::Result<()> {
        if !self.predictor.active() {
            return Ok(());
        }
        if let Some(screen) = self.screen.as_ref() {
            let now = self.now();
            self.predictor.cull(&screen.frame, now);
        }
        self.repaint()
    }

    fn repaint(&mut self) -> io::Result<()> {
        let Some(screen) = self.screen.as_ref() else {
            return Ok(());
        };
        let mut frame = screen.frame.clone();
        self.predictor.apply(&mut frame);
        let banner = self.outage.map(banner_line);
        // Predictions are always generated, but on a fast link `apply` leaves
        // the frame unchanged. Skip the write so timer ticks are silent, unless
        // the banner text changed (including appearing or clearing).
        if self.display.as_ref() == Some(&frame) && self.shown_banner == banner {
            return Ok(());
        }
        let rows = frame.rows();
        paint_frame(&frame, banner.is_none())?;
        if let Some(line) = &banner {
            write_banner(rows, line)?;
        }
        self.display = Some(frame);
        self.shown_banner = banner;
        Ok(())
    }

    /// Show (or update) the outage banner. It stays until [`clear_outage`].
    fn set_outage(&mut self, elapsed: Duration) -> io::Result<()> {
        self.outage = Some(elapsed);
        self.repaint()
    }

    fn clear_outage(&mut self) -> io::Result<()> {
        if self.outage.take().is_some() {
            self.repaint()?;
        }
        Ok(())
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_session(
    host: &str,
    port: u16,
    hash: [u8; 32],
    session_id: [u8; 16],
    token: [u8; 32],
    mut cols: u16,
    mut rows: u16,
    never: bool,
) -> Result<()> {
    let url = wt_url(host, port)?;
    eprintln!("quosh: connecting to {url} (UDP {port})");
    let mut raw: Option<RawMode> = None;
    let mut last_ok = Instant::now();
    let mut seq: u64 = 0;
    let mut unacked: Vec<(u64, Vec<u8>)> = Vec::new();
    let mut render = Render::new(never);
    let mut hungup = false;
    let mut exit_code = 0i32;

    let (bytes_tx, mut bytes_rx) = mpsc::channel::<Vec<u8>>(8);
    let (ctrl_tx, mut ctrl_rx) = mpsc::channel::<CtrlEvent>(8);
    match dup_stdin() {
        Ok(tty) => {
            spawn_tty_reader(bytes_tx, ctrl_tx.clone(), tty);
            tokio::spawn(signal_task(ctrl_tx));
        }
        Err(e) => {
            bail!("dup stdin for input: {e}");
        }
    }

    let mut connect_fut = start_connect(url.clone(), hash, false);
    let result: Result<()> = loop {
        if hungup {
            break Ok(());
        }
        let tick = render.tick_delay();
        tokio::select! {
            c = &mut connect_fut => {
                match c {
                    Ok(conn) => {
                        last_ok = Instant::now();
                        if raw.is_none() {
                            raw = Some(RawMode::enter()?);
                        }
                        // Restore the current confirmed frame (no banner, no
                        // predictions) before resuming, e.g. after a reconnect.
                        render.clear_outage()?;
                        match session_loop(
                            conn,
                            session_id,
                            token,
                            &mut Live {
                                cols: &mut cols,
                                rows: &mut rows,
                                seq: &mut seq,
                                unacked: &mut unacked,
                                last_ok: &mut last_ok,
                                hungup: &mut hungup,
                                exit_code: &mut exit_code,
                                bytes_rx: &mut bytes_rx,
                                ctrl_rx: &mut ctrl_rx,
                            },
                            &mut render,
                        )
                        .await
                        {
                            Ok(true) => break Ok(()),
                            Ok(false) => {}
                            Err(e) if e.downcast_ref::<FatalServer>().is_some() => {
                                // Unknown session, bad token, or protocol
                                // mismatch: reconnecting cannot help.
                                break Err(e);
                            }
                            Err(e) => {
                                let io_kind = e.downcast_ref::<io::Error>().map(|ie| ie.kind());
                                if io_kind != Some(io::ErrorKind::WouldBlock) {
                                    eprintln!("\r\nquosh: {e:#}");
                                }
                            }
                        }
                        render.reset();
                        connect_fut = start_connect(url.clone(), hash, false);
                    }
                    Err(e) => {
                        if raw.is_none() {
                            eprintln!("quosh: connect failed: {e:#}");
                        } else if last_ok.elapsed() > Duration::from_secs(OUTAGE_BANNER_SECS) {
                            let _ = render.set_outage(last_ok.elapsed());
                        }
                        connect_fut = start_connect(url.clone(), hash, true);
                    }
                }
            }
            ev = ctrl_rx.recv() => {
                match ev {
                    None => break Err(anyhow::anyhow!("input task exited")),
                    Some(CtrlEvent::Failed(m)) => break Err(anyhow::anyhow!("input: {m}")),
                    Some(CtrlEvent::Interrupt) | Some(CtrlEvent::Hangup) => {
                        hungup = true;
                    }
                    Some(CtrlEvent::Eof) if raw.is_some() => {
                        hungup = true;
                    }
                    Some(CtrlEvent::Eof) => {}
                    Some(CtrlEvent::Winch) => {
                        let (c, r) = tty_size();
                        cols = c;
                        rows = r;
                    }
                }
            }
            b = bytes_rx.recv(), if unacked_bytes(&unacked) < UNACKED_CAP => {
                if let Some(b) = b {
                    seq += 1;
                    render.predict(seq, &b)?;
                    unacked.push((seq, b));
                }
            }
            _ = async {
                match tick {
                    Some(d) => tokio::time::sleep(d).await,
                    None => std::future::pending::<()>().await,
                }
            } => {
                render.tick()?;
            }
            _ = tokio::time::sleep(Duration::from_secs(1)) => {
                if last_ok.elapsed() > Duration::from_secs(OUTAGE_BANNER_SECS) {
                    let _ = render.set_outage(last_ok.elapsed());
                }
            }
        }
    };
    drop(raw);
    result?;
    if exit_code != 0 {
        std::process::exit(exit_code);
    }
    Ok(())
}

fn wt_url(host: &str, port: u16) -> Result<Url> {
    let host = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_string()
    };
    Ok(Url::parse(&format!("https://{host}:{port}{WT_PATH}"))?)
}

fn start_connect(
    url: Url,
    hash: [u8; 32],
    delay: bool,
) -> Pin<Box<dyn Future<Output = Result<web_transport_quinn::Session>> + Send>> {
    Box::pin(async move {
        if delay {
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        connect_once(&url, &hash).await
    })
}

async fn connect_once(url: &Url, hash: &[u8; 32]) -> Result<web_transport_quinn::Session> {
    let client = web_transport_quinn::ClientBuilder::new()
        .with_server_certificate_hashes(vec![hash.to_vec()])
        .context("wt client")?;
    let session = client
        .connect(url.clone())
        .await
        .context("webtransport connect")?;
    Ok(session)
}

fn unacked_bytes(unacked: &[(u64, Vec<u8>)]) -> usize {
    unacked.iter().map(|(_, d)| d.len()).sum()
}

fn pump_unacked(feed: &mut FrameFeed, unacked: &[(u64, Vec<u8>)], sent_seq: &mut u64) {
    if feed.writing() {
        return;
    }
    if let Some((seq, data)) = unacked.iter().find(|(s, _)| *s > *sent_seq)
        && feed.push_ctrl(encode_input(*seq, data))
    {
        *sent_seq = *seq;
    }
}

struct Live<'a> {
    cols: &'a mut u16,
    rows: &'a mut u16,
    seq: &'a mut u64,
    unacked: &'a mut Vec<(u64, Vec<u8>)>,
    last_ok: &'a mut Instant,
    hungup: &'a mut bool,
    exit_code: &'a mut i32,
    bytes_rx: &'a mut mpsc::Receiver<Vec<u8>>,
    ctrl_rx: &'a mut mpsc::Receiver<CtrlEvent>,
}

async fn session_loop(
    conn: web_transport_quinn::Session,
    session_id: [u8; 16],
    token: [u8; 32],
    live: &mut Live<'_>,
    render: &mut Render,
) -> Result<bool> {
    let cols = &mut *live.cols;
    let rows = &mut *live.rows;
    let seq = &mut *live.seq;
    let unacked = &mut *live.unacked;
    let last_ok = &mut *live.last_ok;
    let hungup = &mut *live.hungup;
    let exit_code = &mut *live.exit_code;
    let bytes_rx = &mut *live.bytes_rx;
    let ctrl_rx = &mut *live.ctrl_rx;
    let (mut send, mut recv) = conn.open_bi().await.context("open control")?;
    let mut feed = FrameFeed::default();
    let _ = feed.push_ctrl(
        Hello {
            protocol: PROTOCOL_VERSION,
            session_id,
            token,
            cols: *cols,
            rows: *rows,
        }
        .encode(),
    );

    let mut sent_seq: u64 = 0;
    let mut pending_ack: Option<u64> = None;
    let mut buf = Vec::new();
    let mut tmp_recv = [0u8; 4096];
    let mut ping = tokio::time::interval(Duration::from_secs(1));
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut last_mode: u16 = 0;
    let mut srtt_ms: Option<f64> = None;
    let mut ping_sent: Option<Instant> = None;
    let mut hello_ok = false;

    loop {
        // Every frame parse failure is a protocol violation, not a transient
        // network problem: retrying the same pair cannot help.
        while let Some((typ, payload)) =
            split_frame(&mut buf).map_err(|e| fatal(format!("protocol error: {e}")))?
        {
            match typ {
                MSG_HELLO_OK => {
                    let hello = HelloOk::decode(&payload)
                        .map_err(|e| handshake_failed(format!("invalid HelloOk: {e}")))?;
                    if hello.protocol != PROTOCOL_VERSION {
                        return Err(fatal(format!(
                            "server protocol {} != client {PROTOCOL_VERSION}; update the other side",
                            hello.protocol
                        )));
                    }
                    hello_ok = true;
                }
                MSG_SCREEN => {
                    if let Ok(s) = Screen::decode_compressed(&payload) {
                        *last_ok = Instant::now();
                        if let Some((version, mode)) = render.apply_screen(s)? {
                            pending_ack = Some(version);
                            apply_modes(mode, &mut last_mode);
                        }
                    }
                }
                MSG_INPUT_ACK => {
                    let ack =
                        decode_u64(&payload).map_err(|e| fatal(format!("protocol error: {e}")))?;
                    unacked.retain(|(s, _)| *s > ack);
                }
                MSG_PONG => {
                    if let Some(t) = ping_sent.take() {
                        let sample = t.elapsed().as_secs_f64() * 1000.0;
                        let srtt = srtt_ms.map(|s| s * 0.75 + sample * 0.25).unwrap_or(sample);
                        srtt_ms = Some(srtt);
                        render.set_rtt_ms(srtt);
                    }
                    *last_ok = Instant::now();
                    render.clear_outage()?;
                }
                MSG_EXIT => {
                    let st =
                        decode_exit(&payload).map_err(|e| fatal(format!("protocol error: {e}")))?;
                    *exit_code = st;
                    *hungup = true;
                    return Ok(true);
                }
                MSG_ERROR => {
                    let (c, m) = decode_error(&payload)
                        .map_err(|e| fatal(format!("protocol error: {e}")))?;
                    return Err(fatal(format!("server error {c}: {m}")));
                }
                _ => {}
            }
        }
        if let Some(v) = pending_ack
            && feed.push_ctrl(encode_ack_state(v))
        {
            pending_ack = None;
        }
        pump_unacked(&mut feed, unacked, &mut sent_seq);
        let tick = render.tick_delay();
        tokio::select! {
            n = recv.read(&mut tmp_recv) => {
                match n {
                    Ok(Some(n)) if n > 0 => {
                        *last_ok = Instant::now();
                        buf.extend_from_slice(&tmp_recv[..n]);
                    }
                    // Clean EOF (None or zero-length) before the handshake is
                    // a rejection; after it, a transport loss to retry.
                    Ok(_) => {
                        return if hello_ok { Ok(false) } else { Err(handshake_closed()) };
                    }
                    Err(e) => {
                        return if hello_ok {
                            Ok(false)
                        } else {
                            Err(handshake_failed(format!("transport error: {e}")))
                        };
                    }
                }
            }
            dg = conn.read_datagram() => {
                match dg {
                    Ok(bytes) => {
                        *last_ok = Instant::now();
                        if let Ok(s) = Screen::decode_compressed(&bytes)
                            && let Some((version, mode)) = render.apply_screen(s)?
                        {
                            pending_ack = Some(version);
                            apply_modes(mode, &mut last_mode);
                        }
                    }
                    Err(_) => return if hello_ok { Ok(false) } else { Err(handshake_closed()) },
                }
            }
            n = send.write(feed.rest()), if feed.writing() => {
                match n {
                    Ok(0) => return if hello_ok { Ok(false) } else { Err(handshake_closed()) },
                    Ok(n) => feed.advance(n),
                    Err(_) => return if hello_ok { Ok(false) } else { Err(handshake_closed()) },
                }
            }
            ev = ctrl_rx.recv() => {
                match ev {
                    None | Some(CtrlEvent::Eof) | Some(CtrlEvent::Interrupt) | Some(CtrlEvent::Hangup) => {
                        feed.push_fin(encode_hangup());
                        *hungup = true;
                        flush_bounded(&mut send, &mut feed, Duration::from_millis(400)).await;
                        return Ok(true);
                    }
                    Some(CtrlEvent::Failed(m)) => {
                        bail!("input: {m}");
                    }
                    Some(CtrlEvent::Winch) => {
                        let (c, r) = tty_size();
                        *cols = c;
                        *rows = r;
                        let _ = feed.push_ctrl(encode_resize(c, r));
                    }
                }
            }
            b = bytes_rx.recv(), if unacked_bytes(unacked) < UNACKED_CAP => {
                if let Some(b) = b {
                    *seq += 1;
                    render.predict(*seq, &b)?;
                    unacked.push((*seq, b));
                }
            }
            _ = async {
                match tick {
                    Some(d) => tokio::time::sleep(d).await,
                    None => std::future::pending::<()>().await,
                }
            } => {
                render.tick()?;
            }
            _ = ping.tick() => {
                // One probe outstanding at a time: overwriting `ping_sent`
                // while a pong is pending would time an old pong against a new
                // send and collapse a slow link's RTT to ~0.
                if ping_sent.is_none() && feed.push_ctrl(encode_ping()) {
                    ping_sent = Some(Instant::now());
                }
                if last_ok.elapsed() > Duration::from_secs(OUTAGE_BANNER_SECS) {
                    let _ = render.set_outage(last_ok.elapsed());
                }
            }
        }
    }
}

async fn flush_bounded<S>(send: &mut S, feed: &mut FrameFeed, limit: Duration)
where
    S: tokio::io::AsyncWriteExt + Unpin,
{
    let deadline = tokio::time::Instant::now() + limit;
    loop {
        feed.pump();
        if !feed.writing() {
            break;
        }
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            break;
        }
        match tokio::time::timeout(left, send.write(feed.rest())).await {
            Ok(Ok(0)) | Err(_) | Ok(Err(_)) => break,
            Ok(Ok(n)) => feed.advance(n),
        }
    }
}

fn apply_modes(mode: u16, last: &mut u16) {
    let was_paste = *last & MODE_BRACKETED_PASTE != 0;
    let now_paste = mode & MODE_BRACKETED_PASTE != 0;
    if was_paste != now_paste {
        let seq = if now_paste {
            b"\x1b[?2004h"
        } else {
            b"\x1b[?2004l"
        };
        let _ = io::stdout().write_all(seq);
    }
    *last = mode;
}

fn paint_frame(frame: &FrameState, show_cursor: bool) -> io::Result<()> {
    let mut buf = Vec::with_capacity(16 * 1024);
    let cursor_on = show_cursor && frame.mode() & MODE_CURSOR_VISIBLE != 0;
    buf.extend_from_slice(b"\x1b[?25l\x1b[H\x1b[2J");
    buf.extend_from_slice(&frame_ansi(frame));
    buf.extend_from_slice(
        format!(
            "\x1b[{};{}H",
            frame.cursor_row() + 1,
            frame.cursor_col() + 1
        )
        .as_bytes(),
    );
    if cursor_on {
        buf.extend_from_slice(b"\x1b[?25h");
    }
    let mut out = io::stdout();
    let mut off = 0;
    while off < buf.len() {
        match out.write(&buf[off..]) {
            Ok(0) => break,
            Ok(n) => off += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                // Stdout must stay blocking; if it isn't, spinning here would
                // recreate the flicker. Fail the paint instead of looping.
                return Err(e);
            }
            Err(e) => return Err(e),
        }
    }
    out.flush()
}

fn banner_line(elapsed: Duration) -> String {
    format!(
        " quosh: {} seconds without network  [Ctrl-^ . to quit] ",
        elapsed.as_secs()
    )
}

fn write_banner(rows: u16, line: &str) -> io::Result<()> {
    let mut out = io::stdout();
    write!(out, "\x1b[s\x1b[{rows};1H\x1b[7m{line}\x1b[0m\x1b[K\x1b[u")?;
    out.flush()
}

fn tty_size() -> (u16, u16) {
    let mut ws = libc::winsize {
        ws_row: 24,
        ws_col: 80,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let fd = libc::STDOUT_FILENO;
    unsafe {
        if libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) == 0 && ws.ws_col > 0 && ws.ws_row > 0 {
            return (ws.ws_col, ws.ws_row);
        }
    }
    (80, 24)
}

struct RawMode {
    term: Option<libc::termios>,
}

impl RawMode {
    fn enter() -> Result<Self> {
        if !io::stdin().is_terminal() {
            return Ok(Self { term: None });
        }
        unsafe {
            let mut saved = std::mem::zeroed();
            if libc::tcgetattr(0, &mut saved) != 0 {
                return Ok(Self { term: None });
            }
            let mut raw = saved;
            libc::cfmakeraw(&mut raw);
            raw.c_cc[libc::VMIN] = 1;
            raw.c_cc[libc::VTIME] = 0;
            if libc::tcsetattr(0, libc::TCSANOW, &raw) != 0 {
                bail!("tcsetattr");
            }
            Ok(Self { term: Some(saved) })
        }
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        if let Some(saved) = self.term.take() {
            unsafe {
                libc::tcsetattr(0, libc::TCSANOW, &saved);
            }
        }
        let _ = io::stdout().write_all(b"\x1b[?2004l\x1b[?25h\r\n");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quit_is_delivered_when_the_input_channel_is_full() {
        // Reproduces the stranding: the main loop stops draining `bytes`
        // during an outage, so the reader must not block on it.
        let (btx, mut brx) = mpsc::channel::<Vec<u8>>(1);
        let (ctx, mut crx) = mpsc::channel::<CtrlEvent>(4);
        btx.try_send(vec![b'x']).expect("fill input channel");

        let mut writer = TtyWriter::new(btx, ctx);
        // Ordinary input backs up locally instead of blocking the reader.
        writer.send(vec![b'a']);

        // Ctrl-^ . arrives: the quit event must get through regardless.
        writer.quit(vec![b'z']);
        assert!(matches!(crx.try_recv(), Ok(CtrlEvent::Hangup)));

        // Once the consumer drains, the buffered 'a' is delivered.
        assert!(matches!(brx.try_recv(), Ok(v) if v == vec![b'x']));
        writer.flush();
        assert!(matches!(brx.try_recv(), Ok(v) if v == vec![b'a']));
    }

    #[test]
    fn quit_sequence_is_consumed_and_other_keys_forwarded() {
        let mut escape = false;
        assert_eq!(filter_input(b"ab", &mut escape), (b"ab".to_vec(), false));
        assert!(!escape);

        // Ctrl-^ . quits, and the escape key is not forwarded.
        assert_eq!(filter_input(&[ESCAPE_KEY], &mut escape), (vec![], false));
        assert!(escape);
        assert_eq!(filter_input(&[QUIT_KEY], &mut escape), (vec![], true));
        assert!(!escape);

        // Ctrl-^ x forwards both bytes.
        let mut escape = false;
        assert_eq!(
            filter_input(&[ESCAPE_KEY, b'x'], &mut escape),
            (vec![ESCAPE_KEY, b'x'], false)
        );

        // Ctrl-^ Ctrl-^ sends one literal Ctrl-^.
        let mut escape = false;
        assert_eq!(
            filter_input(&[ESCAPE_KEY, ESCAPE_KEY], &mut escape),
            (vec![ESCAPE_KEY], false)
        );

        // '.' alone is ordinary input.
        let mut escape = false;
        assert_eq!(filter_input(b".", &mut escape), (b".".to_vec(), false));
    }
}
