import { DEFAULT_TEXT_FONT_SIZE, type FinishOutcome, type TextDraft } from "./textDraft";

export const TEXT_FONT_SIZES = [240, 280, 340, 400, 480, 560, 680] as const;
export const DEFAULT_TEXT_SESSION_FONT_SIZE = DEFAULT_TEXT_FONT_SIZE;

export function stepTextFontSize(current: number, direction: -1 | 1): number {
  if (direction > 0) return TEXT_FONT_SIZES.find((size) => size > current) ?? TEXT_FONT_SIZES.at(-1)!;
  return [...TEXT_FONT_SIZES].reverse().find((size) => size < current) ?? TEXT_FONT_SIZES[0];
}

export function textFontPercent(fontSize: number): number {
  return Math.round((fontSize / DEFAULT_TEXT_SESSION_FONT_SIZE) * 100);
}

/** Whether a transition may proceed after finalizing the active text draft. */
export type TextFinalizeResult = "success" | "blocked";

export interface TextFinalizationPort {
  /** The exact draft which currently owns the editor, if any. */
  current: () => TextDraft | null;
  /** Run the existing whole-object persistence/liveness finalizer. */
  finish: (draft: TextDraft) => Promise<FinishOutcome>;
  /** Clear this draft only if it still owns the editor. */
  clear: (draft: TextDraft) => void;
  /** Restore this draft only if it still owns the editor. */
  restore: (draft: TextDraft) => void;
}

interface InFlight {
  draft: TextDraft;
  promise: Promise<TextFinalizeResult>;
}

/**
 * Single-flight transition gate for the active whole-object text editor.
 *
 * Every caller observing the same draft receives the same promise. A different
 * draft cannot be skipped: once the older in-flight work succeeds, the caller
 * re-enters the gate and finalizes the current draft before proceeding.
 */
export class TextFinalizationGate {
  private inFlight: InFlight | null = null;

  constructor(private readonly port: TextFinalizationPort) {}

  finalizeActive = (): Promise<TextFinalizeResult> => {
    const active = this.port.current();
    const pending = this.inFlight;
    if (pending) {
      if (active === null || active === pending.draft) return pending.promise;
      return pending.promise.then((result) => (result === "blocked" ? result : this.finalizeActive()));
    }
    if (!active) return Promise.resolve("success");

    const promise = this.finishExact(active);
    const flight = { draft: active, promise };
    this.inFlight = flight;
    void promise.then(() => {
      if (this.inFlight === flight) this.inFlight = null;
    });
    return promise;
  };

  private async finishExact(draft: TextDraft): Promise<TextFinalizeResult> {
    let outcome: FinishOutcome;
    try {
      outcome = await this.port.finish(draft);
    } catch {
      outcome = "restored";
    }
    if (outcome === "cleared") {
      this.port.clear(draft);
      return "success";
    }
    if (outcome === "restored") this.port.restore(draft);
    return "blocked";
  }
}
