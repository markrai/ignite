// Real-server harness for Playwright. The HARNESS (Node) seeds and advances the ledger over
// HTTP with the token (standing in for the Kindle). Browser traffic is recorded per test so the
// gates can assert exactly which `/v1` requests the browser made (S3A: GET only; S3B: plus one
// operations POST per completed stroke, and only in write mode).

import { test as base, expect, type Page, type Request } from "@playwright/test";
import { spawn, execFileSync, type ChildProcessWithoutNullStreams } from "node:child_process";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

export const WEB_DIR = resolve(import.meta.dirname, "..");
export const WEB_DIST = join(WEB_DIR, "dist");
/** Ignite Cargo workspace root (`CARGO_TARGET_DIR`/default `target/` lives here). */
export const REPO_ROOT = resolve(WEB_DIR, "../..");
/** @deprecated alias — prefer REPO_ROOT */
export const NATIVE_DIR = REPO_ROOT;
const FIX = join(WEB_DIR, "tests/fixtures");

export function serverBin(): string {
  return (
    process.env.SCRIBE_SYNC_SERVER_BIN ??
    join(REPO_ROOT, "target/debug", process.platform === "win32" ? "scribe-sync-server.exe" : "scribe-sync-server")
  );
}

export const manifest = JSON.parse(readFileSync(join(FIX, "fixture-manifest.json"), "utf8")) as {
  snapshot_sha256: string;
  library_id: string;
  blob: { sha256: string; file: string };
  snapshot_sha256_after_live_ops: string[];
  second_library: { snapshot: string; snapshot_sha256: string; library_id: string };
};
export const snapshotText = readFileSync(join(FIX, "snapshot-v1.json"), "utf8");
export const snapshot = JSON.parse(snapshotText) as {
  notebooks: { title: string; pages: { strokes: { root_id: string; stroke_id: string }[] }[] }[];
};
const liveOps = (JSON.parse(readFileSync(join(FIX, "live-ops-v1.json"), "utf8")) as { envelopes: string[] }).envelopes;

export interface SyncServer {
  url: string;
  token: string;
  /** Harness-side (not browser) HTTP with the token. */
  api(method: string, path: string, body?: Uint8Array | string): Promise<{ status: number; json: unknown }>;
  seedFixture(): Promise<void>;
  seedSecondLibrary(): Promise<void>;
  /** Submit fixture live op `i` (0 = AddStroke, 1 = partial EraseRegion) as the Kindle would. */
  pushLiveOp(i: number): Promise<void>;
  head(): Promise<number>;
  /** Current server snapshot (harness-side GET). */
  snapshot(libraryId?: string): Promise<SnapshotDoc>;
  /** Ledger tail as raw envelope text (exact stored bytes), in server_seq order. */
  operations(libraryId?: string): Promise<{ server_seq: number; envelope: string }[]>;
  /** Stop the server process (the ledger stays on disk): the browser goes "offline". */
  halt(): Promise<void>;
  /** Start it again on the same port with the same ledger. */
  resume(): Promise<void>;
}

export interface SnapshotDoc {
  through_server_seq: number;
  snapshot_sha256: string;
  snapshot: {
    notebooks: {
      notebook_id: string;
      pages: {
        page_id: string;
        strokes: { stroke_id: string; root_id: string; style: string; points: { x: number; y: number; pressure?: number }[] }[];
        texts?: { text_id: string; content: string; face: string; x: number; y: number; width: number; font_size: number }[];
      }[];
    }[];
  };
}

