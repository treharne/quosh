# Quosh v1 specification

Authoritative product and protocol spec after the 2026-09-17 design grill.
Historical research notes remain in `07`–`10`; where they conflict, this document wins.

Implementation is authorised. Slice 1 is the CLI session path. Prediction and the PWA are later slices.

## Product

Quosh is a Mosh replacement: reconnectable predicted shells over QUIC/WebTransport.
It is not a Herdr replacement. Multi-writer shared workspaces are a later project that may *use* Quosh.

The first complete demo still needs CLI **and** Chrome. Build order:

1. CLI session path (this slice)
2. Mosh-parity prediction
3. PWA at `https://quosh.jtcs.dev`

## Decisions

| Topic | v1 | Explicitly later |
|---|---|---|
| PWA origin | `https://quosh.jtcs.dev` (static host, no app backend) | Server-hosted PWA |
| Out-of-band channel | SSH only | Long-lived identity without SSH |
| Browser enrol | `ssh user@host quosh enrol` prints an authenticated `https://quosh.jtcs.dev/...` link | — |
| CLI auth | SSH on every `quosh user@host`, like Mosh | Passkey CLI |
| Invocation | `quosh [--ssh=cmd] user@host` → login shell | Arbitrary remote command |
| Sessions | Connect without id creates a shell. Live process reconnects with that id. Close / `exit` / hangup ends it. New tab/process always creates. | Herdr-style reattach from a new client |
| Orphans | Unclean death lingers (Mosh). After 7 days detached, next create prompts to kill. | — |
| Control | Exclusive: one live WebTransport per session. Reconnect of the *same* token replaces the transport. No viewers, no takeover. | Multi-writer |
| Daemon | Root systemd, one WebTransport listener on **UDP 443** | User-level daemon |
| Tenancy | Unix user. SSH uid is source of truth (`SO_PEERCRED`). Passkeys bound at enrol to that uid. | — |
| Dead daemon | Hard error. Connect never sudoes or installs. | — |
| Transport | One WebTransport endpoint for CLI and browser. No raw-QUIC dialect. No WebSocket. | — |
| Screen sync | Last-state-wins. Versioned payloads. Prefer datagrams; reliable snapshot/resync when the frame does not fit. Stale versions dropped. | Ordered Blit diffs (rejected) |
| Blit | Unmodified crates only. `blit-alacritty` for PTY→`FrameState`. Do not use `feed_compressed` as the sync engine. If a crate needs a patch, rewrite that layer. | Blit gateway/server/UI |
| Prediction | Mosh-parity *aim* in slice 2. `--predict=adaptive\|never` disable switch. Not in slice 1. | — |
| Platforms | Ubuntu server, macOS CLI, Chrome | iOS/Android PWA |
| License | GNU GPLv3 | — |

## Process model

`quosh-server` is a root systemd service.

- Binds UDP `443` (WebTransport / HTTP/3).
- Listens on Unix socket `/run/quosh/quosh.sock` (world-connectable, daemon authenticates with `SO_PEERCRED`).
- Stores TLS material and durable enrol state under `/var/lib/quosh`.
- Shells and session metadata do not survive daemon restart. Passkey registrations do (slice 3).

`quosh user@host`:

1. Local binary SSHs to `user@host` (default `ssh`, override `--ssh=`).
2. Remote command: `/usr/local/bin/quosh create-session --cols N --rows M` (absolute path; non-interactive SSH `PATH` is unreliable).
3. That helper, running as `user`, asks the daemon over the Unix socket to create a session for **this uid**.
4. If the uid has sessions detached ≥ 7 days and stdin is a TTY, prompt `There's an idle connection. Kill it?` then create.
5. Helper prints one line to stdout: `QUOSH CONNECT <port> <sha256hex> <session_id> <token>`
6. Local process connects `https://<host>:443/quosh` with `serverCertificateHashes`.
7. On clean local exit, SIGINT, SIGHUP, or shell `exit`, send hangup and destroy the session.
8. On network loss, keep the process, queue input, show an elapsed-time outage banner, reconnect with the same session id and token.

If the Unix socket is missing or the daemon is down, the helper prints how to `systemctl enable --now quosh-server` and exits non-zero. It never sudoes.

### Privilege

The daemon binds 443 as root and `setuid`/`setgid`/`initgroups` to the peer uid before execing the login shell. The network never supplies a username.

## Wire protocol (slice 1)

WebTransport session to path `/quosh`.

- Client opens one bidirectional **control** stream.
- Screen updates: datagram when the compressed payload fits; otherwise a server-initiated unidirectional stream. Same payload type either way.
- Every screen payload has a monotonically increasing `version` (u64, starts at 1). The client applies a payload iff `version > current`. The client acks the highest applied version. The server diffs conceptually against the last ack but slice 1 may send a full `FrameState` every time (still last-state-wins).
- Input, resize, hangup, hello, acks, process-exit, errors: control stream, length-prefixed messages.

### Length-prefixed framing

`u32le length` of the following bytes, then `u8 type` and payload. Max frame 16 MiB.

