# Mosh source reference

Local checkout: `reference/mosh` (gitignored). v1 behavior we copied is in [11-v1-spec.md](11-v1-spec.md).

Fetched on 2026-09-17 at Jesse's request. This is a reference checkout only; no build was run and no Mosh code was copied into Quosh.

- Official repository: https://github.com/mobile-shell/mosh
- Local checkout: `/Users/jt/projects/fun/quosh-reference-mosh` (adjacent to Quosh).
- Inspected commit: `decd9b705eb81626f694335b8d5940538beb06da`.
- Shallow clone: current history only; expand if historical investigation becomes useful.
- `COPYING` contains GPL version 3; individual source headers also describe licensing. Record applicable terms before any proposed reuse. Current use is source study.

## Findings from static source inspection

`src/frontend/terminaloverlay.h`, NotificationEngine, defines server-late after 6.5 seconds and reply-late after 10 seconds. `src/frontend/terminaloverlay.cc`, NotificationEngine::apply, draws a top-row banner reporting elapsed time since contact or acknowledgement, distinguishing an uplink-only problem when possible.

`src/frontend/stmclient.cc`, the main event loop, continues polling standard input; process_user_input appends ordinary user bytes to network state. There is no outage-banner check disabling that path. Therefore banner visibility does not itself mean typing is disabled or keystrokes are discarded. This is a code-reading result, not a live outage test.

## Implication for Quosh

After this correction was explained, Jesse explicitly chose Mosh's actual behavior: an elapsed-time outage banner with continued input acceptance and queueing. The earlier pause-input preference is superseded. Already-sent, unacknowledged input remains a separate recovery/deduplication problem.

## Useful areas for later study

- `src/frontend/stmclient.cc`: input, rendering, connection loop.
- `src/frontend/terminaloverlay.h` and `.cc`: notifications, prediction, reconciliation.
- `src/statesync/completeterminal.h` and `.cc`: terminal synchronization and echo acknowledgements.
- `src/statesync/`: synchronization machinery.

Consult this checkout during later design decisions. Jesse has not yet authorised Quosh implementation.
