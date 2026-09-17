# Confirmed design decisions

Confirmed by Jesse during the voice design discussion on 2026-09-17. These decisions refine the initial proposals; no implementation has been authorised.

## First version

Deliver a small CLI and browser demo that validates the shared core. Mobile remains a later target. A CLI-only milestone does not complete the first demo.

## Workload

Shell use is required. Editors are not required for the initial demo. Detailed terminal feature coverage and richer application support remain open.

## Session lifetime

Shell sessions survive client disconnection while the server continues running. Shell sessions do not survive server restarts. Retention limits and multi-client access policy remain undecided.

## Browser authentication and ergonomics

Use existing SSH access once to authorise browser passkey enrolment. Thereafter, the user opens Quosh and signs in using the passkey without another shell command or pairing link. Do not require importing an existing SSH private key into the web app.

Passkey registration persists across server restarts even though shell sessions do not. Access remains valid until revoked or the credential becomes unavailable; it is not an unconditional lifetime guarantee. SSH-assisted replacement enrolment provides a recovery direction. Exact enrolment, recovery, revocation, origin/domain, and reconnect authorisation mechanisms require design and verification. CLI authentication details remain open.

## Disconnection UX — confirmed

After reviewing the difference between his recollection and Mosh's source, Jesse explicitly chose Mosh's actual behavior: show an elapsed-time outage banner while continuing to accept and queue input for delivery when communication resumes. Do not disable input solely because the outage banner is visible. No separate draft-confirmation step is planned.

Banner timing, queue limits, prediction during extended outages, and recovery criteria still require detailed design. Already-sent, unacknowledged input must be reconciled/deduplicated; missing acknowledgement does not imply loss. Do not replay queued input into a replacement session after server restart without a separately defined policy.

## Reference source

At Jesse's request, cloned the official Mosh repository beside Quosh for design reference. See [Mosh reference findings](07-mosh-reference.md). This does not authorise Quosh implementation or adoption of Mosh code.

## Reuse direction and research

Jesse requested investigation of Blit, with a preference to use it where it already supplies the needed functionality, and a brief comparison of alternatives. The [source review](08-blit-and-transports.md) recommends reusing Blit's terminal driver, state/diff code, and browser renderer, initially evaluating the existing server/gateway as the integration baseline. It does not establish that unmodified Blit meets all requirements: input replay/deduplication, shared prediction, and passkey authentication need extensions. Specific dependency/fork boundaries await integration validation after implementation authorisation.

## Server deployment preference and transport clarification

Jesse does not want a domain or manually managed TLS certificates on the shell server. Encryption remains required; investigate automatically generated certificates and authenticated identity pinning. Browser app hosting/passkey origin and certificate rotation/discovery remain open. The unchanged Blit ordered stream protocol is a baseline only; final replaceable screen delivery requires sync/protocol and adapter changes. See [clarification](09-transport-and-server-identity.md).

## Transport fallback — confirmed

Jesse explicitly rejected retaining WebSocket. No WS transport or fallback is planned. The target is QUIC/WebTransport with reliable input/control and replaceable screen datagrams. Do not silently downgrade when UDP/QUIC connectivity fails. WebRTC is not selected for the initial build. See [current reuse boundary and remaining decisions](10-reuse-and-remaining-decisions.md).

## Hosting, reachability, control, platforms, and license — confirmed

- The browser client is a separately hosted static HTTPS PWA. Quosh has no project-operated application backend, account server, discovery service, or terminal relay. The browser connects directly to the user's own Quosh server; that server owns sessions, authentication verification, and durable enrolment state. Static website hosting serves application assets only.
- The PWA host's HTTPS certificate protects delivery of the application. The user's server has its own QUIC/TLS identity, generated and managed automatically; no user-provisioned domain/public certificate is required by the intended design. Static hosting alone does not solve server certificate trust or renewal.
- One inbound UDP listening port on the user's server is acceptable. Use WebTransport over HTTP/3 over QUIC; the configured port need not be 443. No WS fallback or central relay. Initial SSH-assisted enrolment uses the user's existing SSH access separately.
- One active controller per shell, with explicit takeover. The current controller owns input and terminal size; stale controller input must not be accepted after takeover. Viewer behavior and queued-input disposition on takeover remain details to specify.
- Initial required platforms: Ubuntu server, macOS CLI, Chrome browser. Aim for PWA use on iOS and Android, and design platform boundaries for eventual broad modern server/OS/browser support. This is a portability goal, not a claim that all browser capabilities work everywhere today.
- Project license: GNU GPLv3. Record this selection in design documentation; exact notices and dependency obligations will be handled before source distribution. Do not silently substitute a different license or version option.

## Passkey portability and takeover input — confirmed

Jesse wants normal passkey behavior: enrol a credential once, with subsequent access through the browser/password manager's usual passkey availability or synchronization. Do not impose artificial per-browser enrolment when the same enrolled credential is already available. A new independent credential still needs authorised enrolment. Credential synchronization does not itself synchronize Quosh server addresses, pinned identity, or certificate-refresh metadata; specify how those become available on a new device without assuming a project-operated backend.

Jesse agreed that explicit takeover invalidates the prior controller's remaining queued input. The old client must be told that its pending input was invalidated when it reconnects; it must not replay it into the newly controlled shell. Enforce the rule on the server using controller generations, not only by clearing a frontend queue. Input already accepted/written before the takeover boundary cannot be undone; resolve ambiguous acknowledgements against that boundary and discard unaccepted old-generation input.

## Next discussion

Main product choices are now recorded. Next specify backend-free server-identity bootstrap/refresh, passkey verification at the user server, controller handoff with queued input, and the remaining protocol details. Implementation remains paused.

## Implementation gate

Continue documenting design answers. Do not create implementation code, manifests, dependencies, or prototypes until Jesse explicitly says to start implementation.
