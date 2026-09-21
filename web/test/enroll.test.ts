import assert from "node:assert/strict";
import { test } from "node:test";

import {
  b64urlToBytes,
  bytesToB64url,
  bytesToHex,
  encodeEnrollPayload,
  hexToBytes,
  parseEnrollFragment,
  serverKey,
  type EnrollPayload,
} from "../src/enroll.ts";

const HASH = "ab".repeat(32);
const NONCE = "cd".repeat(16);

const payload: EnrollPayload = {
  v: 1,
  host: "203.0.113.5",
  port: 443,
  hash: HASH,
  nonce: NONCE,
  user: "ubuntu",
};

test("enrol payload round trips through the fragment", () => {
  const fragment = `#enroll=${encodeEnrollPayload(payload)}`;
  assert.deepEqual(parseEnrollFragment(fragment), payload);
});

test("no fragment returns null", () => {
  assert.equal(parseEnrollFragment(""), null);
  assert.equal(parseEnrollFragment("#something-else"), null);
});

test("rejects a future payload version", () => {
  const future = { ...payload, v: 2 };
  assert.throws(() => parseEnrollFragment(`#enroll=${encodeEnrollPayload(future)}`), /version/);
});

test("rejects a bad hash or nonce", () => {
  assert.throws(() => parseEnrollFragment(`#enroll=${encodeEnrollPayload({ ...payload, hash: "zz" })}`));
  assert.throws(() => parseEnrollFragment(`#enroll=${encodeEnrollPayload({ ...payload, nonce: "00" })}`));
});

test("base64url has no padding and handles arbitrary bytes", () => {
  const bytes = Uint8Array.from([0, 1, 2, 250, 251, 252, 253, 254, 255]);
  const encoded = bytesToB64url(bytes);
  assert.ok(!encoded.includes("="), "no padding");
  assert.deepEqual([...b64urlToBytes(encoded)], [...bytes]);
});

test("hex helpers round trip and reject odd input", () => {
  assert.equal(bytesToHex(hexToBytes("00ff10")), "00ff10");
  assert.throws(() => hexToBytes("abc"));
});

test("server keys are stable", () => {
  assert.equal(serverKey("h", 443), "h:443");
});