// Keyboard/byte encoding. Pure, so it is unit-tested in Node.

export interface Modifiers {
  ctrl: boolean;
  alt: boolean;
}

const encoder = new TextEncoder();

const NAMED: Record<string, number[]> = {
  Enter: [0x0d],
  Backspace: [0x7f],
  Tab: [0x09],
  Escape: [0x1b],
  ArrowUp: [0x1b, 0x5b, 0x41],
  ArrowDown: [0x1b, 0x5b, 0x42],
  ArrowRight: [0x1b, 0x5b, 0x43],
  ArrowLeft: [0x1b, 0x5b, 0x44],
  Home: [0x1b, 0x5b, 0x48],
  End: [0x1b, 0x5b, 0x46],
  PageUp: [0x1b, 0x5b, 0x35, 0x7e],
  PageDown: [0x1b, 0x5b, 0x36, 0x7e],
  Delete: [0x1b, 0x5b, 0x33, 0x7e],
  Insert: [0x1b, 0x5b, 0x32, 0x7e],
};

const IGNORED = new Set(["Shift", "Control", "Alt", "Meta", "CapsLock", "Dead"]);

function ctrlByte(key: string): number[] | null {
  if (key.length !== 1) return null;
  const code = key.toLowerCase().charCodeAt(0);
  if (code >= 97 && code <= 122) return [code - 96]; // Ctrl-A..Ctrl-Z
  switch (key) {
    case " ":
    case "@":
      return [0x00];
    case "[":
      return [0x1b];
    case "\\":
      return [0x1c];
    case "]":
      return [0x1d];
    case "^":
      return [0x1e];
    case "_":
      return [0x1f];
    default:
      return null;
  }
}

/** Encode one keypress, applying sticky modifiers. Returns null to ignore. */
export function encodeKey(
  key: string,
  shift: boolean,
  mods: Modifiers,
): Uint8Array | null {
  if (IGNORED.has(key)) return null;

  let bytes: number[] | null = null;
  if (mods.ctrl) {
    // A named key with Ctrl keeps its escape sequence (e.g. Ctrl+Arrow).
    bytes = NAMED[key] ?? ctrlByte(key);
  } else {
    bytes = NAMED[key] ?? null;
  }

  if (bytes === null) {
    if (key.length === 1) {
      bytes = Array.from(encoder.encode(key));
    } else {
      return null;
    }
  }
  if (mods.alt) bytes = [0x1b, ...bytes];
  void shift;
  return Uint8Array.from(bytes);
}

/** Encode a key selected from the on-screen key bar, e.g. `"|"`, `"ArrowUp"`. */
export function encodeBarKey(key: string, mods: Modifiers): Uint8Array | null {
  return encodeKey(key, false, mods);
}

const PASTE_START = Array.from(encoder.encode("\x1b[200~"));
const PASTE_END = Array.from(encoder.encode("\x1b[201~"));

/** Wrap text in a bracketed paste when the application asked for one. */
export function encodePaste(text: string, bracketed: boolean): Uint8Array {
  const body = Array.from(encoder.encode(text));
  if (!bracketed) return Uint8Array.from(body);
  return Uint8Array.from([...PASTE_START, ...body, ...PASTE_END]);
}