# Quosh

**Quosh = “Mosh using QUIC”.**

Reconnectable remote shells over WebTransport, with Mosh-style prediction as a later slice. Not a Herdr clone.

License: [GNU GPLv3](LICENSE).

## Status

Implementation is underway. Slice 1 (the CLI session path) is complete, and slice 2 (adaptive local prediction with `--predict=adaptive|never`) is wired through the client, protocol, and server. The PWA is not built yet.

Canonical spec: [docs/11-v1-spec.md](docs/11-v1-spec.md).

## Reading order

1. [v1 specification](docs/11-v1-spec.md) — current product, protocol, and slice plan
2. [Confirmed decisions](docs/06-decisions.md)
3. [Goals and principles](docs/01-goals.md)
4. [Architecture](docs/02-architecture.md)
5. [Protocol concepts](docs/03-protocol.md)
6. [Plan](docs/05-plan.md)
7. [Local prediction](docs/12-prediction.md)
8. Historical research: [candidates](docs/04-candidates.md), [Mosh](docs/07-mosh-reference.md), [Blit](docs/08-blit-and-transports.md), [identity](docs/09-transport-and-server-identity.md), [reuse notes](docs/10-reuse-and-remaining-decisions.md)

## v1 in one page

- `quosh user@host` SSHs, creates a session on a **root** `quosh-server`, then speaks **WebTransport on UDP 443**.
- New connect = new login shell. That process reconnects to the same session; closing it or `exit` ends the session.
- Last-state-wins versioned screen updates (datagrams, reliable resync). No WebSocket.
- Unix user is the tenancy boundary.
- PWA origin will be `https://quosh.jtcs.dev`. Not built yet.
- Unmodified `blit-alacritty` for VT parsing; Quosh owns transport, sessions, and (later) prediction.

## Build

```bash
cargo build --release -p quosh-server -p quosh
```

Install the binaries to `/usr/local/bin`, install `systemd/quosh-server.service`, then `systemctl enable --now quosh-server`. Connecting does not start or sudo the daemon.
