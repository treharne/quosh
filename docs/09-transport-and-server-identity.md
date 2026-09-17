# Transport boundary and server identity clarification

Recorded 2026-09-17 after Jesse questioned ordered Blit delivery and rejected requiring a domain/manual TLS setup on the shell server. Documentation only; implementation remains paused.

## Reuse boundary

The current Blit network protocol requires ordered reliable streams. It cannot be used unchanged for independently replaceable terminal screen updates. Reuse its parser, terminal model, rendering, and selected frame encoding, but extend/replace the sync envelope and network adapters for versioned datagram delivery. Reusing its existing QUIC/WebTransport library or connection-establishment code does not imply retaining the single ordered stream policy. [Blit transport contract](https://github.com/indent-com/blit/blob/7ec6085c54430f003fbafa7542613c46994c09c8/docs/transports.md).

Proposed traffic split:

- Reliable streams: input/control, authentication, full-snapshot recovery as needed. Input still needs application-level retention and duplicate suppression across connections.
- Replaceable datagrams: appropriately versioned screen updates with explicit baseline validation and recovery.
- No WebSocket fallback: Jesse subsequently rejected retaining WS.

An unchanged Blit server/gateway remains a useful baseline, not the final Mosh-style network path. A gateway-only wrapper cannot establish precise input acceptance at the PTY owner or simply discard dependent screen deltas. Keep final extension/component boundaries open until implementation validation.

## Confirmed deployment preference

Jesse does not want to obtain a domain or manually provision/manage TLS certificates for each shell server. Treat that as a deployment requirement. It does not mean disabling transport authentication or encryption.

QUIC v1 integrates TLS 1.3, but requiring TLS does not inherently require a public CA certificate or a DNS domain. A native client can use an automatically created self-signed server certificate verified against previously trusted server identity. [QUIC/TLS specification](https://www.rfc-editor.org/rfc/rfc9001.html).

Browser WebTransport exposes `serverCertificateHashes`: the client supplies a trusted hash and verifies the server certificate against it instead of ordinary public-PKI validation. This allows a direct connection to an IP-address endpoint using an automatically generated certificate. The fingerprint must be obtained through an authenticated bootstrap/discovery path, not trusted merely because the unknown endpoint supplies it. Browser constraints include short certificate validity (roughly two weeks), supported key algorithms, and target-browser compatibility. [WebTransport certificate-hash option](https://developer.mozilla.org/en-US/docs/Web/API/WebTransport/WebTransport#servercertificatehashes).

Blit's gateway already generates self-signed certificates and has rotation/hash distribution machinery. Its existing distribution goes through the gateway/config path, so it is not by itself a complete solution for an independently hosted PWA connecting to an IP-only server. [Certificate generation](https://github.com/indent-com/blit/blob/7ec6085c54430f003fbafa7542613c46994c09c8/crates/gateway/src/lib.rs#L1795), [certificate management](https://github.com/indent-com/blit/blob/7ec6085c54430f003fbafa7542613c46994c09c8/docs/transports.md#certificate-management).

## Browser app origin is separate from shell-server identity

The browser app must run in a secure context for WebTransport; browser passkeys are scoped to a relying-party domain associated with the app origin. A bare remote HTTP page at the shell server's IP is not a complete solution for this UX. [WebTransport secure context](https://developer.mozilla.org/en-US/docs/Web/API/WebTransport), [passkey relying-party ID](https://developer.mozilla.org/en-US/docs/Web/API/PublicKeyCredentialCreationOptions#rp).

A candidate deployment is a separately hosted HTTPS PWA connecting directly to the user's IP-addressed shell server over pinned WebTransport. That requires no domain/public certificate management on the shell server. The app host supplies a stable origin; it need not relay terminal traffic or hold private authentication keys, but its delivered frontend code remains part of the trust model. Local/native packaging is another topology to evaluate, with platform-specific passkey constraints.

Jesse has now selected the static HTTPS PWA topology, with no project-operated application backend, discovery service, or relay. The user server must perform authentication verification directly. Certificate rotation needs authenticated fingerprint refresh even after a client has been offline beyond certificate expiry; a one-time pinned leaf hash alone will not provide indefinite access. Specify a durable server identity/bootstrap mechanism and refresh flow without a central backend before promising fully automatic rotation. Historical consideration, now superseded by the decision to omit WS: WebSocket fallback also has separate certificate/proxy requirements: it does not expose WebTransport's certificate-hash override. Do not promise direct WSS fallback to an IP-only self-signed endpoint without solving that.

## Open implementation questions

- Static HTTPS PWA hosting is confirmed; specify its stable passkey relying-party identity and verification at the user server.
- Trusted initial certificate fingerprint exchange through the agreed SSH bootstrap.
- Authenticated publication/refresh of rotating fingerprints, including long-offline recovery.
- Direct IP/port reachability and target-browser pinning support.
- WebSocket fallback is resolved: omit it. Handle unavailable QUIC connectivity explicitly.
