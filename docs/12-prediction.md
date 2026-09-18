# Local prediction (slice 2)

Status: landed. `quosh-predict` is wired through `quosh-proto` (QS2 screen
payload with `echo_ack`), `quosh-server` (per-message write completion and
checkpoint re-publish), and the CLI (`--predict=adaptive|never`, RTT estimate,
timer-driven ticks). Canonical scope: [v1 spec](11-v1-spec.md).

The CLI ships `--predict=adaptive|never` (default `adaptive`). `Always` exists
only for tests and the oracle.

## What prediction is

- **Confirmed state** — the last `FrameState` the server acknowledged with a
  version. Authoritative. Never written by the predictor.
- **Display state** — confirmed plus surviving predictions. What the terminal
  shows.
- **Prediction** — a reversible cell/cursor overlay derived from local input.
- Predictions are evaluated against confirmed state on every new server frame
  and are discarded or confirmed by evidence, never fed back into sync state.

`crates/quosh-predict` is a transport-free port of Mosh's `PredictionEngine`
(`reference/mosh/src/frontend/terminaloverlay.{h,cc}`, commit `decd9b7`). It
depends only on `blit-remote`, `vte`, and `unicode-width`, and is shared with
the slice-3 PWA. The port is a reimplementation, not a copy; fidelity is
enforced by the differential oracle, not by inspection.

## Adaptive triggers (Mosh's numbers)

The client feeds the predictor a `send_interval` derived from the smoothed RTT:
`clamp(ceil(SRTT / 2), 20, 250)` ms. Predictions are always generated and
always reconciled; the triggers below decide whether [`Predictor::apply`]
*copies them onto the display*. Generation is independent of display so the
glitch trigger can still fire on a link that looked fast.

- **SRTT trigger** (`srtt_trigger`): display only when the link is slow enough
  to notice. On when `send_interval > 30` ms, off only when it drops to
  `≤ 20` ms *and* no prediction is active (hysteresis).
- **Underline flagging** (`flagging`): draw predicted cells underlined so
  speculation is distinguishable. On when `> 80` ms, off at `≤ 50` ms.
  Required in v1.
- **Glitch trigger**: a backstop independent of RTT. A prediction unconfirmed
  for `≥ 250` ms forces display on (and at `≥ 5000` ms, underlining). Each fast
  confirmation (`< 250` ms) peels off one of 10 repair tokens, at most once per
  150 ms, so a latency spike does not pin predictions on.

## Echo checkpoints: acknowledgements belong to a screen

A standalone acknowledgement message cannot be reconciled safely. The server
may echo input 7 into screen version 20, lose or delay that screen, and deliver
ack 7 independently; the client would then reconcile input 7 against an older
screen and reject a valid prediction.

**Contract.** The echo acknowledgement is carried *inside each screen
snapshot*, so state and causality travel together:

```
magic      b"QS2\0"
version    u64le
echo_ack   u64le      newly added
rows ...              (unchanged)
```

Rules:

1. The client applies a screen atomically: confirmed frame and `echo_ack` are
   installed together, last-state-wins on `version`. A stale screen's ack is
   ignored with the screen.
2. After installing a screen the client runs `cull(confirmed, now)` with the
   predictor's late ack set to that screen's `echo_ack`; only then is the new
   display computed with `apply`.
3. An ack is never advanced without its screen. The client must not act on an
   ack for a version it has not applied.

Point 3 also makes `Predictor::new_user_byte`'s internal `cull` (which Mosh
runs against the *display* frame) sound: immediately after a screen, every live
overlay is `Pending` (ack below its expiration) or tentative, so no speculative
cell can be mistaken for evidence. If an ack were advanced first, a later
display frame could confirm a prediction against another prediction. We keep
Mosh's display-frame cull for parity, and rely on rule 3.

