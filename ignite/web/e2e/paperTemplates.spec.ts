// Slice 6B: Select Paper → title → Create; patterned PUT before CreateNotebook; cancel; blank.

import { connect, expect, test, testPaperGateOff } from "./harness";
import { envelope, synced } from "./ink";

test("select paper cancel does not mutate notebooks", async ({ page, server }) => {
  await server.seedFixture();
  await page.setViewportSize({ width: 1280, height: 900 });
  await connect(page, server);
  const sidebar = page.getByTestId("notebooks");
  await expect(sidebar.getByTestId("notebook-item")).toHaveCount(2);

  await sidebar.getByTestId("new-notebook").click();
  const dialog = page.getByTestId("notebook-dialog");
  await expect(dialog.getByTestId("select-paper")).toBeVisible();
  await dialog.getByTestId("paper-cancel").click();
  await expect(dialog).toHaveCount(0);
  await expect(sidebar.getByTestId("notebook-item")).toHaveCount(2);
  expect(await server.operations()).toHaveLength(0);
});

test("title and folder context survive template selection", async ({ page, server }) => {
  await server.seedFixture();
  await page.setViewportSize({ width: 1280, height: 900 });
  await connect(page, server);
  const sidebar = page.getByTestId("notebooks");

  await sidebar.getByTestId("new-folder").click();
  const folderDialog = page.getByTestId("folder-dialog");
  await folderDialog.getByTestId("folder-name-input").fill("Papers");
  await folderDialog.getByTestId("dialog-submit").click();
  await synced(page, server, 1);

  await sidebar.getByTestId("folder-menu").click();
  await folderDialog.getByTestId("action-new-notebook").click();
  const nbDialog = page.getByTestId("notebook-dialog");
  await nbDialog.getByTestId("paper-ruled").click();
  await expect(nbDialog.getByTestId("notebook-name-input")).toHaveValue("Notebook");
  await nbDialog.getByTestId("notebook-name-input").fill("Ruled filed");
  await nbDialog.getByTestId("dialog-submit").click();
  await expect(nbDialog).toHaveCount(0);
  await synced(page, server, 3); // create notebook + set folder (blob PUT is not an op)

  const folder = sidebar.getByTestId("folder-item");
  await expect(folder.getByTestId("notebook-item")).toHaveText(/Ruled filed/);

  const ops = await server.operations();
  const types = ops.map((o) => envelope(o.envelope).operation.type);
  expect(types).toEqual(["create_folder", "create_notebook", "set_notebook_folder"]);
  const create = envelope(ops[1]!.envelope).operation as unknown as {
    type: string;
    first_page: { background: { kind: string; sha256?: string } };
  };
  expect(create.first_page.background.kind).toBe("blob");
  expect(create.first_page.background.sha256).toMatch(/^[0-9a-f]{64}$/);
});

test("blank create skips blob PUT; patterned PUT precedes CreateNotebook", async ({ page, server }) => {
  await server.seedFixture();
  await page.setViewportSize({ width: 1280, height: 900 });
  await connect(page, server);
  const sidebar = page.getByTestId("notebooks");

  const puts: string[] = [];
  page.on("request", (req) => {
    const u = new URL(req.url());
    if (req.method() === "PUT" && u.pathname.startsWith("/v1/blobs/")) puts.push(u.pathname);
  });

  await sidebar.getByTestId("new-notebook").click();
  let dialog = page.getByTestId("notebook-dialog");
  await dialog.getByTestId("paper-blank").click();
  await dialog.getByTestId("notebook-name-input").fill("Plain");
  await dialog.getByTestId("dialog-submit").click();
  await synced(page, server, 1);
  expect(puts).toEqual([]);
  const blankOp = envelope((await server.operations())[0]!.envelope).operation as unknown as {
    first_page: { background: { kind: string } };
  };
  expect(blankOp.first_page.background.kind).toBe("blank");

  await sidebar.getByTestId("new-notebook").click();
  dialog = page.getByTestId("notebook-dialog");
  await dialog.getByTestId("paper-grid").click();
  await dialog.getByTestId("notebook-name-input").fill("Grid book");
  await dialog.getByTestId("dialog-submit").click();
  await synced(page, server, 2);
  expect(puts.length).toBe(1);
  expect(puts[0]).toMatch(/^\/v1\/blobs\/[0-9a-f]{64}$/);

  const ops = await server.operations();
  const gridOp = envelope(ops[1]!.envelope).operation as unknown as {
    first_page: { background: { kind: string; sha256: string } };
  };
  expect(gridOp.first_page.background.kind).toBe("blob");
  expect(puts[0]).toBe(`/v1/blobs/${gridOp.first_page.background.sha256}`);
});

