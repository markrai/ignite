// The replica-local web UI preference store (`scribe-web-prefs`): one IndexedDB database with
// one key/value object store. It holds display preferences only (theme, library sort mode,
// this browser's notebook-open recency) — never notebook content, replica/outbox state or
// credentials, and nothing in it is ever synchronized. The schema is a bare key/value store,
// so adding a key needs no version bump and an older database stays fully usable.

export const DB = "scribe-web-prefs";
const STORE = "prefs";

function openDb(idb: IDBFactory): Promise<IDBDatabase> {
  return new Promise((res, rej) => {
    const r = idb.open(DB, 1);
    r.onupgradeneeded = () => r.result.createObjectStore(STORE);
    r.onsuccess = () => res(r.result);
    r.onerror = () => rej(r.error);
  });
}

/** Key/value access to the preference store; tests pass their own `IDBFactory`. */
export interface PrefsStore {
  get(key: string): Promise<unknown>;
  put(key: string, value: unknown): Promise<void>;
}

export function prefsStore(factory?: IDBFactory): PrefsStore {
  // Resolved lazily: a missing IndexedDB (or a blocked one) is just a failed promise.
  const idb = () => factory ?? globalThis.indexedDB;
  return {
    async get(key) {
      const db = await openDb(idb());
      try {
        return await new Promise<unknown>((res, rej) => {
          const g = db.transaction(STORE).objectStore(STORE).get(key);
          g.onsuccess = () => res(g.result);
          g.onerror = () => rej(g.error);
        });
      } finally {
        db.close();
      }
    },
    async put(key, value) {
      const db = await openDb(idb());
      try {
        await new Promise<void>((res, rej) => {
          const tx = db.transaction(STORE, "readwrite");
          tx.objectStore(STORE).put(value, key);
          tx.oncomplete = () => res();
          tx.onerror = () => rej(tx.error);
        });
      } finally {
        db.close();
      }
    },
  };
}
