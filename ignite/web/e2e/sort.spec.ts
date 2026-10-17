// Home/library sort button: one cycling control (Creation → Alphabetical → Recently Accessed),
// presentation only. The mode and this browser's notebook-open recency live in the UI
// preference store (`scribe-web-prefs`); neither sorting nor opening a notebook ever writes to
// the server. (Alphabetical/creation ordering details are covered by tests/librarySort.test.ts.)

import { connect, expect, test } from "./harness";
import { writes } from "./ink";

test("sort cycles with toasts, persists silently, orders by recency; no sync writes", async ({ page, server, traffic }) => {
  await server.seedFixture();
  await page.setViewportSize({ width: 1280, height: 900 });
  await connect(page, server);
  const sidebar = page.getByTestId("notebooks");
  const titles = () => sidebar.locator('[data-testid="notebook-item"]:not([data-folder-id]) .notebook-title');
  const open = (title: string) => sidebar.locator('[data-testid="notebook-item"]', { hasText: title }).click();
  await expect(titles()).toHaveText([/Field Notes/, /Sketches/]);

  // Placement: directly after New notebook / New folder; default Creation Order, no toast.
  const sort = sidebar.getByTestId("sort-order");
  const order = await sidebar.locator(".sidebar-actions > *").evaluateAll((els) => els.map((e) => e.getAttribute("data-testid")));
  expect(order).toEqual(["new-notebook", "new-folder", "sort-order"]);
  await expect(sort).toHaveAttribute("data-sort-mode", "creation");
  await expect(sort).toHaveAttribute("aria-label", "Change sort order — Creation Order");
  const toast = page.getByTestId("sort-toast");
  await expect(toast).toHaveCount(0);

  await sort.click();
  await expect(toast).toHaveText("Alphabetical Order");
  await expect(sort).toHaveAttribute("data-sort-mode", "alphabetical");
  await expect(titles()).toHaveText([/Field Notes/, /Sketches/]);

  await sort.click();
  await expect(toast).toHaveText("Recently Accessed");
  await expect(sort).toHaveAttribute("data-sort-mode", "recent");
  // Nothing opened yet (the load's auto-selection is not an open) → library order.
  await expect(titles()).toHaveText([/Field Notes/, /Sketches/]);
  await open("Sketches");
  await expect(titles()).toHaveText([/Sketches/, /Field Notes/]);
  await open("Field Notes");
  await expect(titles()).toHaveText([/Field Notes/, /Sketches/]);
  await open("Sketches");
  await expect(titles()).toHaveText([/Sketches/, /Field Notes/]);

  await sort.click();
  await expect(toast).toHaveText("Creation Order");
  await expect(sort).toHaveAttribute("data-sort-mode", "creation");
  await expect(titles()).toHaveText([/Field Notes/, /Sketches/]);

  // Back to Recently Accessed, then reload: restored silently with history intact, and the
  // notebook the reload auto-selects (Field Notes, first in library order) is not promoted.
  await sort.click();
  await sort.click();
  await expect(sort).toHaveAttribute("data-sort-mode", "recent");
  await expect(toast).toHaveCount(0);
  await page.reload();
  await expect(sort).toHaveAttribute("data-sort-mode", "recent");
  await expect(page.getByTestId("page-label")).toHaveText("1 of 3"); // Field Notes is shown
  await expect(titles()).toHaveText([/Sketches/, /Field Notes/]);
  await expect(toast).toHaveCount(0);

  // Sorting and opening notebooks never wrote anything to the server.
  expect(writes(traffic)).toEqual([]);
  expect(traffic.consoleErrors).toEqual([]);
});
