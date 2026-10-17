// Folders: the browser authors folder operations through the one sync path, the real server's
// reducer accepts them, and every browser converges to the server's grouping.

import { connect, expect, manifest, test } from "./harness";
import { envelope, opPosts, synced, writes } from "./ink";

test("create a folder, file a notebook, rename, change/remove, delete — synced and converged", async ({ page, server, traffic, browser }) => {
  await server.seedFixture();
  await page.setViewportSize({ width: 1280, height: 900 });
  await connect(page, server);
  const sidebar = page.getByTestId("notebooks");
  await expect(sidebar.getByTestId("notebook-item")).toHaveCount(2);
  // Viewing never writes; the sidebar shows no folders yet.
  expect(writes(traffic)).toEqual([]);
  await expect(sidebar.getByTestId("folder-item")).toHaveCount(0);

  // New folder (a name dialog, like the Kindle's naming flow).
  await sidebar.getByTestId("new-folder").click();
  const dialog = page.getByTestId("folder-dialog");
  await dialog.getByTestId("folder-name-input").fill("Journals");
  await dialog.getByTestId("dialog-submit").click();
  await expect(dialog).toHaveCount(0);
  await expect(sidebar.getByTestId("folder-item")).toHaveCount(1);
  await synced(page, server, 1);

  // Add "Sketches" to it.
  const sketches = sidebar.locator('[data-testid="notebook-item"]', { hasText: "Sketches" });
  await sketches.locator("xpath=..").getByTestId("notebook-menu").click();
  await page.getByTestId("notebook-dialog").getByTestId("action-add-to-folder").click();
  await dialog.getByTestId("pick-folder").filter({ hasText: "Journals" }).click();
  await synced(page, server, 2);

  const ops = (await server.operations()).map((o) => envelope(o.envelope));
  expect(ops.map((o) => o.operation.type)).toEqual(["create_folder", "set_notebook_folder"]);
  const folderId = (ops[0]!.operation as unknown as { folder_id: string }).folder_id;
  expect(opPosts(traffic).length).toBeGreaterThanOrEqual(1);

  // The server's canonical snapshot groups it; the browser shows exactly that.
  const snap = (await server.snapshot()).snapshot as unknown as {
    folders: { folder_id: string; title: string }[];
    notebooks: { title: string; folder_id?: string }[];
  };
  expect(snap.folders).toEqual([{ folder_id: folderId, title: "Journals" }]);
  expect(snap.notebooks.find((n) => n.title === "Sketches")!.folder_id).toBe(folderId);
  expect(snap.notebooks.find((n) => n.title === "Field Notes")!.folder_id).toBeUndefined();
  const folder = sidebar.getByTestId("folder-item");
  await folder.getByTestId("folder-toggle").click();
  await expect(folder.getByTestId("notebook-item")).toHaveText(/Sketches/);
  await expect(sidebar.locator('[data-testid="notebook-item"]:not([data-folder-id])')).toHaveText([/Field Notes/]);

  // A second browser converges to the same grouping from the server alone.
  const otherContext = await browser.newContext();
  const other = await otherContext.newPage();
  await other.setViewportSize({ width: 1280, height: 900 });
  await connect(other, server);
  const otherFolder = other.getByTestId("notebooks").getByTestId("folder-item");
  await expect(otherFolder).toHaveCount(1);
  await expect(otherFolder).toContainText("Journals");
  await otherFolder.getByTestId("folder-toggle").click();
  await expect(otherFolder.getByTestId("notebook-item")).toHaveText(/Sketches/);
  await otherContext.close(); // never leave a polling browser behind for later tests

  // Rename.
  await folder.getByTestId("folder-menu").click();
  await dialog.getByTestId("action-rename").click();
  await dialog.getByTestId("folder-name-input").fill("Diaries");
  await dialog.getByTestId("dialog-submit").click();
  await synced(page, server, 3);
  await expect(folder).toContainText("Diaries");

  // Remove from folder → back on the main list.
  await folder.getByTestId("notebook-menu").click();
  await page.getByTestId("notebook-dialog").getByTestId("action-remove-from-folder").click();
  await synced(page, server, 4);
  await expect(sidebar.locator('[data-testid="notebook-item"]:not([data-folder-id])')).toHaveCount(2);

  // File it again, then delete the folder: its notebook returns, nothing is deleted.
  await sketches.locator("xpath=..").getByTestId("notebook-menu").click();
  await page.getByTestId("notebook-dialog").getByTestId("action-add-to-folder").click();
  await dialog.getByTestId("pick-folder").click();
  await synced(page, server, 5);
  await folder.getByTestId("folder-menu").click();
  await dialog.getByTestId("action-delete").click();
  await dialog.getByTestId("confirm-delete").click();
  await synced(page, server, 6);
  await expect(sidebar.getByTestId("folder-item")).toHaveCount(0);
  await expect(sidebar.getByTestId("notebook-item")).toHaveCount(2);
  const end = (await server.snapshot()).snapshot as unknown as { folders?: unknown; notebooks: { folder_id?: string }[] };
  expect(end.folders).toBeUndefined(); // empty folder list is omitted on the wire
  expect(end.notebooks.every((n) => n.folder_id === undefined)).toBe(true);
  expect((await server.snapshot()).snapshot).toBeTruthy();
  expect(manifest.library_id).toBeTruthy();
});
