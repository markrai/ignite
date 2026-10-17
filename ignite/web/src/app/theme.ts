// Light/dark appearance (replica-local UI preference; never notebook content, never synced).
//
// Default: the system setting (`prefers-color-scheme`). An explicit choice from the top-bar
// toggle is remembered in the UI preference store (`scribe-web-prefs`, see prefsDb.ts) under
// one key holding only the word "light" or "dark" — the app never uses localStorage (see
// tests/api.test.ts), and the preference store never shares a database with replica state or
// credentials. Storage failures
// are harmless: the theme then simply follows the system setting.

import { prefsStore } from "./prefsDb";

export type Theme = "light" | "dark";

const KEY = "theme";
const prefs = prefsStore();

let chosen: Theme | null = null;
const listeners = new Set<() => void>();

function systemTheme(): Theme {
  return typeof matchMedia === "function" && matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light";
}

export function currentTheme(): Theme {
  return chosen ?? systemTheme();
}

function apply(): void {
  const t = currentTheme();
  document.documentElement.dataset.theme = t;
  for (const l of listeners) l();
}

export function subscribeTheme(fn: () => void): () => void {
  listeners.add(fn);
  return () => listeners.delete(fn);
}

async function load(): Promise<Theme | null> {
  const v = await prefs.get(KEY);
  return v === "light" || v === "dark" ? v : null;
}

/** Apply the system theme now, then any remembered choice; follow system changes. */
export function initTheme(): void {
  apply();
  if (typeof matchMedia === "function") {
    matchMedia("(prefers-color-scheme: dark)").addEventListener("change", () => {
      if (chosen === null) apply();
    });
  }
  load()
    .then((t) => {
      if (t && chosen === null) {
        chosen = t;
        apply();
      }
    })
    .catch(() => undefined);
}

// Saves run one after another, so rapid toggles can never commit out of order.
let saving: Promise<void> = Promise.resolve();

export function toggleTheme(): void {
  const t: Theme = currentTheme() === "dark" ? "light" : "dark";
  chosen = t;
  apply();
  saving = saving.then(() => prefs.put(KEY, t)).catch(() => undefined);
}
