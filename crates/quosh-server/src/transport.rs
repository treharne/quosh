use anyhow::{Context, Result, bail};
use quosh_proto::{
    FrameFeed, Hello, HelloOk, MAX_FRAME, MAX_INPUT_BYTES, MSG_ACK_STATE, MSG_HANGUP, MSG_HELLO,
    MSG_INPUT, MSG_PING, MSG_RESIZE, WT_PATH, decode_input, decode_resize, decode_u64,
    encode_error, encode_exit, encode_frame, encode_input_ack, encode_pong, peek_len, split_frame,
};
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tracing::{info, warn};
use wtransport::endpoint::IncomingSession;

use crate::helper::Registry;
use crate::session::{Latest, QueueResult, Session};

pub(crate) const BUSY_CAP: usize = 256 * 1024;
/// Counted against the busy-input budget in addition to payload, so empty
/// or tiny records cannot accumulate without bound.
const INPUT_RECORD_OVERHEAD: usize = 32;

pub async fn handle_incoming(incoming: IncomingSession, sessions: Registry) -> Result<()> {
    let req = incoming.await.context("incoming WT")?;
    let path = req.path().to_string();
    if path != WT_PATH && path != "/" {
        req.not_found().await;
        bail!("rejected path {path}");
    }
    let conn = req.accept().await.context("accept WT")?;
    let (mut send, mut recv) = conn.accept_bi().await.context("control stream")?;

    let mut buf = Vec::new();
    let hello = loop {
        let mut tmp = [0u8; 2048];
        let n = recv.read(&mut tmp).await?.context("eof before hello")?;
        buf.extend_from_slice(&tmp[..n]);
        if let Some((typ, payload)) = split_frame(&mut buf)? {
            if typ != MSG_HELLO {
                send.write_all(&encode_error(1, "expected hello")).await?;
                bail!("expected hello got {typ}");
            }
            break Hello::decode(&payload)?;
        }
    };

    let sess = {
        let g = sessions.lock().await;
        g.get(&hello.session_id).cloned()
    };
    let Some(sess) = sess else {
        send.write_all(&encode_error(2, "unknown session")).await?;
        bail!("unknown session");
    };
    if sess.token != hello.token {
        send.write_all(&encode_error(3, "bad token")).await?;
        bail!("bad token");
    }

    let pending_hello_size = if hello.cols > 0 && hello.rows > 0 {
        Some((hello.cols, hello.rows))
    } else {
        None
    };

    let epoch = sess.claim_transport().await;
    let mut epoch_rx = sess.subscribe_epoch();
    let mut latest_rx = sess.subscribe_latest();
    let mut exit_rx = sess.subscribe_exit();
    let mut accepted_rx = sess.subscribe_accepted();
    let result = run_transport(
        &sess,
        epoch,
        &mut TransportIo {
            epoch_rx: &mut epoch_rx,
            latest_rx: &mut latest_rx,
            exit_rx: &mut exit_rx,
            accepted_rx: &mut accepted_rx,
            conn: &conn,
            send: &mut send,
            recv: &mut recv,
        },
        buf,
        pending_hello_size,
    )
    .await;
    sess.detach_if_epoch(epoch).await;
    result
}

struct TransportIo<'a> {
    epoch_rx: &'a mut tokio::sync::watch::Receiver<u64>,
    latest_rx: &'a mut tokio::sync::watch::Receiver<Option<Arc<Latest>>>,
    exit_rx: &'a mut tokio::sync::watch::Receiver<Option<i32>>,
    accepted_rx: &'a mut tokio::sync::watch::Receiver<u64>,
    conn: &'a wtransport::Connection,
    send: &'a mut wtransport::SendStream,
    recv: &'a mut wtransport::RecvStream,
}