test("change paper this page ignores overlapping confirmation", async ({ page, server }) => {
  await server.seedFixture();
  await page.setViewportSize({ width: 1280, height: 900 });
  await connect(page, server);
  const sidebar = page.getByTestId("notebooks");
  await sidebar.getByTestId("new-notebook").click();
  const dialog = page.getByTestId("notebook-dialog");
  await dialog.getByTestId("paper-blank").click();
  await dialog.getByTestId("notebook-name-input").fill("Once book");
  await dialog.getByTestId("dialog-submit").click();
  await synced(page, server, 1);

  await sidebar.locator('[data-testid="notebook-item"]', { hasText: "Once book" }).locator("xpath=..").getByTestId("notebook-menu").click();
  await dialog.getByTestId("action-change-paper").click();
  await dialog.getByTestId("paper-ruled").click();
  await dialog.getByTestId("paper-this-page").click();
  const confirm = dialog.getByTestId("confirm-change-paper");
  await confirm.click();
  await confirm.click({ timeout: 250 }).catch(() => undefined);
  await synced(page, server, 2);
  const types = (await server.operations()).map((o) => envelope(o.envelope).operation.type);
  expect(types).toEqual(["create_notebook", "set_page_background"]);
});

test("change paper this page publishes patterned blob then SetPageBackground", async ({ page, server }) => {
  await server.seedFixture();
  await page.setViewportSize({ width: 1280, height: 900 });
  await connect(page, server);
  const sidebar = page.getByTestId("notebooks");
  const puts: string[] = [];
  page.on("request", (req) => {
    const u = new URL(req.url());
    if (req.method() === "PUT" && u.pathname.startsWith("/v1/blobs/")) puts.push(u.pathname);
  });

  await sidebar.getByTestId("new-notebook").click();
  const dialog = page.getByTestId("notebook-dialog");
  await dialog.getByTestId("paper-blank").click();
  await dialog.getByTestId("notebook-name-input").fill("Paper book");
  await dialog.getByTestId("dialog-submit").click();
  await synced(page, server, 1);

  await sidebar.locator('[data-testid="notebook-item"]', { hasText: "Paper book" }).locator("xpath=..").getByTestId("notebook-menu").click();
  await dialog.getByTestId("action-change-paper").click();
  await expect(dialog.getByTestId("change-paper")).toBeVisible();
  await dialog.getByTestId("paper-ruled").click();
  await dialog.getByTestId("paper-this-page").click();
  await dialog.getByTestId("confirm-change-paper").click();
  await synced(page, server, 2);

  expect(puts).toHaveLength(1);
  const ops = await server.operations();
  const types = ops.map((o) => envelope(o.envelope).operation.type);
  expect(types).toEqual(["create_notebook", "set_page_background"]);
  const change = envelope(ops[1]!.envelope).operation as unknown as {
    type: string;
    background: { kind: string; sha256?: string };
  };
  expect(change.background.kind).toBe("blob");
  expect(puts[0]).toBe(`/v1/blobs/${change.background.sha256}`);
});

test("change paper all pages queues one SetPageBackground per page", async ({ page, server }) => {
  await server.seedFixture();
  await page.setViewportSize({ width: 1280, height: 900 });
  await connect(page, server);
  const sidebar = page.getByTestId("notebooks");
  await sidebar.getByTestId("new-notebook").click();
  const dialog = page.getByTestId("notebook-dialog");
  await dialog.getByTestId("paper-blank").click();
  await dialog.getByTestId("notebook-name-input").fill("All pages book");
  await dialog.getByTestId("dialog-submit").click();
  await synced(page, server, 1);
  await page.getByTestId("insert-page").click();
  await synced(page, server, 2);

  await sidebar.locator('[data-testid="notebook-item"]', { hasText: "All pages book" }).locator("xpath=..").getByTestId("notebook-menu").click();
  await dialog.getByTestId("action-change-paper").click();
  await dialog.getByTestId("paper-grid").click();
  await dialog.getByTestId("paper-all-pages").click();
  await dialog.getByTestId("confirm-change-paper").click();
  await synced(page, server, 4);
  const types = (await server.operations()).map((o) => envelope(o.envelope).operation.type);
  expect(types).toEqual(["create_notebook", "create_page", "set_page_background", "set_page_background"]);
});

