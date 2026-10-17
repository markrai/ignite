import { useCallback, useEffect, useMemo, useState } from "react";
import { createControlApi, type ControlApi, type ServerStatusPublic } from "../api/controlApi";
import { createReadOnlyApi, UnauthorizedError, type ReadOnlyApi } from "../api/readOnlyApi";
import { createReplicaApi, type ReplicaApi } from "../api/replicaApi";
import { clearToken, loadToken, saveToken } from "../api/tokenStore";
import { AppVersion } from "../components/AppVersion";
import { ConnectScreen } from "../components/ConnectScreen";
import { LibraryChooser } from "../components/LibraryChooser";
import { RequestAccessScreen, SetupScreen } from "../components/ProductScreens";
import { ServerSettings } from "../components/ServerSettings";
import { Viewer, type ReplicaHandle } from "../components/Viewer";
import type { LibrarySummary } from "../model/types";
import { BlobCache, decodeToCanvas } from "../rendering/background";
import { SyncCoordinator } from "../replica/coordinator";
import { bridgeEncoder } from "../replica/encoder";
import { replicaUnavailableReason, startWritableReplica } from "../replica/start";
import { ReplicaStore } from "../replica/store";
import { LibrarySession, browserClock } from "../state/librarySession";
import { loadBridge } from "../wasm/bridge";
import { recordNotebookAccess } from "./libraryPrefs";

/** One tab-wide teardown so a StrictMode remount (and Safari) never opens IDB while the previous connection is closing. */
let replicaTeardown: Promise<void> = Promise.resolve();

interface Connection {
  api: ReadOnlyApi;
  /** POST-only replica API; handed to the sync coordinator and nothing else. */
  replicaApi: ReplicaApi;
  /** Session-scoped verified blob cache (dropped with the connection). */
  blobs: BlobCache;
  libraries: LibrarySummary[];
  /** D1 product mode (cookie session), or null for developer token mode. */
  product: ServerStatusPublic | null;
}

type Phase =
  | { kind: "probing" }
  | { kind: "unreachable"; message: string }
  // D1 product mode (Scribe Server control plane present)
  | { kind: "setup"; status: ServerStatusPublic }
  | { kind: "access"; status: ServerStatusPublic; revoked: boolean }
  // Developer token mode (scribe-sync-server --token-verifier-file)
  | { kind: "locked"; error: string | null; busy: boolean }
  | { kind: "connected"; conn: Connection; libraryId: string | null };

function describe(e: unknown): string {
  if (e instanceof UnauthorizedError) return "Token rejected by the server.";
  return e instanceof Error ? e.message : "Connection failed.";
}

/** One connection attempt (never retried automatically: a bad token stays locked). */
async function attempt(token: string | null, remember: boolean, product: ServerStatusPublic | null = null): Promise<Phase> {
  const api = createReadOnlyApi(token);
  try {
    const libraries = await api.listLibraries();
    if (token && remember) saveToken(token);
    const blobs = new BlobCache((sha) => api.blob(sha), decodeToCanvas);
    return {
      kind: "connected",
      conn: { api, replicaApi: createReplicaApi(token), blobs, libraries, product },
      libraryId: libraries.length === 1 ? libraries[0]!.library_id : null,
    };
  } catch (e) {
    clearToken();
    if (product) return { kind: "access", status: product, revoked: e instanceof UnauthorizedError };
    return { kind: "locked", error: describe(e), busy: false };
  }
}

/**
 * Decide the entry state. A Scribe Server (product) exposes `/control/v1/status`: unclaimed →
 * setup; no session → request access; session → connect with the HttpOnly cookie (no token).
 * Without a control plane this is the developer server: the token screen (sessionStorage).
 */
