/** Fullscreen only for an installed PWA — never a normal browser tab. */

export interface PwaFullscreenHost {
  matchMedia(query: string): boolean;
  iosStandalone: boolean;
  fullscreenElement(): Element | null;
  requestFullscreen(): Promise<void>;
  addGestureListener(fn: () => void): void;
  removeGestureListener(fn: () => void): void;
}

export function isInstalledPwa(host: Pick<PwaFullscreenHost, "matchMedia" | "iosStandalone">): boolean {
  return (
    host.matchMedia("(display-mode: fullscreen)") ||
    host.matchMedia("(display-mode: standalone)") ||
    host.matchMedia("(display-mode: minimal-ui)") ||
    host.iosStandalone
  );
}

/**
 * Manifest `display: fullscreen` applies to new installs. An existing home-screen app may
 * still be `standalone`. Request the Fullscreen API only in that installed-PWA case.
 */
export function startPwaFullscreen(host: PwaFullscreenHost): () => void {
  if (!isInstalledPwa(host)) return () => undefined;
  // Already launched as a fullscreen PWA; don't stack the Fullscreen API on top.
  if (host.matchMedia("(display-mode: fullscreen)")) return () => undefined;

  const tryEnter = () => {
    if (host.fullscreenElement()) return;
    void host.requestFullscreen().catch(() => undefined);
  };

  tryEnter();
  const onGesture = () => {
    tryEnter();
    host.removeGestureListener(onGesture);
  };
  host.addGestureListener(onGesture);
  return () => host.removeGestureListener(onGesture);
}

export function startInstalledPwaFullscreen(): () => void {
  const nav = window.navigator as Navigator & { standalone?: boolean };
  const gestures = new Set<() => void>();
  const add = (fn: () => void) => {
    const opts = { once: true } as const;
    window.addEventListener("pointerdown", fn, opts);
    window.addEventListener("keydown", fn, opts);
    gestures.add(fn);
  };
  const remove = (fn: () => void) => {
    window.removeEventListener("pointerdown", fn);
    window.removeEventListener("keydown", fn);
    gestures.delete(fn);
  };
  return startPwaFullscreen({
    matchMedia: (query) => window.matchMedia(query).matches,
    iosStandalone: nav.standalone === true,
    fullscreenElement: () => document.fullscreenElement,
    requestFullscreen: () => document.documentElement.requestFullscreen({ navigationUI: "hide" }),
    addGestureListener: add,
    removeGestureListener: remove,
  });
}
