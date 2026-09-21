import assert from "node:assert/strict";
import { test } from "node:test";

import { encodeBarKey, encodeKey, encodePaste, type Modifiers } from "../src/input.ts";

const none: Modifiers = { ctrl: false, alt: false };

test("named keys map to their escape sequences", () => {
  assert.deepEqual([...encodeKey("Enter", false, none)!], [0x0d]);
  assert.deepEqual([...encodeKey("Backspace", false, none)!], [0x7f]);
  assert.deepEqual([...encodeKey("ArrowUp", false, none)!], [0x1b, 0x5b, 0x41]);
  assert.deepEqual([...encodeKey("Escape", false, none)!], [0x1b]);
});

test("modifier-only keys are ignored", () => {
  assert.equal(encodeKey("Shift", false, none), null);
  assert.equal(encodeKey("Control", false, none), null);
});

test("ctrl maps letters and symbols", () => {
  assert.deepEqual([...encodeKey("c", false, { ctrl: true, alt: false })!], [0x03]);
  assert.deepEqual([...encodeKey(" ", false, { ctrl: true, alt: false })!], [0x00]);
  assert.deepEqual([...encodeKey("[", false, { ctrl: true, alt: false })!], [0x1b]);
});

test("alt prefixes with ESC", () => {
  assert.deepEqual([...encodeKey("x", false, { ctrl: false, alt: true })!], [0x1b, 0x78]);
});

test("printable keys are UTF-8", () => {
  assert.deepEqual([...encodeKey("|", false, none)!], [0x7c]);
  assert.deepEqual([...encodeKey("é", false, none)!], [0xc3, 0xa9]);
});

test("the key bar shares the same encoding", () => {
  assert.deepEqual([...encodeBarKey("|", none)!], [0x7c]);
  assert.deepEqual([...encodeBarKey("ArrowLeft", { ctrl: true, alt: false })!], [0x1b, 0x5b, 0x44]);
});

test("bracketed paste wraps, plain paste does not", () => {
  const plain = [...encodePaste("hi", false)];
  assert.deepEqual(plain, [0x68, 0x69]);
  const wrapped = [...encodePaste("hi", true)];
  assert.deepEqual(wrapped, [
    0x1b, 0x5b, 0x32, 0x30, 0x30, 0x7e, // ESC [ 200 ~
    0x68, 0x69,
    0x1b, 0x5b, 0x32, 0x30, 0x31, 0x7e, // ESC [ 201 ~
  ]);
});