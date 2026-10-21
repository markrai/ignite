// Whole-object text editor: one finalization path so blur/Done/mode-switch cannot double-commit.

import { CANONICAL_PAGE_HEIGHT, CANONICAL_PAGE_WIDTH } from "../model/generated/protocolConstants";
import type { SyncText } from "../model/types";
import { prevF32, textWidthFits } from "../model/validate";

export const DEFAULT_TEXT_FONT_SIZE = 340;
export const DEFAULT_TEXT_WIDTH = 10_000;

/** Clamp a moved origin so the object stays valid (mirrors `SyncText::validate`). */
export function clampTextOrigin(x: number, y: number, width: number): { x: number; y: number } {
  let cx = Math.fround(Math.min(Math.max(0, x), CANONICAL_PAGE_WIDTH - width));
  // fround(W - width) can round up past the f32 remaining width; step down to the edge f32.
  while (cx > 0 && !textWidthFits(cx, width)) cx = prevF32(cx);
  const cy = Math.fround(Math.min(Math.max(0, y), CANONICAL_PAGE_HEIGHT - 1e-3));
  return { x: cx, y: cy };
}

// `UpdateText` is whole-object LWW: a move must rebase onto the latest live
// object with only x/y replaced. Committing the grab-time copy would clobber a
// concurrent content change.
export function rebaseMovedText(latest: SyncText, x: number, y: number): SyncText {
  return { ...latest, x, y };
}

/**
 * The drop commit for a drag: the latest live object moved by the gesture delta,
 * clamped to valid geometry. Null when the drop must commit nothing (unmoved).
 * The caller passes the latest object it knows; a text that disappeared before
 * drop never reaches this helper.
 */
export function buildMoveUpdate(latest: SyncText, originX: number, originY: number, dx: number, dy: number): SyncText | null {
  const c = clampTextOrigin(originX + dx, originY + dy, latest.width);
  if (c.x === latest.x && c.y === latest.y) return null;
  return rebaseMovedText(latest, c.x, c.y);
}

export type TextDraftStatus = "editing" | "committing" | "finalized";

export interface TextDraft {
  text_id: string;
  /** null = new object (not yet AddText). */
  original: SyncText | null;
  current: SyncText;
  status: TextDraftStatus;
}

export type TextCommit =
  | { kind: "busy" }
  | { kind: "nothing" }
  | { kind: "add"; text: SyncText }
  | { kind: "update"; text: SyncText }
  | { kind: "delete"; text_id: string };

export type TextPersistIntent =
  | { kind: "add" | "update"; text: SyncText }
  | { kind: "delete"; text_id: string };

export type FinishOutcome = "busy" | "cleared" | "restored";

export function normalizeLineEndings(s: string): string {
  return s.replace(/\r\n/g, "\n").replace(/\r/g, "\n");
}

/** Legal box origin + width for a click at canonical (x, y). */
export function placeTextBox(x: number, y: number): { x: number; y: number; width: number } {
  const W = CANONICAL_PAGE_WIDTH;
  // Page interaction rejects outside anchors before minting an id. Keep the
  // accepted canonical anchor instead of shifting near-edge blocks left.
  // An anchor just inside an edge can round onto it as f32; keep it on the page.
  let px = Math.fround(x);
  if (px >= W) px = prevF32(W);
  let py = Math.fround(y);
  if (py >= CANONICAL_PAGE_HEIGHT) py = prevF32(CANONICAL_PAGE_HEIGHT);
  // fround(W - px) is exactly Rust's f32 remaining width, so this width always fits.
  const width = Math.min(DEFAULT_TEXT_WIDTH, W - px);
  return { x: px, y: py, width: Math.fround(width) };
}

export function newDraft(text_id: string, x: number, y: number, fontSize = DEFAULT_TEXT_FONT_SIZE): TextDraft {
  const box = placeTextBox(x, y);
  return {
    text_id,
    original: null,
    current: {
      text_id,
      x: box.x,
      y: box.y,
      width: box.width,
      font_size: fontSize,
      face: "default-sans-v1",
      content: "",
    },
    status: "editing",
  };
}

export function editDraft(existing: SyncText): TextDraft {
  return {
    text_id: existing.text_id,
    original: { ...existing },
    current: { ...existing },
    status: "editing",
  };
}