async fn run_transport(
    sess: &Arc<Session>,
    epoch: u64,
    t: &mut TransportIo<'_>,
    mut leftover: Vec<u8>,
    mut pending_resize: Option<(u16, u16)>,
) -> Result<()> {
    let epoch_rx = &mut *t.epoch_rx;
    let latest_rx = &mut *t.latest_rx;
    let exit_rx = &mut *t.exit_rx;
    let accepted_rx = &mut *t.accepted_rx;
    let conn = t.conn;
    let send = &mut *t.send;
    let recv = &mut *t.recv;
    let (cols, rows) = sess.size().await;
    let version = sess.version();
    let mut feed = FrameFeed::default();
    let _ = feed.push_ctrl(
        HelloOk {
            session_id: sess.id,
            version,
            cols,
            rows,
        }
        .encode(),
    );
    if let Some(first) = sess.latest() {
        push_screen(&mut feed, conn, &first.blob, true);
    }
    let mut pending_input_ack = if sess.accepted_seq() > 0 {
        Some(sess.accepted_seq())
    } else {
        None
    };

    let mut resync = tokio::time::interval(Duration::from_millis(300));
    resync.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut busy_input: VecDeque<(u64, Vec<u8>)> = VecDeque::new();
    loop {
        drain_leftover(
            sess,
            epoch,
            &mut leftover,
            &mut feed,
            &mut busy_input,
            BUSY_CAP,
            &mut pending_resize,
        )?;
        #[cfg(test)]
        sess.note_control_leftover(&leftover);
        drain_busy(sess, &mut busy_input);
        if let Some((c, r)) = pending_resize.take()
            && let Ok(QueueResult::Busy) = sess.try_resize(c, r)
        {
            pending_resize = Some((c, r));
        }
        if let Some(n) = pending_input_ack
            && feed.push_ctrl(encode_input_ack(n))
        {
            pending_input_ack = None;
        }
        // A new watch receiver treats the current value as already seen, so
        // exit_rx.changed() will not fire for an exit that happened between
        // lookup and subscribe. Inspect the current value every iteration.
        if let Some(st) = exit_status(exit_rx) {
            queue_exit(sess, epoch, st, &mut feed);
            flush_bounded(send, &mut feed, Duration::from_millis(400)).await;
            break;
        }
        let busy_full = busy_bytes(&busy_input) >= BUSY_CAP;
        tokio::select! {
            changed = epoch_rx.changed() => {
                if changed.is_err() || *epoch_rx.borrow() != epoch {
                    info!("session {} replaced transport", hex::encode(sess.id));
                    break;
                }
            }
            changed = exit_rx.changed() => {
                if changed.is_err() {
                    break;
                }
            }
            changed = latest_rx.changed() => {
                if changed.is_err() {
                    break;
                }
                let snap = latest_rx.borrow().clone();
                if let Some(s) = snap {
                    push_screen(&mut feed, conn, &s.blob, false);
                }
            }
            changed = accepted_rx.changed() => {
                if changed.is_ok() {
                    let n = *accepted_rx.borrow();
                    if n > 0 {
                        pending_input_ack = Some(n);
                    }
                }
            }
            _ = resync.tick() => {
                if let Some(s) = sess.latest()
                    && s.version > sess.last_acked()
                {
                    push_screen(&mut feed, conn, &s.blob, true);
                }
            }
            n = send.write(feed.rest()), if feed.writing() => {
                match n {
                    Ok(0) => break,
                    Ok(n) => feed.advance(n),
                    Err(_) => break,
                }
            }
            n = read_more(recv, &mut leftover), if !busy_full => {
                match n {
                    Ok(0) => break,
                    Ok(_) => {
                        #[cfg(test)]
                        sess.note_control_leftover(&leftover);
                    }
                    Err(e) => {
                        warn!("control read: {e}");
                        break;
                    }
                }
            }
        }
        if sess.current_epoch() != epoch {
            break;
        }
    }
    Ok(())
}

async fn read_more(recv: &mut wtransport::RecvStream, leftover: &mut Vec<u8>) -> Result<usize> {
    if leftover.len() > 4 + MAX_FRAME {
        bail!("control frame too large");
    }
    let mut tmp = [0u8; 4096];
    let n = recv
        .read(&mut tmp)
        .await
        .map_err(|e| anyhow::anyhow!("stream read: {e}"))?
        .unwrap_or(0);
    leftover.extend_from_slice(&tmp[..n]);
    Ok(n)
}

