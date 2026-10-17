/** PWA update detection: register a lifecycle-only service worker and surface a waiting worker. */

import { APP_VERSION } from "../version";

export const PWA_UPDATE_PENDING_KEY = "pwaUpdatePending";
export const UPDATE_APPLIED_KEY = "updateApplied";
export const ENABLE_SW_DEV_KEY = "enableServiceWorkerDev";
export const SHOW_INTERVAL_MS = 60 * 1000;
export const UPDATE_INTERVAL_MS = 5 * 60 * 1000;

export interface StorageLike {
  getItem(key: string): string | null;
  setItem(key: string, value: string): void;
  removeItem(key: string): void;
}

export type WaitingWorker = { postMessage(message: unknown): void };
type InstallingWorker = {
  state: string;
  addEventListener(type: "statechange", listener: () => void): void;
};

export interface RegistrationLike {
  waiting: WaitingWorker | null;
  installing: InstallingWorker | null;
  update(): Promise<unknown>;
  addEventListener(type: "updatefound", listener: () => void): void;
}

export interface ServiceWorkerContainerLike {
  register(scriptURL: string): Promise<RegistrationLike>;
  controller: unknown;
}

export interface UpdateWatcherHost {
  appVersion: string;
  hostname: string;
  isSecureContext: boolean;
  serviceWorker: ServiceWorkerContainerLike | undefined;
  localStorage: StorageLike;
  sessionStorage: StorageLike;
  visibilityState: () => string;
  addVisibilityListener(listener: () => void): void;
  removeVisibilityListener(listener: () => void): void;
  whenLoaded(fn: () => void): void;
  setInterval(fn: () => void, ms: number): ReturnType<typeof setInterval>;
  clearInterval(id: ReturnType<typeof setInterval>): void;
  defer(fn: () => void, ms: number): void;
  reload(): void;
  logWarn?: (message: string, err: unknown) => void;
}

export interface UpdateHandle {
  apply(): void;
  dismiss(): void;
  stop(): void;
}

export function shouldRegisterServiceWorker(host: Pick<
  UpdateWatcherHost,
  "hostname" | "isSecureContext" | "serviceWorker" | "localStorage"
>): boolean {
  if (!host.serviceWorker) return false;
  if (!host.isSecureContext) return false;
  const localhost =
    host.hostname === "localhost" || host.hostname === "127.0.0.1" || host.hostname === "[::1]";
  if (localhost && host.localStorage.getItem(ENABLE_SW_DEV_KEY) !== "true") return false;
  return true;
}

export function attachUpdateWatcher(host: UpdateWatcherHost, onAvailable: (available: boolean) => void): UpdateHandle {
  let registration: RegistrationLike | null = null;
  let shown = false;
  const timers: ReturnType<typeof setInterval>[] = [];
  let visibilityListener: (() => void) | null = null;
  let stopped = false;

  const setAvailable = (available: boolean) => {
    if (available) {
      if (shown) return;
      shown = true;
      host.localStorage.setItem(PWA_UPDATE_PENDING_KEY, "1");
      onAvailable(true);
      return;
    }
    shown = false;
    onAvailable(false);
  };

  const maybeShow = () => {
    if (stopped) return;
    if (registration?.waiting) setAvailable(true);
  };

  const apply = () => {
    host.localStorage.removeItem(PWA_UPDATE_PENDING_KEY);
    registration?.waiting?.postMessage({ type: "SKIP_WAITING" });
    host.sessionStorage.setItem(UPDATE_APPLIED_KEY, "true");
    host.defer(() => host.reload(), 100);
  };

  const dismiss = () => setAvailable(false);

  const stop = () => {
    stopped = true;
    for (const id of timers) host.clearInterval(id);
    timers.length = 0;
    if (visibilityListener) {
      host.removeVisibilityListener(visibilityListener);
      visibilityListener = null;
    }
  };

  if (!shouldRegisterServiceWorker(host)) {
    return { apply: () => undefined, dismiss: () => undefined, stop };
  }

  host.whenLoaded(() => {
    if (stopped) return;
    const sw = host.serviceWorker;
    if (!sw) return;
    sw.register(`/sw.js?v=${encodeURIComponent(host.appVersion)}`)
      .then((reg) => {
        if (stopped) return;
        registration = reg;

        if (host.sessionStorage.getItem(UPDATE_APPLIED_KEY)) {
          host.sessionStorage.removeItem(UPDATE_APPLIED_KEY);
          host.localStorage.removeItem(PWA_UPDATE_PENDING_KEY);
        } else if (reg.waiting) {
          setAvailable(true);
        } else if (host.localStorage.getItem(PWA_UPDATE_PENDING_KEY)) {
          void reg.update();
        }

        reg.addEventListener("updatefound", () => {
          const worker = reg.installing;
          if (!worker) return;
          worker.addEventListener("statechange", () => {
            if (worker.state === "installed" && sw.controller) setAvailable(true);
          });
        });

        visibilityListener = () => {
          if (host.visibilityState() === "visible") maybeShow();
        };
        host.addVisibilityListener(visibilityListener);

        timers.push(host.setInterval(maybeShow, SHOW_INTERVAL_MS));
        timers.push(host.setInterval(() => void reg.update(), UPDATE_INTERVAL_MS));
      })
      .catch((err: unknown) => host.logWarn?.("ServiceWorker registration failed:", err));
  });

  return { apply, dismiss, stop };
}

export function startUpdateWatcher(onAvailable: (available: boolean) => void): UpdateHandle {
  return attachUpdateWatcher(
    {
      appVersion: APP_VERSION,
      hostname: window.location.hostname,
      isSecureContext: window.isSecureContext,
      serviceWorker: "serviceWorker" in navigator ? (navigator.serviceWorker as unknown as ServiceWorkerContainerLike) : undefined,
      localStorage,
      sessionStorage,
      visibilityState: () => document.visibilityState,
      addVisibilityListener: (fn) => document.addEventListener("visibilitychange", fn),
      removeVisibilityListener: (fn) => document.removeEventListener("visibilitychange", fn),
      whenLoaded: (fn) => {
        if (document.readyState === "complete") fn();
        else window.addEventListener("load", fn, { once: true });
      },
      setInterval,
      clearInterval,
      defer: (fn, ms) => {
        window.setTimeout(fn, ms);
      },
      reload: () => {
        window.location.reload();
      },
      logWarn: (message, err) => console.warn(message, err),
    },
    onAvailable,
  );
}
