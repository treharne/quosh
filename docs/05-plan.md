# Plan

Canonical scope: [v1 spec](11-v1-spec.md). Implementation is authorised.

## Slice 1 — CLI session path (current)

Root `quosh-server` on UDP 443 + Unix socket. `quosh user@host` creates a login shell as that Unix user, speaks WebTransport, hangs up on clean exit, reconnects on network loss, Mosh-style outage banner. No prediction, no PWA.

## Slice 2 — prediction

Adaptive-only local prediction via the shared `quosh-predict` crate. Design and status: [12-prediction.md](12-prediction.md). `PredictionEngine` in the Mosh tree is the behavioural spec; `prediction-unicode.test` is covered by the differential oracle, not a vim gate.

## Slice 3 — PWA

Static app at `https://quosh.jtcs.dev`. SSH `quosh enrol` prints the authenticated link. Passkeys bound to the enrol uid. Same WebTransport endpoint. Unmodified blit-browser only if it can consume our frames without patches.

## Later

Arbitrary remote commands; passkey CLI; system-hosted PWA; longer-lived identity without SSH; Herdr-on-Quosh; iOS/Android.

## Evidence

Same workloads with prediction on and off. Measure typing latency, correction rate, reconnect, bandwidth. High RTT, loss, reordered datagrams, resize, echo-off.
