// Slice 6C: Web → disposable server acceptance for Blank/Ruled/Grid/Dotted.
// Creates notebooks through the UI, checks CreateNotebook + blob SHAs against Rust goldens,
// snapshot survival across server restart, insert-page inheritance, offline/reconnect.

import { createHash } from "node:crypto";
import { readFileSync } from "node:fs";
import { join } from "node:path";
import { gunzipSync } from "node:zlib";
import type { Page } from "@playwright/test";
import { canvasInfo, connect, expect, test, WEB_DIR, type SyncServer } from "./harness";
import { envelope, synced } from "./ink";

const FIX = join(WEB_DIR, "tests/fixtures");
const goldens = JSON.parse(readFileSync(join(FIX, "paper-templates-v1.json"), "utf8")) as {
  templates: Record<
    "blank" | "ruled" | "grid" | "dotted",
    { sha256: string; byte_len: number; width: number; height: number; stride: number; encoding: string; file: string }
  >;
};

type Paper = "blank" | "ruled" | "grid" | "dotted";

type Bg =
  | { kind: "blank" }
  | { kind: "blob"; sha256: string; byte_len: number; format: { encoding: string; width: number; height: number; stride: number } };

type CreateOp = {
  type: string;
  title: string;
  first_page: { page_id: string; background: Bg };
};

async function createNotebook(page: Page, paper: Paper, title: string) {
  const sidebar = page.getByTestId("notebooks");
  await sidebar.getByTestId("new-notebook").click();
  const dialog = page.getByTestId("notebook-dialog");
  await dialog.getByTestId(`paper-${paper}`).click();
  await dialog.getByTestId("notebook-name-input").fill(title);
  await dialog.getByTestId("dialog-submit").click();
  await expect(dialog).toHaveCount(0);
}

async function fetchBlob(server: SyncServer, sha: string): Promise<{ status: number; bytes: Uint8Array }> {
  const r = await fetch(`${server.url}/v1/blobs/${sha}`, {
    headers: { Authorization: `Bearer ${server.token}` },
  });
  return { status: r.status, bytes: new Uint8Array(await r.arrayBuffer()) };
}

function sha256(bytes: Uint8Array): string {
  return createHash("sha256").update(bytes).digest("hex");
}

function goldenBytes(paper: Exclude<Paper, "blank">): Uint8Array {
  return new Uint8Array(gunzipSync(readFileSync(join(FIX, goldens.templates[paper].file))));
}

function createOps(ops: { envelope: string }[]): CreateOp[] {
  return ops
    .map((o) => envelope(o.envelope).operation as unknown as CreateOp)
    .filter((op) => op.type === "create_notebook");
}

test("Web creates all four templates; blobs match Rust goldens; snapshot survives restart; insert inherits", async ({
  page,
  server,
}) => {
  test.setTimeout(180_000);
  await server.seedFixture();
  await page.setViewportSize({ width: 1280, height: 900 });
  await connect(page, server);

  const titles: Record<Paper, string> = {
    blank: "Accept Blank",
    ruled: "Accept Ruled",
    grid: "Accept Grid",
    dotted: "Accept Dotted",
  };
  const order: Paper[] = ["blank", "ruled", "grid", "dotted"];
  for (let i = 0; i < order.length; i++) {
    const paper = order[i]!;
    await createNotebook(page, paper, titles[paper]);
    await synced(page, server, i + 1);
  }

  const ops = await server.operations();
  const creates = createOps(ops);
  expect(creates.map((c) => c.title)).toEqual(order.map((p) => titles[p]));

  // 1–3: CreateNotebook backgrounds + retrievable patterned blobs vs Rust goldens.
  expect(creates[0]!.first_page.background).toEqual({ kind: "blank" });
  for (const paper of ["ruled", "grid", "dotted"] as const) {
    const create = creates.find((c) => c.title === titles[paper])!;
    const bg = create.first_page.background;
    expect(bg.kind).toBe("blob");
    if (bg.kind !== "blob") continue;
    const g = goldens.templates[paper];
    expect(bg.sha256).toBe(g.sha256);
    expect(bg.byte_len).toBe(g.byte_len);
    expect(bg.format).toEqual({
      encoding: "gray8",
      width: g.width,
      height: g.height,
      stride: g.stride,
    });
    const got = await fetchBlob(server, bg.sha256);
    expect(got.status).toBe(200);
    expect(got.bytes.length).toBe(g.byte_len);
    expect(sha256(got.bytes)).toBe(g.sha256);
    expect(Buffer.from(got.bytes).equals(Buffer.from(goldenBytes(paper)))).toBe(true);
  }

  // Shared content-addressed SHA: a second Ruled notebook reuses the same blob object.
  await createNotebook(page, "ruled", "Accept Ruled 2");
  await synced(page, server, 5);
  const ruledCreates = createOps(await server.operations()).filter((c) => c.title.startsWith("Accept Ruled"));
  expect(ruledCreates).toHaveLength(2);
  const shaA = (ruledCreates[0]!.first_page.background as Extract<Bg, { kind: "blob" }>).sha256;
  const shaB = (ruledCreates[1]!.first_page.background as Extract<Bg, { kind: "blob" }>).sha256;
  expect(shaA).toBe(shaB);
  expect(shaA).toBe(goldens.templates.ruled.sha256);

  // 4: Server restart preserves snapshot background refs.
  const before = await server.snapshot();
  await server.halt();
  await server.resume();
  const after = await server.snapshot();
  expect(after.snapshot_sha256).toBe(before.snapshot_sha256);
  expect(after.through_server_seq).toBe(before.through_server_seq);
  const afterNotebooks = after.snapshot.notebooks as unknown as { title: string; pages: { background: Bg }[] }[];
  for (const paper of order) {
    const nb = afterNotebooks.find((n) => n.title === titles[paper]);
    expect(nb, paper).toBeTruthy();
    const page0 = nb!.pages[0]!;
    if (paper === "blank") {
      expect(page0.background).toEqual({ kind: "blank" });
    } else {
      expect(page0.background).toMatchObject({ kind: "blob", sha256: goldens.templates[paper].sha256 });
      const again = await fetchBlob(server, goldens.templates[paper].sha256);
      expect(again.status).toBe(200);
      expect(sha256(again.bytes)).toBe(goldens.templates[paper].sha256);
    }
  }

  // Reload against the restarted server (token still in sessionStorage → already connected).
  await page.reload();
  const sidebar = page.getByTestId("notebooks");
  await expect(sidebar.getByTestId("notebook-item").filter({ hasText: titles.grid })).toBeVisible({ timeout: 20_000 });
  await sidebar.locator('[data-testid="notebook-item"]', { hasText: titles.grid }).click();
  await expect.poll(async () => (await canvasInfo(page)).rendered).toBe(true);
  // PageView records background kind on the canvas (`blank` | `blob`), not the SHA.
  await expect.poll(async () => (await canvasInfo(page)).background).toBe("blob");

  // 8: Insert page inherits patterned background (no second blob PUT).
  const putsBefore = (await server.operations()).length;
  const putPaths: string[] = [];
  page.on("request", (req) => {
    const u = new URL(req.url());
    if (req.method() === "PUT" && u.pathname.startsWith("/v1/blobs/")) putPaths.push(u.pathname);
  });
  await page.getByTestId("insert-page").click();
  await synced(page, server, putsBefore + 1);
  expect(putPaths).toEqual([]);
  const snap = await server.snapshot();
  const snapNotebooks = snap.snapshot.notebooks as unknown as { title: string; pages: { background: Bg }[] }[];
  const gridNb = snapNotebooks.find((n) => n.title === titles.grid)!;
  expect(gridNb.pages).toHaveLength(2);
  expect(gridNb.pages[0]!.background).toEqual(gridNb.pages[1]!.background);
  expect(gridNb.pages[1]!.background).toMatchObject({ kind: "blob", sha256: goldens.templates.grid.sha256 });
  await expect(page.getByTestId("page-label")).toHaveText("2 of 2");
});

