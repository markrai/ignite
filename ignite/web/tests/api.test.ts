import { readFileSync, readdirSync, statSync } from "node:fs";
import { join } from "node:path";
import { describe, expect, it } from "vitest";
import { createReadOnlyApi, UnauthorizedError, type FetchLike } from "../src/api/readOnlyApi";
import { clearToken, loadToken, saveToken, type StorageLike } from "../src/api/tokenStore";
import { blobBytes, manifest, snapshotJson } from "./fixtures";

const TOKEN = "f".repeat(64);

function recorder(status = 200) {
  const seen: { url: string; init: RequestInit }[] = [];
  const fetchFn: FetchLike = async (url, init) => {
    seen.push({ url, init });
    const lib = manifest.library_id;
    const body = url.startsWith("/v1/blobs/")
      ? blobBytes
      : url === "/v1/libraries"
        ? { v: 1, libraries: [] }
        : url.endsWith("/snapshot")
          ? { library_id: lib, through_server_seq: 0, snapshot_sha256: manifest.snapshot_sha256, snapshot: snapshotJson }
          : { library_id: lib, genesis_sha256: manifest.snapshot_sha256, head_server_seq: 0, snapshot_sha256: manifest.snapshot_sha256 };
    return new Response(body instanceof Uint8Array ? (body as Uint8Array<ArrayBuffer>) : JSON.stringify(body), { status });
  };
  return { seen, fetchFn };
}

