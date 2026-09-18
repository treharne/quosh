# Current reuse boundary and remaining decisions

Updated 2026-09-17 after the grill. Historical research; **current** boundary is [11-v1-spec.md](11-v1-spec.md). Implementation is authorised.

## Intended component boundary

| Area | Direction |
|---|---|
| Terminal emulation | Reuse Blit's Alacritty-based terminal driver. |
| Confirmed screen state | Reuse Blit's screen representation and applicable snapshot/diff encoding primitives. Add metadata where Quosh needs it. |
| Browser display | Reuse Blit's WASM/WebGL renderer; adapt its frontend/state integration. Do not add xterm.js by default. |
| PTY/process and CLI mechanics | Reuse separable Blit or existing library code; adapt lifecycle and native display plumbing. Exact dependency/fork/extraction boundary needs validation. |
| Screen synchronization | Quosh last-state-wins versioned payloads. Do not use Blit `feed_compressed` as the sync engine. |
| Network adapter | Compose an existing QUIC/WebTransport implementation with Quosh's reliable-stream/datagram policy. Reuse useful connection code where appropriate; no new QUIC or TLS implementation. |
| Input recovery | Build bounded client queues, session-scoped input identities, acknowledgements, replay suppression at the PTY owner, reconnect handling, and outage display. |
| Prediction | Build shared transport-independent prediction/reconciliation against confirmed state. Treat Blit's browser echo as a reference, not the final shared predictor. |
| Authentication and trust | Build SSH-assisted passkey enrolment/recovery, persistent registrations, server identity pinning, and authenticated certificate refresh using existing security libraries. |

The architecture boundary is clear. Direct dependencies versus a pinned fork/selective extraction, and the exact callable seams, still need compile/runtime validation after authorisation. Do not promise an unmodified Blit server plus a superficial network wrapper will satisfy the design.

## Confirmed network scope

No WebSocket or fallback. Use QUIC/WebTransport; keep reliable streams for input/control and datagrams for appropriate screen updates. WebRTC is not selected for the initial version. Blocked UDP or unsupported browser capability produces a clear failure. The server should not require a user-provisioned domain or manually managed certificates.

## Hosting, reachability, control, platforms, and license — confirmed

- The browser client is a separately hosted static HTTPS PWA. Quosh has no project-operated application backend, account server, discovery service, or terminal relay. The browser connects directly to the user's own Quosh server; that server owns sessions, authentication verification, and durable enrolment state. Static website hosting serves application assets only.
- The PWA host's HTTPS certificate protects delivery of the application. The user's server has its own QUIC/TLS identity, generated and managed automatically; no user-provisioned domain/public certificate is required by the intended design. Static hosting alone does not solve server certificate trust or renewal.
- One inbound UDP port: **443**. WebTransport only. No WS, no relay.
- No takeover. New process = new shell. Same-token reconnect replaces transport.
- Initial required platforms: Ubuntu server, macOS CLI, Chrome browser. Aim for PWA use on iOS and Android, and design platform boundaries for eventual broad modern server/OS/browser support. This is a portability goal, not a claim that all browser capabilities work everywhere today.
- Project license: GNU GPLv3. Record this selection in design documentation; exact notices and dependency obligations will be handled before source distribution. Do not silently substitute a different license or version option.

## Passkey portability — confirmed

Use normal passkey behavior, including a password manager's synced credentials where available. Do not require repeat enrolment solely because the browser changed when the same enrolled credential can authenticate. A new independent credential requires authorised enrolment. Server address/trust metadata availability on a new browser is a separate engineering question from passkey availability.

## Engineering questions, not a questionnaire for Jesse

Specify input acknowledgement semantics, delta baseline/version rules, resize ordering, bounded buffers and expiry policies, conservative prediction rules, authenticated certificate rotation after long-offline periods, passkey origin verification, and measurement thresholds. Propose concrete designs and validate them after implementation is authorised; ask Jesse only when a choice changes product behavior or operational requirements.

Product questions are settled in [11-v1-spec.md](11-v1-spec.md).
