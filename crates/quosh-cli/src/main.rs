use anyhow::{Context, Result, bail};
use blit_remote::FrameState;
use clap::{Parser, Subcommand};
use quosh_client::{Client, ClientError, Tick};
use quosh_proto::{
    CONNECT_PREFIX, DEFAULT_PORT, HelperRequest, HelperResponse, MODE_BRACKETED_PASTE,
    MODE_CURSOR_VISIBLE, WT_PATH, frame_ansi, parse_connect_line,
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
    let title = format!("quosh: {target}");
    run_session(
        &host,
        port,
        hash,
        session_id,
        token,
        cols,
        rows,
        predict == PredictMode::Never,
        &title,
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
    /// The tty reader could not buffer ordinary input without dropping bytes.
    Overflow,
    Failed(String),
}

/// Bound on consecutive transport failures while attaching. A genuine network
/// blip should retry; a server that never completes the handshake must not spin
/// forever.
const MAX_HANDSHAKE_FAILURES: u32 = 5;

/// Why a control-stream session ended.
enum LinkEnd {
    /// The server sent Exit; the shell is gone.
    Ended,
    /// The connection dropped after the handshake; reconnect.
    Lost,
    /// The handshake did not complete for a transport reason; reconnect, but
    /// counted against [`MAX_HANDSHAKE_FAILURES`].
    HandshakeFailed,
}

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
/// `Ctrl-^ .`. Past the cap we fail closed (see [`TtyWriter::send`]) rather than
/// drop bytes and keep forwarding a command stream with holes in it.
const TTY_PENDING_CAP: usize = 64 * 1024;

/// While input is buffered locally, wake up this often to retry handing it to
/// the main loop; there is no capacity notification on a synchronous `try_send`.
const TTY_FLUSH_POLL_MS: libc::c_int = 20;

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

    fn has_pending(&self) -> bool {
        !self.pending.is_empty()
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

    /// Hand ordinary input to the main loop without ever blocking. Returns
    /// `false` when the local buffer is full: the stream would have to be
    /// truncated to keep reading, so the caller fails closed instead. The
    /// reader stays responsive to a later quit either way.
    fn send(&mut self, out: Vec<u8>) -> bool {
        self.flush();
        if out.is_empty() {
            return true;
        }
        if self.pending_bytes + out.len() > TTY_PENDING_CAP {
            return false;
        }
        self.pending_bytes += out.len();
        self.pending.push_back(out);
        self.flush();
        true
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
                // If input is buffered locally, poll with a deadline so we
                // retry handing it over once the main loop starts consuming
                // again. Otherwise wait indefinitely for the next keystroke.
                let timeout = if writer.has_pending() {
                    TTY_FLUSH_POLL_MS
                } else {
                    -1
                };
                let pr = unsafe { libc::poll(&mut pfd, 1, timeout) };
                if pr < 0 {
                    let e = io::Error::last_os_error();
                    if e.kind() == io::ErrorKind::Interrupted {
                        continue;
                    }
                    let _ = err_ctrl.blocking_send(CtrlEvent::Failed(format!("tty poll: {e}")));
                    break;
                }
                // Retry any locally buffered input on every wake, including the
                // timeout above.
                writer.flush();
                if pr == 0 {
                    continue;
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
                if !writer.send(out) {
                    // Fail closed: forwarding the buffered prefix and then
                    // dropping bytes would corrupt the command stream. The main
                    // loop tears the attachment down instead; the remote
                    // session survives for a reattach.
                    let _ = err_ctrl.blocking_send(CtrlEvent::Overflow);
                    break;
                }
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

/// Fail closed rather than forward a command stream with bytes missing.
fn input_overflowed() -> anyhow::Error {
    fatal(
        "local input buffer overflowed while the connection was stalled; \
         refusing to forward a partial command stream (reconnect and retry)",
    )
}

/// so a transient network interruption during attach is retried first.
fn handshake_failed(reason: impl std::fmt::Display) -> anyhow::Error {
    fatal(format!(
        "handshake with quosh-server failed: {reason} \
         (is quosh-server up to date with this client?)"
    ))
}

/// Owns the confirmed screen, the predictor, and the last displayed frame.
/// Every repaint starts from confirmed state and reapplies predictions.
/// Paints the client's display frame and outage banner to the terminal.
///
/// All protocol and prediction state lives in [`Client`]; the painter only
/// remembers what was last drawn so redundant writes are skipped.
struct Painter {
    title: String,
    shown_frame: Option<FrameState>,
    shown_banner: Option<String>,
    shown_title: Option<String>,
}

impl Painter {
    fn new(title: String) -> Self {
        Self {
            title,
            shown_frame: None,
            shown_banner: None,
            shown_title: None,
        }
    }

    /// Forget what was drawn so the next paint writes everything, e.g. after a
    /// reconnect.
    fn force_repaint(&mut self) {
        self.shown_frame = None;
        self.shown_banner = None;
    }

    fn paint(&mut self, client: &Client) -> io::Result<()> {
        let banner = client.outage().map(banner_line);
        self.paint_title(client.outage())?;
        let Some(frame) = client.display() else {
            return Ok(());
        };
        // Predictions are always generated, but on a fast link `apply` leaves
        // the frame unchanged. Skip the write so timer ticks are silent, unless
        // the banner text changed (including appearing or clearing).
        if self.shown_frame.as_ref() == Some(frame) && self.shown_banner == banner {
            return Ok(());
        }
        let rows = frame.rows();
        paint_frame(frame, banner.is_none())?;
        if let Some(line) = &banner {
            write_banner(rows, line)?;
        }
        self.shown_frame = Some(frame.clone());
        self.shown_banner = banner;
        Ok(())
    }

    /// Set the base tab title (`quosh: user@host`). Kept in sync with the
    /// outage banner on later paints.
    fn paint_title(&mut self, outage: Option<Duration>) -> io::Result<()> {
        let desired = title_for(&self.title, outage);
        if self.shown_title.as_deref() == Some(desired.as_str()) {
            return Ok(());
        }
        emit_title(&desired)?;
        self.shown_title = Some(desired);
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
    cols: u16,
    rows: u16,
    never: bool,
    title: &str,
) -> Result<()> {
    let url = wt_url(host, port)?;
    eprintln!("quosh: connecting to {url} (UDP {port})");
    let start = Instant::now();
    let now = || start.elapsed().as_millis() as u64;
    let mut client = Client::new(session_id, token, cols, rows, never);
    let mut painter = Painter::new(title.to_string());
    // Push the shell's title so it can be restored on exit, then label the
    // tab with something shorter and more useful than the full command line.
    push_title()?;
    painter.paint_title(None)?;
    let mut raw: Option<RawMode> = None;

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
    let mut handshake_failures = 0u32;
    let result: Result<()> = loop {
        if client.is_hungup() {
            break Ok(());
        }
        let tick = client.tick_delay_ms();
        tokio::select! {
            c = &mut connect_fut => {
                match c {
                    Ok(conn) => {
                        if raw.is_none() {
                            raw = Some(RawMode::enter()?);
                        }
                        match session_loop(
                            conn,
                            &mut client,
                            &mut painter,
                            &mut bytes_rx,
                            &mut ctrl_rx,
                            start,
                        )
                        .await
                        {
                            Ok(LinkEnd::Ended) => break Ok(()),
                            Ok(LinkEnd::Lost) => {
                                handshake_failures = 0;
                            }
                            Ok(LinkEnd::HandshakeFailed) => {
                                handshake_failures += 1;
                                if handshake_failures >= MAX_HANDSHAKE_FAILURES {
                                    break Err(handshake_failed(format!(
                                        "gave up after {handshake_failures} transport \
                                         failures during attach"
                                    )));
                                }
                            }
                            Err(e) if e.downcast_ref::<FatalServer>().is_some() => {
                                // Protocol/authentication rejection, or local
                                // input overflow: reconnecting cannot help.
                                break Err(e);
                            }
                            Err(e) => {
                                let io_kind = e.downcast_ref::<io::Error>().map(|ie| ie.kind());
                                if io_kind != Some(io::ErrorKind::WouldBlock) {
                                    eprintln!("\r\nquosh: {e:#}");
                                }
                            }
                        }
                        client.reset();
                        painter.force_repaint();
                        connect_fut = start_connect(url.clone(), hash, false);
                    }
                    Err(e) => {
                        if raw.is_none() {
                            eprintln!("quosh: connect failed: {e:#}");
                        } else {
                            client.tick(now());
                            painter.paint(&client)?;
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
                        client.request_hangup();
                    }
                    Some(CtrlEvent::Overflow) => break Err(input_overflowed()),
                    Some(CtrlEvent::Eof) if raw.is_some() => {
                        client.request_hangup();
                    }
                    Some(CtrlEvent::Eof) => {}
                    Some(CtrlEvent::Winch) => {
                        let (c, r) = tty_size();
                        client.set_size(c, r);
                    }
                }
            }
            b = bytes_rx.recv(), if client.can_accept_input() => {
                if let Some(b) = b {
                    client.queue_input(b, now());
                }
            }
            _ = async {
                match tick {
                    Some(d) => tokio::time::sleep(Duration::from_millis(d)).await,
                    None => std::future::pending::<()>().await,
                }
            } => {
                client.tick(now());
                painter.paint(&client)?;
            }
            _ = tokio::time::sleep(Duration::from_secs(1)) => {
                client.tick(now());
                painter.paint(&client)?;
            }
        }
    };
    drop(raw);
    let _ = pop_title();
    result?;
    if client.exit_code() != 0 {
        std::process::exit(client.exit_code());
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

/// Classify a clean stream close/EOF before `HelloOk` as a rejection.
fn stream_end(client: &Client) -> Result<LinkEnd> {
    if client.hello_ok() {
        Ok(LinkEnd::Lost)
    } else {
        Err(handshake_closed())
    }
}

/// A transport failure is retryable even mid-attach.
fn transport_error(client: &Client) -> Result<LinkEnd> {
    if client.hello_ok() {
        Ok(LinkEnd::Lost)
    } else {
        Ok(LinkEnd::HandshakeFailed)
    }
}

/// `ClientError` is always fatal: a protocol violation or server rejection.
fn client_fatal(e: ClientError) -> anyhow::Error {
    let ClientError::Fatal(m) = e;
    fatal(m)
}

async fn session_loop(
    conn: web_transport_quinn::Session,
    client: &mut Client,
    painter: &mut Painter,
    bytes_rx: &mut mpsc::Receiver<Vec<u8>>,
    ctrl_rx: &mut mpsc::Receiver<CtrlEvent>,
    start: Instant,
) -> Result<LinkEnd> {
    let now = || start.elapsed().as_millis() as u64;
    let (mut send, mut recv) = conn.open_bi().await.context("open control")?;
    let mut tmp_recv = [0u8; 4096];
    let mut last_mode: u16 = 0;
    client.begin_connection(now());
    painter.force_repaint();
    loop {
        if client.tick(now()) == Tick::LinkDead {
            return transport_error(client);
        }
        painter.paint(client)?;
        if client.mode() != last_mode {
            apply_modes(client.mode(), &mut last_mode);
        }
        if client.is_hungup() {
            return Ok(LinkEnd::Ended);
        }
        let next = client.tick_delay_ms().unwrap_or(1000).min(1000);
        tokio::select! {
            n = recv.read(&mut tmp_recv) => {
                match n {
                    Ok(Some(n)) if n > 0 => {
                        if let Err(e) = client.recv_control(&tmp_recv[..n], now()) {
                            return Err(client_fatal(e));
                        }
                    }
                    Ok(_) => return stream_end(client),
                    Err(_) => return transport_error(client),
                }
            }
            dg = conn.read_datagram() => {
                match dg {
                    Ok(bytes) => client.recv_datagram(&bytes, now()),
                    Err(_) => return transport_error(client),
                }
            }
            n = send.write(client.outbound()), if client.writing() => {
                match n {
                    Ok(0) => return stream_end(client),
                    Ok(n) => client.advance_outbound(n),
                    Err(_) => return transport_error(client),
                }
            }
            ev = ctrl_rx.recv() => {
                match ev {
                    None
                    | Some(CtrlEvent::Eof)
                    | Some(CtrlEvent::Interrupt)
                    | Some(CtrlEvent::Hangup) => {
                        client.request_hangup();
                        flush_bounded(&mut send, client, Duration::from_millis(400)).await;
                        return Ok(LinkEnd::Ended);
                    }
                    Some(CtrlEvent::Failed(m)) => bail!("input: {m}"),
                    Some(CtrlEvent::Overflow) => return Err(input_overflowed()),
                    Some(CtrlEvent::Winch) => {
                        let (c, r) = tty_size();
                        client.set_size(c, r);
                    }
                }
            }
            b = bytes_rx.recv(), if client.can_accept_input() => {
                if let Some(b) = b {
                    client.queue_input(b, now());
                }
            }
            _ = tokio::time::sleep(Duration::from_millis(next)) => {}
        }
    }
}

async fn flush_bounded<S>(send: &mut S, client: &mut Client, limit: Duration)
where
    S: tokio::io::AsyncWriteExt + Unpin,
{
    let deadline = tokio::time::Instant::now() + limit;
    loop {
        client.pump();
        if !client.writing() {
            break;
        }
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            break;
        }
        match tokio::time::timeout(left, send.write(client.outbound())).await {
            Ok(Ok(0)) | Err(_) | Ok(Err(_)) => break,
            Ok(Ok(n)) => client.advance_outbound(n),
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

/// OSC 2 sets the terminal window/tab title; BEL terminates it. A no-op when
/// stdout is not a terminal.
fn emit_title(title: &str) -> io::Result<()> {
    if !io::stdout().is_terminal() {
        return Ok(());
    }
    let mut out = io::stdout();
    write!(out, "\x1b]2;{title}\x07")?;
    out.flush()
}

/// xterm title stack, so quosh restores whatever the shell had set. Terminals
/// without support ignore both sequences.
fn push_title() -> io::Result<()> {
    if !io::stdout().is_terminal() {
        return Ok(());
    }
    let mut out = io::stdout();
    out.write_all(b"\x1b[22;0t")?;
    out.flush()
}

fn pop_title() -> io::Result<()> {
    if !io::stdout().is_terminal() {
        return Ok(());
    }
    let mut out = io::stdout();
    out.write_all(b"\x1b[23;0t")?;
    out.flush()
}

/// What the tab should read. The `(offline)` suffix keeps the tab honest
/// during a reconnect, when the screen is otherwise frozen.
fn title_for(base: &str, outage: Option<Duration>) -> String {
    if outage.is_some() {
        format!("{base} (offline)")
    } else {
        base.to_string()
    }
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
        assert!(writer.send(vec![b'a']));

        // Ctrl-^ . arrives: the quit event must get through regardless.
        writer.quit(vec![b'z']);
        assert!(matches!(crx.try_recv(), Ok(CtrlEvent::Hangup)));

        // Once the consumer drains, the buffered 'a' is delivered.
        assert!(matches!(brx.try_recv(), Ok(v) if v == vec![b'x']));
        writer.flush();
        assert!(matches!(brx.try_recv(), Ok(v) if v == vec![b'a']));
    }

    #[test]
    fn reader_flushes_buffered_input_when_the_channel_drains() {
        // The actual reader, with a pipe standing in for the tty. The bug this
        // guards: after buffering, the reader returned to an indefinite poll
        // and did not flush until another keystroke arrived.
        let mut fds = [0i32; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        let read_end = unsafe { File::from_raw_fd(fds[0]) };
        let write_end = unsafe { File::from_raw_fd(fds[1]) };

        let (btx, mut brx) = mpsc::channel::<Vec<u8>>(1);
        let (ctx, _crx) = mpsc::channel::<CtrlEvent>(4);
        btx.try_send(vec![b'x']).expect("fill input channel");
        spawn_tty_reader(btx, ctx, read_end);

        // One keystroke while the channel is full: buffered, not delivered.
        let mut w = &write_end;
        w.write_all(b"a").expect("write to pipe");
        std::thread::sleep(Duration::from_millis(150));
        assert!(matches!(brx.try_recv(), Ok(v) if v == vec![b'x']));

        // Drain: the reader must flush 'a' on its own, with no further input.
        assert_eq!(brx.blocking_recv(), Some(vec![b'a']));
    }

    #[test]
    fn input_overflow_fails_closed_instead_of_dropping_bytes() {
        // The channel stays full, so the local buffer is the only place input
        // can go. Past the cap the writer must refuse rather than discard
        // bytes and keep forwarding a truncated stream.
        let (btx, _brx) = mpsc::channel::<Vec<u8>>(1);
        let (ctx, _crx) = mpsc::channel::<CtrlEvent>(4);
        btx.try_send(vec![b'x']).expect("fill input channel");
        let mut writer = TtyWriter::new(btx, ctx);

        let chunk = vec![b'a'; 4096];
        while writer.pending_bytes + chunk.len() <= TTY_PENDING_CAP {
            assert!(writer.send(chunk.clone()));
        }
        assert!(
            !writer.send(chunk),
            "overflow must be reported, not dropped"
        );
    }

    #[test]
    fn title_tracks_the_outage() {
        assert_eq!(title_for("quosh: a@b", None), "quosh: a@b");
        assert_eq!(
            title_for("quosh: a@b", Some(Duration::from_secs(4))),
            "quosh: a@b (offline)"
        );
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