test("change paper all pages cancel leaves prefix and does not imply rollback", async ({ page, server }) => {
  await server.seedFixture();
  await page.setViewportSize({ width: 1280, height: 900 });
  await connect(page, server);
  const sidebar = page.getByTestId("notebooks");
  await sidebar.getByTestId("new-notebook").click();
  const dialog = page.getByTestId("notebook-dialog");
  await dialog.getByTestId("paper-blank").click();
  await dialog.getByTestId("notebook-name-input").fill("Cancel book");
  await dialog.getByTestId("dialog-submit").click();
  await synced(page, server, 1);
  await page.getByTestId("insert-page").click();
  await synced(page, server, 2);
  await page.getByTestId("insert-page").click();
  await synced(page, server, 3);

  // Hold the patterned blob PUT so the progress dialog stays up long enough to cancel.
  // Three local commits otherwise finish before Playwright's click actionability check.
  let releaseBlob: () => void = () => undefined;
  const blobHeld = new Promise<void>((resolve) => {
    releaseBlob = resolve;
  });
  await page.route(/\/v1\/blobs\//, async (route) => {
    if (route.request().method() === "PUT") await blobHeld;
    await route.continue();
  });

  await sidebar.locator('[data-testid="notebook-item"]', { hasText: "Cancel book" }).locator("xpath=..").getByTestId("notebook-menu").click();
  await dialog.getByTestId("action-change-paper").click();
  await dialog.getByTestId("paper-dotted").click();
  await dialog.getByTestId("paper-all-pages").click();
  await dialog.getByTestId("confirm-change-paper").click();
  await expect(dialog.getByTestId("change-paper-progress")).toBeVisible();
  await dialog.getByTestId("cancel-paper-change").click();
  releaseBlob();
  await expect(page.getByTestId("save-error")).toBeVisible({ timeout: 15_000 });
  const notice = (await page.getByTestId("save-error").innerText()).toLowerCase();
  expect(notice).not.toMatch(/roll/);
  const types = (await server.operations()).map((o) => envelope(o.envelope).operation.type);
  const changed = types.filter((t) => t === "set_page_background").length;
  expect(changed).toBeLessThan(3);
  if (changed > 0) {
    expect(notice).toMatch(/stay changed|changed \d+ of/);
  } else {
    expect(notice).toMatch(/no pages were changed/);
  }
});

testPaperGateOff("change paper refuses when the server gate is off", async ({ page, server }) => {
  await server.seedFixture();
  await page.setViewportSize({ width: 1280, height: 900 });
  await connect(page, server);
  const sidebar = page.getByTestId("notebooks");
  await sidebar.getByTestId("new-notebook").click();
  const dialog = page.getByTestId("notebook-dialog");
  await dialog.getByTestId("paper-blank").click();
  await dialog.getByTestId("notebook-name-input").fill("Gate book");
  await dialog.getByTestId("dialog-submit").click();
  await synced(page, server, 1);

  await sidebar.locator('[data-testid="notebook-item"]', { hasText: "Gate book" }).locator("xpath=..").getByTestId("notebook-menu").click();
  await dialog.getByTestId("action-change-paper").click();
  await dialog.getByTestId("paper-ruled").click();
  await dialog.getByTestId("paper-this-page").click();
  await dialog.getByTestId("confirm-change-paper").click();
  await expect(page.getByTestId("save-error")).toContainText(/does not accept Change paper/i);
  const types = (await server.operations()).map((o) => envelope(o.envelope).operation.type);
  expect(types).toEqual(["create_notebook"]);
});

test("change paper cancel at picker does not mutate", async ({ page, server }) => {
  await server.seedFixture();
  await page.setViewportSize({ width: 1280, height: 900 });
  await connect(page, server);
  const sidebar = page.getByTestId("notebooks");
  await sidebar.getByTestId("notebook-menu").first().click();
  const dialog = page.getByTestId("notebook-dialog");
  await dialog.getByTestId("action-change-paper").click();
  await dialog.getByTestId("paper-cancel").click();
  await expect(dialog).toHaveCount(0);
  expect(await server.operations()).toHaveLength(0);
});
