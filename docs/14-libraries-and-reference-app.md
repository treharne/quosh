# Libraries and the reference PWA

Status: direction and decomposition agreed; not implemented. This sits
alongside [13-browser-auth-and-trust.md](13-browser-auth-and-trust.md).

## Direction

- The PWA is a **reference implementation**, not the product. Its job is to
  show how a browser Quosh terminal is built and to exercise the shared
  libraries end to end.
- Every reusable piece is a **separately consumable library**. A third party
  (e.g. Sheepdog) should be able to assemble the subset it needs into its own
  browser/PWA terminal without taking the reference app.
- Libraries must not depend on the reference app, and should not depend on each
  other beyond a clear direction.
- The CLI is a second consumer of the same core; the browser is not a special
  case in the protocol or the state machine.

## Proposed components

Rust crates:

- `quosh-proto` (exists) — wire framing, message types, QS2 screen payload.
- `quosh-predict` (exists) — Mosh-style prediction engine.
- `quosh-client` (new) — transport-agnostic client state machine: Hello/HelloOk,
  session id/token, input sequencing and unacked replay, ack handling,
  epoch/reconnect logic, ping/pong RTT, outage timing, and feeding the
  predictor. No I/O, no DOM, no tokio. This is what the CLI and the browser
  share.

Browser-facing (npm workspace, separate packages):

- `@quosh/proto` — wire framing and QS2 screen decode (`wasm-bindgen`).
- `@quosh/predict` — prediction engine bindings (`wasm-bindgen`).
- `@quosh/client` — transport-agnostic session state-machine bindings
  (`wasm-bindgen`), driven by events and emitting actions.
- `@quosh/auth` — passkey enrol/attach, certificate-hash chain, session token,
  server list in IndexedDB.
- `@quosh/transport` — browser WebTransport adapter (pinned hashes, control
  stream, datagrams).
- `@quosh/terminal` — confirmed frame state, renderer integration (Blit or
  thin), key bar and input mapping.

The reference PWA composes the browser packages; the CLI composes the crates.

## Constraints

- Core crates stay transport-agnostic, WASM-safe, and free of DOM/tokio.
- Browser-API access (WebTransport, WebAuthn, Clipboard) lives only in the TS
  adapters.
- The reference app should be thin: composition and chrome, not logic.

## Decisions

- **Language split:** Rust core (`quosh-proto`, `quosh-predict`,
  `quosh-client`) compiled to WASM; TypeScript only for browser-API adapters
  (WebTransport, WebAuthn, Clipboard, DOM).
- **Package granularity:** separate packages (`@quosh/proto`, `@quosh/predict`,
  `@quosh/client`, `@quosh/auth`, `@quosh/transport`, `@quosh/terminal`) rather
  than one bundle, so consumers can take a subset.- **One state machine:** `quosh-client` is extracted and the CLI is refactored
  onto it as part of slice 3, so the CLI and browser share the session,
  reconnect, and prediction-feed logic.
- **Distribution:** git/VCS dependencies for now; publish to npm and crates
  once the API stabilises.

## Open

- npm workspace layout and the exact crate/package names.
- Slice 3 acceptance criteria.