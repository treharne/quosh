# Shared-terminal screen metadata

Sheepdog has one authoritative terminal with independently sequenced client attachments. Each attachment receives its own `echo_ack`; foreign input and geometry changes can invalidate speculative overlays independently of that acknowledgment.

## Wire

`MSG_SHARED_SCREEN = 19` is an additive message alongside ordinary `MSG_SCREEN`. Its payload consists of:

- `QP1!` (four-byte magic; cannot collide with a valid QS2 decompression-size prefix);
- prediction epoch (`u64`, little-endian);
- prediction permitted (`u8`, exactly 0 or 1);
- an ordinary compressed QS2 screen, including version and echo acknowledgment.

Policy and generation travel in the same payload as the authoritative screen. They cannot be updated through a separate message that races streamed or datagram screen delivery. Frame/decompression bounds remain enforced. Ordinary QS2 and the existing CLI path are unchanged.

This message requires an adapter that explicitly supports shared screens; it is not a feature that an old client gains by merely agreeing to protocol version 2. Sheepdog's native Control Plane v1 selects such adapters. The ordinary Quosh daemon continues emitting ordinary screens.

## Client rules

The client applies last-state-wins to the screen before changing prediction context. An older datagram cannot change policy or restore an old generation. A newer reliable screen with a regressing generation is a fatal protocol error; malformed or regressing datagrams are ignored.

A changed generation or eligibility resets speculative overlays. A disallowed context suppresses new prediction, but keeps unacknowledged input for normal replay. Returning to an allowed context does not predictively replay those earlier keystrokes. The user's `predict=never` remains authoritative.

After entering shared-screen mode, ordinary reliable screens without metadata are rejected, and ordinary datagram screens are ignored. Metadata is retained across transport reconnect; a newly bound session uses a new client state machine.

## Current policy

Sheepdog conservatively permits prediction only with a single connected writer and no unsettled foreign input. Multiple connected writers remain supported, but prediction is suppressed in that case. The metadata mechanism also supports subsequent refinement of eligibility without a private fork of the predictor.

Tests cover foreign-context invalidation, retained unacknowledged bytes, reordered datagrams, generation regression, mixed-mode rejection, and the user's Never preference. Sheepdog additionally tests the real WebTransport-to-client path.
