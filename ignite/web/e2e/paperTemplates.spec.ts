// Slice 6B: Select Paper → title → Create; patterned PUT before CreateNotebook; cancel; blank.

import { connect, expect, test } from "./harness";
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