`echo_ack` is not a standalone message; the earlier `MSG_ECHO_ACK` proposal is
withdrawn. When the ack advances on a quiet link the server re-publishes the
current frame as a new screen version (same cells, new ack). Mosh does the
equivalent by re-sending state. The version bump also makes the existing
reliable resync resend it.

### Input sequence mapping

One `MSG_INPUT` message carries one sequence number. Every prediction created
while processing the bytes of that message uses `expiration_frame = seq`. The
predictor expires at `local_frame_sent + 1` (Mosh's convention), so the client
calls `set_local_frame_sent(seq - 1)` once before the byte loop, exactly as
Mosh's `process_user_input` sets it once per read. A multi-byte message is
therefore acknowledged as a unit. Queued offline input keeps the sequence it
was assigned; prediction is reset on transport loss, so no stale expirations
survive a reconnect.

### Server-side tracking

Three distinct states, not one:

| State | Meaning | Wire |
|---|---|---|
| **accepted** | the owner took the message into its pending-write queue | `MSG_INPUT_ACK` (unchanged: owner acceptance, *not* PTY completion) |
| **written** | every byte reached the PTY master | internal only |
| **checkpoint** | a written frame is eligible for reconciliation | `echo_ack` in the screen |

The owner records `(seq, completed_at)` when the last byte of a message is
written, not when it is enqueued. The pending buffer is a queue of sequenced
chunks (`PendingWrites`); generated replies share an unsequenced chunk so they
never complete an input sequence. `echo_ack` advances to the newest entry older
than 50 ms, and the owner re-publishes the current frame as a new screen
version when it moves. The **50 ms is a settling heuristic, not proof** that
the application has read, processed, or echoed the input.

## Safety boundaries

The epoch gate provides *conservative prediction*, not a confidentiality
guarantee.

- `become_tentative` starts a new epoch on Enter, Escape, unidentified CSI, and
  non-width-1 print. A prediction is displayed only once a prediction in its
  epoch is confirmed by real server output. A password prompt reached through
  Enter (the common `sudo` case) is therefore never drawn: nothing in the new
  epoch is ever confirmed. Covered by `oracle_password_after_enter` and the
  fresh `oracle_echo_off`.
- It does **not** cover every transition. Once an epoch is confident from
  normal echoed typing, an application that starts consuming silent input
  without an epoch-breaking byte leaves the next printable character in the
  confirmed epoch, where it can be displayed until reconciliation removes it.
  `oracle_silent_midline` documents this limitation against Mosh's own
  behaviour. Unrelated output and delayed echo during silent input are also
  covered there.
- A termios `ECHO`-off heuristic does not fix this: readline and other
  programs turn `ECHO` off and echo in user space, so `ECHO` off is not a
  reliable "do not predict" signal.
- The escape hatch is the user-facing switch: `--predict=adaptive|never`.
  `never` disables prediction entirely for privacy-critical sessions. This is
  the v1 spec's required disable switch.

## Representation limits

- Only width-1 characters are predicted. Predicted cells never need the
  overflow table.
- Confirmation copies complete renditions, including foreground/background
  color values, not just the attribute flags.
- Insert and backspace shift existing cells. `blit-remote` keeps text longer
  than four bytes in a side table keyed by cell index, and OSC 8 links in a
  parallel per-cell array. Shifting those faithfully is not possible through
  the public `FrameState` API, so the predictor **declines** (becomes
  tentative) when an affected cell has overflow text or a link. This is a
  deliberate divergence from Mosh on rare combining/link lines: it costs a
  little latency and never corrupts the line.
- Blanking a row removes overflow entries for the blanked cells.

## Testing

Unit tests in `crates/quosh-predict` cover the epoch gate, echo-off,
confident-epoch silent input, adaptive hysteresis after warm-up, multi-byte
message sequencing, backspace, cursor keys, rendition copying, overflow
decline, and Unicode.

**Differential oracle.** `tools/mosh-oracle` builds Mosh's unmodified
`PredictionEngine` into a scripted driver (stripping the network includes it
does not use), and `crates/quosh-predict/tests/oracle.rs` replays the same
stream through the Rust predictor over the same `blit-alacritty` emulator the
server uses. Scenarios: `basic`, `cursor`, `cr`, `unicode`, `echo_off`,
`password_after_enter`, `silent_midline`, `glitch`, `delay`, `mismatch`,
`adaptive`. Content, underline, and cursor are compared. Colors are checked by
unit test rather than the oracle because the two emulators encode palettes
differently.

```sh
tools/mosh-oracle/build.sh      # needs reference/mosh and g++
tools/mosh-oracle/generate.sh   # regenerate fixtures
cargo test -p quosh-predict
```

`prediction-unicode.test` (`glück faĩl`) is covered by `unicode`.

## CLI wiring

1. `--predict=adaptive|never`; RTT EWMA from ping/pong → `send_interval`.
2. On an input read: assign one `seq`, feed the bytes to `new_user_byte` with
   the current display frame, then repaint. Because the predictor expires at
   `local_frame_sent + 1`, message `seq` is fed as `seq - 1` so the prediction
   expires exactly at the server's checkpoint for that message.
3. On a screen: apply last-state-wins, set the predictor late ack from the
   screen, `cull(confirmed)`, repaint.
4. Timer ticks while any prediction is active (50 ms), so a pending prediction
   still reaches the 250 ms glitch threshold on a quiet link. Each tick
   recomputes the display from the *confirmed* frame and reapplies overlays.
5. Repaints are skipped when the predicted frame equals the last displayed
   frame. This is what keeps ticks silent on a fast link, where predictions
   are generated but `apply` leaves the frame unchanged.
6. Bulk input (a read over 100 bytes) calls `reset()` and does not predict
   that batch; the predictor cannot see stdin read sizes, so the CLI decides.
7. Reset on transport loss, reconnect, and dimension change.
8. The tty reader never blocks on the ordinary input channel. It buffers up
   to 64 KiB locally and retries on a 20 ms poll deadline, so input resumes
   when the consumer recovers and `Ctrl-^ .` is always seen. Past the cap it
   fails closed: the attachment is torn down rather than dropping bytes and
   forwarding a command stream with holes in it. The remote session survives
   for a reattach.
9. Failure classification is explicit. Protocol/authentication rejection — a
   server `Error` frame, a malformed `HelloOk`, a wrong protocol, or a clean
   stream close before `HelloOk` — is fatal, because retrying the same pair
   cannot help. Transport failures (read/write/datagram errors) are retried,
   but a handshake that keeps failing for a transport reason is fatal after
   `MAX_HANDSHAKE_FAILURES` attempts. After `HelloOk`, any transport loss
   reconnects. Treating pre-handshake transport errors as retryable is an
   availability choice, not a correctness guarantee. Every terminal frame
   (Exit or a rejection Error) is followed by a bounded `send.finish()` so the
   connection is not dropped before the peer acks it.
10. Liveness is application-level, not left to QUIC. The client pings every
    second and re-probes if a pong is lost (a single lost pong must not stop
    probing). If nothing arrives for `max(8s, 4*SRTT)` it drops the connection
    and reconnects, instead of waiting for QUIC's ~30 s idle timeout — so an
    IP/VPN path change becomes a reconnect in seconds. The remote session is
    unaffected throughout: the PTY and shell live in the server's
    `Session`/`Owner`, and the client re-attaches with the same session id and
    token.

Latency e2e (`crates/quosh-cli/tests/e2e.rs`): warm the epoch with one echoed
character, slow the proxy to 400 ms each way, then assert the next character
appears within 300 ms (before its ~800 ms echo); `--predict=never` asserts the
same character does *not* appear early. A silent-app e2e runs `stty -echo` and
asserts a post-Enter keystroke is never drawn.