describe("read-only API boundary", () => {
  it("exposes only the four GET reads", () => {
    const api = createReadOnlyApi(TOKEN, recorder().fetchFn);
    expect(Object.keys(api).sort()).toEqual(["blob", "libraryHead", "listLibraries", "snapshot"]);
  });

  it("issues only same-origin /v1 GETs; token only in the Authorization header", async () => {
    const r = recorder();
    const api = createReadOnlyApi(TOKEN, r.fetchFn);
    await api.listLibraries();
    await api.libraryHead(manifest.library_id);
    await api.snapshot(manifest.library_id);
    await api.blob(manifest.blob.sha256);
    expect(r.seen).toHaveLength(4);
    for (const { url, init } of r.seen) {
      expect(init.method).toBe("GET");
      expect(init.body).toBeUndefined();
      expect(url.startsWith("/v1/")).toBe(true);
      expect(url).not.toContain(TOKEN);
      expect((init.headers as Record<string, string>).Authorization).toBe(`Bearer ${TOKEN}`);
    }
  });

  it("no-auth mode sends no Authorization header", async () => {
    const r = recorder();
    await createReadOnlyApi(null, r.fetchFn).listLibraries();
    expect((r.seen[0]!.init.headers as Record<string, string>).Authorization).toBeUndefined();
  });

  it("401 is an UnauthorizedError whose message never contains the token", async () => {
    const api = createReadOnlyApi(TOKEN, recorder(401).fetchFn);
    const e = await api.listLibraries().catch((x: unknown) => x);
    expect(e).toBeInstanceOf(UnauthorizedError);
    expect(String((e as Error).message)).not.toContain(TOKEN);
    expect(JSON.stringify(e)).not.toContain(TOKEN);
  });

  it("refuses path injection through ids", async () => {
    const api = createReadOnlyApi(TOKEN, recorder().fetchFn);
    await expect(api.snapshot("../../etc")).rejects.toThrow();
    await expect(api.blob("../x")).rejects.toThrow();
  });

  it("S3B boundary: notebook data is mutated only by replicaApi's operations POST", () => {
    const src = join(import.meta.dirname, "../src");
    const files: string[] = [];
    const walk = (d: string) => {
      for (const f of readdirSync(d)) {
        const p = join(d, f);
        if (statSync(p).isDirectory()) walk(p);
        else if (/\.(ts|tsx)$/.test(f)) files.push(p);
      }
    };
    walk(src);
    const rel = (f: string) => f.slice(src.length + 1).split("\\").join("/");
    for (const f of files) {
      const text = readFileSync(f, "utf8");
      const r = rel(f);
      // The control-plane client (claim, pairing, administration, 1G-B restore upload/cancel)
      // mutates only /control/v1, never notebook data (pinned in control.test.ts).
      const control = r === "api/controlApi.ts";
      if (!control) expect(text, r).not.toMatch(/method:\s*["'](PUT|PATCH|DELETE)["']/i);
      if (r !== "api/replicaApi.ts" && !control) expect(text, r).not.toMatch(/method:\s*["']POST["']/i);
      expect(text, r).not.toMatch(/new\s+(XMLHttpRequest|WebSocket|EventSource)\b|sendBeacon\s*\(/);
      const pwa = r === "pwa/registerUpdate.ts";
      if (!pwa) {
        expect(text, r).not.toMatch(/serviceWorker/);
        expect(text, r).not.toMatch(/\blocalStorage\s*[.[]/);
      }
      expect(text, r).not.toMatch(/\bcaches\s*\.\s*open/);
      expect(text, r).not.toMatch(/Math\.random/);
      // Components, pointer capture and rendering never touch the network directly.
      if (/^(components|authoring|rendering)\//.test(r)) expect(text, r).not.toMatch(/\bfetch\s*\(|replicaApi/);
      // The replica API is created by the app shell and used only by the sync coordinator.
      if (text.includes('/replicaApi"')) expect(["app/App.tsx", "replica/coordinator.ts"]).toContain(r);
      // Only the replica store opens IndexedDB — plus, by explicit exception, the UI preference
      // store (its own database; checked below to hold no replica state or credentials).
      if (/\bindexedDB\b/.test(text)) expect(["replica/store.ts", "app/prefsDb.ts"]).toContain(r);
    }
    const replica = readFileSync(join(src, "api/replicaApi.ts"), "utf8");
    expect(replica.match(/method:\s*"POST"/g)).toHaveLength(1);
    expect(replica).toContain("`/v1/libraries/${id}/operations`");
    expect(replica).not.toMatch(/\/v1\/blobs|genesis"/);
    // The preference exception is narrow: its own database, UI preferences only, no tokens,
    // no replica/sync path.
    expect(readFileSync(join(src, "app/prefsDb.ts"), "utf8")).toContain('export const DB = "scribe-web-prefs";');
    for (const f of ["app/prefsDb.ts", "app/theme.ts", "app/libraryPrefs.ts"]) {
      expect(readFileSync(join(src, f), "utf8"), f).not.toMatch(/scribe-web-sync|tokenStore|loadToken|Authorization|Bearer|fetch\s*\(|coordinator|replicaApi/);
    }
    // Theme writes one key holding only "light"/"dark".
    const theme = readFileSync(join(src, "app/theme.ts"), "utf8");
    expect(theme.match(/\.put\(/g)).toEqual([".put("]);
    expect(theme).toContain("prefs.put(KEY, t)");
    // The bearer token never reaches IndexedDB.
    expect(readFileSync(join(src, "replica/store.ts"), "utf8")).not.toMatch(/tokenStore|loadToken|Authorization|Bearer/);
    const pwa = readFileSync(join(src, "pwa/registerUpdate.ts"), "utf8");
    expect(pwa).toContain("serviceWorker");
    expect(pwa).not.toMatch(/\bcaches\s*\.\s*open|\bfetch\s*\(|indexedDB|Authorization|Bearer/);
  });
});

describe("token storage", () => {
  function mem(): StorageLike & { data: Map<string, string> } {
    const data = new Map<string, string>();
    return { data, getItem: (k) => data.get(k) ?? null, setItem: (k, v) => void data.set(k, v), removeItem: (k) => void data.delete(k) };
  }
  it("round-trips through the given (session) storage and clears", () => {
    const s = mem();
    expect(loadToken(s)).toBeNull();
    saveToken(TOKEN, s);
    expect(loadToken(s)).toBe(TOKEN);
    clearToken(s);
    expect(loadToken(s)).toBeNull();
    expect(s.data.size).toBe(0);
  });
  it("tolerates blocked storage", () => {
    const broken: StorageLike = {
      getItem: () => {
        throw new Error("blocked");
      },
      setItem: () => {
        throw new Error("blocked");
      },
      removeItem: () => {
        throw new Error("blocked");
      },
    };
    expect(loadToken(broken)).toBeNull();
    saveToken(TOKEN, broken);
    clearToken(broken);
  });
});
