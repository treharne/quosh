// Reference PWA glue: routing, server list, enrolment, terminal view.
//
// Deliberately small: the protocol, prediction, and session logic all live in
// the wasm packages; this file is DOM and orchestration only.

import { CELL_SIZE, decodeCell } from "./cells.js";
import { MODE_BRACKETED_PASTE } from "./constants.js";
import {
  hexToBytes,
  parseEnrollFragment,
  serverKey,
  type EnrollPayload,
} from "./enroll.js";
import { encodeBarKey, encodeKey, encodePaste } from "./input.js";
import { createKeyBar } from "./keybar.js";
import { Renderer, type PaintFrame } from "./render.js";
import { Session, type Status } from "./session.js";
import {
  forgetServer,
  listServers,
  putServer,
  type ServerEntry,
} from "./store.js";
import { initWasm, type FrameView } from "./wasm.js";

const app = document.getElementById("app") as HTMLElement;

function el(tag: string, className?: string, text?: string): HTMLElement {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text !== undefined) node.textContent = text;
  return node;
}

function button(label: string, className: string, onClick: () => void): HTMLButtonElement {
  const b = document.createElement("button");
  b.type = "button";
  b.textContent = label;
  b.className = className;
  b.addEventListener("click", onClick);
  return b;
}

/** Copy a wasm frame into a plain object and free the wasm handle. */
function snapshot(f: FrameView): PaintFrame {
  const { cols, rows } = f;
  const cells = f.cells;
  const overflow = new Map<number, string>();
  for (let i = 0; i < rows * cols; i++) {
    if (((cells[i * CELL_SIZE + 1]! >> 3) & 7) === 7) {
      overflow.set(i, f.cell_content(Math.floor(i / cols), i % cols) || " ");
    }
  }
  const frame: PaintFrame = {
    rows,
    cols,
    cursorRow: f.cursor_row,
    cursorCol: f.cursor_col,
    mode: f.mode,
    title: f.title,
    cells,
    cellContent: (row, col) => overflow.get(row * cols + col) ?? " ",
  };
  f.free();
  return frame;
}

function frameText(frame: PaintFrame): string {
  const lines: string[] = [];
  for (let r = 0; r < frame.rows; r++) {
    let line = "";
    for (let c = 0; c < frame.cols; c++) {
      const cell = decodeCell(frame.cells, (r * frame.cols + c) * CELL_SIZE);
      line += cell.overflow ? frame.cellContent(r, c) : cell.wide ? "" : cell.glyph;
    }
    lines.push(line.replace(/\s+$/, ""));
  }
  return lines.join("\n").replace(/\n+$/, "\n");
}

function statusText(status: Status, detail?: string): string {
  switch (status) {
    case "connecting":
      return "connecting…";
    case "online":
      return "connected";
    case "offline":
      return detail ? `reconnecting… (${detail})` : "reconnecting…";
    case "ended":
      return detail ? `ended: ${detail}` : "session ended";
  }
}

// -- Routing -----------------------------------------------------------------

let cleanup: (() => void) | null = null;

function route(): void {
  cleanup?.();
  cleanup = null;

  let enrol: EnrollPayload | null = null;
  try {
    enrol = parseEnrollFragment(location.hash);
  } catch (e) {
    showMessage("Enrolment failed", (e as Error).message);
    return;
  }
  if (enrol) {
    showEnroll(enrol);
  } else {
    void showServers();
  }
}

function showMessage(title: string, detail: string): void {
  app.replaceChildren(el("h1", undefined, title), el("p", "sub", detail));
}

// -- Server list -------------------------------------------------------------

async function showServers(): Promise<void> {
  const servers = await listServers();
  app.replaceChildren();
  app.append(
    el("h1", undefined, "Quosh"),
    el("p", "sub", "Remote shells over WebTransport."),
  );
  if (servers.length === 0) {
    app.append(
      el("p", "empty", "No servers yet. Run `quosh enroll user@host` and open the link."),
    );
    return;
  }
  const list = el("ul", "servers");
  for (const entry of servers) {
    const li = el("li", "server");
    const info = el("div", "server-info");
    info.append(
      el("div", "server-user", entry.user),
      el("div", "server-host", `${entry.host}:${entry.port}`),
    );
    li.append(
      info,
      button("Connect", "connect", () => showTerminal(entry)),
      button("Forget", "forget", () => {
        void forgetServer(entry.key).then(showServers);
      }),
    );
    list.append(li);
  }
  app.append(list);
}

// -- Enrolment ---------------------------------------------------------------