export function sameText(a: SyncText, b: SyncText): boolean {
  return (
    a.text_id === b.text_id &&
    a.x === b.x &&
    a.y === b.y &&
    a.width === b.width &&
    a.font_size === b.font_size &&
    a.face === b.face &&
    a.content === b.content
  );
}

/**
 * Enter the single in-flight commit.
 * `busy` = already committing/finalized (a second finalize must not clear the editor).
 * `nothing` = this draft truly needs no operation.
 * Caller must `completeFinish` after persistence succeeds or `abortFinish` to restore editing.
 */
export function beginFinish(draft: TextDraft): TextCommit {
  if (draft.status !== "editing") return { kind: "busy" };
  draft.status = "committing";
  const content = normalizeLineEndings(draft.current.content);
  draft.current = { ...draft.current, content };
  if (draft.original === null) {
    return content === "" ? { kind: "nothing" } : { kind: "add", text: draft.current };
  }
  if (content === "") return { kind: "delete", text_id: draft.text_id };
  if (sameText(draft.original, draft.current)) return { kind: "nothing" };
  return { kind: "update", text: draft.current };
}

/**
 * One finalize event. A second call while persist is in flight returns `busy`
 * without clearing or emitting another operation.
 *
 * `isLive` gates existing drafts against remote deletion: an UpdateText or
 * DeleteText for a known-deleted id would be refused by the server and poison
 * the coordinator with a conflict, so it is never sent. Emptied drafts clear
 * silently (their end state already holds); drafts with unsaved typing restore
 * like a persist failure so the content is never silently discarded.
 */
export async function runFinish(
  draft: TextDraft,
  persist: ((intent: TextPersistIntent) => Promise<boolean>) | undefined,
  isLive?: (text_id: string) => boolean,
): Promise<FinishOutcome> {
  if (draft.status === "editing" && draft.original !== null && isLive && !isLive(draft.text_id)) {
    const content = normalizeLineEndings(draft.current.content);
    draft.current = { ...draft.current, content };
    if (content === "") {
      completeFinish(draft);
      return "cleared";
    }
    return "restored";
  }
  const commit = beginFinish(draft);
  if (commit.kind === "busy") return "busy";
  if (commit.kind === "nothing") {
    completeFinish(draft);
    return "cleared";
  }
  if (!persist) {
    abortFinish(draft);
    return "restored";
  }
  const intent: TextPersistIntent =
    commit.kind === "delete"
      ? { kind: "delete", text_id: commit.text_id }
      : { kind: commit.kind, text: commit.text };
  const ok = await persist(intent);
  if (ok) {
    completeFinish(draft);
    return "cleared";
  }
  abortFinish(draft);
  return "restored";
}

export function completeFinish(draft: TextDraft): void {
  draft.status = "finalized";
}

/**
 * Explicit delete of an existing object (the contextual affordance while editing).
 * Restores the draft (content intact) when persistence fails; new drafts own no
 * persisted object, so deleting one is a no-op restore. A remotely deleted object
 * clears without an operation: its end state already holds.
 */
export async function runDelete(
  draft: TextDraft,
  persist: ((intent: TextPersistIntent) => Promise<boolean>) | undefined,
  isLive?: (text_id: string) => boolean,
): Promise<FinishOutcome> {
  if (draft.status !== "editing" || draft.original === null) return "restored";
  if (isLive && !isLive(draft.text_id)) {
    completeFinish(draft);
    return "cleared";
  }
  draft.status = "committing";
  if (!persist) {
    abortFinish(draft);
    return "restored";
  }
  const ok = await persist({ kind: "delete", text_id: draft.text_id });
  if (ok) {
    completeFinish(draft);
    return "cleared";
  }
  abortFinish(draft);
  return "restored";
}

export function abortFinish(draft: TextDraft): void {
  if (draft.status === "committing") draft.status = "editing";
}

/**
 * Decide the single op and mark finalized (unit tests / success-only paths).
 */
export function finishTextEdit(draft: TextDraft): TextCommit {
  const commit = beginFinish(draft);
  if (commit.kind === "busy") return commit;
  completeFinish(draft);
  return commit;
}
