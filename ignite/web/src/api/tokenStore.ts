// Bearer token storage: sessionStorage only (cleared with the tab). Never localStorage,
// IndexedDB or Cache Storage; never URLs, logs or notebook data.

const KEY = "scribe.syncToken";

export interface StorageLike {
  getItem(k: string): string | null;
  setItem(k: string, v: string): void;
  removeItem(k: string): void;
}

function session(): StorageLike | null {
  try {
    return globalThis.sessionStorage ?? null;
  } catch {
    return null; // storage blocked: the token then lives in memory only
  }
}

export function loadToken(store: StorageLike | null = session()): string | null {
  try {
    const t = store?.getItem(KEY) ?? null;
    return t && t.length > 0 ? t : null;
  } catch {
    return null;
  }
}

export function saveToken(token: string, store: StorageLike | null = session()): void {
  try {
    store?.setItem(KEY, token);
  } catch {
    // memory-only fallback
  }
}

export function clearToken(store: StorageLike | null = session()): void {
  try {
    store?.removeItem(KEY);
  } catch {
    // nothing stored
  }
}
