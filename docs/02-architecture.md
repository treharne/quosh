# Architecture and proposed workspace

## Four logical responsibilities

| Layer | Owns | Does not own |
|---|---|---|
| Transport/communications | Connections, reliable delivery channels, optional datagrams, capacity/backpressure signals | Terminal meaning, prediction, PTYs |
| Terminal-state synchronisation | Snapshots, deltas, versions, confirmed replicas, resynchronisation | Network implementation, speculative display, process lifecycle |
| Client prediction | Confirmed state plus pending input → speculative display; confidence and reconciliation | Authoritative state, transport delivery, execution of commands |
| Server/runtime | PTY and child processes, authenticated sessions, authoritative emulator, composition of sync and transport | Frontend rendering or client speculation |

These may form three reusable library families plus an application runtime. Logical separation matters more than the number of crates.

## Data flow

Server: PTY output → terminal parser/emulator → authoritative terminal state → snapshot/delta generation → transport.

Client: transport → sync engine → confirmed state → optional prediction overlay → frontend renderer.

Local input reaches both the pending-input/prediction path and the sequenced send path. Authoritative updates reconcile speculative display. The predictor does not sit in, or control, transport delivery.

Confirmed: no WebSocket. One WebTransport endpoint (CLI and browser). Reliable streams for input/control/resync; versioned last-state-wins datagrams for screen. Missing QUIC is a hard failure. See [v1 spec](11-v1-spec.md).

## Server responsibilities

- Create and manage PTYs, child processes, environment, terminal type, dimensions, exit status, and cleanup.
- Maintain authoritative terminal state from PTY output, including cursor, modes, attributes, and selected screen/history features.
- Keep a session alive across transport loss for the client that holds its token; destroy it on hangup/`exit`. Unclean-death orphans linger until the 7-day prompt.
- Create sessions only via the local Unix socket (SSH helper, `SO_PEERCRED` uid). Attach over WebTransport with session id + token. Never take a Unix username from the network.
- Track input sequence numbers and per-client synchronization baselines; handle reconnect and stale connection epochs.
- Supply input receipt/application metadata and state versions needed to evaluate prediction. Avoid claiming that a PTY write proves the application has rendered its response.
- Negotiate capabilities, order resize against input, and decide writer ownership if multiple clients attach.
- Bound output/history/delta queues and recover slow clients using fresh snapshots.

Confirmed: shell persistence means surviving a disconnected client while the server and child remain alive; shell sessions do not survive server restarts. Passkey registrations must survive server restarts independently of shell sessions.

Confirmed browser authentication: use existing SSH access once to authorise passkey enrolment, then allow independent browser sign-in with the passkey. SSH can authorise replacement enrolment for recovery. Detailed challenge, origin, credential storage, revocation, and reconnect authorisation mechanics remain to be specified. See [decision record](06-decisions.md).

## Deployment and portability

A separately hosted static HTTPS PWA connects directly to the user's own Quosh server. No project-operated application backend, authentication service, discovery service, or relay is part of the architecture. The user server verifies passkeys and owns sessions. Initial server target: Ubuntu; CLI: macOS; browser: Chrome. Account for future iOS/Android PWA use and broader modern platforms through explicit platform adapters. One UDP server port is accepted; no WS fallback. GNU GPLv3 is the selected project license.

One live WebTransport per session. Same-token reconnect replaces the transport. No viewers and no takeover. A second process always creates a new shell.

## Clients

| Client | Integration concerns |
|---|---|
| CLI | ssh-like connection UX, local terminal input and rendering, raw-mode restoration, resize, signals, detach/reattach |
| Browser | Browser WebTransport adapter, shared core/WASM, reused Blit renderer, credentials, origin policy, reconnect after suspension |
| Mobile (later) | Embedded terminal, keyboard/IME/touch input, native or web integration, background suspension, shared core bindings |

CLI and browser both speak WebTransport to the same listener.

## Blit reuse boundary

Unmodified crates only. Slice 1 uses `blit-alacritty` to parse PTY bytes into `FrameState` and `blit-remote` for the cell grid / ANSI paint. Quosh owns the versioned last-state-wins envelope. Do not use Blit’s ordered `feed_compressed` sync, gateway, or server. If a crate needs a patch, rewrite that layer. PWA renderer reuse is slice 3.

## Workspace (slice 1)

| Location | Responsibility |
|---|---|
| `crates/quosh-proto` | Framing, screen payload, Unix-socket JSON |
| `crates/quosh-server` | Root daemon: PTY, sessions, WebTransport, Unix helper socket |
| `crates/quosh-cli` | `quosh` binary: SSH bootstrap, WT client, raw TTY, reconnect, banner |
