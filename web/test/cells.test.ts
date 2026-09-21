import assert from "node:assert/strict";
import { test } from "node:test";

import {
  CELL_SIZE,
  decodeCell,
  decodeFrame,
  effectiveColors,
  palette256,
} from "../src/cells.ts";

test("decodes the cell byte layout", () => {
  const cell = new Uint8Array(CELL_SIZE);
  cell[0] = (1 << 0) | (2 << 2) | (1 << 4); // fg indexed, bg rgb, bold
  cell[1] = 1 << 3; // content_len 1
  cell[2] = 1; // fg index
  cell[5] = 1; // bg r
  cell[6] = 2; // bg g
  cell[7] = 3; // bg b
  cell[8] = "A".charCodeAt(0);

  const decoded = decodeCell(cell, 0);
  assert.equal(decoded.glyph, "A");
  assert.deepEqual(decoded.fg, { kind: "indexed", index: 1 });
  assert.deepEqual(decoded.bg, { kind: "rgb", r: 1, g: 2, b: 3 });
  assert.equal(decoded.bold, true);
  assert.equal(decoded.underline, false);
  assert.equal(decoded.wide, false);
  assert.equal(decoded.overflow, false);
});

test("a wide continuation cell has no glyph", () => {
  const cell = new Uint8Array(CELL_SIZE);
  cell[0] = 3; // multiline? just a byte
  cell[1] = (1 << 3) | 4; // content_len 1 + wide continuation
  cell[8] = "x".charCodeAt(0);
  const decoded = decodeCell(cell, 0);
  assert.equal(decoded.wide, true);
  assert.equal(decoded.glyph, " ");
});

test("overflow cells are flagged for the host to resolve", () => {
  const cell = new Uint8Array(CELL_SIZE);
  cell[1] = 7 << 3;
  const decoded = decodeCell(cell, 0);
  assert.equal(decoded.overflow, true);
  assert.equal(decoded.glyph, " ");
});

test("decodeFrame resolves overflow through the callback", () => {
  const cells = new Uint8Array(2 * CELL_SIZE);
  cells[1] = 7 << 3; // cell 0 overflow
  cells[CELL_SIZE + 1] = 1 << 3;
  cells[CELL_SIZE + 8] = "B".charCodeAt(0);
  const frame = decodeFrame(1, 2, cells, (_r, c) => (c === 0 ? "👍" : ""));
  assert.equal(frame[0][0].glyph, "👍");
  assert.equal(frame[0][1].glyph, "B");
});

test("the palette is a full xterm 256", () => {
  const p = palette256();
  assert.equal(p.length, 256);
  assert.equal(p[16], "rgb(0,0,0)");
  assert.equal(p[231], "rgb(255,255,255)");
  assert.equal(p[232], "rgb(8,8,8)");
});

test("inverse swaps foreground and background", () => {
  const cell = new Uint8Array(CELL_SIZE);
  cell[0] = (1 << 0) | (1 << 2); // fg indexed 0, bg indexed 1
  cell[2] = 0;
  cell[5] = 1;
  cell[1] = 0;
  const decoded = decodeCell(cell, 0);
  const normal = effectiveColors(decoded);
  decoded.inverse = true;
  const inverted = effectiveColors(decoded);
  assert.equal(inverted.fg, normal.bg);
  assert.equal(inverted.bg, normal.fg);
});