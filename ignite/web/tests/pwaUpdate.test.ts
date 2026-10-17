import { describe, expect, it, vi } from "vitest";
import {
  ENABLE_SW_DEV_KEY,
  PWA_UPDATE_PENDING_KEY,
  SHOW_INTERVAL_MS,
  UPDATE_APPLIED_KEY,
  UPDATE_INTERVAL_MS,
  attachUpdateWatcher,
  shouldRegisterServiceWorker,
  type RegistrationLike,
  type StorageLike,
  type UpdateWatcherHost,
  type WaitingWorker,
} from "../src/pwa/registerUpdate";

function mem(): StorageLike & { data: Map<string, string> } {
  const data = new Map<string, string>();
  return {
    data,
    getItem: (k) => data.get(k) ?? null,
    setItem: (k, v) => void data.set(k, v),
    removeItem: (k) => void data.delete(k),
  };
}

async function flush(): Promise<void> {
  for (let i = 0; i < 8; i += 1) await Promise.resolve();
}

type Installing = {
  state: string;
  listeners: Array<() => void>;
  addEventListener(_type: "statechange", listener: () => void): void;
};

function installingWorker(state = "installing"): Installing {
  const listeners: Array<() => void> = [];
  return {
    state,
    listeners,
    addEventListener(_type, listener) {
      listeners.push(listener);
    },
  };
}

function waitingWorker(): WaitingWorker & { postMessage: ReturnType<typeof vi.fn<(message: unknown) => void>> } {
  return { postMessage: vi.fn<(message: unknown) => void>() };
}

function registration(init: {
  waiting?: WaitingWorker | null;
  installing?: Installing | null;
} = {}): RegistrationLike & {
  updatefound: Array<() => void>;
  installing: Installing | null;
} {
  const updatefound: Array<() => void> = [];
  return {
    waiting: init.waiting === undefined ? null : init.waiting,
    installing: init.installing ?? null,
    update: vi.fn(async () => undefined),
    addEventListener(_type: "updatefound", listener: () => void) {
      updatefound.push(listener);
    },
    updatefound,
  };
}

function makeHost(overrides: Partial<UpdateWatcherHost> = {}): {
  host: UpdateWatcherHost;
  localStorage: StorageLike;
  sessionStorage: StorageLike;
  intervals: Array<{ fn: () => void; ms: number }>;
  visibility: Array<() => void>;
  reload: ReturnType<typeof vi.fn>;
} {
  const localStorage = overrides.localStorage ?? mem();
  const sessionStorage = overrides.sessionStorage ?? mem();
  const intervals: Array<{ fn: () => void; ms: number }> = [];
  const visibility: Array<() => void> = [];
  const reload = vi.fn();
  return {
    localStorage,
    sessionStorage,
    intervals,
    visibility,
    reload,
    host: {
      appVersion: "0.9.6",
      hostname: "example.ts.net",
      isSecureContext: true,
      serviceWorker: undefined,
      visibilityState: () => "visible",
      addVisibilityListener: (fn) => visibility.push(fn),
      removeVisibilityListener: (fn) => {
        const i = visibility.indexOf(fn);
        if (i >= 0) visibility.splice(i, 1);
      },
      whenLoaded: (fn) => fn(),
      setInterval: (fn, ms) => {
        intervals.push({ fn, ms });
        return intervals.length as unknown as ReturnType<typeof setInterval>;
      },
      clearInterval: () => undefined,
      defer: (fn) => fn(),
      reload,
      ...overrides,
      localStorage,
      sessionStorage,
    },
  };
}

const gateBase = {
  hostname: "example.ts.net",
  isSecureContext: true,
  serviceWorker: { register: async () => registration(), controller: {} },
  localStorage: mem(),
};

describe("shouldRegisterServiceWorker", () => {
  it("skips insecure contexts, missing SW, and localhost unless opted in", () => {
    expect(shouldRegisterServiceWorker({ ...gateBase, isSecureContext: false })).toBe(false);
    expect(shouldRegisterServiceWorker({ ...gateBase, serviceWorker: undefined })).toBe(false);
    expect(shouldRegisterServiceWorker({ ...gateBase, hostname: "localhost" })).toBe(false);
    expect(shouldRegisterServiceWorker({ ...gateBase, hostname: "127.0.0.1" })).toBe(false);
    const opted = mem();
    opted.setItem(ENABLE_SW_DEV_KEY, "true");
    expect(shouldRegisterServiceWorker({ ...gateBase, hostname: "localhost", localStorage: opted })).toBe(true);
    expect(shouldRegisterServiceWorker(gateBase)).toBe(true);
  });
});

