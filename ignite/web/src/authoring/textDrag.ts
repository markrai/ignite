// Scroll-free text grab/drag tracker. Pure; no DOM, no React.
// Separate from PanTracker on purpose: advancing a pan scrolls the page as a
// side effect, which a text drag must never do. Slop (TAP_SLOP_PX / isTap) is
// shared so click-vs-drag matches tap-vs-pan.

import { isTap, TAP_SLOP_PX } from "./gesture";

export type TextDragPhase = "armed" | "dragging";

export interface TextGrab {
  phase: TextDragPhase;
  /** Client px of the pointer at grab time. */
  startClientX: number;
  startClientY: number;
  /** Canonical origin of the text object at grab time (geometry is frozen here). */
  originX: number;
  originY: number;
  /** CSS px per canonical unit at grab time (deltas are converted with this). */
  scale: number;
}

export function beginTextGrab(
  clientX: number,
  clientY: number,
  originX: number,
  originY: number,
  scale: number,
): TextGrab {
  return { phase: "armed", startClientX: clientX, startClientY: clientY, originX, originY, scale };
}

/** Advance a grab. True exactly once, on the transition to dragging. */
export function advanceTextGrab(grab: TextGrab, clientX: number, clientY: number, slop = TAP_SLOP_PX): boolean {
  if (grab.phase === "dragging") return false;
  if (isTap(grab.startClientX, grab.startClientY, clientX, clientY, slop)) return false;
  grab.phase = "dragging";
  return true;
}

/** Screen-space displacement since grab, in client px (for the drag preview). */
export function dragPreviewDelta(grab: TextGrab, clientX: number, clientY: number): { dx: number; dy: number } {
  return { dx: clientX - grab.startClientX, dy: clientY - grab.startClientY };
}

/** Canonical displacement since grab (single division by the grab-time scale). */
export function dragCanonicalDelta(grab: TextGrab, clientX: number, clientY: number): { dx: number; dy: number } {
  return { dx: (clientX - grab.startClientX) / grab.scale, dy: (clientY - grab.startClientY) / grab.scale };
}

// Terminal-release suppression, keyed by pointer ID. A cancelled grab consumes
// its own pointer's release so it cannot fall through into edit/create; a fresh
// press re-arms its ID (a pointer cannot go down twice without an up, so a new
// press proves any earlier suppression for that ID is stale).

/** Remember that this pointer's terminal release is consumed. */
export function suppressPointerRelease(suppressed: Set<number>, pointerId: number): void {
  suppressed.add(pointerId);
}

/** True once for a suppressed pointer (consumes it); never affects other pointers. */
export function consumeSuppressedRelease(suppressed: Set<number>, pointerId: number): boolean {
  if (!suppressed.has(pointerId)) return false;
  suppressed.delete(pointerId);
  return true;
}

/** A fresh press clears any stale suppression for its own pointer ID. */
export function clearSuppressedRelease(suppressed: Set<number>, pointerId: number): void {
  suppressed.delete(pointerId);
}
