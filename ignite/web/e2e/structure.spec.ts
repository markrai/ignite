// Browser notebook/page structure: create/rename/delete notebook, insert/delete page,
// create-in-folder (two ops, non-atomic), projected sole-page/notebook disable.

import { connect, expect, test } from "./harness";
import { envelope, synced } from "./ink";

test("create, rename, delete notebook; insert and delete page; sole rules; create-in-folder", async ({ page, server, browser }) => {
  await server.seedFixture();
  await page.setViewportSize({ width: 1280, height: 900 });
  await connect(page, server);
  const sidebar = page.getByTestId("notebooks");
  await expect(sidebar.getByTestId("notebook-item")).toHaveCount(2);
  await expect(sidebar.getByTestId("new-notebook")).toBeEnabled();
  await expect(sidebar.getByTestId("library-readonly")).toHaveCount(0);
  await expect(page.getByTestId("page-label")).toHaveText("1 of 3");

  await sidebar.getByTestId("new-notebook").click();
  const nbDialog = page.getByTestId("notebook-dialog");
  await nbDialog.getByTestId("notebook-name-input").fill("Lab book");
  await nbDialog.getByTestId("dialog-submit").click();
  await expect(nbDialog).toHaveCount(0);
  await expect(sidebar.getByTestId("notebook-item")).toHaveCount(3);
  await expect(sidebar.locator('[data-testid="notebook-item"]', { hasText: "Lab book" })).toHaveCount(1);
  await expect(page.getByTestId("page-label")).toHaveText("1 of 1");
  await synced(page, server, 1);

  await sidebar.locator('[data-testid="notebook-item"]', { hasText: "Lab book" }).locator("xpath=..").getByTestId("notebook-menu").click();
  await nbDialog.getByTestId("action-rename-notebook").click();
  await nbDialog.getByTestId("notebook-name-input").fill("Lab notes");
  await nbDialog.getByTestId("dialog-submit").click();
  await synced(page, server, 2);
  await expect(sidebar.locator('[data-testid="notebook-item"]', { hasText: "Lab notes" })).toHaveCount(1);

  await page.getByTestId("insert-page").click();
  await expect(page.getByTestId("page-label")).toHaveText("2 of 2");
  await synced(page, server, 3);

  await page.getByTestId("delete-page").click();
  await page.getByTestId("confirm-delete-page").click();
  await expect(page.getByTestId("page-label")).toHaveText("1 of 1");
  await synced(page, server, 4);
  await expect(page.getByTestId("delete-page")).toBeDisabled();

  // Create-in-folder: two ops; notebook appears under the folder.
  await sidebar.getByTestId("new-folder").click();
  const folderDialog = page.getByTestId("folder-dialog");
  await folderDialog.getByTestId("folder-name-input").fill("Work");
  await folderDialog.getByTestId("dialog-submit").click();
  await synced(page, server, 5);
  await sidebar.getByTestId("folder-menu").click();
  await folderDialog.getByTestId("action-new-notebook").click();
  await nbDialog.getByTestId("notebook-name-input").fill("Filed");
  await nbDialog.getByTestId("dialog-submit").click();
  await expect(sidebar.getByTestId("notebook-item")).toHaveCount(4);
  await synced(page, server, 7); // create notebook + set folder
  const folder = sidebar.getByTestId("folder-item");
  await expect(folder.getByTestId("notebook-item")).toHaveText(/Filed/);

  const otherContext = await browser.newContext();
  const other = await otherContext.newPage();
  await other.setViewportSize({ width: 1280, height: 900 });
  await connect(other, server);
  await expect(other.getByTestId("folder-item")).toHaveCount(1);
  await other.getByTestId("folder-toggle").click();
  await expect(other.getByTestId("notebook-item")).toHaveCount(4);
  await otherContext.close();

  // Delete Lab notes; remaining notebooks still allow delete until one left.
  await sidebar.locator('[data-testid="notebook-item"]', { hasText: "Lab notes" }).locator("xpath=..").getByTestId("notebook-menu").click();
  await nbDialog.getByTestId("action-delete-notebook").click();
  await nbDialog.getByTestId("confirm-delete-notebook").click();
  await synced(page, server, 8);
  await expect(sidebar.getByTestId("notebook-item")).toHaveCount(3);

  const types = (await server.operations()).map((o) => envelope(o.envelope).operation.type);
  expect(types).toEqual([
    "create_notebook",
    "rename_notebook",
    "create_page",
    "delete_page",
    "create_folder",
    "create_notebook",
    "set_notebook_folder",
    "delete_notebook",
  ]);
});