function showEnroll(payload: EnrollPayload): void {
  app.replaceChildren();
  app.append(
    el("h1", undefined, "Enrol this device"),
    el("p", "sub", `${payload.user}@${payload.host}:${payload.port}`),
    el(
      "p",
      "hint",
      "A passkey is created on this device. The server never sees your biometrics.",
    ),
  );
  const go = button("Create passkey", "primary", () => {
    go.disabled = true;
    const entry: ServerEntry = {
      key: serverKey(payload.host, payload.port),
      host: payload.host,
      port: payload.port,
      user: `${payload.user}@${payload.host}`,
      hashes: [payload.hash],
    };
    void putServer(entry).then(() => {
      history.replaceState(null, "", location.pathname);
      showTerminal(entry, hexToBytes(payload.nonce));
    });
  });
  app.append(go);
}

// -- Terminal ----------------------------------------------------------------

function showTerminal(entry: ServerEntry, enrollNonce?: Uint8Array): void {
  app.replaceChildren();
  const root = el("div", "terminal");
  const top = el("div", "topbar");
  const title = el("span", "title", entry.user);
  top.append(title);

  const canvas = document.createElement("canvas");
  const screen = el("div", "screen");
  screen.append(canvas);
  const status = el("div", "status", "connecting…");
  const barHost = el("div", "keybar-host");

  const hidden = document.createElement("textarea");
  hidden.className = "hidden-input";
  hidden.autocapitalize = "none";
  hidden.autocomplete = "off";
  hidden.spellcheck = false;
  hidden.setAttribute("autocorrect", "off");

  root.append(top, screen, status, barHost, hidden);
  app.append(root);

  const renderer = new Renderer(canvas);
  let fit = renderer.resizeToParent();
  let last: PaintFrame | null = null;
  const bracketed = () => ((last?.mode ?? 0) & MODE_BRACKETED_PASTE) !== 0;

  const session = new Session({
    entry,
    cols: fit.cols,
    rows: fit.rows,
    predictNever: false,
    enrollNonce,
    onFrame: (view) => {
      last = snapshot(view);
      renderer.paint(last);
      if (last.title) title.textContent = last.title;
    },
    onStatus: (s, detail) => {
      status.textContent = statusText(s, detail);
      status.dataset.state = s;
    },
    onExit: (code) => {
      status.textContent = `session ended (${code >= 0 ? code : `signal ${-code}`})`;
      status.dataset.state = "ended";
    },
  });

  const keybar = createKeyBar({
    onKey: (key) => {
      const bytes = encodeBarKey(key, keybar.mods);
      if (bytes) session.input(bytes);
      keybar.reset();
    },
    onPaste: () => {
      void paste();
    },
    onCopy: () => {
      void copy();
    },
    onQuit: () => {
      session.quit();
    },
  });
  barHost.append(keybar.el);

  async function paste(): Promise<void> {
    try {
      const text = await navigator.clipboard.readText();
      if (text) session.input(encodePaste(text, bracketed()));
    } catch {
      hidden.focus();
    }
  }

  async function copy(): Promise<void> {
    if (!last) return;
    try {
      await navigator.clipboard.writeText(frameText(last));
    } catch {
      // Clipboard permission denied; nothing else to do.
    }
  }

  const sendKey = (e: KeyboardEvent): void => {
    if (e.metaKey) return;
    const sticky = keybar.mods.ctrl || keybar.mods.alt;
    if (sticky) {
      const bytes = encodeKey(e.key, e.shiftKey, keybar.mods);
      keybar.reset();
      if (bytes) {
        e.preventDefault();
        session.input(bytes);
      }
      return;
    }
    // Let the `input` event handle plain text (IME-friendly); handle the rest.
    if (e.key.length === 1 && !e.ctrlKey && !e.altKey) return;
    const bytes = encodeKey(e.key, e.shiftKey, { ctrl: e.ctrlKey, alt: e.altKey });
    if (bytes) {
      e.preventDefault();
      session.input(bytes);
    }
  };

  const onInput = (): void => {
    const text = hidden.value;
    hidden.value = "";
    if (text) session.input(encodePaste(text, false));
  };

  const onResize = (): void => {
    fit = renderer.resizeToParent();
    session.resize(fit.cols, fit.rows);
    if (last) renderer.paint(last);
  };

  const focusInput = (): void => hidden.focus();

  document.addEventListener("keydown", sendKey);
  hidden.addEventListener("input", onInput);
  window.addEventListener("resize", onResize);
  screen.addEventListener("pointerdown", focusInput);
  focusInput();
  void session.start();

  const back = button("‹ Servers", "back", () => {
    session.stop();
    history.replaceState(null, "", location.pathname);
    void showServers();
  });
  top.prepend(back);

  cleanup = () => {
    document.removeEventListener("keydown", sendKey);
    window.removeEventListener("resize", onResize);
    session.stop();
  };
}

// -- Boot --------------------------------------------------------------------

async function main(): Promise<void> {
  await initWasm();
  window.addEventListener("hashchange", route);
  route();
}

void main();