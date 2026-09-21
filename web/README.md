# Quosh reference browser client

A static, mobile-first PWA that connects a browser to a Quosh server over
WebTransport after one SSH-assisted enrolment. Plain TypeScript, no framework;
the protocol, prediction, and session logic come from the `@quosh/*` wasm
packages built by `tools/build-wasm.sh`.

## Layout

- `src/enroll.ts` — parse/build the `#enroll=` link payload (pure).
- `src/cells.ts` — decode the shared 12-byte cell format (pure).
- `src/input.ts` — key/byte encoding and bracketed paste (pure).
- `src/store.ts` — durable server list (IndexedDB) + tab-scoped session.
- `src/transport.ts` — browser `WebTransport`, pinned by `serverCertificateHashes`.
- `src/auth.ts` — the auth handshake and the WebAuthn ceremonies.
- `src/session.ts` — the reconnect loop driving the wasm client.
- `src/render.ts` — canvas2D renderer for the cell format.
- `src/keybar.ts` — sticky Ctrl/Alt, Esc/Tab/arrows, punctuation, paste/copy/quit.
- `src/app.ts` — routing, server list, enrolment, terminal view.

The `/e#enroll=…` path is the enrolment handler; the fragment never reaches the
static host. The server list is shown on every open, even with one entry.

## Build and test

```sh
../tools/build-wasm.sh   # requires wasm32 target + wasm-bindgen 0.2.100
npm install
npm run typecheck
npm test                 # pure-logic unit tests (Node, no browser)
npm run build            # -> dist/
npm run serve            # http://localhost:8099/ for manual testing
```

`dist/` contains `app.js`, `index.html`, `style.css`, and the three
`*_bg.wasm` files the bundle fetches via `import.meta.url`.

## What is and is not verified here

The pure logic (link parsing, cell decoding, key encoding) is unit-tested in
Node, and `cargo test` covers the Rust behind every wasm call. The
browser-only paths — WebAuthn, WebTransport, canvas rendering, the service
worker (later) — need a real browser and a running `quosh-server`; they are
written but not exercised in CI.

## Deployment

Serve `dist/` as static files. The host must:

- serve `application/wasm` for `.wasm`;
- set a CSP that allows `connect-src` to the user's servers and
  `wasm-unsafe-eval` for the predictor;
- be reachable at the Relying Party origin (`https://quosh.jtcs.dev`), since
  the passkey is bound to that origin.