| Type | Name | Direction | Body |
|---|---|---|---|
| 1 | Hello | C→S | `protocol` u16, `session_id` 16 bytes, `token` 32 bytes, `cols` u16, `rows` u16 |
| 2 | HelloOk | S→C | `protocol` u16, `session_id` 16 bytes, `version` u64, `cols` u16, `rows` u16 |
| 3 | Input | C→S | `seq` u64, `data` bytes |
| 4 | Resize | C→S | `cols` u16, `rows` u16 |
| 5 | Hangup | C→S | empty |
| 6 | AckState | C→S | `version` u64 |
| 7 | InputAck | S→C | `seq` u64 |
| 8 | Screen | S→C | see below (also datagrams / uni streams) |
| 9 | Exit | S→C | `status` i32le |
| 10 | Error | S→C | `u16le code`, `u16le len`, utf-8 |

Hello with a known `session_id`+`token` attaches (or replaces a dead/previous transport for that token). Unknown id is an error; sessions are created only on the Unix socket.

Input `seq` is strictly increasing from 1 per session. The server writes to the PTY in order and ignores `seq <= last_applied`. Reconnect resends unacked seqs. Do not replay after session destroy or server restart. `InputAck` means the session owner accepted the message into its write queue; it is not proof the PTY write completed.

### Screen payload

Uncompressed body, then LZ4 (size-prepended, as `lz4_flex::compress_prepend_size`):

```
magic      b"QS2\0"
version    u64le
echo_ack   u64le   (input seq whose effect this screen may be reconciled against)
rows       u16le
cols       u16le
cursor_row u16le
cursor_col u16le
mode       u16le
title_len  u16le
title      bytes
cells_len  u32le
cells      bytes   (rows*cols*12, blit-remote cell encoding)
ovf_count  u32le
ovf        (u32le index, u16le len, bytes) × ovf_count
```

Datagrams that fail to send for size are not fragmented; send the same payload on a uni stream.

### Unix socket (JSON line)

Request: `{"op":"create","cols":80,"rows":24,"kill_idle":false}`
or `{"op":"create","cols":80,"rows":24,"kill_idle":true}`
or `{"op":"ping"}`

Success create:

```json
{
  "ok": true,
  "session_id": "<hex 16 bytes>",
  "token": "<hex 32 bytes>",
  "port": 443,
  "cert_sha256": "<hex 32 bytes>",
  "idle": [{"id": "<hex>", "idle_secs": 700000}]
}
```

`kill_idle: true` destroys that uid's sessions detached ≥ 7 days before creating.

Ping success: `{"ok": true}`. Daemon down: helper cannot connect.

## Limits and liveness

- Terminal size is rejected unless `2 ≤ cols ≤ 512`, `2 ≤ rows ≤ 512`, and `cols * rows ≤ 500_000`.
- Each session owns a PTY pump, input writer, and child reaper for its lifetime. A WebTransport connection is a replaceable subscriber (epoch); a superseded handler must not apply input or hangup.
- Control `Ping`/`Pong` (types 11/12) once a second. The CLI outage banner is based on missing liveness, not missing screen updates.
- Screen payloads are versioned. Datagrams carry replaceable updates; unacked latest state is resent on the **control** stream (v1 does not use a separate unidirectional screen stream).
- PTY masters are opened with `O_CLOEXEC`. Session I/O lives in one owner task.

## TLS identity

Automatically generated ECDSA P-256 self-signed certificate, validity ≤ 14 days, stored under `/var/lib/quosh/tls/`. Clients pin SHA-256 of the DER certificate (WebTransport `serverCertificateHashes` rules). Slice 1 refreshes only by restart/regenerate; SSH prints the current hash every connect. Long-offline recovery without SSH is out of v1.

## Outage banner (CLI)

If no successful communication for 3 seconds, show an elapsed-time banner on the last screen row (Mosh-style). Keep accepting and queueing input. Do not require a confirm-to-send step. Clear the banner on resume.

`Ctrl-^ .` quits the client (Mosh's escape). The client/server `protocol` field in Hello/HelloOk must match; a mismatch is a fatal error, and an unknown session or bad token is fatal too rather than a reconnect loop.

## Slice 1 acceptance

- Root daemon running; `quosh user@host` gets a login shell as that user.
- Typing and output work over WebTransport.
- Network drop: process stays, banner, reconnect to the same shell.
- `exit` or killing the CLI cleanly ends the server-side shell.
- `kill -9` of the CLI leaves an orphan; a later create after 7 days can prompt to kill (prompt itself may be tested by lowering the threshold in tests).
- Dead daemon: clear error, no sudo.
- No prediction, no PWA, no passkeys in this slice.

## Prediction (slice 2)

Shared transport-free port of Mosh `PredictionEngine` in `quosh-predict`; full design in [12-prediction.md](12-prediction.md). CLI modes: `--predict=adaptive|never` (default adaptive). The echo acknowledgement travels inside each screen snapshot (`echo_ack`), so an ack is never reconciled against a screen that does not reflect it. `prediction-unicode.test` is covered by the differential oracle against unmodified Mosh.

Conservative prediction is not a confidentiality guarantee: the epoch gate prevents drawing unconfirmed input, including the common Enter-then-password case, but a confident epoch that starts silent input without an epoch-breaking byte can display a character until reconciliation. Use `--predict=never` when that matters.

## PWA (slice 3, not built now)

Static app at `https://quosh.jtcs.dev`, passkeys at RP `quosh.jtcs.dev`, same
WebTransport endpoint and protocol as the CLI. After one SSH-assisted
enrolment the browser reconnects without SSH: the passkey authenticates the
user and a pinned certificate chain authenticates the server (no user domain
or public CA). WebAuthn identity is `<unix_user>@<server_address>` so one
passkey manager can hold credentials for several servers. Full design:
[13-browser-auth-and-trust.md](13-browser-auth-and-trust.md).
