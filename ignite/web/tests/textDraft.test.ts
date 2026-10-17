import { describe, expect, it } from "vitest";
import {
  abortFinish,
  beginFinish,
  buildMoveUpdate,
  completeFinish,
  finishTextEdit,
  newDraft,
  placeTextBox,
  editDraft,
  runDelete,
  runFinish,
} from "../src/authoring/textDraft";
import { CANONICAL_PAGE_HEIGHT, CANONICAL_PAGE_WIDTH } from "../src/model/generated/protocolConstants";
import type { SyncText } from "../src/model/types";

function existing(over: Partial<SyncText> = {}) {
  return editDraft({
    text_id: "t1",
    x: 1000,
    y: 2000,
    width: 5000,
    font_size: 340,
    face: "default-sans-v1",
    content: "keep",
    ...over,
  });
}

describe("placeTextBox", () => {
  it("uses a paragraph width while preserving an interior click anchor", () => {
    const box = placeTextBox(2000, 100);
    expect(box).toEqual({ x: Math.fround(2000), y: Math.fround(100), width: Math.fround(10_000) });
  });

  it("preserves a near-right click anchor and uses only the remaining width", () => {
    const x = CANONICAL_PAGE_WIDTH - 10;
    const near = placeTextBox(x, 100);
    expect(near.x).toBe(Math.fround(x));
    expect(near.width).toBe(Math.fround(10));
    expect(near.x + near.width).toBeLessThanOrEqual(CANONICAL_PAGE_WIDTH);
  });

  it("editing a stored block preserves its exact legacy width", () => {
    expect(existing({ width: 4321 }).current.width).toBe(4321);
  });

  it("new drafts inherit an explicit Text-session font size", () => {
    expect(newDraft("t1", 100, 200, 480).current.font_size).toBe(480);
  });
});

describe("finishTextEdit", () => {
  it("adds once and ignores a second call", () => {
    const d = newDraft("t1", 100, 200);
    d.current.content = "hello";
    expect(finishTextEdit(d)).toEqual({ kind: "add", text: d.current });
    expect(finishTextEdit(d)).toEqual({ kind: "busy" });
  });

  it("deletes an emptied existing object", () => {
    const d = editDraft({
      text_id: "t1",
      x: 1,
      y: 2,
      width: 100,
      font_size: 340,
      face: "default-sans-v1",
      content: "keep",
    });
    d.current.content = "";
    expect(finishTextEdit(d)).toEqual({ kind: "delete", text_id: "t1" });
  });

  it("does nothing for a new empty draft", () => {
    expect(finishTextEdit(newDraft("t1", 10, 10))).toEqual({ kind: "nothing" });
  });

  it("updates an existing object that changed", () => {
    const d = editDraft({
      text_id: "t1",
      x: 1,
      y: 2,
      width: 100,
      font_size: 340,
      face: "default-sans-v1",
      content: "keep",
    });
    d.current.content = "changed";
    expect(finishTextEdit(d)).toEqual({ kind: "update", text: d.current });
    expect(finishTextEdit(d)).toEqual({ kind: "busy" });
  });

  it("does nothing when an existing object is unchanged", () => {
    const t = {
      text_id: "t1",
      x: 1,
      y: 2,
      width: 100,
      font_size: 340,
      face: "default-sans-v1",
      content: "keep",
    } as const;
    expect(finishTextEdit(editDraft({ ...t }))).toEqual({ kind: "nothing" });
  });

  it("restores editing after a failed persist so the draft is not lost", () => {
    const d = newDraft("t1", 100, 200);
    d.current.content = "keep me";
    expect(beginFinish(d)).toEqual({ kind: "add", text: d.current });
    expect(d.status).toBe("committing");
    expect(beginFinish(d)).toEqual({ kind: "busy" });
    abortFinish(d);
    expect(d.status).toBe("editing");
    expect(d.current.content).toBe("keep me");
    expect(beginFinish(d).kind).toBe("add");
    completeFinish(d);
    expect(beginFinish(d)).toEqual({ kind: "busy" });
  });

  it("second finalize while persist is in flight is busy; success clears once", async () => {
    const d = newDraft("t1", 100, 200);
    d.current.content = "hello";
    let release!: (ok: boolean) => void;
    const persist = (): Promise<boolean> => new Promise((resolve) => { release = resolve; });
    const first = runFinish(d, persist);
    expect(d.status).toBe("committing");
    expect(await runFinish(d, persist)).toBe("busy");
    expect(d.status).toBe("committing");
    expect(d.current.content).toBe("hello");
    release(true);
    expect(await first).toBe("cleared");
    expect(d.status).toBe("finalized");
    expect(d.current.content).toBe("hello");
    expect(await runFinish(d, persist)).toBe("busy");
  });

  it("second finalize while persist is in flight is busy; failure restores the draft", async () => {
    const d = newDraft("t1", 100, 200);
    d.current.content = "keep me";
    let release!: (ok: boolean) => void;
    let calls = 0;
    const persist = (): Promise<boolean> => {
      calls += 1;
      return new Promise((resolve) => { release = resolve; });
    };
    const first = runFinish(d, persist);
    expect(await runFinish(d, persist)).toBe("busy");
    expect(calls).toBe(1);
    expect(d.status).toBe("committing");
    release(false);
    expect(await first).toBe("restored");
    expect(d.status).toBe("editing");
    expect(d.current.content).toBe("keep me");
    const retry = runFinish(d, persist);
    expect(calls).toBe(2);
    release(true);
    expect(await retry).toBe("cleared");
  });
});

