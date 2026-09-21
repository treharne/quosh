# Plan

Canonical scope: [v1 spec](11-v1-spec.md). Implementation is authorised.

## Slice 1 — CLI session path (current)

Root `quosh-server` on UDP 443 + Unix socket. `quosh user@host` creates a login shell as that Unix user, speaks WebTransport, hangs up on clean exit, reconnects on network loss, Mosh-style outage banner. No prediction, no PWA.

## Slice 2 — prediction

Implemented: shared `quosh-predict` crate (adaptive/never), `echo_ack` in the QS2 screen payload, server-side write-completion checkpoints, and CLI wiring with RTT estimate and timer-driven reconciliation. Design and evidence: [12-prediction.md](12-prediction.md).

## Slice 3 — browser reference

Extract `quosh-client` (transport-agnostic session state machine) and move the
CLI onto it, then build the `@quosh/*` browser libraries and a thin reference
PWA that composes them. Static website at `https://quosh.jtcs.dev`
(installable PWA later). Client `quosh enroll [--ssh=cmd] user@host` prints the
authenticated link/QR. Passkeys bound to the enrol uid, with WebAuthn identity
`<unix_user>@<server_address>`. Reconnect without SSH via a pinned certificate
chain of 7 certs on a 13-day stride with 14-day validity (~92 days), topped up
per connection to ~92 days from the last connect. Same WebTransport endpoint.
Unmodified blit-browser only if it can consume our frames without patches.
Design: [13-browser-auth-and-trust.md](13-browser-auth-and-trust.md) and
[14-libraries-and-reference-app.md](14-libraries-and-reference-app.md).

Progress: `quosh-client` is extracted and the CLI runs on it
([14](14-libraries-and-reference-app.md)). The server trust foundation —
long-lived identity key, persisted 13-day/14-day certificate chain
(`--cert-chain N`, default 7), and a rustls resolver that serves the current
certificate without restarts — is implemented in
`crates/quosh-server/src/cert.rs` ([13](13-browser-auth-and-trust.md)). The
server-side auth core is also implemented: WebAuthn registration/assertion
verification, durable device registrations with sliding session tokens,
one-time enrolment nonces, the `enroll-nonce`/`devices`/`revoke` helper ops,
and `quosh enroll` / `quosh devices` / `quosh revoke`. The WebTransport auth
handshake (enrolment / assertion / token) is wired into the control stream.
Next: WASM bindings and the PWA.

## Later

Arbitrary remote commands; passkey CLI; server-local `quosh enrol`; system-hosted PWA; longer-lived identity without SSH; Herdr-on-Quosh; iOS/Android.

## Evidence

Same workloads with prediction on and off. Measure typing latency, correction rate, reconnect, bandwidth. High RTT, loss, reordered datagrams, resize, echo-off.
