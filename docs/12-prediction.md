# Local prediction (slice 2)

Status: predictor core landed (`crates/quosh-predict`) with a differential
oracle against Mosh. Wire/protocol and CLI wiring are the remaining steps.
Canonical scope: [v1 spec](11-v1-spec.md).

Only `--predict=adaptive` ships. `Always`/`Never` exist internally for tests
and the oracle.

## What prediction is

- **Confirmed state** — the last `FrameState` the server acknowledged with a
  version. Authoritative. Never written by the predictor.
- **Display state** — confirmed plus surviving predictions. What the terminal
  shows.
- **Prediction** — a reversible cell/cursor overlay derived from local input.
- Predictions are evaluated against confirmed state on every new server frame
  and are discarded or confirmed by evidence, never fed back into sync state.

## Components

`crates/quosh-predict` is a transport-free port of Mosh's `PredictionEngine`
(`reference/mosh/src/frontend/terminaloverlay.{h,cc}`, commit `decd9b7`).
It depends only on `blit-remote` (the shared cell representation), `vte` for
keystroke classification, and `unicode-width`. The CLI and the slice-3 PWA use
the same crate.

The port is a reimplementation, not a copy (Quosh and Mosh are both GPLv3; the
provenance is recorded in the module header). Fidelity is enforced by a
differential oracle, not by inspection.

## Adaptive triggers (Mosh's numbers)

The client feeds the predictor a `send_interval` derived from the smoothed RTT:
`clamp(ceil(SRTT / 2), 20, 250)` ms.

- **SRTT trigger** (`srtt_trigger`): predicts only when the link is slow enough
  to notice. Turns on when `send_interval > 30` ms, off only when it drops to
  `≤ 20` ms *and* no prediction is active (hysteresis). On a fast link
  predictions are not merely hidden late — they are never generated.
- **Underline flagging** (`flagging`): draws predicted cells underlined so the
  user can tell speculation from confirmation. On when `> 80` ms, off at
  `≤ 50` ms. Required in v1.
- **Glitch trigger**: a reliability backstop independent of RTT. A prediction
  that stays unconfirmed for `≥ 250` ms forces display on (and, at `≥ 5000` ms,
  underlining) even on a link that looked fast. Each fast confirmation
  (`< 250` ms) peels off one of the 10 repair tokens, at most once per 150 ms,
  so a burst of latency does not permanently pin predictions on.

## Epochs: why echo-off and passwords are safe

Predictions carry `tentative_until_epoch`. A prediction is displayed only once
`confirmed_epoch ≥ tentative_until_epoch`, and `confirmed_epoch` advances only
when a real server cell matches a prediction. `become_tentative` starts a new
epoch on unpredictable input (Enter, Escape, unidentified CSI, non-width-1
print). Consequences:

- After Enter, the first character is not shown until the server echoes it
  (one RTT). The rest of the line then flows immediately.
- A password prompt never echoes, so nothing in that epoch is ever confirmed
  and **no character is ever drawn**. This is the "must not predict echo-off"
  guarantee, and it is why the epoch machinery is core rather than polish.

## Acknowledgement inputs

Mosh distinguishes:

- `local_frame_sent` — input frames the client has sent.
- `local_frame_acked` — transport delivery ack (unused for validity).
- `local_frame_late_acked` — the input frame the server's authoritative state
  reflects. Mosh calls this the **echo ack** and advances it 50 ms after input
  is written to the PTY (`Complete::set_echo_ack`).

`crates/quosh-predict` takes the echo ack via `set_local_frame_late_acked`. The
v1 protocol adds `MSG_ECHO_ACK` (type 13, S→C, `u64 seq`) for it;
`MSG_INPUT_ACK` remains the PTY-write ack. Adding a screen-version-based
substitute was rejected: it reconciles against unrelated output.

## Stability properties

- Display is bounded: a predicted cell is written only if the confirmed cell
  differs, never grows, and is undone by `cull`.
- A wrong non-tentative prediction resets the whole engine; a wrong tentative
  one kills only its epoch.
- Dimension changes reset prediction.
- Bulk input (`> 100` bytes in one read) resets and does not predict.
- Predictions never use overflow cells: only width-1 characters are predicted.

## Testing

Unit tests in `crates/quosh-predict` cover the epoch gate, echo-off, adaptive
gating, backspace, cursor keys, and Unicode.

**Differential oracle.** `tools/mosh-oracle` builds Mosh's unmodified
`PredictionEngine` into a small driver (stripping the network includes it does
not use), runs a scripted stream of keystrokes/echo/acks, and dumps the grid.
`crates/quosh-predict/tests/oracle.rs` replays the same stream through the Rust
predictor over the same `blit-alacritty` emulator the server uses, and compares
content, underline, and cursor. Scenarios live in
`tools/mosh-oracle/scenarios/`, golden traces in
`crates/quosh-predict/tests/fixtures/`.

```sh
tools/mosh-oracle/build.sh      # needs reference/mosh and g++
tools/mosh-oracle/generate.sh   # regenerate fixtures
cargo test -p quosh-predict
```

`prediction-unicode.test` (`glück faĩl`) is covered by the `unicode` scenario.
The fixtures are committed because the reference checkout is not vendored.

## Remaining slice-2 work

1. `MSG_ECHO_ACK` plus server-side 50 ms echo-ack tracking (`Session`, `Owner`).
2. CLI wiring: `--predict=adaptive`, RTT estimate, repaint on keystroke, and
   `cull`/`apply` around every confirmed frame.
3. A PTY/tmux e2e that asserts a predicted character appears before the echo
   under injected latency, and a password-style silent app never shows one.
