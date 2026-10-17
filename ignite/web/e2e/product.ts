// D1 product-mode harness: the real `scribe-server` binary (control plane + sync plane +
// ignite-web), started from a config file exactly like the Linux service. The HARNESS plays
// the Kindle: it pairs over the real control API and seeds a library with its own per-device
// credential. Browsers never see a token.

import { test as base, expect, type BrowserContext } from "@playwright/test";
import { execFileSync, spawn, type ChildProcessWithoutNullStreams } from "node:child_process";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { manifest, REPO_ROOT, snapshotText, WEB_DIR, WEB_DIST } from "./harness";

const FIX = join(WEB_DIR, "tests/fixtures");

export function productBin(): string {
  return (
    process.env.SCRIBE_SERVER_BIN ?? join(REPO_ROOT, "target/debug", process.platform === "win32" ? "scribe-server.exe" : "scribe-server")
  );
}

export interface ProductServer {
  url: string;
  host: string;
  /** One-time setup code printed by `scribe-server init` (as the installer shows it). */
  setupCode: string;
  serverId(): Promise<string>;
  /** Harness-side control-plane call with an optional admin cookie / bearer. */
  call(method: string, path: string, opts?: { cookie?: string; bearer?: string; body?: unknown; csrf?: boolean }): Promise<{ status: number; json: unknown; setCookie: string | null }>;
  /** Claim via HTTP (harness as the first admin); returns the admin cookie value. */
  claimAsHarness(): Promise<string>;
  /** Pair the harness "Kindle" (approved with `adminCookie`); returns its credential. */
  pairKindle(adminCookie: string): Promise<string>;
  /** Seed the fixture library with a device credential (genesis + blob). */
  seedFixture(bearer: string): Promise<void>;
  head(bearer: string): Promise<number>;
  operations(bearer: string): Promise<{ server_seq: number; envelope: string }[]>;
  restart(): Promise<void>;
}

async function startProduct(): Promise<{ server: ProductServer; stop: () => void }> {
  const dir = mkdtempSync(join(tmpdir(), "scribe-d1-e2e-"));
  const configPath = join(dir, "config.toml");
  const state = join(dir, "state").replace(/\\/g, "/");
  const web = WEB_DIST.replace(/\\/g, "/");
  let listen = "127.0.0.1:0";
  const writeConfig = () =>
    writeFileSync(
      configPath,
      `[server]\nlisten = "${listen}"\nstate_dir = "${state}"\nweb_root = "${web}"\nname = "Ignite Server on Testhost"\n\n[discovery]\nadvertise = false\n`,
    );
  writeConfig();
  const init = execFileSync(productBin(), ["init", "--config", configPath], { encoding: "utf8" });
  const setupCode = /setup_code=(\S+)/.exec(init)?.[1];
  if (!setupCode) throw new Error(`no setup code from init: ${init}`);

  let child: ChildProcessWithoutNullStreams;
  const launch = () => {
    child = spawn(productBin(), ["serve", "--config", configPath]);
    const c = child;
    return new Promise<string>((ok, fail) => {
      let out = "";
      const timer = setTimeout(() => fail(new Error(`scribe-server did not start: ${out}`)), 15_000);
      const on = (d: Buffer) => {
        out += d.toString();
        const m = /event=started .*addr=(127\.0\.0\.1:\d+)/.exec(out);
        if (m) {
          clearTimeout(timer);
          ok(m[1]!);
        }
      };
      c.stdout.on("data", on);
      c.stderr.on("data", on);
      c.on("exit", (code) => fail(new Error(`scribe-server exited ${code}: ${out}`)));
    });
  };
  const host = await launch();
  listen = host; // restarts keep the same port (the browser's origin/cookie stay valid)
  writeConfig();
  const url = `http://${host}`;

  const call: ProductServer["call"] = async (method, path, opts = {}) => {
    const headers: Record<string, string> = {};
    if (opts.cookie) headers.Cookie = `scribe_session=${opts.cookie}`;
    if (opts.bearer) headers.Authorization = `Bearer ${opts.bearer}`;
    if (opts.csrf ?? method !== "GET") headers["X-Scribe-Request"] = "1";
    let body: BodyInit | undefined;
    if (opts.body instanceof Uint8Array) body = opts.body as Uint8Array<ArrayBuffer>;
    else if (typeof opts.body === "string") body = opts.body;
    else if (opts.body !== undefined) {
      body = JSON.stringify(opts.body);
      headers["Content-Type"] = "application/json";
    }
    const r = await fetch(url + path, { method, headers, body });
    const text = await r.text();
    let json: unknown = text;
    try {
      json = JSON.parse(text);
    } catch {
      // raw
    }
    return { status: r.status, json, setCookie: r.headers.get("set-cookie") };
  };

  const server: ProductServer = {
    url,
    host,
    setupCode,
    async serverId() {
      return ((await call("GET", "/control/v1/status")).json as { server_id: string }).server_id;
    },
    call,
    async claimAsHarness() {
      const r = await call("POST", "/control/v1/claim", { body: { setup_code: setupCode, display_name: "Admin browser" } });
      expect(r.status).toBe(200);
      return /scribe_session=([^;]+)/.exec(r.setCookie!)![1]!;
    },
    async pairKindle(adminCookie) {
      const c = (await call("POST", "/control/v1/pairing", { body: { client_type: "kindle", display_name: "Ignite" } })).json as {
        request_id: string;
        request_secret: string;
      };
      expect((await call("POST", `/control/v1/admin/pairing/${c.request_id}/approve`, { cookie: adminCookie })).status).toBe(200);
      const p = (await call("POST", `/control/v1/pairing/${c.request_id}/poll`, { body: { request_secret: c.request_secret } })).json as { credential: string };
      return p.credential;
    },
    async seedFixture(bearer) {
      const blob = new Uint8Array(readFileSync(join(FIX, manifest.blob.file)));
      expect((await call("PUT", `/v1/blobs/${manifest.blob.sha256}`, { bearer, body: blob, csrf: false })).status).toBeLessThan(300);
      expect((await call("POST", "/v1/libraries", { bearer, body: snapshotText, csrf: false })).status).toBe(201);
    },
    async head(bearer) {
      return ((await call("GET", `/v1/libraries/${manifest.library_id}`, { bearer })).json as { head_server_seq: number }).head_server_seq;
    },
    async operations(bearer) {
      const r = await fetch(`${url}/v1/libraries/${manifest.library_id}/operations?after=0&limit=500`, { headers: { Authorization: `Bearer ${bearer}` } });
      const text = await r.text();
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
    async restart() {
      await new Promise<void>((ok) => {
        if (child.exitCode !== null || child.signalCode !== null) return ok();
        child.once("exit", () => ok());
        child.kill();
      });
      await launch();
    },
  };
  return {
    server,
    stop: () => {
      child.kill();
      try {
        rmSync(dir, { recursive: true, force: true });
      } catch {
        // Windows may hold files briefly after kill
      }
    },
  };
}

export const test = base.extend<{ product: ProductServer }>({
  product: async ({}, use) => {
    const s = await startProduct();
    await use(s.server);
    s.stop();
  },
});

export { expect };

/** Give a browser context the admin session (as if it had claimed the server). */
export async function asAdmin(context: BrowserContext, product: ProductServer, cookie: string): Promise<void> {
  await context.addCookies([{ name: "scribe_session", value: cookie, url: product.url, httpOnly: true, sameSite: "Strict" }]);
}
