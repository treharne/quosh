# Quosh

**Quosh = “Mosh using QUIC”.**

Status: design notes only. No implementation has started. Wait for Jesse to explicitly say to start implementation before creating code, a Cargo workspace, dependencies, or prototypes.

Quosh explores responsive, reconnectable remote terminals with a shared core for an ssh-like CLI, a browser terminal, and a mobile app. Its central idea is that communications and Mosh-style client prediction are orthogonal; terminal-state synchronisation is a separate concern between them.

## Reading order

1. [Goals and principles](docs/01-goals.md)
2. [Architecture, clients, and proposed workspace](docs/02-architecture.md)
3. [Protocol and prediction concepts](docs/03-protocol.md)
4. [Candidate libraries and projects](docs/04-candidates.md)
5. [Open questions and research/build plan](docs/05-plan.md)
6. [Confirmed decisions](docs/06-decisions.md)
7. [Mosh source reference and findings](docs/07-mosh-reference.md)
8. [Blit reuse, transports, ports, and alternatives](docs/08-blit-and-transports.md)
9. [Transport reuse and server identity clarification](docs/09-transport-and-server-identity.md)
10. [Current reuse boundary and remaining decisions](docs/10-reuse-and-remaining-decisions.md)

## Confirmed decisions

Jesse confirmed the initial scope and browser authentication flow during the follow-up design discussion on 2026-09-17. See [decision record](docs/06-decisions.md). The first demo covers CLI and browser shell use, with no editor requirement; shells survive disconnects but not server restarts. Browser access uses one-time SSH-assisted passkey enrolment followed by independent passkey sign-in.

Deployment: static HTTPS PWA → direct connection to each user's server; no project-operated application backend or relay. Initial targets: Ubuntu server, macOS CLI, Chrome. Future goal: broad modern platforms and iOS/Android PWAs. One UDP port; no WebSocket fallback. One active controller with explicit takeover. License selection: GNU GPLv3.

## Provenance and decision status

These notes capture Jesse's ideas and the proposals in the ChatGPT conversation “Browser Mosh Alternatives” (conversation ID `6aabdbe2-a65c-83ec-9d5e-13b3faf1db99`), reviewed on 2026-09-17. They also spell out design questions needed to evaluate those proposals.

Firm direction: separate transport, synchronisation, prediction, and server/runtime; target CLI/browser/mobile; prefer QUIC/WebTransport while keeping transport pluggable; reuse existing components where appropriate.

Crate boundaries, wire formats, dependencies, detailed authentication mechanics, terminal compatibility beyond the confirmed shell scope, and implementation order remain proposals. The confirmed decisions above supersede the original open alternatives. The initial prior-chat claims were unverified. The [Blit and transport research](docs/08-blit-and-transports.md) now records source-verified findings, corrections, and remaining validation needs. No runtime integration or performance claims have been verified.
