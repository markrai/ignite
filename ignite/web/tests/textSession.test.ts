import { describe, expect, it, vi } from "vitest";
import { DEFAULT_TEXT_SESSION_FONT_SIZE, stepTextFontSize, textFontPercent, TextFinalizationGate } from "../src/authoring/textSession";
import { newDraft, type FinishOutcome, type TextDraft } from "../src/authoring/textDraft";

function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((r) => {
    resolve = r;
  });
  return { promise, resolve };
}

function harness(initial: TextDraft | null, finish: (draft: TextDraft) => Promise<FinishOutcome>) {
  let active = initial;
  const clear = vi.fn((draft: TextDraft) => {
    if (active === draft) active = null;
  });
  const restore = vi.fn((draft: TextDraft) => {
    if (active === draft) active = draft;
  });
  const gate = new TextFinalizationGate({
    current: () => active,
    finish,
    clear,
    restore,
  });
  return {
    gate,
    clear,
    restore,
    active: () => active,
    replace: (draft: TextDraft | null) => {
      active = draft;
    },
  };
}

describe("TextFinalizationGate", () => {
  it("joins simultaneous finalization callers to one durable persist and one result", async () => {
    const draft = newDraft("t1", 100, 200);
    draft.current.content = "one commit";
    const pending = deferred<FinishOutcome>();
    const finish = vi.fn(() => pending.promise);
    const h = harness(draft, finish);

    const first = h.gate.finalizeActive();
    const second = h.gate.finalizeActive();

    expect(second).toBe(first);
    expect(finish).toHaveBeenCalledTimes(1);
    pending.resolve("cleared");
    await expect(first).resolves.toBe("success");
    await expect(second).resolves.toBe("success");
    expect(h.clear).toHaveBeenCalledOnce();
    expect(h.active()).toBeNull();
  });

  it("reports blocked and preserves the exact active draft after failure", async () => {
    const draft = newDraft("t1", 100, 200);
    draft.current.content = "keep me";
    const h = harness(draft, async () => "restored");

    await expect(h.gate.finalizeActive()).resolves.toBe("blocked");
    expect(h.restore).toHaveBeenCalledWith(draft);
    expect(h.clear).not.toHaveBeenCalled();
    expect(h.active()).toBe(draft);
    expect(h.active()?.current.content).toBe("keep me");
  });

  it("clears only the draft whose successful finalization completed", async () => {
    const completing = newDraft("t1", 100, 200);
    completing.current.content = "first";
    const replacement = newDraft("t2", 300, 400);
    replacement.current.content = "second";
    const pending = deferred<FinishOutcome>();
    const h = harness(completing, () => pending.promise);

    const result = h.gate.finalizeActive();
    h.replace(replacement);
    pending.resolve("cleared");

    await expect(result).resolves.toBe("success");
    expect(h.clear).toHaveBeenCalledWith(completing);
    expect(h.active()).toBe(replacement);
  });

  it("treats an unexpected low-level busy result as blocked", async () => {
    const draft = newDraft("t1", 100, 200);
    const h = harness(draft, async () => "busy");

    await expect(h.gate.finalizeActive()).resolves.toBe("blocked");
    expect(h.clear).not.toHaveBeenCalled();
    expect(h.active()).toBe(draft);
  });
});

describe("Text-session font scale", () => {
  it("steps on-grid values and clamps at both ends", () => {
    expect(stepTextFontSize(340, 1)).toBe(400);
    expect(stepTextFontSize(340, -1)).toBe(280);
    expect(stepTextFontSize(240, -1)).toBe(240);
    expect(stepTextFontSize(680, 1)).toBe(680);
  });

  it("steps off-grid values without mutating them merely by inspection", () => {
    const authored = 375;
    expect(authored).toBe(375);
    expect(stepTextFontSize(authored, 1)).toBe(400);
    expect(stepTextFontSize(authored, -1)).toBe(340);
    expect(stepTextFontSize(100, -1)).toBe(240);
    expect(stepTextFontSize(900, 1)).toBe(680);
  });

  it("reports rounded percentages relative to the 340 default", () => {
    expect(DEFAULT_TEXT_SESSION_FONT_SIZE).toBe(340);
    expect(textFontPercent(240)).toBe(71);
    expect(textFontPercent(340)).toBe(100);
    expect(textFontPercent(680)).toBe(200);
  });
});
