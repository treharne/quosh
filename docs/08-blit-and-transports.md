# Blit reuse, transport choices, and alternatives

Research date: 2026-09-17. Prepared for Quosh's agreed first version: a small CLI/browser shell demo; sessions survive disconnections but not server restarts; one-time SSH-assisted passkey enrolment; Mosh-style outage banner with continued input queueing.

## Follow-up clarification

Jesse has since explicitly rejected WebSocket fallback. Any fallback recommendation below is historical and superseded. The [current reuse plan](10-reuse-and-remaining-decisions.md) defines the intended boundary.

Jesse subsequently rejected requiring a domain/manual TLS management on the shell server. See [transport and server identity clarification](09-transport-and-server-identity.md). The public-443 deployment below was a proposal, not a requirement. Blit's ordered stream path is a baseline; final replaceable screen updates need changes to sync and transport adapters. Direct self-signed WebTransport and WebSocket fallback have different browser trust constraints.

## Recommendation

**Use Blit as the starting point for terminal-state handling and the browser frontend. It provides substantial reusable functionality, but it is not already the complete Quosh design.** Prefer its existing parser, state/diff machinery, and renderer over building equivalents or adding xterm.js unnecessarily. Evaluate its server/gateway as the first integration route; keep the independent prediction and input-recovery layer as Quosh's work.

This is a source-backed architecture recommendation, not a successful integration test or an implementation decision about every crate. No Quosh code, dependency manifests, builds, installations, services, or firewall changes were made. Implementation remains paused.

Primary source checkouts examined:

