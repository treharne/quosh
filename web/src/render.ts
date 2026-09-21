// Canvas2D renderer for the shared cell format.
//
// This is the thin fallback described in the plan (rather than the Blit
// renderer spike). It decodes cells and draws them; prediction overlays arrive
// already applied in the frame (the wasm client owns the predictor), and
// predicted cells are underlined.

import {
  CELL_SIZE,
  DEFAULT_BG,
  DEFAULT_FG,
  decodeCell,
  effectiveColors,
} from "./cells.js";
import { MODE_CURSOR_VISIBLE } from "./constants.js";

export interface Metrics {
  cellWidth: number;
  cellHeight: number;
  fontSize: number;
  fontFamily: string;
}

/** A plain snapshot of a frame, safe to retain for repaints. */
export interface PaintFrame {
  rows: number;
  cols: number;
  cursorRow: number;
  cursorCol: number;
  mode: number;
  title: string;
  cells: Uint8Array;
  cellContent(row: number, col: number): string;
}

export class Renderer {
  private readonly ctx: CanvasRenderingContext2D;
  private metrics: Metrics;
  private width = 0;
  private height = 0;

  constructor(
    private readonly canvas: HTMLCanvasElement,
    fontSize = 15,
  ) {
    const ctx = canvas.getContext("2d");
    if (!ctx) throw new Error("canvas 2d context unavailable");
    this.ctx = ctx;
    this.metrics = this.measure(fontSize);
    this.resizeToParent();
  }

  get cell(): Metrics {
    return this.metrics;
  }

  private measure(fontSize: number): Metrics {
    const fontFamily =
      'ui-monospace, SFMono-Regular, Menlo, Consolas, "Liberation Mono", monospace';
    this.ctx.font = `${fontSize}px ${fontFamily}`;
    const cellWidth = Math.max(6, Math.ceil(this.ctx.measureText("M").width));
    const cellHeight = Math.ceil(fontSize * 1.28);
    return { cellWidth, cellHeight, fontSize, fontFamily };
  }

  /** Size the backing store to the CSS box (device-pixel aware). */
  resizeToParent(): { cols: number; rows: number } {
    const rect = this.canvas.parentElement?.getBoundingClientRect();
    const cssWidth = Math.max(120, Math.floor(rect?.width ?? this.canvas.clientWidth));
    const cssHeight = Math.max(60, Math.floor(rect?.height ?? this.canvas.clientHeight));
    const dpr = Math.min(3, globalThis.devicePixelRatio || 1);
    this.width = cssWidth;
    this.height = cssHeight;
    this.canvas.width = Math.round(cssWidth * dpr);
    this.canvas.height = Math.round(cssHeight * dpr);
    this.canvas.style.width = `${cssWidth}px`;
    this.canvas.style.height = `${cssHeight}px`;
    this.ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    return this.fit();
  }

  fit(): { cols: number; rows: number } {
    const { cellWidth, cellHeight } = this.metrics;
    return {
      cols: Math.max(2, Math.floor(this.width / cellWidth)),
      rows: Math.max(2, Math.floor(this.height / cellHeight)),
    };
  }

  paint(frame: PaintFrame): void {
    const ctx = this.ctx;
    const { cellWidth: cw, cellHeight: ch, fontSize, fontFamily } = this.metrics;
    const rows = frame.rows;
    const cols = frame.cols;
    const cells = frame.cells;

    ctx.fillStyle = DEFAULT_BG;
    ctx.fillRect(0, 0, this.width, this.height);
    ctx.font = `${fontSize}px ${fontFamily}`;
    ctx.textBaseline = "top";

    for (let r = 0; r < rows; r++) {
      for (let c = 0; c < cols; c++) {
        const cell = decodeCell(cells, (r * cols + c) * CELL_SIZE);
        if (cell.overflow) cell.glyph = frame.cellContent(r, c) || " ";
        const x = c * cw;
        const y = r * ch;
        const { fg, bg } = effectiveColors(cell);
        if (bg !== DEFAULT_BG) {
          ctx.fillStyle = bg;
          ctx.fillRect(x, y, cw, ch);
        }
        if (cell.wide) continue;
        if (cell.glyph !== " " && cell.glyph !== "") {
          ctx.fillStyle = fg;
          ctx.fillText(cell.glyph, x, y);
          if (cell.underline) {
            ctx.fillStyle = fg;
            ctx.fillRect(x, y + ch - 1, cw, 1);
          }
        }
      }
    }

    if ((frame.mode & MODE_CURSOR_VISIBLE) !== 0) {
      const x = frame.cursorCol * cw;
      const y = frame.cursorRow * ch;
      const cell = decodeCell(cells, (frame.cursorRow * cols + frame.cursorCol) * CELL_SIZE);
      ctx.fillStyle = DEFAULT_FG;
      ctx.fillRect(x, y, cw, ch);
      if (cell.glyph !== " " && cell.glyph !== "") {
        ctx.fillStyle = DEFAULT_BG;
        ctx.fillText(cell.glyph, x, y);
      }
    }
  }
}