describe("remote-delete liveness gate", () => {
  it("finalizing a deleted text with unsaved typing restores without persisting", async () => {
    const d = existing();
    d.current.content = "unsaved typing";
    let calls = 0;
    const persist = async (): Promise<boolean> => {
      calls += 1;
      return true;
    };
    expect(await runFinish(d, persist, () => false)).toBe("restored");
    expect(calls).toBe(0);
    expect(d.status).toBe("editing");
    expect(d.current.content).toBe("unsaved typing");
  });

  it("finalizing an emptied deleted text clears without persisting", async () => {
    const d = existing();
    d.current.content = "";
    let calls = 0;
    const persist = async (): Promise<boolean> => {
      calls += 1;
      return true;
    };
    expect(await runFinish(d, persist, () => false)).toBe("cleared");
    expect(calls).toBe(0);
    expect(d.status).toBe("finalized");
  });

  it("a live text finalizes normally through the gate", async () => {
    const d = existing();
    d.current.content = "changed";
    let calls = 0;
    const persist = async (): Promise<boolean> => {
      calls += 1;
      return true;
    };
    expect(await runFinish(d, persist, () => true)).toBe("cleared");
    expect(calls).toBe(1);
  });

  it("explicit delete of a deleted text clears without persisting", async () => {
    const d = existing();
    let calls = 0;
    const persist = async (): Promise<boolean> => {
      calls += 1;
      return true;
    };
    expect(await runDelete(d, persist, () => false)).toBe("cleared");
    expect(calls).toBe(0);
  });

  it("explicit delete of a live text persists", async () => {
    const d = existing();
    let calls = 0;
    const persist = async (): Promise<boolean> => {
      calls += 1;
      return true;
    };
    expect(await runDelete(d, persist, () => true)).toBe("cleared");
    expect(calls).toBe(1);
  });
});

describe("buildMoveUpdate", () => {
  const latest: SyncText = {
    text_id: "t1",
    x: 1000,
    y: 2000,
    width: 5000,
    font_size: 340,
    face: "default-sans-v1",
    content: "concurrent edit",
  };

  it("rebases the gesture delta onto the latest object, preserving its content", () => {
    const moved = buildMoveUpdate(latest, 100, 200, 1400, 2300);
    expect(moved).toEqual({ ...latest, x: Math.fround(1500), y: Math.fround(2500) });
    expect(moved!.content).toBe("concurrent edit");
  });

  it("returns null when the drop leaves the position unchanged", () => {
    expect(buildMoveUpdate(latest, 1000, 2000, 0, 0)).toBeNull();
  });

  it("clamps the drop to valid geometry", () => {
    const moved = buildMoveUpdate(latest, 1000, 2000, CANONICAL_PAGE_WIDTH, CANONICAL_PAGE_HEIGHT);
    expect(moved).not.toBeNull();
    expect(moved!.x + moved!.width).toBeLessThanOrEqual(CANONICAL_PAGE_WIDTH);
    expect(moved!.y).toBeLessThan(CANONICAL_PAGE_HEIGHT);
    expect(moved!.x).toBe(CANONICAL_PAGE_WIDTH - latest.width);
  });
});

describe("type-anywhere placement", () => {
  it("places a box at an interior point (including over ink)", () => {
    const box = placeTextBox(2000, 4000);
    expect(box.x).toBeCloseTo(2000);
    expect(box.y).toBeCloseTo(4000);
    expect(box.width).toBeGreaterThan(0);
    expect(box.x + box.width).toBeLessThanOrEqual(CANONICAL_PAGE_WIDTH);
  });
});
