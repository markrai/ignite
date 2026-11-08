import { describe, expect, it, vi } from "vitest";
import type { BlobApi } from "../src/api/blobApi";
import {
  changePaperTargets,
  PaperChangeRun,
  publishThenChangePaper,
} from "../src/paper/changePaperPublish";
import * as paperTemplates from "../src/paper/paperTemplates";
import type { BackgroundRef, SnapshotNotebook, SnapshotPage } from "../src/model/types";

const render = {
  profile_version: 2,
  ink_byte: 0,
  legacy_fixed_radius: 2,
  transform_id: "t",
  rasterizer_id: "r",
};

function page(id: string, background: BackgroundRef = { kind: "blank" }): SnapshotPage {
  return { page_id: id, background, render, strokes: [], texts: [], links: [] };
}

function notebook(pages: SnapshotPage[]): SnapshotNotebook {
  return { notebook_id: "nb-1", title: "Notes", pages };
}

const fakeBlob = {
  bytes: new Uint8Array([0xff, 0x80]),
  sha256: "b".repeat(64),
  background: {
    kind: "blob" as const,
    sha256: "b".repeat(64),
    byte_len: 2,
    format: { encoding: "gray8" as const, width: 1, height: 2, stride: 1 },
  },
};

describe("changePaperTargets", () => {
  const nb = notebook([page("p1"), page("p2"), page("p3")]);

  it("this page uses the current page when it belongs to the notebook", () => {
    expect(changePaperTargets(nb, "this", "p2").map((p) => p.page_id)).toEqual(["p2"]);
  });

  it("this page falls back to the first page", () => {
    expect(changePaperTargets(nb, "this", "missing").map((p) => p.page_id)).toEqual(["p1"]);
    expect(changePaperTargets(nb, "this", null).map((p) => p.page_id)).toEqual(["p1"]);
  });

  it("all pages is every page in order", () => {
    expect(changePaperTargets(nb, "all", "p2").map((p) => p.page_id)).toEqual(["p1", "p2", "p3"]);
  });
});

const acceptGate = async () => true;

describe("PaperChangeRun", () => {
  it("ignores overlapping confirmation and does not reset an in-flight cancel", () => {
    const run = new PaperChangeRun();
    expect(run.tryBegin()).toBe(true);
    run.cancel();
    expect(run.tryBegin()).toBe(false);
    expect(run.isCancelled()).toBe(true);
    run.finish();
    expect(run.tryBegin()).toBe(true);
    expect(run.isCancelled()).toBe(false);
  });
});

