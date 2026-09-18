# Confirmed design decisions

Voice discussion plus the 2026-09-17 grill. The concrete spec is [11-v1-spec.md](11-v1-spec.md). That document wins if anything here is shorter.

Implementation is authorised. Slice 1 is CLI only; the first *demo* is not complete until prediction and the PWA exist.

## Product

- Quosh reimplements Mosh with WebTransport, not Herdr. Multi-writer Google-doc terminals are a later project.
- First complete demo: CLI + Chrome PWA sharing the core. Mobile later. Editors are not a demo gate.
- Build order: CLI session path → Mosh-parity prediction → PWA.

## Session model

- Connect without a session id creates a new login shell.
- The live client process holds the id and reconnects automatically (network loss, sleep).
- Intentionally closing the CLI, hanging up, or `exit` ends that session.
- A new process or tab always creates, even if orphans exist.
- Unclean client death leaves an orphan (Mosh). After **7 days** detached, the next create prompts `There's an idle connection. Kill it?` It does not auto-resume and does not auto-kill.
- No viewers. No takeover. Reconnect of the same session token replaces the transport.

## Bootstrap and identity

- PWA origin: `https://quosh.jtcs.dev` (static, no application backend). Server-hosted PWA is later.
- SSH is the only v1 out-of-band channel. `quosh enrol` (slice 3) prints an authenticated link with token and server address.
- CLI: SSH every launch, like Mosh. Passkey CLI is later.
- Invocation: `quosh [--ssh=cmd] user@host`. Login shell only. No arbitrary remote command in v1.
- Server TLS: auto-generated ECDSA P-256, ≤14 days, pin SHA-256. No user domain on the shell server. Hash refresh is SSH (every CLI connect prints it).
- Dead daemon: hard visible error. Connect never sudoes or installs.

## Daemon and tenancy

- Root systemd daemon, **UDP 443**, one WebTransport endpoint for CLI and browser (middlebox argument).
- Long-term this also enables SSH-like “any Unix user”. v1 already uses that model: peer uid from the Unix socket, `setuid` to that user, passkeys (later) bound at enrol to that uid.
- The daemon never takes a username from the network.

## Transport and Blit

- No WebSocket, no raw-QUIC second personality, no relay, no NAT traversal.
- Screen: last-state-wins versioned datagrams; reliable control and resync. Blit’s ordered `feed_compressed` path is not the sync engine.
- Reuse unmodified `blit-alacritty` (and `blit-remote` types as needed). If a component requires a patch, rewrite that layer instead of forking Blit.
- Do not wrap blit-gateway/server.

## Disconnection UX

Mosh-style elapsed-time outage banner; keep queueing input. No confirm-to-send. Reconnect of the same session; do not replay into a new session after restart.

## Platforms and license

Ubuntu server, macOS CLI, Chrome. GNU GPLv3.

## Explicitly not v1

Server-hosted PWA; passkey CLI; longer-lived identity without SSH; Herdr mux; viewers/takeover; arbitrary remote commands; user-level daemon; WebSocket; modifying Blit.