describe("attachUpdateWatcher", () => {
  it("does not register when the host is ineligible", async () => {
    const register = vi.fn(async () => registration());
    const ctx = makeHost({
      isSecureContext: false,
      serviceWorker: { register, controller: {} },
    });
    attachUpdateWatcher(ctx.host, () => undefined);
    await flush();
    expect(register).not.toHaveBeenCalled();
  });

  it("shows the banner when a waiting worker is already present", async () => {
    const waiting = waitingWorker();
    const reg = registration({ waiting });
    const register = vi.fn(async () => reg);
    const ctx = makeHost({ serviceWorker: { register, controller: {} } });
    const seen: boolean[] = [];
    attachUpdateWatcher(ctx.host, (v) => seen.push(v));
    await flush();
    expect(register).toHaveBeenCalledWith("/sw.js?v=0.9.6");
    expect(seen).toEqual([true]);
    expect(ctx.localStorage.getItem(PWA_UPDATE_PENDING_KEY)).toBe("1");
  });

  it("Update now posts SKIP_WAITING and reloads; Later dismisses", async () => {
    const waiting = waitingWorker();
    const reg = registration({ waiting });
    const ctx = makeHost({
      serviceWorker: { register: async () => reg, controller: {} },
    });
    const seen: boolean[] = [];
    const handle = attachUpdateWatcher(ctx.host, (v) => seen.push(v));
    await flush();
    handle.dismiss();
    expect(seen).toEqual([true, false]);
    handle.apply();
    expect(waiting.postMessage).toHaveBeenCalledWith({ type: "SKIP_WAITING" });
    expect(ctx.reload).toHaveBeenCalled();
    expect(ctx.sessionStorage.getItem(UPDATE_APPLIED_KEY)).toBe("true");
    expect(ctx.localStorage.getItem(PWA_UPDATE_PENDING_KEY)).toBeNull();
  });

  it("shows when a new worker installs while a controller exists", async () => {
    const worker = installingWorker();
    const reg = registration({ installing: worker });
    const ctx = makeHost({
      serviceWorker: { register: async () => reg, controller: {} },
    });
    const seen: boolean[] = [];
    attachUpdateWatcher(ctx.host, (v) => seen.push(v));
    await flush();
    expect(seen).toEqual([]);
    for (const fn of reg.updatefound) fn();
    worker.state = "installed";
    for (const fn of worker.listeners) fn();
    expect(seen).toEqual([true]);
  });

  it("does not show on first install when there is no controller", async () => {
    const worker = installingWorker();
    const reg = registration({ installing: worker, waiting: null });
    const ctx = makeHost({
      serviceWorker: { register: async () => reg, controller: null },
    });
    const seen: boolean[] = [];
    attachUpdateWatcher(ctx.host, (v) => seen.push(v));
    await flush();
    for (const fn of reg.updatefound) fn();
    worker.state = "installed";
    for (const fn of worker.listeners) fn();
    expect(seen).toEqual([]);
  });

  it("clears a just-applied update instead of showing the waiting worker", async () => {
    const waiting = waitingWorker();
    const sessionStorage = mem();
    sessionStorage.setItem(UPDATE_APPLIED_KEY, "true");
    const localStorage = mem();
    localStorage.setItem(PWA_UPDATE_PENDING_KEY, "1");
    const ctx = makeHost({
      serviceWorker: { register: async () => registration({ waiting }), controller: {} },
      sessionStorage,
      localStorage,
    });
    const seen: boolean[] = [];
    attachUpdateWatcher(ctx.host, (v) => seen.push(v));
    await flush();
    expect(seen).toEqual([]);
    expect(sessionStorage.getItem(UPDATE_APPLIED_KEY)).toBeNull();
    expect(localStorage.getItem(PWA_UPDATE_PENDING_KEY)).toBeNull();
  });

  it("re-shows after Later when the page becomes visible and a worker is waiting", async () => {
    const waiting = waitingWorker();
    const ctx = makeHost({
      serviceWorker: { register: async () => registration({ waiting }), controller: {} },
    });
    const seen: boolean[] = [];
    const handle = attachUpdateWatcher(ctx.host, (v) => seen.push(v));
    await flush();
    handle.dismiss();
    expect(seen).toEqual([true, false]);
    ctx.visibility[0]?.();
    expect(seen).toEqual([true, false, true]);
  });

  it("polls for waiting workers and calls registration.update on an interval", async () => {
    const waiting = waitingWorker();
    const reg = registration({ waiting: null });
    const ctx = makeHost({
      serviceWorker: { register: async () => reg, controller: {} },
    });
    const seen: boolean[] = [];
    attachUpdateWatcher(ctx.host, (v) => seen.push(v));
    await flush();
    expect(ctx.intervals.map((i) => i.ms).sort((a, b) => a - b)).toEqual([SHOW_INTERVAL_MS, UPDATE_INTERVAL_MS]);
    const show = ctx.intervals.find((i) => i.ms === SHOW_INTERVAL_MS);
    const update = ctx.intervals.find((i) => i.ms === UPDATE_INTERVAL_MS);
    show?.fn();
    expect(seen).toEqual([]);
    reg.waiting = waiting;
    show?.fn();
    expect(seen).toEqual([true]);
    await update?.fn();
    expect(reg.update).toHaveBeenCalled();
  });
});
