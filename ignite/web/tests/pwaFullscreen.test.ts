import { describe, expect, it, vi } from "vitest";
import { isInstalledPwa, startPwaFullscreen, type PwaFullscreenHost } from "../src/pwa/fullscreen";

function host(overrides: Partial<PwaFullscreenHost> & { modes?: string[] } = {}): PwaFullscreenHost & {
  request: ReturnType<typeof vi.fn>;
  gestures: Array<() => void>;
} {
  const modes = new Set(overrides.modes ?? []);
  const request = vi.fn(async () => undefined);
  const gestures: Array<() => void> = [];
  return {
    request,
    gestures,
    matchMedia: (query) => {
      for (const m of modes) {
        if (query === `(display-mode: ${m})`) return true;
      }
      return false;
    },
    iosStandalone: false,
    fullscreenElement: () => null,
    addGestureListener: (fn) => {
      gestures.push(fn);
    },
    removeGestureListener: (fn) => {
      const i = gestures.indexOf(fn);
      if (i >= 0) gestures.splice(i, 1);
    },
    ...overrides,
    requestFullscreen: overrides.requestFullscreen ?? (() => request()),
  };
}

describe("isInstalledPwa", () => {
  it("is false in a normal browser tab", () => {
    expect(isInstalledPwa(host())).toBe(false);
  });

  it("is true for installed display modes and iOS standalone", () => {
    expect(isInstalledPwa(host({ modes: ["standalone"] }))).toBe(true);
    expect(isInstalledPwa(host({ modes: ["fullscreen"] }))).toBe(true);
    expect(isInstalledPwa(host({ modes: ["minimal-ui"] }))).toBe(true);
    expect(isInstalledPwa(host({ iosStandalone: true }))).toBe(true);
  });
});

describe("startPwaFullscreen", () => {
  it("does not request fullscreen in a browser tab", () => {
    const h = host();
    startPwaFullscreen(h);
    expect(h.request).not.toHaveBeenCalled();
    expect(h.gestures).toHaveLength(0);
  });

  it("does not stack the Fullscreen API when the PWA already launched fullscreen", () => {
    const h = host({ modes: ["fullscreen"] });
    startPwaFullscreen(h);
    expect(h.request).not.toHaveBeenCalled();
  });

  it("requests fullscreen for an installed standalone PWA, and retries on first gesture", async () => {
    const h = host({ modes: ["standalone"] });
    startPwaFullscreen(h);
    expect(h.request).toHaveBeenCalledTimes(1);
    expect(h.gestures).toHaveLength(1);
    h.gestures[0]!();
    expect(h.request).toHaveBeenCalledTimes(2);
    expect(h.gestures).toHaveLength(0);
  });

  it("does not request again when already in the Fullscreen API", () => {
    const h = host({
      modes: ["standalone"],
      fullscreenElement: () => ({}) as Element,
    });
    startPwaFullscreen(h);
    expect(h.request).not.toHaveBeenCalled();
  });
});
