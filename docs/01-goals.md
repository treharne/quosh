# Goals, non-goals, and principles

## Goals

- Make ordinary remote typing feel immediate through conservative local prediction and authoritative reconciliation.
- Preserve logical terminal sessions across connection loss, sleep, and network changes; reattach to current state.
- Synchronise terminal state so obsolete visual updates need not delay useful current state.
- Share transport, sync, and prediction components across an ssh-like in-terminal client, a browser terminal, and an embedded mobile terminal.
- Prefer QUIC/WebTransport communications without coupling terminal semantics to one transport.
- Reuse terminal parsing, PTY, transport, and rendering work where the interfaces and licenses fit.

Mosh-quality ergonomics are a goal to measure, not a promised outcome. QUIC alone does not provide prediction or persistent application sessions.

## Confirmed first-demo scope

A small CLI and browser demo must demonstrate the shared core. Shell use is required; editors are not required. See [v1 spec](11-v1-spec.md).

Session rule (Mosh, not “shells live after the client is gone”): the **live client process** reconnects to its session across network loss. Closing the client, hanging up, or `exit` ends the session. Unclean death may orphan a shell until the 7-day idle prompt. Sessions do not survive server restart.

## Initial non-goals

- SSH or Mosh wire compatibility, and the full SSH feature set such as forwarding and file transfer.
- A new terminal emulator, cryptographic protocol, or browser renderer without evidence that reuse is inadequate.
- Predicting arbitrary application output, shell completion, or command execution.
- Guaranteeing session survival across server crashes or machine reboots in the first version.
- Shipping all three clients simultaneously, or committing to a mobile framework now.
- A general remote-desktop protocol. Reusable transport/session components are a possible consequence, not the initial scope.

## Design principles

1. Transport moves messages; it does not interpret terminal cells or predict input.
2. Synchronisation defines authoritative state and convergence independently of rendering and connection type.
3. Prediction is optional, local, reversible, and never authoritative. Turning it off must preserve correctness.
4. A session outlives **transport** loss for one live client; it does not outlive that client process (except unclean-death orphans).
5. Reuse unmodified third-party crates only. If a component needs a patch, rewrite it.
6. Prefer current useful state over obsolete visual work, without dropping required delta dependencies or terminal side effects.
7. Specify bounded buffers, recovery, and failure behavior alongside the happy path.
8. Keep shared core logic separate from OS, browser, and frontend integrations.