test("offline: Blank queues; patterned upload failure is safe; reconnect retry works; queued ops survive", async ({
  page,
  server,
}) => {
  test.setTimeout(120_000);
  await server.seedFixture();
  await page.setViewportSize({ width: 1280, height: 900 });
  await connect(page, server);
  const sidebar = page.getByTestId("notebooks");
  await expect(sidebar.getByTestId("notebook-item")).toHaveCount(2);
  const baseline = 2;

  // Patterned when blob PUT cannot complete (server still serves GETs): no CreateNotebook, no phantom.
  await page.route("**/v1/blobs/**", (route) => {
    if (route.request().method() === "PUT") return route.abort("connectionfailed");
    return route.continue();
  });
  await createNotebook(page, "dotted", "Offline Dotted");
  await expect(page.getByTestId("save-error")).toContainText(/upload|connection|background/i, { timeout: 20_000 });
  await expect(sidebar.getByTestId("notebook-item")).toHaveCount(baseline);
  expect(await server.operations()).toHaveLength(0);
  await page.unroute("**/v1/blobs/**");

  // Blank follows existing offline outbox semantics (halt/resume + online event, like stroke tests).
  await server.halt();
  await createNotebook(page, "blank", "Offline Blank");
  await expect(sidebar.getByTestId("notebook-item")).toHaveCount(baseline + 1);
  await expect(page.getByTestId("sync-status")).toHaveAttribute("data-pending", "1", { timeout: 15_000 });

  await server.resume();
  await page.evaluate(() => window.dispatchEvent(new Event("online")));
  await synced(page, server, 1);
  const blankCreate = createOps(await server.operations())[0]!;
  expect(blankCreate.title).toBe("Offline Blank");
  expect(blankCreate.first_page.background).toEqual({ kind: "blank" });

  // Retry patterned after upload path works — single successful notebook, no duplicate.
  await createNotebook(page, "dotted", "Retry Dotted");
  await synced(page, server, 2);
  const creates = createOps(await server.operations());
  expect(creates.map((c) => c.title)).toEqual(["Offline Blank", "Retry Dotted"]);
  const dottedBg = creates[1]!.first_page.background;
  expect(dottedBg).toMatchObject({ kind: "blob", sha256: goldens.templates.dotted.sha256 });
  const blob = await fetchBlob(server, goldens.templates.dotted.sha256);
  expect(blob.status).toBe(200);
  expect(sha256(blob.bytes)).toBe(goldens.templates.dotted.sha256);

  await expect(sidebar.locator('[data-testid="notebook-item"]', { hasText: "Offline Dotted" })).toHaveCount(0);
  await expect(sidebar.locator('[data-testid="notebook-item"]', { hasText: "Retry Dotted" })).toHaveCount(1);
  await expect(sidebar.locator('[data-testid="notebook-item"]', { hasText: "Offline Blank" })).toHaveCount(1);
});
