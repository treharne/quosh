# Protocol and prediction concepts

All message names and delivery choices below are illustrative design proposals, not a wire specification.

## State and identity

- A logical session ID identifies the terminal; authentication/resume credentials authorise access separately.
- A session generation/connection epoch distinguishes obsolete traffic after resets or reattachment.
- Server state versions identify authoritative snapshots. Deltas name both their required base version and resulting version.
- Input sequence numbers identify ordered user input within a defined client/session scope.
- A client retains confirmed state separately from a speculative overlay and pending input.

Candidate message families: negotiate capabilities; create/attach/detach; input; resize; input acknowledgement; snapshot; delta; state acknowledgement/resync request; process exit; session error/expiry.

## Delivery sketch

| Traffic | Initial proposal |
|---|---|
| Authentication, attachment, capabilities, lifecycle | Reliable stream |
| Ordered input and resize/control | Reliable stream with application sequencing across reconnects |
| Snapshots and resynchronisation | Reliable transfer with atomic installation |
| Replaceable visual updates | Explore datagrams once recovery and dependency rules are established |

Do not assume a chain of incremental deltas survives datagram loss. A delta whose base is missing must not be applied. Possible strategies include diffing against a client-confirmed baseline, self-contained replaceable updates, or requesting a fresh snapshot. Compare them before selecting one.

Drop stale visual updates only when their state is superseded and no required dependency or nonvisual effect is lost. A screen snapshot does not automatically capture every terminal event: bells, clipboard requests, and other side effects need explicit handling and policy.

Specify datagram size limits, chunking/reassembly if needed, queue bounds, version wrap behavior, and recovery from missing/corrupt/unsupported state. Large snapshots should not rely on a single datagram.

## Input acknowledgements and reconciliation

Distinguish input received by the server, input written to the PTY, and effects observed in authoritative terminal output. None should silently stand in for the others. A generic PTY does not necessarily provide exact causal attribution between an input and an output frame.

The predictor consumes confirmed state, pending local input, and any justified confidence/mode metadata. Begin by investigating printable characters, backspace, and limited cursor movement; do not assume these are safe in every application. Completion, unknown modes, full-screen TUIs, IME composition, and password/no-echo input require conservative handling.

On a new authoritative update, update confirmed state and then confirm, revise, or discard predictions using explicitly defined evidence. Recompute remaining speculative display from the new base. Never feed predicted screen changes back into authoritative sync state. Unconfirmed display should be distinguishable where useful.

## Reconnection sketch

1. Re-establish transport and authenticate attachment to the logical session.
2. Exchange session generation, retained state version, and input acknowledgement/deduplication information.
3. Resume from a supported baseline or install a fresh authoritative snapshot.
4. Reconcile pending input and prediction before resuming normal display.

The protocol must prevent duplicate execution when a disconnect occurs after input reaches the PTY but before its acknowledgement arrives. Define server deduplication retention and behavior when that history is unavailable. Do not blindly replay ambiguous input after a server restart or session replacement. Confirmed: continue accepting and queueing disconnected typing while showing an elapsed-time outage banner, following Mosh. Queue bounds and detailed recovery behavior remain to be specified.

## Controller takeover

Confirmed: takeover invalidates the previous controller's remaining queued input. Bind input to a server-enforced controller generation and reject stale-generation submissions, including after reconnect. Notify the replaced client that its queue has been invalidated. Define the acceptance/handoff ordering so input already written before takeover is not falsely described as cancelled; unresolved acknowledgements need reconciliation against that boundary.

## Resize and compatibility

Dimensions and their ordering relative to input/state updates must be explicit. A resize can invalidate prediction and delta baselines. Define character-width/Unicode handling, alternate screen behavior, scrollback ownership, terminal modes, mouse input, paste, and supported control sequences before promising terminal compatibility.
