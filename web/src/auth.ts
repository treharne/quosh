// Authentication: the WebTransport auth handshake plus the WebAuthn ceremonies.
//
// The server sends a challenge; the browser answers with a passkey registration
// (`credentials.create`, enrolment) or assertion (`credentials.get`, reconnect).
// A live auth token is rotated silently and skips the ceremony.

import { MSG_AUTH_FAIL, MSG_AUTH_OK, MSG_CHALLENGE } from "./constants.js";
import { b64urlToBytes, bytesToB64url } from "./enroll.js";
import type { Control } from "./transport.js";
import { proto } from "./wasm.js";

/** The server wants a passkey and we have no ceremony to offer. */
export class NeedsPasskey extends Error {}

/** Protocol or credential rejection: reconnecting will not help. */
export class AuthRejected extends Error {}

export interface AuthResult {
  authToken: Uint8Array;
  uid: number;
  sessionId: Uint8Array;
  sessionToken: Uint8Array;
  hashes: string[];
  /** Any control bytes read past the auth frame; hand them to the client. */
  remaining: Uint8Array;
}

export interface TabTokens {
  sessionId?: Uint8Array;
  sessionToken?: Uint8Array;
}

export interface Ceremony {
  /** Build the framed ENROLL or ASSERT reply for a server challenge. */
  respond(challenge: Uint8Array, rpId: string): Promise<Uint8Array>;
}

const ZERO16 = new Uint8Array(16);
const ZERO32 = new Uint8Array(32);

async function nextFrame(
  fb: InstanceType<typeof proto.FrameBuffer>,
  recv: ReadableStreamDefaultReader<Uint8Array>,
): Promise<{ typ: number; payload: Uint8Array }> {
  for (;;) {
    const f = fb.take();
    if (f) return { typ: f.typ, payload: new Uint8Array(f.payload) };
    const { value, done } = await recv.read();
    if (done || !value) throw new Error("control stream closed during authentication");
    fb.push(value);
  }
}

export async function runAuth(
  control: Control,
  opts: {
    authToken?: Uint8Array;
    tab: TabTokens;
    cols: number;
    rows: number;
    /** `null` means "only a valid session token will do". */
    ceremony: Ceremony | null;
  },
): Promise<AuthResult> {
  const fb = new proto.FrameBuffer();
  control.send.write(
    proto.encodeAuthHello(
      opts.authToken ?? ZERO32,
      opts.tab.sessionId ?? ZERO16,
      opts.tab.sessionToken ?? ZERO32,
      opts.cols,
      opts.rows,
    ),
  );

  let frame = await nextFrame(fb, control.recv);
  if (frame.typ === MSG_CHALLENGE) {
    if (!opts.ceremony) throw new NeedsPasskey("this server requires a passkey");
    const ch = proto.decodeChallenge(frame.payload);
    control.send.write(await opts.ceremony.respond(new Uint8Array(ch.challenge), ch.rp_id));
    frame = await nextFrame(fb, control.recv);
  }
  if (frame.typ === MSG_AUTH_FAIL) throw new AuthRejected(proto.decodeAuthFail(frame.payload));
  if (frame.typ !== MSG_AUTH_OK) throw new Error(`unexpected frame ${frame.typ} during auth`);

  const ok = proto.decodeAuthOk(frame.payload);
  return {
    authToken: new Uint8Array(ok.auth_token),
    uid: ok.uid,
    sessionId: new Uint8Array(ok.session_id),
    sessionToken: new Uint8Array(ok.session_token),
    hashes: ok.hashes,
    remaining: fb.remaining(),
  };
}

export function enrollmentCeremony(
  nonce: Uint8Array,
  userName: string,
  displayName: string,
  onCredential: (credentialIdB64: string) => void,
): Ceremony {
  return {
    async respond(challenge, rpId) {
      const cred = (await navigator.credentials.create({
        publicKey: {
          rp: { id: rpId, name: "Quosh" },
          user: {
            id: new TextEncoder().encode(userName),
            name: userName,
            displayName,
          },
          challenge: new Uint8Array(challenge),
          pubKeyCredParams: [{ type: "public-key", alg: -7 }],
          authenticatorSelection: {
            residentKey: "required",
            userVerification: "required",
          },
          attestation: "none",
          timeout: 60_000,
        },
      })) as PublicKeyCredential | null;
      if (!cred) throw new Error("passkey registration was cancelled");
      onCredential(bytesToB64url(new Uint8Array(cred.rawId)));
      const r = cred.response as AuthenticatorAttestationResponse;
      return proto.encodeEnroll(
        nonce,
        new Uint8Array(r.clientDataJSON),
        new Uint8Array(r.attestationObject),
      );
    },
  };
}

export function assertionCeremony(credentialIdB64: string): Ceremony {
  const id = b64urlToBytes(credentialIdB64);
  return {
    async respond(challenge, rpId) {
      const cred = (await navigator.credentials.get({
        publicKey: {
          rpId,
          challenge: new Uint8Array(challenge),
          allowCredentials: [{ type: "public-key", id: new Uint8Array(id) }],
          userVerification: "required",
          timeout: 60_000,
        },
      })) as PublicKeyCredential | null;
      if (!cred) throw new Error("passkey prompt was cancelled");
      const r = cred.response as AuthenticatorAssertionResponse;
      return proto.encodeAssert(
        new Uint8Array(cred.rawId),
        new Uint8Array(r.authenticatorData),
        new Uint8Array(r.clientDataJSON),
        new Uint8Array(r.signature),
      );
    },
  };
}