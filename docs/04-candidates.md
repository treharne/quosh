# Candidate libraries and projects

This preserves the initial evaluation backlog. The [2026-09-17 source review](08-blit-and-transports.md) supersedes its unverified assumptions: Blit is the recommended reuse starting point, with remaining input-recovery, shared-prediction, and passkey work. Current Blit includes browser speculative echo and a native terminal attach client, but its terminal transports are reliable/ordered. No packages have been installed into Quosh; no integration has been built.

| Candidate | Proposed role | Questions to verify |
|---|---|---|
| Quinn | Native Rust QUIC transport | Datagram and stream APIs, endpoint configuration, migration behavior, runtime/platform fit, relationship to chosen WebTransport stack |
| WebTransport | Preferred browser communications approach | Browser/platform coverage, server HTTP/3 support, certificate/origin deployment, datagrams and limits, reconnect behavior |
| `web_transport` | Candidate shared native/WASM transport facade mentioned in the chat | Exact crate/repository identity, Quinn integration, browser API coverage, API parity, maintenance and licensing |
| `termwiz` / WezTerm ecosystem | Terminal state/change representation, parsing/rendering building blocks | Whether Surface/change tracking is sufficient; which additional emulator component is required for arbitrary PTY output; state completeness, diff semantics, WASM fit |
| `vt100` | Simpler parser/screen candidate for an early comparison | Terminal feature coverage, state access, diff behavior, escape-sequence output versus structured deltas, Unicode and resizing |
| Blit | Potential reuse of server, session, and state-sync architecture | Locate the exact project; inspect architecture/crate boundaries; verify claimed transports, session persistence, cell diffs, compression, prediction status, and license; compare dependency/extraction/fork approaches |
| xterm.js | Browser terminal frontend | Supported public integration APIs, structured-state-to-VT rendering, snapshot restoration, prediction overlay/reconciliation, parser divergence, IME and accessibility |
| `portable-pty` / OS PTY APIs | Server process/terminal integration | Platform coverage, resize and exit handling, process cleanup and embedding model |
| Mosh | Behavioral and prediction reference | Published prediction/reconciliation approach, evaluation methodology, scope and license of any proposed code reuse |
| RoSE | Related QUIC terminal design mentioned in chat | Identify exact project; verify architecture, terminal integration, reconnection, and relevant lessons |
| Tere | Related reconnecting browser-terminal approach mentioned in chat | Identify exact project and investigate reconnect design and prediction/transport tradeoffs |

## Evaluation priorities

Investigate Blit's architecture first to assess how much sync/server work can be reused. Compare termwiz/WezTerm and vt100 against representative terminal behavior before choosing a canonical model. Evaluate Quinn alongside a WebTransport server/native/WASM path rather than treating raw QUIC as browser-compatible.

The conversation suggested translating state changes back into VT operations for xterm.js as a pragmatic starting point. Evaluate correctness and prediction reconciliation before accepting that adapter; avoid depending on private renderer/buffer internals without a clear maintenance plan.

The prior chat made specific feature and license claims about these projects. Consult the source-pinned findings in [the research report](08-blit-and-transports.md) for verified facts and corrections; remaining claims still need verification. There is no assumption that a screen surface is a full emulator, or that an emitted VT diff is already a loss-tolerant sync protocol.
