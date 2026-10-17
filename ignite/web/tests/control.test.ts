import { readFileSync } from "node:fs";
import { join } from "node:path";
import { describe, expect, it } from "vitest";
import { ControlError, createControlApi, defaultBrowserName, type FetchLike } from "../src/api/controlApi";

function recorder(responses: Record<string, { status: number; body: string }>) {
  const seen: { url: string; init: RequestInit }[] = [];
  const fetchFn: FetchLike = async (url, init) => {
    seen.push({ url, init });
    const r = responses[url] ?? { status: 404, body: "{}" };
    return new Response(r.body, { status: r.status });
  };
  return { seen, fetchFn };
}

const STATUS = JSON.stringify({
  v: 1,
  mode: "product",
  server_id: "01a0de1f-f9d6-7036-a2b7-478dc58007ff",
  server_name: "Scribe Server on Vega",
  version: "0.1.0",
  claimed: true,
  session: null,
});

describe("D1 control API client", () => {
  it("detects product mode, and developer mode when there is no control plane", async () => {
    const product = createControlApi(recorder({ "/control/v1/status": { status: 200, body: STATUS } }).fetchFn);
    expect((await product.status())?.server_name).toBe("Scribe Server on Vega");
    const dev404 = createControlApi(recorder({}).fetchFn);
    expect(await dev404.status()).toBeNull();
    const devStatic = createControlApi(recorder({ "/control/v1/status": { status: 200, body: "<!doctype html>" } }).fetchFn);
    expect(await devStatic.status()).toBeNull();
  });

  it("mutations are same-origin /control/v1 POSTs with the CSRF marker and never a token", async () => {
    const r = recorder({
      "/control/v1/claim": { status: 200, body: '{"device_id":"d","role":"admin","display_name":"x"}' },
      "/control/v1/pairing": { status: 201, body: '{"request_id":"r","request_secret":"s","comparison_code":"482 731","expires_in_s":300}' },
      "/control/v1/pairing/r/poll": { status: 200, body: '{"state":"pending","server_id":"x"}' },
    });
    const api = createControlApi(r.fetchFn);
    await api.claim("K7M4-P2Q9", "Firefox on Windows");
    const created = await api.requestAccess("Browser on Bobby");
    expect(created.comparison_code).toBe("482 731");
    expect((await api.poll("r", "s")).state).toBe("pending");
    for (const s of r.seen) {
      expect(s.url.startsWith("/control/v1/")).toBe(true);
      expect(s.init.method).toBe("POST");
      expect(s.init.credentials).toBe("same-origin");
      const h = s.init.headers as Record<string, string>;
      expect(h["X-Scribe-Request"]).toBe("1");
      expect(h.Authorization).toBeUndefined();
    }
    expect(JSON.parse(r.seen[1]!.init.body as string)).toEqual({ client_type: "browser", display_name: "Browser on Bobby" });
  });

  it("surfaces server refusals as ControlError (message, not secrets)", async () => {
    const api = createControlApi(recorder({ "/control/v1/claim": { status: 403, body: '{"error":"wrong_setup_code","message":"That setup code is not valid."}' } }).fetchFn);
    const e = await api.claim("0000-0000", "x").catch((x: unknown) => x);
    expect(e).toBeInstanceOf(ControlError);
    expect((e as ControlError).status).toBe(403);
    expect((e as ControlError).message).toBe("That setup code is not valid.");
  });

  it("friendly default browser names", () => {
    expect(defaultBrowserName("Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:140.0) Gecko/20100101 Firefox/140.0")).toBe("Firefox on Windows");
    expect(defaultBrowserName("Mozilla/5.0 (Macintosh; Intel Mac OS X 14_5) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.5 Safari/605.1.15")).toBe("Safari on macOS");
    expect(defaultBrowserName("Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/139.0 Safari/537.36")).toBe("Chrome on Linux");
  });

  it("the control client never touches notebook APIs, tokens or browser storage", () => {
    const src = readFileSync(join(import.meta.dirname, "../src/api/controlApi.ts"), "utf8");
    expect(src).not.toMatch(/\/v1\/libraries|\/v1\/blobs|tokenStore|sessionStorage|localStorage|indexedDB|Authorization/);
    for (const f of ["components/ProductScreens.tsx", "components/ServerSettings.tsx"]) {
      const t = readFileSync(join(import.meta.dirname, "../src", f), "utf8");
      expect(t, f).not.toMatch(/\bfetch\s*\(|tokenStore|sessionStorage|localStorage|indexedDB/);
    }
  });
});
