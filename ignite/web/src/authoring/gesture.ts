// Touch tap vs pan ownership. Shared by PageView gesture capture and type-mode placement.
// Canonical geometry is unchanged — this is only CSS-pixel movement on the scroller.

/** Finger movement (CSS px) beyond which a touch is a pan, not a tap. */
export const TAP_SLOP_PX = 10;

export function displacement(ax: number, ay: number, bx: number, by: number): number {
  return Math.hypot(bx - ax, by - ay);
}

/** True when the pointer stayed within the tap slop of its origin. */
export function isTap(originX: number, originY: number, x: number, y: number, slop = TAP_SLOP_PX): boolean {
  return displacement(originX, originY, x, y) <= slop;
}

/**
 * One-finger pan tracker: armed only after movement exceeds tap slop.
 * Until armed, the gesture is still a potential tap (no scroll yet).
 */
export interface PanTracker {
  pointerId: number;
  originX: number;
  originY: number;
  x: number;
  y: number;
  armed: boolean;
}

export function beginPan(pointerId: number, x: number, y: number): PanTracker {
  return { pointerId, originX: x, originY: y, x, y, armed: false };
}

/**
 * Apply a move. Returns scroll deltas (0,0) until the pan arms.
 * When it first arms, `justArmed` is true so callers can mark the gesture consumed.
 */
export function advancePan(
  pan: PanTracker,
  clientX: number,
  clientY: number,
  slop = TAP_SLOP_PX,
): { scrollX: number; scrollY: number; justArmed: boolean } {
  if (!pan.armed) {
    if (isTap(pan.originX, pan.originY, clientX, clientY, slop)) {
      return { scrollX: 0, scrollY: 0, justArmed: false };
    }
    pan.armed = true;
    // First armed frame: scroll by total displacement from the last tracked point (origin).
    const scrollX = pan.x - clientX;
    const scrollY = pan.y - clientY;
    pan.x = clientX;
    pan.y = clientY;
    return { scrollX, scrollY, justArmed: true };
  }
  const scrollX = pan.x - clientX;
  const scrollY = pan.y - clientY;
  pan.x = clientX;
  pan.y = clientY;
  return { scrollX, scrollY, justArmed: false };
}