async function start(): Promise<{ server: SyncServer; stop: () => void }> {
  const dir = mkdtempSync(join(tmpdir(), "scribe-s3a-e2e-"));
  const verifier = join(dir, "verifier");
  const token = execFileSync(serverBin(), ["new-token", "--verifier-out", verifier], {
    encoding: "utf8",
    stdio: ["ignore", "pipe", "ignore"],
  }).trim();
  let child: ChildProcessWithoutNullStreams;
  const launch = (bind: string) => {
    child = spawn(serverBin(), ["--data-dir", join(dir, "ledger"), "--bind", bind, "--token-verifier-file", verifier, "--web-root", WEB_DIST]);
    const c = child;
    return new Promise<string>((ok, fail) => {
      let out = "";
      const timer = setTimeout(() => fail(new Error(`server did not start: ${out}`)), 15_000);
      c.stdout.on("data", (d: Buffer) => {
        out += d.toString();
        const m = /listening on (http:\/\/127\.0\.0\.1:\d+)/.exec(out);
        if (m) {
          clearTimeout(timer);
          ok(m[1]!);
        }
      });
      c.stderr.on("data", (d: Buffer) => (out += d.toString()));
      c.on("exit", (code) => fail(new Error(`server exited ${code}: ${out}`)));
    });
  };
  const halt = () =>
    new Promise<void>((ok) => {
      if (child.exitCode !== null || child.signalCode !== null) return ok();
      child.once("exit", () => ok());
      child.kill();
    });
  const url = await launch("127.0.0.1:0");
  const api: SyncServer["api"] = async (method, path, body) => {
    const r = await fetch(url + path, {
      method,
      headers: { Authorization: `Bearer ${token}` },
      body: body as BodyInit | undefined,
    });
    const text = await r.text();
    let json: unknown;
    try {
      json = JSON.parse(text);
    } catch {
      json = text;
    }
    return { status: r.status, json };
  };
  const server: SyncServer = {
    url,
    token,
    api,
    async seedFixture() {
      const blob = new Uint8Array(readFileSync(join(FIX, manifest.blob.file)));
      expect((await api("PUT", `/v1/blobs/${manifest.blob.sha256}`, blob)).status).toBeLessThan(300);
      expect((await api("POST", "/v1/libraries", snapshotText)).status).toBe(201);
    },
    async seedSecondLibrary() {
      const text = readFileSync(join(FIX, manifest.second_library.snapshot), "utf8");
      expect((await api("POST", "/v1/libraries", text)).status).toBe(201);
    },
    async pushLiveOp(i) {
      const r = await api("POST", `/v1/libraries/${manifest.library_id}/operations`, `{"envelopes":[${liveOps[i]}]}`);
      expect(r.status, JSON.stringify(r.json)).toBe(200);
    },
    async head() {
      const r = await api("GET", `/v1/libraries/${manifest.library_id}`);
      return (r.json as { head_server_seq: number }).head_server_seq;
    },
    async snapshot(libraryId = manifest.library_id) {
      const r = await api("GET", `/v1/libraries/${libraryId}/snapshot`);
      expect(r.status).toBe(200);
      return r.json as SnapshotDoc;
    },
    async operations(libraryId = manifest.library_id) {
      const r = await fetch(`${url}/v1/libraries/${libraryId}/operations?after=0&limit=500`, { headers: { Authorization: `Bearer ${token}` } });
      const text = await r.text();
      // Extract each raw envelope exactly as stored (no JSON re-serialization).
      const out: { server_seq: number; envelope: string }[] = [];
      const re = /\{"server_seq":(\d+),"envelope":/g;
      let m: RegExpExecArray | null;
      while ((m = re.exec(text))) {
        const start = re.lastIndex;
        let depth = 0;
        let i = start;
        let inStr = false;
        for (; i < text.length; i++) {
          const ch = text[i];
          if (inStr) {
            if (ch === "\\") i++;
            else if (ch === '"') inStr = false;
          } else if (ch === '"') inStr = true;
          else if (ch === "{") depth++;
          else if (ch === "}" && --depth === 0) break;
        }
        out.push({ server_seq: Number(m[1]), envelope: text.slice(start, i + 1) });
        re.lastIndex = i + 1;
      }
      return out;
    },
    halt,
    async resume() {
      const port = new URL(url).port;
      await launch(`127.0.0.1:${port}`);
    },
  };
  return {
    server,
    stop: () => {
      child.kill();
      try {
        rmSync(dir, { recursive: true, force: true });
      } catch {
        // Windows may hold the ledger briefly after kill; temp dir is disposable.
      }
    },
  };
}

export interface Traffic {
  requests: { method: string; url: string }[];
  consoleErrors: string[];
}

export const test = base.extend<{ server: SyncServer; traffic: Traffic }>({
  server: async ({}, use) => {
    const s = await start();
    await use(s.server);
    s.stop();
  },
  traffic: async ({ page }, use) => {
    const t: Traffic = { requests: [], consoleErrors: [] };
    page.on("request", (r: Request) => t.requests.push({ method: r.method(), url: r.url() }));
    page.on("console", (m) => {
      if (m.type() === "error") t.consoleErrors.push(m.text());
    });
    page.on("pageerror", (e) => t.consoleErrors.push(String(e)));
    await use(t);
  },
});

export { expect };

/** Every browser request to /v1 must be a GET (the S3A observational gate). */
export function expectReadOnly(t: Traffic): void {
  const v1 = t.requests.filter((r) => new URL(r.url).pathname.startsWith("/v1"));
  expect(v1.length).toBeGreaterThan(0);
  const writes = v1.filter((r) => r.method !== "GET");
  expect(writes, `mutating browser requests: ${JSON.stringify(writes)}`).toEqual([]);
}

export async function connect(page: Page, server: SyncServer, token = server.token): Promise<void> {
  await page.goto(server.url + "/");
  await expect(page.getByTestId("connect-form")).toBeVisible();
  await page.getByTestId("token-input").fill(token);
  await page.getByTestId("connect").click();
}

export async function canvasInfo(page: Page) {
  return page.getByTestId("page-canvas").evaluate((c: HTMLCanvasElement) => ({
    pageId: c.dataset.pageId ?? "",
    strokes: Number(c.dataset.strokes ?? "-1"),
    rendered: c.dataset.rendered === "true",
    background: c.dataset.background ?? "",
    rect: (c.dataset.pageRect ?? "0,0,0,0").split(",").map(Number) as [number, number, number, number],
    viewport: (c.dataset.viewport ?? "0,0").split(",").map(Number) as [number, number],
    zoom: Number(c.dataset.zoom ?? "0"),
  }));
}

/** Dark (ink) pixels in the canvas backing store — proves vector ink was actually painted. */
export async function inkPixels(page: Page): Promise<number> {
  return page.getByTestId("page-canvas").evaluate((c: HTMLCanvasElement) => {
    const ctx = c.getContext("2d")!;
    const d = ctx.getImageData(0, 0, c.width, c.height).data;
    let n = 0;
    for (let i = 0; i < d.length; i += 4) if (d[i]! < 60 && d[i + 1]! < 60 && d[i + 2]! < 60) n++;
    return n;
  });
}
