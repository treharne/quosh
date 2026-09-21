// The mobile key bar: sticky modifiers plus the keys a phone keyboard lacks.

import type { Modifiers } from "./input.js";

export interface KeyBarHandlers {
  onKey(key: string): void;
  onPaste(): void;
  onCopy(): void;
  onQuit(): void;
}

export interface KeyBar {
  el: HTMLElement;
  /** Current sticky modifier state; the app reads this for OS keypresses. */
  mods: Modifiers;
  /** Clear sticky modifiers after they have been consumed. */
  reset(): void;
}

function button(label: string, className = ""): HTMLButtonElement {
  const b = document.createElement("button");
  b.type = "button";
  b.textContent = label;
  if (className) b.className = className;
  return b;
}

export function createKeyBar(handlers: KeyBarHandlers): KeyBar {
  const el = document.createElement("div");
  el.className = "keybar";
  const toggles: HTMLButtonElement[] = [];
  const mods: Modifiers = { ctrl: false, alt: false };

  const addToggle = (label: string, which: keyof Modifiers): void => {
    const b = button(label, "toggle");
    b.addEventListener("pointerdown", (e) => {
      e.preventDefault();
      mods[which] = !mods[which];
      b.classList.toggle("armed", mods[which]);
    });
    toggles.push(b);
    el.appendChild(b);
  };

  const addKey = (label: string, key: string, className = ""): void => {
    const b = button(label, className);
    b.addEventListener("pointerdown", (e) => {
      e.preventDefault();
      handlers.onKey(key);
    });
    el.appendChild(b);
  };

  const addAction = (label: string, fn: () => void): void => {
    const b = button(label, "action");
    b.addEventListener("click", (e) => {
      e.preventDefault();
      fn();
    });
    el.appendChild(b);
  };

  addToggle("Ctrl", "ctrl");
  addToggle("Alt", "alt");
  addKey("Esc", "Escape");
  addKey("Tab", "Tab");
  addKey("↑", "ArrowUp");
  addKey("↓", "ArrowDown");
  addKey("←", "ArrowLeft");
  addKey("→", "ArrowRight");
  for (const ch of ["|", "~", "/", "-", "\\", "`"]) addKey(ch, ch, "punct");
  addAction("Paste", handlers.onPaste);
  addAction("Copy", handlers.onCopy);
  addAction("Quit", handlers.onQuit);

  return {
    el,
    mods,
    reset() {
      mods.ctrl = false;
      mods.alt = false;
      for (const b of toggles) b.classList.remove("armed");
    },
  };
}