async function probe(control: ControlApi, savedToken: string | null, revoked = false): Promise<Phase> {
  let status: ServerStatusPublic | null;
  try {
    status = await control.status();
  } catch (e) {
    return { kind: "unreachable", message: e instanceof Error ? e.message : "Ignite Server is not reachable." };
  }
  if (status) {
    if (!status.claimed) return { kind: "setup", status };
    if (!status.session) return { kind: "access", status, revoked };
    return attempt(null, false, status);
  }
  return savedToken ? attempt(savedToken, false) : { kind: "locked", error: null, busy: false };
}

const SETTINGS_PATH = "/settings/server";

export function App() {
  const control = useMemo(() => createControlApi(), []);
  // Developer mode: this tab's session token (if any) is read once.
  const [saved] = useState(() => loadToken());
  const [phase, setPhase] = useState<Phase>({ kind: "probing" });
  const [settings, setSettings] = useState(() => typeof location !== "undefined" && location.pathname === SETTINGS_PATH);

  const reprobe = useCallback(
    (revoked = false) => {
      void probe(control, loadToken(), revoked).then(setPhase);
    },
    [control],
  );

  useEffect(() => {
    let live = true;
    void probe(control, saved).then((p) => live && setPhase(p));
    return () => {
      live = false;
    };
  }, [control, saved]);

  // 401 anywhere (read or replica). Product: back to "request access" — the IndexedDB replica
  // and its outbox are untouched. Developer: back to the token screen.
  const lock = useCallback(
    (error: string | null) => {
      clearToken();
      setPhase((p) => {
        if (p.kind === "connected") p.conn.blobs.clear();
        if (p.kind === "connected" && p.conn.product) {
          void probe(control, null, true).then(setPhase);
          return { kind: "probing" };
        }
        return { kind: "locked", error, busy: false };
      });
    },
    [control],
  );

  const onConnect = (token: string) => {
    setPhase({ kind: "locked", error: null, busy: true });
    void attempt(token || null, true).then(setPhase);
  };

  const openSettings = useCallback((open: boolean) => {
    setSettings(open);
    try {
      history.pushState(null, "", open ? SETTINGS_PATH : "/");
    } catch {
      // history unavailable
    }
  }, []);

  // Product "Sign out": clears this browser's cookie (the device record stays until revoked).
  const signOut = useCallback(() => {
    void control
      .logout()
      .catch(() => undefined)
      .then(() => reprobe(false));
  }, [control, reprobe]);

  const conn = phase.kind === "connected" ? phase.conn : null;
  const libraryId = phase.kind === "connected" ? phase.libraryId : null;
  const session = useMemo(
    () =>
      conn && libraryId
        ? new LibrarySession(conn.api, libraryId, conn.blobs, () => lock("Token rejected by the server."), browserClock, recordNotebookAccess)
        : null,
    [conn, libraryId, lock],
  );
  useEffect(() => {
    if (!session) return;
    void session.start();
    return () => session.stop();
  }, [session]);

  const replica = useReplica(conn, session, lock);

  if (phase.kind === "probing") {
    return (
      <div className="center-screen">
        Connecting…
        <AppVersion />
      </div>
    );
  }
  if (phase.kind === "unreachable") {
    return (
      <div className="center-screen">
        <div role="alert" className="error-text" data-testid="unreachable">
          {phase.message}
        </div>
        <button className="btn" onClick={() => reprobe(false)}>
          Try again
        </button>
        <AppVersion />
      </div>
    );
  }
  if (phase.kind === "setup") return <SetupScreen api={control} serverName={phase.status.server_name} onClaimed={() => reprobe(false)} />;
  if (phase.kind === "access") {
    return <RequestAccessScreen api={control} serverName={phase.status.server_name} revoked={phase.revoked} onAuthorized={() => reprobe(false)} />;
  }
  if (phase.kind === "locked") return <ConnectScreen busy={phase.busy} error={phase.error} onConnect={onConnect} />;
  const product = phase.conn.product;
  const isAdmin = product?.session?.role === "admin";
  if (settings && isAdmin) return <ServerSettings api={control} onClose={() => openSettings(false)} />;
  const disconnect = product ? signOut : () => lock(null);
  if (!session) {
    return (
      <LibraryChooser
        libraries={phase.conn.libraries}
        onOpen={(id) => setPhase({ ...phase, libraryId: id })}
        onDisconnect={disconnect}
        onSettings={isAdmin ? () => openSettings(true) : undefined}
        disconnectLabel={product ? "Sign out" : undefined}
      />
    );
  }
  return (
    <Viewer
      key={session.libraryId}
      session={session}
      blobs={phase.conn.blobs}
      replica={replica}
      onDisconnect={disconnect}
      disconnectLabel={product ? "Sign out" : undefined}
      onSettings={isAdmin ? () => openSettings(true) : undefined}
      onChooseLibrary={phase.conn.libraries.length > 1 ? () => setPhase({ ...phase, libraryId: null }) : undefined}
    />
  );
}

