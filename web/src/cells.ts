// Decoding of the shared `blit-remote` cell format (12 bytes per cell).
// Pure, so the layout and palette are unit-tested in Node.
//
// Byte layout (see blit-remote `cell_style` / `cell_content`):
//   0: fg_type (bits 0-1), bg_type (bits 2-3), bold(4), dim(5), italic(6), underline(7)
//   1: inverse(0), wide-continuation(2), content_len(bits 3-5),
//      link(6). content_len == 7 means the text is in the overflow table.
//   2-4: fg (indexed uses 2; rgb uses 2,3,4)
//   5-7: bg (indexed uses 5; rgb uses 5,6,7)
//   8-11: UTF-8 content (up to 4 bytes)

export const CELL_SIZE = 12;
const CONTENT_OVERFLOW = 7;

export type Color =
  | { kind: "default" }
  | { kind: "indexed"; index: number }
  | { kind: "rgb"; r: number; g: number; b: number };

export interface Cell {
  glyph: string;
  fg: Color;
  bg: Color;
  bold: boolean;
  dim: boolean;
  italic: boolean;
  underline: boolean;
  inverse: boolean;
  /** Second half of a wide character; draw the background only. */
  wide: boolean;
  /** Glyph lives in the overflow table; resolve with `cellContent`. */
  overflow: boolean;
  link: boolean;
}

function color(type: number, a: number, b: number, c: number): Color {
  if (type === 1) return { kind: "indexed", index: a };
  if (type === 2) return { kind: "rgb", r: a, g: b, b: c };
  return { kind: "default" };
}

const decoder = new TextDecoder();

export function decodeCell(bytes: Uint8Array, offset: number): Cell {
  const f0 = bytes[offset]!;
  const f1 = bytes[offset + 1]!;
  const len = (f1 >> 3) & 7;
  const overflow = len === CONTENT_OVERFLOW;
  let glyph = " ";
  if (!overflow && len > 0 && (f1 & 4) === 0) {
    glyph = decoder.decode(bytes.subarray(offset + 8, offset + 8 + len));
  }
  return {
    glyph,
    fg: color(f0 & 3, bytes[offset + 2]!, bytes[offset + 3]!, bytes[offset + 4]!),
    bg: color((f0 >> 2) & 3, bytes[offset + 5]!, bytes[offset + 6]!, bytes[offset + 7]!),
    bold: (f0 & 16) !== 0,
    dim: (f0 & 32) !== 0,
    italic: (f0 & 64) !== 0,
    underline: (f0 & 128) !== 0,
    inverse: (f1 & 1) !== 0,
    wide: (f1 & 4) !== 0,
    overflow,
    link: (f1 & 64) !== 0,
  };
}

/** Decode a whole frame; `cellContent` resolves overflow cells when supplied. */
export function decodeFrame(
  rows: number,
  cols: number,
  cells: Uint8Array,
  cellContent?: (row: number, col: number) => string,
): Cell[][] {
  const out: Cell[][] = [];
  for (let r = 0; r < rows; r++) {
    const line: Cell[] = [];
    for (let c = 0; c < cols; c++) {
      const cell = decodeCell(cells, (r * cols + c) * CELL_SIZE);
      if (cell.overflow && cellContent) cell.glyph = cellContent(r, c) || " ";
      line.push(cell);
    }
    out.push(line);
  }
  return out;
}

const ANSI16 = [
  "#000000", "#cd0000", "#00cd00", "#cdcd00",
  "#0000ee", "#cd00cd", "#00cdcd", "#e5e5e5",
  "#7f7f7f", "#ff0000", "#00ff00", "#ffff00",
  "#5c5cff", "#ff00ff", "#00ffff", "#ffffff",
];

const CUBE = [0, 95, 135, 175, 215, 255];

/** The xterm 256-colour palette as CSS colours. */
export function palette256(): string[] {
  const p = ANSI16.slice();
  for (let r = 0; r < 6; r++) {
    for (let g = 0; g < 6; g++) {
      for (let b = 0; b < 6; b++) {
        p.push(`rgb(${CUBE[r]},${CUBE[g]},${CUBE[b]})`);
      }
    }
  }
  for (let i = 0; i < 24; i++) {
    const v = 8 + i * 10;
    p.push(`rgb(${v},${v},${v})`);
  }
  return p;
}

export const PALETTE = palette256();

export const DEFAULT_FG = "#e5e5e5";
export const DEFAULT_BG = "#101014";

export function cssColor(c: Color, isFg: boolean): string {
  if (c.kind === "rgb") return `rgb(${c.r},${c.g},${c.b})`;
  if (c.kind === "indexed") return PALETTE[c.index] ?? (isFg ? DEFAULT_FG : DEFAULT_BG);
  return isFg ? DEFAULT_FG : DEFAULT_BG;
}

/** Resolve a cell's effective foreground and background (after inverse). */
export function effectiveColors(cell: Cell): { fg: string; bg: string } {
  let fg = cssColor(cell.fg, true);
  let bg = cssColor(cell.bg, false);
  if (cell.inverse) [fg, bg] = [bg, fg];
  if (cell.bold) fg = brighten(fg);
  if (cell.dim) fg = fade(fg, 0.6);
  return { fg, bg };
}

function parseRgb(css: string): [number, number, number] {
  const m = /^rgb\((\d+),(\d+),(\d+)\)$/.exec(css);
  if (m) return [Number(m[1]), Number(m[2]), Number(m[3])];
  const h = css.replace("#", "");
  return [Number.parseInt(h.slice(0, 2), 16), Number.parseInt(h.slice(2, 4), 16), Number.parseInt(h.slice(4, 6), 16)];
}

function brighten(css: string): string {
  const [r, g, b] = parseRgb(css).map((v) => Math.min(255, Math.round(v + 40)));
  return `rgb(${r},${g},${b})`;
}

function fade(css: string, factor: number): string {
  const [r, g, b] = parseRgb(css).map((v) => Math.round(v * factor));
  return `rgb(${r},${g},${b})`;
}