fn push_screen(feed: &mut FrameFeed, conn: &wtransport::Connection, blob: &[u8], reliable: bool) {
    let max = conn.max_datagram_size().unwrap_or(0);
    if !reliable && blob.len() + 1 < max {
        let _ = conn.send_datagram(blob);
        return;
    }
    feed.push_screen(encode_frame(quosh_proto::MSG_SCREEN, blob));
}

fn exit_status(rx: &tokio::sync::watch::Receiver<Option<i32>>) -> Option<i32> {
    *rx.borrow()
}

fn queue_exit(sess: &Session, epoch: u64, st: i32, feed: &mut FrameFeed) {
    if sess.current_epoch() != epoch {
        return;
    }
    if let Some(s) = sess.latest() {
        feed.push_screen(encode_frame(quosh_proto::MSG_SCREEN, &s.blob));
    }
    feed.push_fin(encode_exit(st));
}

fn peek_type(buf: &[u8]) -> Result<Option<u8>> {
    let Some(n) = peek_len(buf)? else {
        return Ok(None);
    };
    if buf.len() < 4 + n {
        return Ok(None);
    }
    Ok(Some(buf[4]))
}

fn input_record_cost(data: &[u8]) -> usize {
    INPUT_RECORD_OVERHEAD.saturating_add(data.len())
}

fn busy_bytes(q: &VecDeque<(u64, Vec<u8>)>) -> usize {
    q.iter().map(|(_, d)| input_record_cost(d)).sum()
}

fn drain_leftover(
    sess: &Arc<Session>,
    epoch: u64,
    leftover: &mut Vec<u8>,
    feed: &mut FrameFeed,
    busy_input: &mut VecDeque<(u64, Vec<u8>)>,
    busy_cap: usize,
    pending_resize: &mut Option<(u16, u16)>,
) -> Result<()> {
    loop {
        if sess.current_epoch() != epoch {
            return Ok(());
        }
        match peek_type(leftover)? {
            None => return Ok(()),
            Some(MSG_INPUT) if busy_bytes(busy_input) >= busy_cap => return Ok(()),
            Some(_) => {}
        }
        let Some((typ, payload)) = split_frame(leftover)? else {
            return Ok(());
        };
        on_control(sess, epoch, typ, &payload, feed, busy_input, pending_resize)?;
    }
}

fn drain_busy(sess: &Session, busy_input: &mut VecDeque<(u64, Vec<u8>)>) {
    while let Some((seq, data)) = busy_input.pop_front() {
        match sess.try_input(seq, data.clone()) {
            Ok(QueueResult::Queued) => {}
            Ok(QueueResult::Busy) => {
                busy_input.push_front((seq, data));
                break;
            }
            Err(_) => break,
        }
    }
}

async fn flush_bounded<S>(send: &mut S, feed: &mut FrameFeed, limit: Duration)
where
    S: tokio::io::AsyncWrite + Unpin,
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

