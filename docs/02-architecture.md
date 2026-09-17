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

Confirmed: no WebSocket transport or fallback. Use QUIC/WebTransport with reliable streams for input/control and datagrams for replaceable screen state. A missing QUIC path must produce a clear connection failure rather than silently downgrade. Keep transport modular without implementing alternative transports by default.

## Server responsibilities

- Create and manage PTYs, child processes, environment, terminal type, dimensions, exit status, and cleanup.
- Maintain authoritative terminal state from PTY output, including cursor, modes, attributes, and selected screen/history features.
- Keep logical sessions alive when clients detach, subject to resource and retention limits.
- Authenticate and authorise creation/attachment; bind resume credentials to sessions rather than treating a session ID as sufficient authority.
- Track input sequence numbers and per-client synchronization baselines; handle reconnect and stale connection epochs.
- Supply input receipt/application metadata and state versions needed to evaluate prediction. Avoid claiming that a PTY write proves the application has rendered its response.
- Negotiate capabilities, order resize against input, and decide writer ownership if multiple clients attach.
- Bound output/history/delta queues and recover slow clients using fresh snapshots.

Confirmed: shell persistence means surviving a disconnected client while the server and child remain alive; shell sessions do not survive server restarts. Passkey registrations must survive server restarts independently of shell sessions.

Confirmed browser authentication: use existing SSH access once to authorise passkey enrolment, then allow independent browser sign-in with the passkey. SSH can authorise replacement enrolment for recovery. Detailed challenge, origin, credential storage, revocation, and reconnect authorisation mechanics remain to be specified. See [decision record](06-decisions.md).

## Deployment and portability

A separately hosted static HTTPS PWA connects directly to the user's own Quosh server. No project-operated application backend, authentication service, discovery service, or relay is part of the architecture. The user server verifies passkeys and owns sessions. Initial server target: Ubuntu; CLI: macOS; browser: Chrome. Account for future iOS/Android PWA use and broader modern platforms through explicit platform adapters. One UDP server port is accepted; no WS fallback. GNU GPLv3 is the selected project license.

One controller owns input and dimensions for each shell, with explicit takeover. Enforce controller generations so a replaced client cannot replay stale queued input into a session after handoff. Exact queue disposition remains to be specified.

## Clients

| Client | Integration concerns |
|---|---|
| CLI | ssh-like connection UX, local terminal input and rendering, raw-mode restoration, resize, signals, detach/reattach |
| Browser | Browser WebTransport adapter, shared core/WASM, reused Blit renderer, credentials, origin policy, reconnect after suspension |
| Mobile (later) | Embedded terminal, keyboard/IME/touch input, native or web integration, background suspension, shared core bindings |

Native QUIC and browser WebTransport are not interchangeable wire endpoints merely because both involve QUIC. Evaluate a common WebTransport endpoint for all clients versus separate native QUIC and WebTransport adapters speaking the same application protocol.

## Blit reuse boundary

The intended boundary is component reuse, not adopting the unchanged Blit application/network path. See [current reuse plan](10-reuse-and-remaining-decisions.md).

Reuse Blit's Alacritty-based parser, screen representation, selected diff/encoding functions, and WASM/WebGL renderer. Adapt the renderer/frontend connection to Quosh's sync/prediction state. Reuse existing PTY and native-terminal code where separable; a dependency versus fork/extraction choice still needs validation. Quosh owns the session/input-recovery semantics, versioned replaceable screen protocol, transport adapters, shared predictor, and passkey/server-trust orchestration. Existing QUIC/WebTransport libraries provide the actual transport and encryption implementation.

## Proposed Rust workspace (documentation only)

| Proposed location | Responsibility |
|---|---|
| `crates/protocol` | Message schemas, identifiers, versions, capability negotiation, snapshot/delta and acknowledgement semantics |
| `crates/transport` | QUIC/WebTransport interface and native/browser adapters; no WebSocket fallback |
| `crates/terminal` | Canonical state, PTY-output parsing adapter, snapshot/delta creation and application |
| `crates/prediction` | Pending input model, conservative speculative changes, confidence, reconciliation |
| `crates/server` | Server runtime, PTYs, sessions, authentication, resource limits |
| `crates/client-core` | Connection/session state machine, synchronization, input queue, predictor orchestration |
| `apps/cli` | Native terminal frontend |
| `apps/web` | Browser UI and WASM/JavaScript integration; need not be entirely Rust |
| `apps/mobile` | Later platform frontend and bindings |

The sync engine may warrant its own crate if it otherwise becomes duplicated between server and client-core. Place shared state types to avoid protocol/terminal dependency cycles. Keep OS PTY APIs out of WASM builds and prediction independent of async networking runtimes. Do not create these directories or manifests until implementation is authorised.
