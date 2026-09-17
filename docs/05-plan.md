# Open questions and initial research/build plan

## Confirmed scope

See [decision record](06-decisions.md): first demonstrate the shared core with a small CLI and browser shell demo; editors are outside the first-demo requirement. Shells survive disconnects but not server restarts. One-time SSH-assisted passkey enrolment enables subsequent independent browser sign-in; registration survives server restarts. Implementation remains unauthorised.

## Deployment and platform decisions

Static HTTPS PWA only; no project-operated backend or relay. Browser traffic connects directly to the user's server. One inbound UDP port is accepted, no WS fallback. Ubuntu server, macOS CLI, and Chrome are initial targets; iOS/Android PWA usability and broad modern platform compatibility are design goals. One active controller with explicit takeover. GNU GPLv3. See [decision record](06-decisions.md).

## Open questions

- Validate the agreed component reuse boundary: direct Blit dependencies versus a pinned fork/selective extraction. Do not treat the unchanged Blit server/gateway as the final protocol.
- Validate Blit screen state/encoding for Quosh snapshots and prediction; identify any missing metadata.
- Common WebTransport endpoint for every client, or separate native QUIC/browser adapters? No WebSocket fallback (confirmed).
- What precisely does input acknowledgement mean, and how can prediction be reconciled without claiming unavailable application-level causality?
- Which state fields and terminal features are in scope, including scrollback, alternate screen, Unicode widths, images, hyperlinks, mouse, and clipboard actions?
- Given the confirmed SSH-assisted passkey enrolment flow, how are browser origin, enrolment expiry, credential revocation, and resume permissions handled? What are the detailed CLI authentication mechanics?
- Confirmed: one active controller with explicit takeover; the controller owns dimensions. Takeover invalidates old-controller queued input and notifies that client on reconnect. Specify atomic handoff and stale-generation rejection, distinguishing already accepted input.
- What are session retention and resource limits? Shell lifetime across disconnect/server restart is settled; how is durable authentication registration stored and recovered?
- Confirmed: follow Mosh with an elapsed-time outage banner and continued input queueing. Specify queue limits, banner thresholds, and ambiguous unacknowledged input handling on reconnect. See [reference findings](07-mosh-reference.md).
- What prediction rules are safe enough for shell use and password prompts? Which richer interfaces should be evaluated later, given that editors are not required for the first demo?
- What latency, correction-rate, bandwidth, and recovery targets define success relative to a no-prediction baseline and Mosh?
- Confirmed project license: GNU GPLv3. Record selected component licenses and notices before distribution.

## Gate 0 — current deliverable

Create the project directory and these Markdown notes only. Stop here. Jesse must explicitly authorise implementation before any code, manifest, dependency installation, prototype, or build work begins. The following phases are a proposed future plan, not work underway.

## Phase 1 — focused research and decisions

Initial source investigation is recorded in [Blit reuse and transport research](08-blit-and-transports.md). It recommends Blit reuse with explicit gaps, compares alternatives, and documents port requirements. Runtime validation remains pending.

Verify the candidates using primary documentation/source; identify the exact Blit and related projects. Record capabilities, licenses, versions, missing pieces, and native/WASM constraints. Compare terminal representations and browser rendering integration. Produce short architecture decisions for terminal model, transport endpoint, sync baseline strategy, and authentication/bootstrap. Any executable spike waits for Jesse's implementation authorisation.

## Phase 2 — minimal authoritative terminal path (after authorisation)

The first-demo acceptance target includes both CLI and browser clients sharing the core. A CLI-only milestone is intermediate, not completion of the demo. Build one server PTY session and one CLI client using reliable messages, full snapshots, and no prediction. Validate terminal state/render fidelity, resize, process exit, detach, and fresh-snapshot reattach. Establish an observable correctness baseline before optimizing delivery.

## Phase 3 — synchronization and recovery

Introduce versioned deltas, input sequencing/deduplication, explicit acknowledgement semantics, and bounded recovery. Exercise dropped connections and stale/missing bases. Add replaceable datagram updates only after recovery rules work; compare against reliable delivery under delay, loss, and heavy output.

## Phase 4 — independent prediction

Add conservative prediction with a disable switch and clear reconciliation. Measure apparent typing latency and correction behavior. Exercise ordinary shell typing, cursor edits, paste, echo-off prompts, resize, disconnect, and reconnect; editor/TUI support is not a first-demo acceptance requirement; ensure disabling prediction leaves identical authoritative results.

## Phase 5 — browser client

Integrate WebTransport and the shared core where practical, with the reused Blit WASM/WebGL renderer and Quosh state/prediction adapter. Validate certificates/deployment, rendering parity, input/IME, suspension, and resume. Confirm that prediction and sync do not require frontend-specific forks.

## Phase 6 — mobile and broader robustness

Choose native bindings or embedded web integration based on the earlier findings. Evaluate keyboard/IME/touch UX and background lifecycle. Broaden terminal compatibility and authentication/session operations based on real usage.

## Evidence to collect

Use the same terminal workloads with prediction enabled and disabled. Measure input-to-display latency, prediction correction frequency, time to authoritative convergence, reconnect recovery, bandwidth, and memory/queue growth. Include high RTT, jitter, loss, reordered/duplicated datagrams, network changes, sustained output, resize, and ambiguous input delivery. Set acceptance thresholds during research rather than inventing performance guarantees now.