describe("publishThenChangePaper", () => {
  it("blank this-page queues one SetPageBackground without PUT", async () => {
    const put = vi.fn();
    const commits: unknown[] = [];
    const nb = notebook([page("p1"), page("p2")]);
    const out = await publishThenChangePaper({
      template: "blank",
      notebook: nb,
      scope: "this",
      currentPageId: "p2",
      blobApi: { put } satisfies BlobApi,
      stillValid: () => true,
      commit: async (intent) => {
        commits.push(intent);
        return true;
      },
      isOnline: () => false,
      setPageBackgroundAccepted: acceptGate,
    });
    expect(out).toEqual({ ok: true, notice: null, applied: 1 });
    expect(put).not.toHaveBeenCalled();
    expect(commits).toEqual([
      { type: "set_page_background", notebook_id: "nb-1", page_id: "p2", background: { kind: "blank" } },
    ]);
  });

  it("patterned PUT happens once before N page ops; cancel leaves prefix applied", async () => {
    const gen = vi.spyOn(paperTemplates, "generateTemplateBackground").mockResolvedValue(fakeBlob);
    const put = vi.fn(async (sha: string, bytes: Uint8Array) => ({
      sha256: sha,
      byte_len: bytes.length,
      created: true,
    }));
    const commits: string[] = [];
    const nb = notebook([page("p1"), page("p2"), page("p3")]);
    let cancelled = false;
    const out = await publishThenChangePaper({
      template: "ruled",
      notebook: nb,
      scope: "all",
      currentPageId: "p1",
      blobApi: { put },
      stillValid: () => true,
      commit: async (intent) => {
        commits.push(intent.page_id);
        if (intent.page_id === "p1") cancelled = true;
        return true;
      },
      isCancelled: () => cancelled,
      isOnline: () => true,
      setPageBackgroundAccepted: acceptGate,
    });
    expect(put).toHaveBeenCalledTimes(1);
    expect(put.mock.calls[0]![0]).toBe(fakeBlob.sha256);
    expect(commits).toEqual(["p1"]);
    expect(out.applied).toBe(1);
    expect(out.ok).toBe(true);
    expect(out.notice).toMatch(/Changed 1 of 3 pages/);
    expect(out.notice).toMatch(/stay changed/);
    gen.mockRestore();
  });

  it("refuses to queue when the server has not activated SetPageBackground", async () => {
    const commits: unknown[] = [];
    const out = await publishThenChangePaper({
      template: "blank",
      notebook: notebook([page("p1")]),
      scope: "this",
      currentPageId: "p1",
      blobApi: { put: vi.fn() } satisfies BlobApi,
      stillValid: () => true,
      commit: async (intent) => {
        commits.push(intent);
        return true;
      },
      setPageBackgroundAccepted: async () => false,
    });
    expect(out.ok).toBe(false);
    expect(out.applied).toBe(0);
    expect(out.notice).toMatch(/does not accept Change paper/);
    expect(commits).toEqual([]);
  });

  it("refuses to queue when the activation probe cannot reach the server", async () => {
    const commits: unknown[] = [];
    const out = await publishThenChangePaper({
      template: "blank",
      notebook: notebook([page("p1")]),
      scope: "this",
      currentPageId: "p1",
      blobApi: { put: vi.fn() } satisfies BlobApi,
      stillValid: () => true,
      commit: async (intent) => {
        commits.push(intent);
        return true;
      },
      setPageBackgroundAccepted: async () => {
        throw new Error("offline");
      },
    });
    expect(out.ok).toBe(false);
    expect(out.applied).toBe(0);
    expect(out.notice).toMatch(/connection to a server/);
    expect(commits).toEqual([]);
  });

  it("partial commit failure reports completed count without implying rollback", async () => {
    const out = await publishThenChangePaper({
      template: "blank",
      notebook: notebook([page("p1"), page("p2"), page("p3")]),
      scope: "all",
      currentPageId: "p1",
      blobApi: { put: vi.fn() } satisfies BlobApi,
      stillValid: () => true,
      commit: async (intent) => intent.page_id !== "p2",
      setPageBackgroundAccepted: acceptGate,
    });
    expect(out.ok).toBe(true);
    expect(out.applied).toBe(1);
    expect(out.notice).toMatch(/Changed 1 of 3 pages/);
    expect(out.notice).toMatch(/could not be changed/);
    expect(out.notice?.toLowerCase()).not.toMatch(/roll/);
  });

  it("offline patterned change does not queue ops", async () => {
    const gen = vi.spyOn(paperTemplates, "generateTemplateBackground").mockResolvedValue(fakeBlob);
    const put = vi.fn();
    const commits: unknown[] = [];
    const out = await publishThenChangePaper({
      template: "grid",
      notebook: notebook([page("p1")]),
      scope: "this",
      currentPageId: "p1",
      blobApi: { put },
      stillValid: () => true,
      commit: async (intent) => {
        commits.push(intent);
        return true;
      },
      isOnline: () => false,
      setPageBackgroundAccepted: acceptGate,
    });
    expect(out.ok).toBe(false);
    expect(out.notice).toMatch(/network connection/);
    expect(put).not.toHaveBeenCalled();
    expect(commits).toEqual([]);
    gen.mockRestore();
  });

  it("repeated confirmation during a multi-page run is ignored and cancel is preserved", async () => {
    const run = new PaperChangeRun();
    expect(run.tryBegin()).toBe(true);
    const commits: string[] = [];
    const out = await publishThenChangePaper({
      template: "blank",
      notebook: notebook([page("p1"), page("p2"), page("p3")]),
      scope: "all",
      currentPageId: "p1",
      blobApi: { put: vi.fn() } satisfies BlobApi,
      stillValid: () => true,
      commit: async (intent) => {
        commits.push(intent.page_id);
        expect(run.tryBegin()).toBe(false);
        if (intent.page_id === "p1") run.cancel();
        return true;
      },
      isCancelled: () => run.isCancelled(),
      setPageBackgroundAccepted: acceptGate,
    });
    expect(commits).toEqual(["p1"]);
    expect(out.applied).toBe(1);
    expect(out.notice).toMatch(/Changed 1 of 3 pages/);
    expect(out.notice).toMatch(/stay changed/);
    expect(run.isCancelled()).toBe(true);
    run.finish();
  });
});