- [Blit, commit 7ec6085](https://github.com/indent-com/blit/tree/7ec6085c54430f003fbafa7542613c46994c09c8), workspace crates versioned 0.55.1.
- [RoSE, commit bf5212a](https://github.com/nikhiljha/rose/tree/bf5212aa0929a3443158afe530ba7766489267d7).
- Existing local Mosh reference documented in `07-mosh-reference.md`.

Blit and RoSE were shallow-cloned for read-only investigation in the research workspace. Primary documentation and source were inspected; runtime behavior and dependency build compatibility have not been tested.

## What Blit already supplies

Blit's server consumes PTY output, runs a terminal emulator, maintains authoritative screen state, and sends per-client compressed state changes. Its browser applies those changes in WASM and renders them with WebGL. That is materially different from simply forwarding a raw terminal byte stream into a browser emulator. [Architecture](https://github.com/indent-com/blit/blob/7ec6085c54430f003fbafa7542613c46994c09c8/ARCHITECTURE.md), [terminal driver](https://github.com/indent-com/blit/blob/7ec6085c54430f003fbafa7542613c46994c09c8/crates/alacritty-driver/src/lib.rs#L439).

| Component | What we can reuse | Qualification |
|---|---|---|
| `blit-alacritty` | Terminal parser/emulator adapter, snapshots, terminal modes, cursor/history handling | Uses a Blit-specific Alacritty package. Its public driver accepts bytes through `process` and returns `FrameState` through `snapshot`; it need not own the PTY. |
| `blit-remote` | Shared screen state, update construction/application, compression, message formats, ANSI export | Small dependency list, but wire semantics assume ordered reliable delivery. Not already a Mosh-style datagram sync engine. |
| `blit-server` | PTY/process hosting, persistent live sessions, per-client delivery | Available as a library, but broad: compositor, media, filesystem, Git/LSP, and extension machinery accompany terminal functionality. |
| Browser WASM + `@blit-sh/core` | Frame application, WebGL rendering, browser input, transport interfaces | Already has a renderer. xterm.js is unnecessary unless we deliberately choose a different integration. |
| `blit-gateway` | Browser WS/WT transport bridging, routing, existing authentication seam | Current authentication is passphrase-based, not the agreed passkey flow. |
| Native CLI attach | A real terminal frontend, applying shared state and repainting ANSI | Unix implementation; whole-screen repainting. It exits its connection loop on loss rather than providing the agreed Mosh-style reconnect/input-retention loop. |

Sources: [driver manifest](https://github.com/indent-com/blit/blob/7ec6085c54430f003fbafa7542613c46994c09c8/crates/alacritty-driver/Cargo.toml), [driver API](https://github.com/indent-com/blit/blob/7ec6085c54430f003fbafa7542613c46994c09c8/crates/alacritty-driver/src/lib.rs#L465), [state and diff crate](https://github.com/indent-com/blit/blob/7ec6085c54430f003fbafa7542613c46994c09c8/crates/remote/src/lib.rs#L1718), [update builder](https://github.com/indent-com/blit/blob/7ec6085c54430f003fbafa7542613c46994c09c8/crates/remote/src/lib.rs#L5280), [server manifest](https://github.com/indent-com/blit/blob/7ec6085c54430f003fbafa7542613c46994c09c8/crates/server/Cargo.toml), [native attach](https://github.com/indent-com/blit/blob/7ec6085c54430f003fbafa7542613c46994c09c8/crates/cli/src/attach.rs#L190).

### Correction: Blit does have some prediction

The earlier conversation's blanket claim that Blit lacks prediction is outdated/incomplete. Current browser code contains dimmed speculative echo and cursor-position-based reconciliation. It also has a separate host-text-prediction integration; that feature should not be confused with Mosh-style network-latency prediction. [Browser prediction and reconciliation](https://github.com/indent-com/blit/blob/7ec6085c54430f003fbafa7542613c46994c09c8/js/core/src/BlitTerminalSurface.ts#L2240).

However, this is not a shared Rust prediction engine. Echo rendering is gated on `t.echo()`, and reconciliation mainly trims pending text according to cursor movement rather than input acknowledgements. The browser source itself notes that interactive bash/fish/zsh prompts commonly disable terminal ECHO to handle line editing. Thus the existence of speculative-echo code does **not** establish that ordinary shell typing already gets Mosh-quality prediction. It needs a shell-focused runtime evaluation. [Echo gate and reconciliation](https://github.com/indent-com/blit/blob/7ec6085c54430f003fbafa7542613c46994c09c8/js/core/src/BlitTerminalSurface.ts#L2245), [shell-mode commentary](https://github.com/indent-com/blit/blob/7ec6085c54430f003fbafa7542613c46994c09c8/js/core/src/BlitTerminalSurface.ts#L2332), [mode accessors](https://github.com/indent-com/blit/blob/7ec6085c54430f003fbafa7542613c46994c09c8/crates/browser/src/lib.rs#L738).

### The remaining Quosh work

1. **Disconnected input retention and replay suppression.** Browser `sendInput` returns without sending when disconnected. Input messages contain terminal ID and bytes, with no input sequence. The ACK message has no payload and is handled as delivery pacing, not cumulative input acknowledgement. Simply adding a client queue is insufficient to prevent duplicate commands after ambiguous disconnects. [Browser input](https://github.com/indent-com/blit/blob/7ec6085c54430f003fbafa7542613c46994c09c8/js/core/src/BlitConnection.ts#L1888), [input format](https://github.com/indent-com/blit/blob/7ec6085c54430f003fbafa7542613c46994c09c8/crates/remote/src/lib.rs#L3865), [ACK handling](https://github.com/indent-com/blit/blob/7ec6085c54430f003fbafa7542613c46994c09c8/crates/server/src/lib.rs#L19321).
2. **Shared, conservative prediction and reconciliation.** Reuse confirmed state; put Quosh's speculative state outside it. Test normal shell prompts, backspace, wrapping, echo-off prompts, delayed output, and reconnect. The current browser-only heuristic is a starting reference, not proof of suitability.
3. **Passkey enrolment and durable credential storage.** The gateway's existing handshake uses a passphrase. No WebAuthn/passkey implementation was found in the inspected Rust/TypeScript sources. The agreed SSH-assisted enrolment and recovery flow needs a new authentication integration. [Current authentication](https://github.com/indent-com/blit/blob/7ec6085c54430f003fbafa7542613c46994c09c8/docs/transports.md#auth-handshake).
4. **Optional replaceable screen delivery.** Blit currently requires reliable ordered transport. Datagrams would require versioned bases, acknowledgements, resynchronisation, size handling, and policies for side effects; compressed diffs alone do not provide this. Preserve reliable delivery for an initial baseline. [Transport contract](https://github.com/indent-com/blit/blob/7ec6085c54430f003fbafa7542613c46994c09c8/docs/transports.md).

## Does speaking to the PTY prevent reuse?

**No. It is precisely what the authoritative server should do.** Prediction belongs on the client, alongside its confirmed screen. It does not replace the PTY or require prediction logic inside the shell.

The useful composition is:

```text
Shell ↔ PTY ↔ Blit terminal driver → authoritative state → state updates
                                                              ↓
                       CLI / browser: confirmed state + Quosh prediction → display
```

Three practical reuse levels, in preference order:

1. **Reuse Blit's server and clients as a baseline, extend the necessary protocol/client behavior.** Best opportunity to avoid rebuilding working integrations. A pinned fork or upstreamable changes may be necessary for input acknowledgement/replay and shared prediction; an unmodified gateway wrapper cannot manufacture reliable PTY-application acknowledgements.
2. **Compose a smaller Quosh runtime from `blit-alacritty` and `blit-remote`, retaining the browser renderer.** Best fallback if the broad server creates too much coupling. The driver accepts bytes independently, so Quosh can own PTY lifecycle while using Blit's emulator and snapshots.
3. **Extract selected code only if direct dependency reuse fails.** This adds a maintenance burden; prefer existing interfaces first.

The server exposes `Config` and `run`, supports disabling compositor startup, and accepts preconnected descriptors through an fd-channel. These are useful embedding seams, but disabling a subsystem at runtime does not remove its compile-time dependencies. [Server configuration](https://github.com/indent-com/blit/blob/7ec6085c54430f003fbafa7542613c46994c09c8/crates/server/src/lib.rs#L432), [fd-channel](https://github.com/indent-com/blit/blob/7ec6085c54430f003fbafa7542613c46994c09c8/docs/transports.md#fd-channel), [dependency graph](https://github.com/indent-com/blit/blob/7ec6085c54430f003fbafa7542613c46994c09c8/crates/server/Cargo.toml).

Blit's own code is MIT-licensed. That supports component reuse; selected dependency/build-feature licenses still need recording. For example, the server manifest explicitly flags its optional x264 feature separately. [License](https://github.com/indent-com/blit/blob/7ec6085c54430f003fbafa7542613c46994c09c8/LICENSE), [feature declarations](https://github.com/indent-com/blit/blob/7ec6085c54430f003fbafa7542613c46994c09c8/crates/server/Cargo.toml#L13).

## WS, WT, WebRTC, and QUIC

The names describe different layers. **Blit's WT path already is QUIC**, through WebTransport over HTTP/3. WebRTC data channels use a different stack. WebSocket is a broadly supported reliable message channel; the usual deployment uses TCP/TLS. [WebTransport stack](https://github.com/moq-dev/web-transport#webtransport), [WebRTC data-channel standard](https://www.rfc-editor.org/rfc/rfc8831.html#section-5), [WebSocket standard](https://www.rfc-editor.org/rfc/rfc6455.html).

| Choice | Browser/PWA access | Advantages | Drawbacks / deployment |
|---|---|---|---|
| Secure WebSocket (WSS) | Yes | Broad compatibility; easy to place behind a normal HTTPS reverse proxy; straightforward fallback | Ordered reliable delivery; on TCP, loss delays later bytes. No API for replaceable unreliable messages. Can normally share the existing HTTPS TCP port. |
| WebTransport over HTTP/3 | Yes, in supporting browsers and a secure context | QUIC streams plus optional unreliable datagrams; suitable for browser-to-known-server use; shared native/WASM libraries exist | Requires UDP reachability and a WebTransport-capable endpoint; a normal TCP-only reverse proxy is insufficient. Browser/certificate compatibility must be tested on target devices. |
| WebRTC DataChannel | Yes | NAT traversal, direct peer connections, TURN relay fallback; configurable ordered/reliable or partially reliable delivery | Signaling plus ICE/STUN/TURN complexity; relays may add latency and bandwidth cost. Direct connectivity is not guaranteed. |
| Raw/native QUIC via Quinn | No general raw QUIC API in an ordinary web page/PWA | Full native control over streams, datagrams, endpoint configuration | Requires a reachable UDP endpoint; browsers need a WebTransport-facing endpoint or bridge. A PWA does not gain native socket access by being installed. |

Sources: [WebSocket browser API](https://developer.mozilla.org/en-US/docs/Web/API/WebSockets_API), [WebTransport browser API and compatibility](https://developer.mozilla.org/en-US/docs/Web/API/WebTransport), [WebRTC browser API](https://developer.mozilla.org/en-US/docs/Web/API/RTCDataChannel), [QUIC](https://www.rfc-editor.org/rfc/rfc9000.html), [QUIC datagrams](https://www.rfc-editor.org/rfc/rfc9221.html), [ICE/STUN/TURN explanation](https://developer.mozilla.org/en-US/docs/Web/API/WebRTC_API/Protocols).

WebTransport API support does not mean every deployment uses QUIC: current API documentation also describes a reliable-only HTTP/2 fallback option. Do not assume that fallback works in the chosen library/browser combination. Blit's inspected implementation is explicitly QUIC/HTTP3. [API options](https://developer.mozilla.org/en-US/docs/Web/API/WebTransport/WebTransport), [Blit gateway](https://github.com/indent-com/blit/blob/7ec6085c54430f003fbafa7542613c46994c09c8/crates/gateway/src/lib.rs#L675).

### What Blit actually uses

Its WebTransport connection carries framed terminal messages over one reliable bidirectional stream (including a multiplexed form). Its WebRTC data channel is ordered and reliable. Therefore neither path currently exploits unreliable terminal updates. QUIC avoids transport-level ordering dependencies *between different streams*, but later data in the *same* stream still waits for missing earlier bytes. Prediction remains useful on every transport. [Blit framing and WebRTC configuration](https://github.com/indent-com/blit/blob/7ec6085c54430f003fbafa7542613c46994c09c8/docs/transports.md#stream-framing), [browser channel creation](https://github.com/indent-com/blit/blob/7ec6085c54430f003fbafa7542613c46994c09c8/js/core/src/transports/webrtc.ts#L185), [QUIC stream delivery](https://www.rfc-editor.org/rfc/rfc9000.html#section-2.2).

### Do new server ports need opening?

- **WS/WSS:** if an HTTPS gateway already exposes TCP 443, a WebSocket route can share it; no additional public port number is inherently needed. If only SSH is exposed today, browser access needs a gateway listener somewhere. It can proxy to the shell machine over existing SSH, so the shell machine need not expose a new service.
- **WT/QUIC:** permit UDP to the endpoint. Using UDP 443 avoids a new port *number*, but TCP 443 and UDP 443 are distinct firewall/listener rules. The endpoint must actually handle WebTransport; ordinary HTTPS support alone is not enough. Directly exposing an alternate UDP port is also possible.
- **Stock Blit gateway:** default address is `0.0.0.0:3264` for HTTP/WebSocket; `BLIT_QUIC=1` adds QUIC on the same address/number over UDP. So a direct default deployment needs TCP 3264 and, for WT, UDP 3264. Production mapping to HTTPS/WSS TCP 443 and WT UDP 443 can avoid exposing 3264 publicly. Certificate and advertised-address configuration still matter. [Defaults and listener setup](https://github.com/indent-com/blit/blob/7ec6085c54430f003fbafa7542613c46994c09c8/crates/gateway/src/lib.rs#L575), [WT configuration](https://github.com/indent-com/blit/blob/7ec6085c54430f003fbafa7542613c46994c09c8/docs/transports.md#webtransport-quic--http3).
- **WebRTC:** often works without manually opening a fixed inbound port on the terminal host because ICE establishes a viable path or uses TURN. This is not a guarantee of zero firewall requirements. Outbound signaling/STUN/TURN must be allowed; running your own TURN service needs its own listener and relay allocation configuration. Blit's share path uses signaling and STUN/TURN services, with UDP and TCP/TLS relay options. [Blit NAT traversal](https://github.com/indent-com/blit/blob/7ec6085c54430f003fbafa7542613c46994c09c8/docs/transports.md#nat-traversal).

For Quosh, the simplest initial production-shaped target is HTTPS/WSS on TCP 443 plus WebTransport on UDP 443, with the gateway near the PTY server. Keep WebSocket fallback; defer WebRTC unless reaching machines behind NAT without inbound configuration becomes a requirement. A browser-to-gateway QUIC hop followed by a long-distance SSH/TCP hop still inherits the latter hop's ordering delays. These are deployment recommendations, not changes made to any host.

## Brief alternatives

| Alternative | Relevance | Recommendation |
|---|---|---|
| RoSE | Closest native Rust/QUIC/Mosh-style architecture. Current source includes acknowledged screen baselines, datagrams, retained input offsets, duplicate overlap suppression, and automatic reconnect. | Strong protocol reference. Not a better ready-made CLI/browser base: no WebTransport/browser client found in the inspected source, and README/spec prediction claims were not matched by an implemented predictor in the inspected library/CLI. Its GPL-3.0-or-later license also differs from Blit's. |
| Terminal7 + webexec | Browser/PWA/mobile terminal over WebRTC, TypeScript/xterm.js frontend and Go backend, direct/relayed connectivity. | Useful mobile/NAT UX reference. Less aligned with a shared Rust state/prediction core; no verified substitute for the complete requested semantics in this brief review. |
| Smaller custom composition | `vt100` or an emulator component, a native/WASM WebTransport library, and a browser frontend. | Fallback if Blit's interfaces or dependency breadth are unsuitable. Requires more custom sync/render integration. |

Sources: [RoSE input retention and replay code](https://github.com/nikhiljha/rose/blob/bf5212aa0929a3443158afe530ba7766489267d7/lib/src/input.rs), [RoSE SSP](https://github.com/nikhiljha/rose/blob/bf5212aa0929a3443158afe530ba7766489267d7/lib/src/ssp.rs), [RoSE CLI reconnect](https://github.com/nikhiljha/rose/blob/bf5212aa0929a3443158afe530ba7766489267d7/lib/src/cli/client.rs), [RoSE scope/license](https://github.com/nikhiljha/rose/blob/bf5212aa0929a3443158afe530ba7766489267d7/README.md), [Terminal7](https://github.com/tuzig/terminal7), [vt100 API](https://docs.rs/vt100/0.16.2/vt100/), [native/WASM WebTransport](https://github.com/moq-dev/web-transport).

One correction to the original library suggestions: `termwiz::Surface` is not itself a complete PTY emulator. The WezTerm maintainer explicitly distinguishes the emulator needed to convert PTY bytes into a screen; using WezTerm's terminal crate also needs attention to its external API stability. [Maintainer explanation](https://github.com/wezterm/wezterm/discussions/5217). Blit already integrates an Alacritty-based emulator, so there is no immediate reason to replace it with termwiz or vt100.

## Next steps after explicit implementation authorisation

1. Pin Blit and validate its unmodified shell path in CLI and browser, including disconnect/reattach. Measure what its existing echo actually does in bash/zsh/fish.
2. Try the full server/gateway integration first, with unrelated functionality disabled where possible; retain the component-only fallback if build footprint or coupling is excessive.
3. Specify and add client/session input identities, cumulative acceptance offsets, retained queue bounds, and deduplication at the PTY owner. Distinguish PTY-write acknowledgement from evidence of displayed input effects.
4. Add the agreed passkey enrolment/recovery flow, independently persistent from live shells.
5. Build shared prediction against Blit's confirmed state. Only then compare reliable screen delivery against a versioned datagram strategy under controlled loss.

The choice is to reuse Blit's working terminal stack and concentrate new work on Quosh's remaining behavior, rather than assume that transport selection alone completes the project.
