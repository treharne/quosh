use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use quosh_proto::{
    CONNECT_PREFIX, DEFAULT_PORT, FrameFeed, Hello, HelloOk, HelperRequest, HelperResponse,
    MODE_BRACKETED_PASTE, MODE_CURSOR_VISIBLE, MSG_ERROR, MSG_EXIT, MSG_HELLO_OK, MSG_INPUT_ACK,
    MSG_PONG, MSG_SCREEN, OUTAGE_BANNER_SECS, Screen, WT_PATH, decode_error, decode_exit,
    decode_u64, encode_ack_state, encode_hangup, encode_input, encode_ping, encode_resize,
    frame_ansi, parse_connect_line, split_frame,
};
use std::fs::File;
use std::future::Future;
use std::io::{self, IsTerminal, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::pin::Pin;
use std::time::{Duration, Instant};
use tokio::io::unix::AsyncFd;
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
    /// Unix socket (create-session helper only).
    #[arg(long, default_value = SOCKET, hide = true)]
    socket: PathBuf,
    #[command(subcommand)]
    cmd: Option<Cmd>,
    /// user@host (when not using a subcommand).
    target: Option<String>,
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
            client(&args.ssh, &target).await
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

async fn client(ssh: &str, target: &str) -> Result<()> {
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
    let host = target.rsplit('@').next().context("host")?;
    run_session(host, port, hash, session_id, token, cols, rows).await
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
}

const UNACKED_CAP: usize = 256 * 1024;

/// New open-file description so O_NONBLOCK does not leak onto stdout/stderr
/// (those often share the tty's OFD with fd 0).
fn open_tty_nonblock() -> io::Result<File> {
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open("/dev/tty")
}

async fn input_task(bytes: mpsc::Sender<Vec<u8>>, ctrl: mpsc::Sender<CtrlEvent>, tty: File) {
    let Ok(afd) = AsyncFd::new(tty) else {
        return;
    };
    let mut sigwinch = match signal(SignalKind::window_change()) {
        Ok(s) => s,
        Err(_) => return,
    };
    let mut sigint = match signal(SignalKind::interrupt()) {
        Ok(s) => s,
        Err(_) => return,
    };
    let mut sighup = match signal(SignalKind::hangup()) {
        Ok(s) => s,
        Err(_) => return,
    };
    let mut buf = [0u8; 4096];
    loop {
        tokio::select! {
            r = afd.readable() => {
                let Ok(mut g) = r else { break };
                let n = match g.try_io(|inner| {
                    let fd = inner.get_ref().as_raw_fd();
                    let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut _, buf.len()) };
                    if n < 0 {
                        Err(std::io::Error::last_os_error())
                    } else {
                        Ok(n as usize)
                    }
                }) {
                    Ok(Ok(n)) => n,
                    Ok(Err(_)) => break,
                    Err(_would_block) => continue,
                };
                if n == 0 {
                    let _ = ctrl.send(CtrlEvent::Eof).await;
                    break;
                }
                let chunk = buf[..n].to_vec();
                tokio::select! {
                    r = bytes.send(chunk) => {
                        if r.is_err() {
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
            _ = sigwinch.recv() => {
                let _ = ctrl.send(CtrlEvent::Winch).await;
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

async fn run_session(
    host: &str,
    port: u16,
    hash: [u8; 32],
    session_id: [u8; 16],
    token: [u8; 32],
    mut cols: u16,
    mut rows: u16,
) -> Result<()> {
    let url = wt_url(host, port)?;
    let raw = RawMode::enter()?;
    let mut last_ok = Instant::now();
    let mut seq: u64 = 0;
    let mut unacked: Vec<(u64, Vec<u8>)> = Vec::new();
    let mut screen: Option<Screen> = None;
    let mut hungup = false;
    let mut exit_code = 0i32;

    let (bytes_tx, mut bytes_rx) = mpsc::channel::<Vec<u8>>(8);
    let (ctrl_tx, mut ctrl_rx) = mpsc::channel::<CtrlEvent>(8);
    match open_tty_nonblock() {
        Ok(tty) => {
            tokio::spawn(input_task(bytes_tx, ctrl_tx, tty));
        }
        Err(e) => {
            drop(raw);
            bail!("open /dev/tty for input: {e}");
        }
    }

    let mut showing_outage = false;
    let mut connect_fut = start_connect(url.clone(), hash, false);
    let result: Result<()> = loop {
        if hungup {
            break Ok(());
        }
        tokio::select! {
            c = &mut connect_fut => {
                match c {
                    Ok(conn) => {
                        last_ok = Instant::now();
                        if showing_outage {
                            if let Some(s) = screen.as_ref() {
                                let _ = paint(s, true);
                            }
                            showing_outage = false;
                        }
                        match session_loop(
                            conn,
                            session_id,
                            token,
                            &mut Live {
                                cols: &mut cols,
                                rows: &mut rows,
                                seq: &mut seq,
                                unacked: &mut unacked,
                                screen: &mut screen,
                                last_ok: &mut last_ok,
                                hungup: &mut hungup,
                                exit_code: &mut exit_code,
                                bytes_rx: &mut bytes_rx,
                                ctrl_rx: &mut ctrl_rx,
                                showing_outage: &mut showing_outage,
                            },
                        )
                        .await
                        {
                            Ok(true) => break Ok(()),
                            Ok(false) => {}
                            Err(e) => {
                                let io_kind = e.downcast_ref::<io::Error>().map(|ie| ie.kind());
                                if io_kind != Some(io::ErrorKind::WouldBlock) {
                                    let msg = format!("{e:#}");
                                    if msg.contains("unknown session") {
                                        break Ok(());
                                    }
                                    eprintln!("\r\nquosh: {msg}");
                                }
                            }
                        }
                        connect_fut = start_connect(url.clone(), hash, false);
                    }
                    Err(_) => {
                        if last_ok.elapsed() > Duration::from_secs(OUTAGE_BANNER_SECS) {
                            let _ = paint_banner(last_ok.elapsed(), screen.as_ref());
                            showing_outage = true;
                        }
                        connect_fut = start_connect(url.clone(), hash, true);
                    }
                }
            }
            ev = ctrl_rx.recv() => {
                match ev {
                    None | Some(CtrlEvent::Eof) | Some(CtrlEvent::Interrupt) | Some(CtrlEvent::Hangup) => {
                        hungup = true;
                    }
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
                    unacked.push((seq, b));
                }
            }
            _ = tokio::time::sleep(Duration::from_secs(1)) => {
                if last_ok.elapsed() > Duration::from_secs(OUTAGE_BANNER_SECS) {
                    let _ = paint_banner(last_ok.elapsed(), screen.as_ref());
                    showing_outage = true;
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
    screen: &'a mut Option<Screen>,
    last_ok: &'a mut Instant,
    hungup: &'a mut bool,
    exit_code: &'a mut i32,
    bytes_rx: &'a mut mpsc::Receiver<Vec<u8>>,
    ctrl_rx: &'a mut mpsc::Receiver<CtrlEvent>,
    showing_outage: &'a mut bool,
}

async fn session_loop(
    conn: web_transport_quinn::Session,
    session_id: [u8; 16],
    token: [u8; 32],
    live: &mut Live<'_>,
) -> Result<bool> {
    let cols = &mut *live.cols;
    let rows = &mut *live.rows;
    let seq = &mut *live.seq;
    let unacked = &mut *live.unacked;
    let screen = &mut *live.screen;
    let last_ok = &mut *live.last_ok;
    let hungup = &mut *live.hungup;
    let exit_code = &mut *live.exit_code;
    let bytes_rx = &mut *live.bytes_rx;
    let ctrl_rx = &mut *live.ctrl_rx;
    let showing_outage = &mut *live.showing_outage;
    let (mut send, mut recv) = conn.open_bi().await.context("open control")?;
    let mut feed = FrameFeed::default();
    let _ = feed.push_ctrl(
        Hello {
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

    loop {
        while let Some((typ, payload)) = split_frame(&mut buf)? {
            match typ {
                MSG_HELLO_OK => {
                    let _ = HelloOk::decode(&payload)?;
                }
                MSG_SCREEN => {
                    if let Ok(s) = Screen::decode_compressed(&payload)
                        && let Some(applied) = Screen::apply_newer(screen.as_ref(), s)
                    {
                        pending_ack = Some(applied.version);
                        apply_modes(applied.frame.mode(), &mut last_mode);
                        paint(&applied, true)?;
                        *showing_outage = false;
                        *screen = Some(applied);
                    }
                }
                MSG_INPUT_ACK => {
                    let ack = decode_u64(&payload)?;
                    unacked.retain(|(s, _)| *s > ack);
                }
                MSG_PONG => {
                    *last_ok = Instant::now();
                    if *showing_outage {
                        if let Some(s) = screen.as_ref() {
                            paint(s, true)?;
                        }
                        *showing_outage = false;
                    }
                }
                MSG_EXIT => {
                    let st = decode_exit(&payload)?;
                    *exit_code = st;
                    *hungup = true;
                    return Ok(true);
                }
                MSG_ERROR => {
                    let (c, m) = decode_error(&payload)?;
                    if c == 2 {
                        *hungup = true;
                        return Ok(true);
                    }
                    bail!("server error {c}: {m}");
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
        tokio::select! {
            n = recv.read(&mut tmp_recv) => {
                let n = n?.unwrap_or(0);
                if n == 0 {
                    return Ok(false);
                }
                *last_ok = Instant::now();
                buf.extend_from_slice(&tmp_recv[..n]);
            }
            dg = conn.read_datagram() => {
                match dg {
                    Ok(bytes) => {
                        *last_ok = Instant::now();
                        if let Ok(s) = Screen::decode_compressed(&bytes)
                            && let Some(applied) = Screen::apply_newer(screen.as_ref(), s)
                        {
                            pending_ack = Some(applied.version);
                            apply_modes(applied.frame.mode(), &mut last_mode);
                            paint(&applied, true)?;
                            *showing_outage = false;
                            *screen = Some(applied);
                        }
                    }
                    Err(_) => return Ok(false),
                }
            }
            n = send.write(feed.rest()), if feed.writing() => {
                match n {
                    Ok(0) => return Ok(false),
                    Ok(n) => feed.advance(n),
                    Err(_) => return Ok(false),
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
                    unacked.push((*seq, b));
                }
            }
            _ = ping.tick() => {
                let _ = feed.push_ctrl(encode_ping());
                if last_ok.elapsed() > Duration::from_secs(OUTAGE_BANNER_SECS) {
                    let _ = paint_banner(last_ok.elapsed(), screen.as_ref());
                    *showing_outage = true;
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

fn paint(screen: &Screen, show_if_mode: bool) -> io::Result<()> {
    let mut buf = Vec::with_capacity(16 * 1024);
    let cursor_on = show_if_mode && screen.frame.mode() & MODE_CURSOR_VISIBLE != 0;
    buf.extend_from_slice(b"\x1b[?25l\x1b[H\x1b[2J");
    buf.extend_from_slice(&frame_ansi(&screen.frame));
    buf.extend_from_slice(
        format!(
            "\x1b[{};{}H",
            screen.frame.cursor_row() + 1,
            screen.frame.cursor_col() + 1
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

fn paint_banner(elapsed: Duration, screen: Option<&Screen>) -> io::Result<()> {
    if let Some(s) = screen {
        paint(s, false)?;
    }
    let mut out = io::stdout();
    let secs = elapsed.as_secs();
    write!(
        out,
        "\x1b[s\x1b[{};1H\x1b[7m quosh: {secs} seconds without network \x1b[0m\x1b[K\x1b[u",
        screen.map(|s| s.frame.rows()).unwrap_or(24)
    )?;
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
