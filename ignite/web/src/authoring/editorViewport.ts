// Presentation-only helpers for keeping a text editor visible above the virtual keyboard.
// Uses visualViewport when available; never mutates canonical text geometry.

/** How long after a visualViewport change to suppress blur→commit (ms). */
export const BLUR_SUPPRESS_MS = 400;

const PAD = 12;

type ViewportGlobals = {
  visualViewport?: { offsetTop: number; offsetLeft: number; height: number; width: number } | null;
  innerHeight: number;
  innerWidth: number;
  addEventListener?: (type: string, fn: () => void) => void;
  removeEventListener?: (type: string, fn: () => void) => void;
};

function globals(): ViewportGlobals {
  return globalThis as unknown as ViewportGlobals;
}

/**
 * Scroll `scroller` so `editor`'s client rect stays inside the visual viewport
 * (or the layout viewport when visualViewport is missing).
 */
export function keepEditorInVisualViewport(editor: HTMLElement, scroller: HTMLElement): void {
  const g = globals();
  const er = editor.getBoundingClientRect();
  const vv = g.visualViewport ?? null;
  const top = vv ? vv.offsetTop : 0;
  const left = vv ? vv.offsetLeft : 0;
  const bottom = vv ? vv.offsetTop + vv.height : g.innerHeight;
  const right = vv ? vv.offsetLeft + vv.width : g.innerWidth;

  let dy = 0;
  let dx = 0;
  if (er.bottom > bottom - PAD) dy = er.bottom - (bottom - PAD);
  else if (er.top < top + PAD) dy = er.top - (top + PAD);
  if (er.right > right - PAD) dx = er.right - (right - PAD);
  else if (er.left < left + PAD) dx = er.left - (left + PAD);

  if (dx !== 0 || dy !== 0) scroller.scrollBy(dx, dy);
}

/** Subscribe to visualViewport (or window resize) changes. Returns unsubscribe. */
export function onVisualViewportChange(fn: () => void): () => void {
  const g = globals();
  const vv = g.visualViewport as
    | (VisualViewport & { addEventListener(type: string, fn: () => void): void; removeEventListener(type: string, fn: () => void): void })
    | null
    | undefined;
  if (vv && typeof vv.addEventListener === "function") {
    vv.addEventListener("resize", fn);
    vv.addEventListener("scroll", fn);
    return () => {
      vv.removeEventListener("resize", fn);
      vv.removeEventListener("scroll", fn);
    };
  }
  g.addEventListener?.("resize", fn);
  return () => g.removeEventListener?.("resize", fn);
}