fn on_control(
    sess: &Arc<Session>,
    epoch: u64,
    typ: u8,
    payload: &[u8],
    feed: &mut FrameFeed,
    busy_input: &mut VecDeque<(u64, Vec<u8>)>,
    pending_resize: &mut Option<(u16, u16)>,
) -> Result<()> {
    if sess.current_epoch() != epoch {
        return Ok(());
    }
    match typ {
        MSG_INPUT => {
            let Ok((seq, data)) = decode_input(payload) else {
                return Ok(());
            };
            if data.is_empty() {
                bail!("empty input");
            }
            if data.len() > MAX_INPUT_BYTES {
                bail!("input too large");
            }
            busy_input.push_back((seq, data));
        }
        MSG_RESIZE => {
            if let Ok((cols, rows)) = decode_resize(payload) {
                *pending_resize = Some((cols, rows));
            }
        }
        MSG_HANGUP => {
            if sess.current_epoch() == epoch {
                sess.request_hangup();
            }
        }
        MSG_ACK_STATE => {
            if let Ok(v) = decode_u64(payload) {
                sess.try_ack(v);
            }
        }
        MSG_PING => {
            let _ = feed.push_ctrl(encode_pong());
        }
        MSG_HELLO => {}
        other => warn!("unknown msg {other}"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::StubSession;
    use blit_remote::{CELL_SIZE, FrameState};
    use quosh_proto::{OutBuf, Screen, encode_input, encode_resize};

    #[test]
    fn invalid_length_rejected_before_body_arrives() {
        for n in [0u32, quosh_proto::MAX_FRAME as u32 + 1] {
            let stub = StubSession::new([9; 16], 1);
            let mut leftover = n.to_le_bytes().to_vec();
            leftover.push(MSG_INPUT);
            let result = drain_leftover(
                &stub.sess,
                0,
                &mut leftover,
                &mut FrameFeed::default(),
                &mut VecDeque::new(),
                BUSY_CAP,
                &mut None,
            );
            assert!(
                result.is_err(),
                "invalid frame length {n} must be rejected before the body arrives"
            );
        }
        let stub = StubSession::new([8; 16], 1);
        let mut leftover = 0u32.to_le_bytes().to_vec();
        assert!(
            drain_leftover(
                &stub.sess,
                0,
                &mut leftover,
                &mut FrameFeed::default(),
                &mut VecDeque::new(),
                BUSY_CAP,
                &mut None,
            )
            .is_err()
        );
    }

    #[test]
    fn empty_input_is_rejected() {
        let stub = StubSession::new([9; 16], 1);
        let mut leftover = encode_input(1, &[]);
        let mut busy = VecDeque::new();
        let result = drain_leftover(
            &stub.sess,
            0,
            &mut leftover,
            &mut FrameFeed::default(),
            &mut busy,
            1024,
            &mut None,
        );
        assert!(result.is_err(), "empty input must be rejected");
        assert!(busy.is_empty());
    }

    #[test]
    fn tiny_inputs_count_against_busy_budget() {
        let stub = StubSession::new([9; 16], 1);
        let mut leftover = Vec::new();
        for seq in 1..=1000 {
            leftover.extend_from_slice(&encode_input(seq, b"x"));
        }
        let mut busy = VecDeque::new();
        drain_leftover(
            &stub.sess,
            0,
            &mut leftover,
            &mut FrameFeed::default(),
            &mut busy,
            1024,
            &mut None,
        )
        .unwrap();
        assert!(
            !leftover.is_empty(),
            "all {} tiny input records queued under a 1024-byte budget; accounted bytes={}",
            busy.len(),
            busy_bytes(&busy)
        );
        assert!(busy_bytes(&busy) <= 1024 + input_record_cost(b"x"));
        assert!(busy.len() < 1000);
    }

    #[test]
    fn oversized_input_is_rejected() {
        let stub = StubSession::new([9; 16], 1);
        let mut leftover = encode_input(1, &vec![b'x'; MAX_INPUT_BYTES + 1]);
        let mut busy = VecDeque::new();
        let result = drain_leftover(
            &stub.sess,
            0,
            &mut leftover,
            &mut FrameFeed::default(),
            &mut busy,
            BUSY_CAP,
            &mut None,
        );
        assert!(result.is_err());
        assert!(busy.is_empty());
    }

    #[test]
    fn leftover_input_stops_at_busy_cap() {
        let stub = StubSession::new([1; 16], 1);
        let mut leftover = Vec::new();
        let chunk = vec![b'x'; 4096];
        for i in 0..80 {
            leftover.extend_from_slice(&encode_input(100 + i, &chunk));
        }
        let start_len = leftover.len();
        let mut feed = FrameFeed::default();
        let mut busy = VecDeque::new();
        let mut pending_resize = None;
        drain_leftover(
            &stub.sess,
            0,
            &mut leftover,
            &mut feed,
            &mut busy,
            BUSY_CAP,
            &mut pending_resize,
        )
        .unwrap();
        assert!(
            busy_bytes(&busy) <= BUSY_CAP + input_record_cost(&chunk),
            "busy_input exceeded cap+one frame: {}",
            busy_bytes(&busy)
        );
        assert!(
            leftover.len() < start_len && !leftover.is_empty(),
            "should consume some frames and leave the rest"
        );
    }

    #[test]
    fn resize_keeps_latest_when_owner_busy() {
        let stub = StubSession::new([2; 16], 1);
        let mut seq = 1u64;
        while let QueueResult::Queued = stub.sess.try_input(seq, vec![1]).unwrap() {
            seq += 1;
        }
        assert!(matches!(
            stub.sess.try_resize(80, 24).unwrap(),
            QueueResult::Busy
        ));
        let mut leftover = Vec::new();
        leftover.extend_from_slice(&encode_resize(80, 24));
        leftover.extend_from_slice(&encode_resize(120, 40));
        let mut feed = FrameFeed::default();
        let mut busy = VecDeque::new();
        let mut pending_resize = None;
        drain_leftover(
            &stub.sess,
            0,
            &mut leftover,
            &mut feed,
            &mut busy,
            BUSY_CAP,
            &mut pending_resize,
        )
        .unwrap();
        assert_eq!(pending_resize, Some((120, 40)));
    }

    #[test]
    fn large_screen_is_streamed_not_dropped() {
        let n = 300usize * 300 * CELL_SIZE;
        let mut cells = vec![0u8; n];
        let mut x: u32 = 1;
        for b in cells.iter_mut() {
            x = x.wrapping_mul(1664525).wrapping_add(1013904223);
            *b = (x >> 24) as u8;
        }
        let blob = Screen {
            version: 1,
            echo_ack: 0,
            frame: FrameState::from_parts(300, 300, 0, 0, 0, "", cells),
        }
        .encode_compressed()
        .unwrap();
        assert!(blob.len() > OutBuf::MAX_UNREAD);
        let framed = encode_frame(quosh_proto::MSG_SCREEN, &blob);
        let mut feed = FrameFeed::default();
        feed.push_screen(framed.clone());
        assert_eq!(feed.rest().len(), framed.len());
        feed.push_fin(encode_exit(0));
        assert_eq!(feed.rest().len(), framed.len());
        feed.advance(framed.len());
        assert_eq!(feed.rest(), encode_exit(0));
    }

    struct StallWrite;

    impl tokio::io::AsyncWrite for StallWrite {
        fn poll_write(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            _buf: &[u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            std::task::Poll::Pending
        }
        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn flush_bounded_returns_when_write_never_completes() {
        let mut feed = FrameFeed::default();
        feed.push_fin(vec![1; 64]);
        let start = std::time::Instant::now();
        let finished = tokio::time::timeout(
            Duration::from_secs(2),
            flush_bounded(&mut StallWrite, &mut feed, Duration::from_millis(400)),
        )
        .await;
        assert!(
            finished.is_ok(),
            "flush_bounded hung; the write deadline is what bounds teardown"
        );
        let dt = start.elapsed();
        assert!(
            dt >= Duration::from_millis(300),
            "flush returned before the deadline, so writes were not blocked: {dt:?}"
        );
        assert!(
            dt < Duration::from_secs(1),
            "flush overran its deadline: {dt:?}"
        );
    }

    #[tokio::test]
    async fn exit_before_subscribe_is_visible_without_changed() {
        let stub = StubSession::new([0xe1; 16], 1);
        stub.sess.publish_exit(12);
        let mut rx = stub.sess.subscribe_exit();
        let st = exit_status(&rx).expect("late subscriber must see already-published exit");
        assert_eq!(st, 12);
        assert_eq!(stub.sess.exit_code(), Some(12));

        let mut feed = FrameFeed::default();
        queue_exit(&stub.sess, stub.sess.current_epoch(), st, &mut feed);
        assert_eq!(feed.rest(), encode_exit(12));

        let missed = tokio::time::timeout(Duration::from_millis(40), rx.changed()).await;
        assert!(
            missed.is_err(),
            "watch::changed does not fire for the value present at subscribe"
        );
    }
}
