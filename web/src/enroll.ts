// Enrolment-link handling. Pure: no DOM, no wasm, so it is unit-tested in Node.

/** The payload carried in `<base-url>/e#enroll=<payload>`. */
export interface EnrollPayload {
  v: number;
  host: string;
  port: number;
  /** Hex SHA-256 of the currently valid server certificate. */
  hash: string;
  /** Hex one-time nonce bound to the SSH-authenticated uid. */
  nonce: string;
  /** Unix user name; the WebAuthn display name is `user@host`. */
  user: string;
}

/** Enrolment payload version this client understands. */
export const ENROLL_VERSION = 1;

export function bytesToB64url(bytes: Uint8Array): string {
  let bin = "";
  for (const b of bytes) bin += String.fromCharCode(b);
  return btoa(bin).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}

export function b64urlToBytes(s: string): Uint8Array {
  const b64 = s.replace(/-/g, "+").replace(/_/g, "/");
  const pad = b64.length % 4 === 0 ? "" : "=".repeat(4 - (b64.length % 4));
  const bin = atob(b64 + pad);
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
  return out;
}

export function hexToBytes(hex: string): Uint8Array {
  if (hex.length % 2 !== 0) throw new Error("odd-length hex");
  const out = new Uint8Array(hex.length / 2);
  for (let i = 0; i < out.length; i++) {
    const v = Number.parseInt(hex.slice(i * 2, i * 2 + 2), 16);
    if (Number.isNaN(v)) throw new Error("bad hex");
    out[i] = v;
  }
  return out;
}

export function bytesToHex(bytes: Uint8Array): string {
  let s = "";
  for (const b of bytes) s += b.toString(16).padStart(2, "0");
  return s;
}

const encoder = new TextEncoder();
const decoder = new TextDecoder();

/** Encode a payload into the fragment value (without the `#enroll=` prefix). */
export function encodeEnrollPayload(payload: EnrollPayload): string {
  return bytesToB64url(encoder.encode(JSON.stringify(payload)));
}

/**
 * Parse a location hash such as `#enroll=eyJ...`. Returns `null` when there is
 * no enrolment fragment, and throws when one is present but malformed.
 */
export function parseEnrollFragment(hash: string): EnrollPayload | null {
  const m = /(?:^|[#&])enroll=([^&]+)/.exec(hash);
  if (!m) return null;
  let parsed: unknown;
  try {
    parsed = JSON.parse(decoder.decode(b64urlToBytes(m[1])));
  } catch {
    throw new Error("enrolment link is corrupt");
  }
  return validate(parsed);
}

/** The URL an operator would print; `quosh enroll` emits exactly this. */
export function buildEnrollUrl(baseUrl: string, payload: EnrollPayload): string {
  return `${baseUrl.replace(/\/+$/, "")}/e#enroll=${encodeEnrollPayload(payload)}`;
}

function validate(p: unknown): EnrollPayload {
  if (typeof p !== "object" || p === null) throw new Error("bad enrol payload");
  const o = p as Record<string, unknown>;
  const str = (k: string): string => {
    const v = o[k];
    if (typeof v !== "string" || v.length === 0) throw new Error(`bad enrol ${k}`);
    return v;
  };
  const version = o.v;
  if (typeof version !== "number") throw new Error("bad enrol version");
  if (version !== ENROLL_VERSION) {
    throw new Error(`enrolment link is version ${version}, this client needs ${ENROLL_VERSION}`);
  }
  const port = o.port;
  if (typeof port !== "number" || !Number.isInteger(port) || port < 1 || port > 65535) {
    throw new Error("bad enrol port");
  }
  const hash = str("hash");
  if (!/^[0-9a-f]{64}$/.test(hash)) throw new Error("bad enrol hash");
  const nonce = str("nonce");
  if (!/^[0-9a-f]{32}$/.test(nonce)) throw new Error("bad enrol nonce");
  return { v: version, host: str("host"), port, hash, nonce, user: str("user") };
}

/** Stable key for a server entry. */
export function serverKey(host: string, port: number): string {
  return `${host}:${port}`;
}