/**
 * S3B browser replica for the open library: the WASM protocol bridge, the IndexedDB store and
 * one sync coordinator. The coordinator gates the session's snapshot refresh (push-first) and
 * compacts its accepted bridge whenever a snapshot is displayed. Viewing works without it.
 */
function useReplica(conn: Connection | null, session: LibrarySession | null, lock: (e: string | null) => void): ReplicaHandle {
  const [slot, setSlot] = useState<{ session: LibrarySession; handle: ReplicaHandle } | null>(null);
  useEffect(() => {
    if (!conn || !session) return;
    let live = true;
    let coordinator: SyncCoordinator | null = null;
    let store: ReplicaStore | null = null;
    const onOnline = () => coordinator?.online();
    const started = replicaTeardown.then(async () => {
      if (!live) return;
      try {
        const parts = await startWritableReplica({
          loadBridge,
          openStore: () => ReplicaStore.open(),
          live: () => live,
        });
        if (!parts) return;
        store = parts.store;
        const { bridge } = parts;
        const lib = session.libraryId;
        let channel: BroadcastChannel | null = null;
        try {
          if (typeof BroadcastChannel === "function") channel = new BroadcastChannel(`scribe-web-sync:${lib}`);
        } catch {
          channel = null;
        }
        coordinator = new SyncCoordinator({
          store: parts.store,
          api: conn.replicaApi,
          libraryId: lib,
          mintReplicaId: () => bridge.mintId("replica"),
          encoder: bridgeEncoder(bridge, lib),
          requestRefresh: () => void session.pollNow(),
          onUnauthorized: () => lock("Token rejected by the server."),
          channel,
          locks: typeof navigator !== "undefined" && navigator.locks ? navigator.locks : null,
        });
        const c = coordinator;
        session.setSyncHooks(
          () => c.canRefresh(),
          (through) => void c.snapshotApplied(through),
        );
        await c.start();
        const shown = session.state.current;
        if (shown) await c.snapshotApplied(shown.through_server_seq);
        if (!live) return;
        window.addEventListener("online", onOnline);
        setSlot({
          session,
          handle: {
            kind: "ready",
            coordinator: c,
            mintId: (k) => bridge.mintId(k),
            project: (s, ops) => bridge.projectPage(s, ops),
            projectTexts: (t, ops) => bridge.projectTexts(t, ops),
            projectLibrary: (snap, ops) => bridge.projectLibrary(snap, ops),
            createNotebookRender: () => bridge.createNotebookRender(),
          },
        });
      } catch (e) {
        console.warn("ignite-web replica init failed:", e);
        coordinator?.stop();
        store?.close();
        store = null;
        coordinator = null;
        if (live) setSlot({ session, handle: { kind: "unavailable", reason: replicaUnavailableReason(e) } });
      }
    });
    return () => {
      live = false;
      window.removeEventListener("online", onOnline);
      session.setSyncHooks(null, null);
      replicaTeardown = started
        .then(async () => {
          coordinator?.stop();
          if (coordinator) await coordinator.idle();
          store?.close();
        })
        .catch(() => {
          store?.close();
        });
    };
  }, [conn, session, lock]);
  return slot && slot.session === session ? slot.handle : { kind: "loading" };
}
