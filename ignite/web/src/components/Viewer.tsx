import { useCallback, useEffect, useMemo, useRef, useState, useSyncExternalStore } from "react";
import { flushSync } from "react-dom";
import { libraryPrefs } from "../app/libraryPrefs";
import type { FinishedStroke } from "../authoring/capture";
import { DEFAULT_TEXT_SESSION_FONT_SIZE, stepTextFontSize, textFontPercent } from "../authoring/textSession";
import { SORT_LABELS } from "../model/librarySort";
import { linkJumpLabel, MISSING_LINK_LABEL, preserveSelection, resolve, resolveLinkTarget, stepPage } from "../model/selection";
import type { PageSpec, SnapshotNotebook, SyncPageLink, SyncStroke, SyncText, SyncSnapshotV1 } from "../model/types";
import type { BlobCache } from "../rendering/background";
import { zoomIn, zoomOut, type ZoomMode } from "../rendering/viewport";
import { conflictText, type CoordinatorState, type OverlayOp, type SyncCoordinator } from "../replica/coordinator";
import type { FolderIntent, StructureIntent } from "../replica/store";
import type { LibrarySession } from "../state/librarySession";
import type { LibraryOp, ProjectOp, TextProjectOp } from "../wasm/bridge";
import igniteLogo from "../assets/ignite.png";
import { AppVersion } from "./AppVersion";
import { KeyboardIcon, PenIcon } from "./Icons";
import { RoughControlButton } from "./RoughControlButton";
import { NotebookSidebar } from "./NotebookSidebar";
import { PageView, type EraseGesture, type TypeSession } from "./PageView";
import { ThemeToggle } from "./ThemeToggle";

/** The browser replica for this library (S3B), or why writing is unavailable. */
export type ReplicaHandle =
  | { kind: "loading" }
  | { kind: "unavailable"; reason: string }
  | {
      kind: "ready";
      coordinator: SyncCoordinator;
      mintId: (kind: "operation" | "stroke" | "folder" | "text" | "notebook" | "page") => string;
      /** The reducer's page projection (WASM): snapshot strokes through pending ops. */
      project: (strokes: readonly SyncStroke[], ops: readonly ProjectOp[]) => { strokes: SyncStroke[]; changed: boolean[] };
      projectTexts: (texts: readonly SyncText[], ops: readonly TextProjectOp[]) => { texts: SyncText[]; changed: boolean[] };
      projectLibrary: (snapshot: SyncSnapshotV1, ops: readonly LibraryOp[]) => { notebooks: SnapshotNotebook[]; folders: import("../model/types").SnapshotFolder[]; changed: boolean[] };
      createNotebookRender: () => import("../model/types").RenderProfileRef;
    };

interface Props {
  session: LibrarySession;
  blobs: BlobCache;
  onDisconnect: () => void;
  onChooseLibrary?: () => void;
  replica?: ReplicaHandle;
  /** D1 product mode: admin browsers reach Settings → Server. */
  onSettings?: () => void;
  disconnectLabel?: string;
}

const FIT: ZoomMode = { kind: "fit" };
const UNAVAILABLE: ReplicaHandle = { kind: "unavailable", reason: "Writing is not available." };
const NO_SYNC: CoordinatorState = { ready: false, replicaId: null, phase: "idle", pending: 0, conflict: null, overlay: [], saveError: null };
const noSubscribe = () => () => undefined;
const TOAST_MS = 1800;

function statusLabel(s: CoordinatorState): string | null {
  switch (s.phase) {
    case "conflict":
      return "Conflict";
    case "locked":
      return "Signed out";
    case "offline":
      return `Offline · ${s.pending} pending`;
    case "syncing":
      return "Syncing…";
    case "saved":
      return "Saved locally";
    case "synced":
      return "Synced";
    default:
      return null;
  }
}

export function Viewer({ session, blobs, onDisconnect, onChooseLibrary, replica = UNAVAILABLE, onSettings, disconnectLabel = "Disconnect" }: Props) {
  const state = useSyncExternalStore(
    useCallback((fn: () => void) => session.subscribe(fn), [session]),
    () => session.state,
  );
  const coord = replica.kind === "ready" ? replica.coordinator : null;
  const sync = useSyncExternalStore(
    useCallback((fn: () => void) => (coord ? coord.subscribe(fn) : noSubscribe()), [coord]),
    () => (coord ? coord.state : NO_SYNC),
  );
  const [drawerOpen, setDrawerOpen] = useState(false);
  // Zoom belongs to the page being viewed: a new page/notebook starts at Fit.
  const [zoom, setZoom] = useState<{ pageId: string; mode: ZoomMode } | null>(null);
  // S3B: the viewer opens read-only; Pen enters ink, Keyboard enters type-anywhere.
  const [uiMode, setUiMode] = useState<"read" | "ink" | "type">("read");
  const [textFontSize, setTextFontSize] = useState(DEFAULT_TEXT_SESSION_FONT_SIZE);
  const [drawWithTouch, setDrawWithTouch] = useState(false);
  const [tool, setTool] = useState<"pen" | "eraser">("pen");
  const [notice, setNotice] = useState<string | null>(null);
  const [pageConfirm, setPageConfirm] = useState<"delete" | null>(null);
  // Sort mode + open history: replica-local UI prefs (`scribe-web-prefs`), never synced.
  const prefs = libraryPrefs();
  const prefsState = useSyncExternalStore(prefs.subscribe, prefs.getState);
  // Transient confirmation, shown only for an explicit sort change (restoring is silent).
  const [toast, setToast] = useState<{ text: string; n: number } | null>(null);
  useEffect(() => {
    if (!toast) return;
    const h = setTimeout(() => setToast(null), TOAST_MS);
    return () => clearTimeout(h);
  }, [toast]);
  const drawing = useRef(false);
  const typeSession = useRef<TypeSession | null>(null);
  const writing = uiMode === "ink";
  const typing = uiMode === "type";

  /** One transition gate for every action that can replace the bound text draft. */
  const finalizeTextForTransition = useCallback(async (): Promise<boolean> => {
    if (uiMode !== "type") return true;
    const active = typeSession.current;
    if (!active) return true;
    return (await active.finalizeActive()) === "success";
  }, [uiMode]);

  const afterTextFinalization = useCallback(
    async (transition: () => void | Promise<void>): Promise<boolean> => {
      if (!(await finalizeTextForTransition())) return false;
      await transition();
      return true;
    },
    [finalizeTextForTransition],
  );

  const stepSessionFontSize = useCallback((direction: -1 | 1) => {
    const next = stepTextFontSize(textFontSize, direction);
    setTextFontSize(next);
    typeSession.current?.setFontSize(next);
  }, [textFontSize]);

  const cur = state.current;
  const snapshot = cur?.snapshot;
  const through = cur?.through_server_seq ?? 0n;
  const projectLibrary = replica.kind === "ready" ? replica.projectLibrary : null;
  const viewSnapshot = useMemo(() => {
    if (!snapshot) return null;
    const ops = sync.overlay.flatMap((o): LibraryOp[] => {
      if (!(o.server_seq === null || o.server_seq > through)) return [];
      if (o.kind === "folder") return [o.folder];
      if (o.kind === "structure") return [o.structure];
      return [];
    });
    if (ops.length === 0 || !projectLibrary) return snapshot;
    try {
      const out = projectLibrary(snapshot, ops);
      return { ...snapshot, notebooks: out.notebooks, folders: out.folders };
    } catch {
      return snapshot;
    }
  }, [snapshot, sync.overlay, through, projectLibrary]);
  const viewSel = viewSnapshot ? preserveSelection(snapshot ?? null, state.selection, viewSnapshot) : state.selection;
  const r = viewSnapshot ? resolve(viewSnapshot, viewSel) : null;
  const pageId = r?.page.page_id ?? "";
  const notebookId = r?.notebook.notebook_id ?? "";
  const mode = zoom && zoom.pageId === pageId ? zoom.mode : FIT;

  const go = useCallback(
    async (delta: number) => {
      if (!viewSnapshot || !viewSel || drawing.current) return; // never switch pages mid-stroke
      const next = stepPage(viewSnapshot, viewSel, delta);
      if (next) await afterTextFinalization(() => session.select(next));
    },
    [viewSnapshot, viewSel, session, afterTextFinalization],
  );

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.target instanceof HTMLInputElement || e.target instanceof HTMLTextAreaElement) return;
      if (e.key === "ArrowLeft" || e.key === "PageUp") void go(-1);
      if (e.key === "ArrowRight" || e.key === "PageDown") void go(1);
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [go]);

  const onLinkClick = useCallback(
    async (link: SyncPageLink) => {
      if (!viewSnapshot || drawing.current) return;
      const next = resolveLinkTarget(viewSnapshot, link);
      if (!next) {
        setNotice(MISSING_LINK_LABEL);
        return;
      }
      await afterTextFinalization(() => session.select(next));
    },
    [viewSnapshot, session, afterTextFinalization],
  );

  const onPinchZoom = useCallback((m: ZoomMode) => setZoom({ pageId, mode: m }), [pageId]);

  const onWheelZoom = useCallback(
    (d: 1 | -1) => setZoom({ pageId, mode: d > 0 ? zoomIn(mode) : zoomOut(mode) }),
    [pageId, mode],
  );

  // Layer 2: this page's durable pending ops the displayed snapshot does not yet hold, projected
  // over the snapshot's strokes by the reducer (WASM) in client_seq order.
  const pageOps = useMemo(
    () =>
      sync.overlay.filter(
        (o): o is Extract<OverlayOp, { kind: "add" | "erase" | "undo" }> =>
          (o.kind === "add" || o.kind === "erase" || o.kind === "undo") &&
          o.page_id === pageId &&
          o.notebook_id === notebookId &&
          (o.server_seq === null || o.server_seq > through),
      ),
    [sync.overlay, pageId, notebookId, through],
  );
  const textOps = useMemo(
    () =>
      sync.overlay.filter(
        (o): o is Extract<typeof o, { kind: "add_text" | "update_text" | "delete_text" }> =>
          (o.kind === "add_text" || o.kind === "update_text" || o.kind === "delete_text") &&
          o.page_id === pageId &&
          o.notebook_id === notebookId &&
          (o.server_seq === null || o.server_seq > through),
      ),
    [sync.overlay, pageId, notebookId, through],
  );
  const pendingOpIds = useMemo(() => new Set(pageOps.map((o) => o.operation_id)), [pageOps]);
  const project = replica.kind === "ready" ? replica.project : null;
  const projectTexts = replica.kind === "ready" ? replica.projectTexts : null;
  const snapshotStrokes = r?.page.strokes;
  const display = useMemo<SyncStroke[] | undefined>(() => {
    if (!snapshotStrokes || pageOps.length === 0 || !project) return undefined;
    try {
      return project(
        snapshotStrokes,
        pageOps.map((o): ProjectOp => {
          if (o.kind === "add") return { op: "add", stroke: o.stroke };
          if (o.kind === "undo") return { op: "delete_lineage", root_id: o.root_id };
          return { op: "erase", operation_id: o.operation_id, ...o.erase };
        }),
      ).strokes;
    } catch {
      return undefined; // a projection problem never hides the server's canonical page
    }
  }, [snapshotStrokes, pageOps, project]);
  const displayTexts = useMemo<SyncText[]>(() => {
    const snapshotTexts = r?.page.texts ?? [];
    if (textOps.length === 0 || !projectTexts) return snapshotTexts;
    try {
      return projectTexts(
        snapshotTexts,
        textOps.map((o): TextProjectOp => {
          if (o.kind === "add_text") return { op: "add", text: o.text };
          if (o.kind === "update_text") return { op: "update", text: o.text };
          return { op: "delete", text_id: o.text_id };
        }),
      ).texts;
    } catch {
      return snapshotTexts;
    }
  }, [r?.page.texts, textOps, projectTexts]);

  const previewErase = useCallback(
    (base: readonly SyncStroke[], g: EraseGesture) => {
      if (!project) return null;
      try {
        const out = project(base, [{ op: "erase", ...g }]);
        return { strokes: out.strokes, changed: out.changed[0] === true };
      } catch {
        return null;
      }
    },
    [project],
  );

  const membership = useMemo(() => {
    const m = new Map<string, string>();
    for (const n of viewSnapshot?.notebooks ?? []) if (n.folder_id) m.set(n.notebook_id, n.folder_id);
    return m;
  }, [viewSnapshot]);

  const commitFolder = useCallback(
    async (folder: FolderIntent): Promise<boolean> => {
      if (replica.kind !== "ready") return false;
      let operationId: string;
      try {
        operationId = replica.mintId("operation");
      } catch {
        setNotice("Couldn’t save that folder change on this device. Nothing was changed.");
        return false;
      }
      const op = await replica.coordinator.commit({ kind: "folder", operation_id: operationId, notebook_id: "", page_id: "", folder });
      return op !== null;
    },
    [replica],
  );

  const commitStructure = useCallback(
    async (structure: StructureIntent): Promise<boolean> => {
      if (!(await finalizeTextForTransition())) return false;
      if (replica.kind !== "ready") return false;
      let operationId: string;
      try {
        operationId = replica.mintId("operation");
      } catch {
        setNotice("Couldn’t save that library change on this device. Nothing was changed.");
        return false;
      }
      const notebook_id = structure.notebook_id;
      const page_id =
        structure.type === "create_notebook"
          ? structure.first_page.page_id
          : structure.type === "create_page"
            ? structure.page.page_id
            : structure.type === "delete_page"
              ? structure.page_id
              : "";
      const op = await replica.coordinator.commit({ kind: "structure", operation_id: operationId, notebook_id, page_id, structure });
      return op !== null;
    },
    [replica, finalizeTextForTransition],
  );

  const createNotebook = useCallback(
    async (title: string, folderId: string | null): Promise<boolean> => {
      if (!(await finalizeTextForTransition())) return false;
      if (replica.kind !== "ready" || !viewSnapshot) return false;
      let notebookId: string;
      let pageId: string;
      let render: import("../model/types").RenderProfileRef;
      try {
        notebookId = replica.mintId("notebook");
        pageId = replica.mintId("page");
        render = replica.createNotebookRender();
      } catch {
        setNotice("Couldn’t create a notebook on this device. Nothing was changed.");
        return false;
      }
      const last = viewSnapshot.notebooks.at(-1)?.notebook_id;
      const first_page: PageSpec = { page_id: pageId, background: { kind: "blank" }, render };
      const ok = await commitStructure({
        type: "create_notebook",
        notebook_id: notebookId,
        title,
        ...(last ? { after_notebook_id: last } : {}),
        first_page,
      });
      if (!ok) return false;
      session.select({ notebookId, pageId });
      if (folderId) {
        const filed = await commitFolder({ type: "set_notebook_folder", notebook_id: notebookId, folder_id: folderId });
        if (!filed) setNotice("Notebook created, but it could not be added to the folder. It is in the main list.");
      }
      return true;
    },
    [replica, viewSnapshot, commitStructure, commitFolder, session, finalizeTextForTransition],
  );

  const mintOperationId = useCallback(() => (replica.kind === "ready" ? replica.mintId("operation") : ""), [replica]);
  const mintTextId = useCallback(() => (replica.kind === "ready" ? replica.mintId("text") : ""), [replica]);

  const onTextCommit = useCallback(
    async (
      intent:
        | { kind: "add" | "update"; text: SyncText; notebook_id: string; page_id: string }
        | { kind: "delete"; text_id: string; notebook_id: string; page_id: string },
    ): Promise<boolean> => {
      if (replica.kind !== "ready") return false;
      // Bound by the caller to the page where the edit or drag began.
      const { notebook_id, page_id } = intent;
      if (!notebook_id || !page_id) return false;
      let operationId: string;
      try {
        operationId = replica.mintId("operation");
      } catch {
        setNotice("Couldn’t save that text on this device. Nothing was changed.");
        return false;
      }
      const draft =
        intent.kind === "delete"
          ? { kind: "delete_text" as const, operation_id: operationId, notebook_id, page_id, text_id: intent.text_id }
          : {
              kind: intent.kind === "add" ? ("add_text" as const) : ("update_text" as const),
              operation_id: operationId,
              notebook_id,
              page_id,
              text: intent.text,
            };
      return (await replica.coordinator.commit(draft)) !== null;
    },
    [replica],
  );

  const leaveAuthoring = useCallback(async (next: "read" | "ink" | "type"): Promise<boolean> => {
    if (drawing.current) return false;
    if (!(await finalizeTextForTransition())) return false;
    if (uiMode === "type" && next !== "type") setTextFontSize(DEFAULT_TEXT_SESSION_FONT_SIZE);
    setUiMode(next);
    if (next === "read") setNotice(null);
    return true;
  }, [finalizeTextForTransition, uiMode]);

  const onErase = useCallback(
    async (g: EraseGesture): Promise<boolean> => {
      if (replica.kind !== "ready" || !pageId || !notebookId) return false;
      const op = await replica.coordinator.commit({
        kind: "erase",
        operation_id: g.operation_id,
        notebook_id: notebookId,
        page_id: pageId,
        erase: { observed_roots: g.observed_roots, path: g.path },
      });
      return op !== null;
    },
    [replica, pageId, notebookId],
  );

  const onStroke = useCallback(
    async (s: FinishedStroke): Promise<string | null> => {
      if (replica.kind !== "ready" || !pageId || !notebookId) return null;
      let strokeId: string;
      let operationId: string;
      try {
        strokeId = replica.mintId("stroke");
        operationId = replica.mintId("operation");
      } catch {
        setNotice("Couldn’t save that stroke on this device. It was not added.");
        return null;
      }
      // Identity is fixed at pointer up: the stroke keeps this NotebookId/PageId for good.
      const op = await replica.coordinator.commit({
        operation_id: operationId,
        notebook_id: notebookId,
        page_id: pageId,
        stroke: { stroke_id: strokeId, root_id: strokeId, style: s.style, points: s.points },
      });
      return op ? strokeId : null;
    },
    [replica, pageId, notebookId],
  );

  const onDrawingChange = useCallback((d: boolean) => {
    drawing.current = d;
  }, []);

  const visibleStrokes = display ?? r?.page.strokes ?? [];
  const undoRoot = visibleStrokes.at(-1)?.root_id ?? null;

  const onUndo = useCallback(async () => {
    if (replica.kind !== "ready" || !pageId || !notebookId || !undoRoot || drawing.current) return;
    let operationId: string;
    try {
      operationId = replica.mintId("operation");
    } catch {
      setNotice("Couldn’t undo on this device. Nothing was changed.");
      return;
    }
    await replica.coordinator.commit({
      kind: "undo",
      operation_id: operationId,
      notebook_id: notebookId,
      page_id: pageId,
      root_id: undoRoot,
    });
  }, [replica, pageId, notebookId, undoRoot]);

  const onInsertPage = useCallback(async () => {
    if (!(await finalizeTextForTransition())) return;
    if (replica.kind !== "ready" || !r) return;
    let pageIdNew: string;
    try {
      pageIdNew = replica.mintId("page");
    } catch {
      setNotice("Couldn’t add a page on this device. Nothing was changed.");
      return;
    }
    const spec: PageSpec = { page_id: pageIdNew, background: r.page.background, render: r.page.render };
    const ok = await commitStructure({
      type: "create_page",
      notebook_id: r.notebook.notebook_id,
      page: spec,
      after_page_id: r.page.page_id,
    });
    if (ok) session.select({ notebookId: r.notebook.notebook_id, pageId: pageIdNew });
  }, [replica, r, commitStructure, session, finalizeTextForTransition]);

  const onDeletePage = useCallback(async () => {
    if (!r || r.notebook.pages.length < 2) return;
    setPageConfirm(null);
    await commitStructure({
      type: "delete_page",
      notebook_id: r.notebook.notebook_id,
      page_id: r.page.page_id,
    });
  }, [r, commitStructure]);

  const disconnect = async () => {
    if (!(await finalizeTextForTransition())) return;
    if (sync.pending > 0) {
      const n = sync.pending;
      const ok = window.confirm(
        `${n} ${n === 1 ? "stroke is" : "strokes are"} still waiting to sync. ${n === 1 ? "It stays" : "They stay"} saved in this browser and will sync the next time you connect. Disconnect anyway?`,
      );
      if (!ok) return;
    }
    onDisconnect();
  };

  if (!cur || !r) {
    return (
      <div className="center-screen">
        {state.fatal ? (
          <div role="alert" className="error-text" data-testid="library-error">
            Couldn’t load the library: {state.fatal}
          </div>
        ) : (
          <div>Loading library…</div>
        )}
        <button className="btn" onClick={() => void disconnect()}>
          Disconnect
        </button>
        <AppVersion />
      </div>
    );
  }

  const { notebook, page, pageIndex } = r;
  const pageCount = notebook.pages.length;
  const percent = Math.round((mode.kind === "fit" ? 1 : mode.factor) * 100);
  const syncLabel = statusLabel(sync);
  const saveMessage = sync.saveError ?? notice;

  return (
    <div className="viewer">
      <header className="topbar">
        <button
          className="btn icon drawer-toggle"
          aria-label="Notebooks"
          aria-expanded={drawerOpen}
          data-testid="drawer-toggle"
          onClick={() => setDrawerOpen((o) => !o)}
        >
          ☰
        </button>
        <span className="brand">
          <img className="brand-logo" src={igniteLogo} alt="Ignite" />
        </span>
        <span className="spacer" />
        {onChooseLibrary && uiMode === "read" && (
          <button className="btn subtle" onClick={onChooseLibrary}>
            Libraries
          </button>
        )}
        {uiMode === "ink" ? (
          <span className="write-tools" role="toolbar" aria-label="Writing tools">
            <button className="btn" aria-pressed={tool === "pen"} data-testid="pen" onClick={() => setTool("pen")}>
              Pen
            </button>
            <button className="btn" aria-pressed={tool === "eraser"} data-testid="eraser" onClick={() => setTool("eraser")}>
              Eraser
            </button>
            <button className="btn" data-testid="undo" disabled={!undoRoot} title={undoRoot ? "Undo last stroke" : "Nothing to undo"} onClick={() => void onUndo()}>
              Undo
            </button>
            <label className="touch-toggle">
              <input type="checkbox" checked={drawWithTouch} onChange={(e) => setDrawWithTouch(e.target.checked)} data-testid="draw-with-touch" />
              <span className="touch-toggle-label">Touch draw</span>
            </label>
            <RoughControlButton identity="done" className="primary" data-testid="done" onClick={() => void leaveAuthoring("read")}>
              Done
            </RoughControlButton>
          </span>
        ) : uiMode === "type" ? (
          <span className="write-tools" role="toolbar" aria-label="Typing tools">
            <RoughControlButton
              identity="text-size-down"
              className="icon text-size-control"
              data-testid="text-size-down"
              data-text-session-control
              aria-label="Decrease text size"
              title="Decrease text size"
              disabled={textFontSize === 240}
              onPointerDown={(e) => e.preventDefault()}
              onClick={() => stepSessionFontSize(-1)}
            >
              A−
            </RoughControlButton>
            <span className="text-size-label" data-testid="text-size-label" aria-label={`Text size ${textFontPercent(textFontSize)} percent`}>
              {textFontPercent(textFontSize)}%
            </span>
            <RoughControlButton
              identity="text-size-up"
              className="icon text-size-control"
              data-testid="text-size-up"
              data-text-session-control
              aria-label="Increase text size"
              title="Increase text size"
              disabled={textFontSize === 680}
              onPointerDown={(e) => e.preventDefault()}
              onClick={() => stepSessionFontSize(1)}
            >
              A+
            </RoughControlButton>
            <RoughControlButton identity="done" className="primary" data-testid="done" onClick={() => void leaveAuthoring("read")}>
              Done
            </RoughControlButton>
          </span>
        ) : (
          <>
            <RoughControlButton
              identity="write"
              className="icon"
              data-testid="write"
              disabled={replica.kind !== "ready" || sync.conflict !== null}
              title={
                replica.kind === "unavailable"
                  ? replica.reason
                  : sync.conflict
                    ? "Writing is paused by a sync conflict."
                    : "Write on this page"
              }
              aria-label="Write"
              onClick={() => {
                if (replica.kind !== "ready") return;
                void leaveAuthoring("ink");
                void replica.coordinator.ensureReplica().catch(() => setNotice("This browser can’t store strokes (storage unavailable)."));
              }}
            >
              <PenIcon />
            </RoughControlButton>
            <RoughControlButton
              identity="type"
              className="icon"
              data-testid="type"
              disabled={replica.kind !== "ready" || sync.conflict !== null}
              title={
                replica.kind === "unavailable"
                  ? replica.reason
                  : sync.conflict
                    ? "Typing is paused by a sync conflict."
                    : "Type on this page"
              }
              aria-label="Type"
              onClick={() => {
                if (replica.kind !== "ready") return;
                void leaveAuthoring("type");
                void replica.coordinator.ensureReplica().catch(() => setNotice("This browser can’t store text (storage unavailable)."));
              }}
            >
              <KeyboardIcon />
            </RoughControlButton>
          </>
        )}
        {syncLabel && (
          <span className={`sync-status ${sync.phase}`} data-testid="sync-status" data-phase={sync.phase} data-pending={sync.pending}>
            {syncLabel}
          </span>
        )}
        <span
          className={`status ${state.stale ? "stale" : "ok"}`}
          data-testid="status"
          data-head={String(cur.through_server_seq)}
          data-snapshot={cur.snapshot_sha256}
          title={state.stale ? `Showing last good state — ${state.stale}` : `Server sequence ${cur.through_server_seq}`}
        >
          {state.stale ? "Stale" : "Connected"} <span className="dot" aria-hidden>●</span>
        </span>
        {onSettings && uiMode === "read" && (
          <button className="btn subtle" onClick={onSettings} data-testid="settings">
            Settings
          </button>
        )}
        <ThemeToggle />
        <button className="btn subtle" onClick={() => void disconnect()} data-testid="disconnect">
          {disconnectLabel}
        </button>
      </header>

      {sync.conflict && (
        <div className="banner conflict" role="alert" data-testid="sync-conflict">
          <span>{conflictText(sync.conflict)}</span>
          {replica.kind === "ready" && (
            <button className="btn subtle" data-testid="retry-sync" onClick={() => void replica.coordinator.retryAfterConflict()}>
              Retry
            </button>
          )}
        </div>
      )}
      {saveMessage && (
        <div className="banner warn" role="status" data-testid="save-error">
          <span>{saveMessage}</span>
          {notice && (
            <button className="btn subtle" onClick={() => setNotice(null)} aria-label="Dismiss">
              ×
            </button>
          )}
        </div>
      )}

      <div className="main">
        <nav className={`sidebar ${drawerOpen ? "open" : ""}`} aria-label="Notebooks" data-testid="notebooks">
          <NotebookSidebar
            notebooks={viewSnapshot!.notebooks}
            folders={viewSnapshot!.folders}
            membership={membership}
            selectedId={notebook.notebook_id}
            onSelect={(n) => {
              if (drawing.current) return;
              void afterTextFinalization(() => {
                session.select({ notebookId: n.notebook_id, pageId: n.pages[0]!.page_id });
                setDrawerOpen(false);
              });
            }}
            readOnlyReason={
              replica.kind !== "ready" ? (replica.kind === "unavailable" ? replica.reason : "Loading…") : sync.conflict ? "Editing is paused by a sync conflict." : null
            }
            mintFolderId={() => (replica.kind === "ready" ? replica.mintId("folder") : "")}
            commit={commitFolder}
            createNotebook={createNotebook}
            renameNotebook={(notebookId, title) => commitStructure({ type: "rename_notebook", notebook_id: notebookId, title })}
            deleteNotebook={(notebookId) => commitStructure({ type: "delete_notebook", notebook_id: notebookId })}
            canDeleteNotebook={viewSnapshot!.notebooks.length > 1}
            sortMode={prefsState.sortMode}
            access={prefsState.access}
            onCycleSort={() => {
              const m = prefs.cycleSortMode();
              setToast((t) => ({ text: SORT_LABELS[m], n: (t?.n ?? 0) + 1 }));
            }}
          />
        </nav>
        {drawerOpen && <div className="scrim" onClick={() => setDrawerOpen(false)} />}

        <main className="page-pane">
          <PageView
            page={page}
            notebookId={notebookId}
            pageSize={cur.snapshot.page_space}
            blobs={blobs}
            mode={mode}
            version={state.version}
            onWheelZoom={onWheelZoom}
            onZoom={onPinchZoom}
            display={display}
            displayTexts={displayTexts}
            onLinkClick={onLinkClick}
            linkLabel={viewSnapshot ? (link) => linkJumpLabel(viewSnapshot, link) : undefined}
            pendingCount={pageOps.length}
            pendingOpIds={pendingOpIds}
            writing={writing}
            typing={typing}
            textFontSize={textFontSize}
            onTextFontSizeChange={setTextFontSize}
            onActivateText={(t) => {
              if (replica.kind !== "ready" || sync.conflict || drawing.current) return;
              void (async () => {
                flushSync(() => setUiMode("type"));
                const opened = await typeSession.current?.edit(t);
                if (opened === false) setUiMode("read");
              })();
            }}
            drawWithTouch={drawWithTouch}
            tool={tool}
            mintOperationId={mintOperationId}
            mintTextId={mintTextId}
            previewErase={previewErase}
            onErase={onErase}
            onStroke={onStroke}
            onTextCommit={onTextCommit}
            onTypeSession={(s) => {
              typeSession.current = s;
            }}
            onDrawingChange={onDrawingChange}
            onRefused={setNotice}
          />
        </main>
      </div>

      <footer className="bottombar">
        <div className="nav-group">
          <RoughControlButton
            identity="previous-page"
            className="icon"
            aria-label="Previous page"
            disabled={pageIndex === 0}
            onClick={() => void go(-1)}
            data-testid="prev"
          >
            ‹
          </RoughControlButton>
          <span className="page-label" data-testid="page-label">
            {pageIndex + 1} of {pageCount}
          </span>
          <RoughControlButton
            identity="next-page"
            className="icon"
            aria-label="Next page"
            disabled={pageIndex + 1 >= pageCount}
            onClick={() => void go(1)}
            data-testid="next"
          >
            ›
          </RoughControlButton>
          {uiMode !== "ink" && (
            <RoughControlButton
              identity="undo"
              data-testid="undo"
              disabled={!undoRoot}
              title={undoRoot ? "Undo last stroke" : "Nothing to undo"}
              onClick={() => void onUndo()}
            >
              Undo
            </RoughControlButton>
          )}
          <RoughControlButton
            identity="insert-page"
            data-testid="insert-page"
            disabled={replica.kind !== "ready" || sync.conflict !== null}
            title="Insert a blank page after this one"
            onClick={() => void onInsertPage()}
          >
            Insert page
          </RoughControlButton>
          <RoughControlButton
            identity="delete-page"
            data-testid="delete-page"
            disabled={replica.kind !== "ready" || sync.conflict !== null || pageCount < 2}
            title={pageCount < 2 ? "A notebook needs at least one page." : "Delete this page"}
            onClick={() => setPageConfirm("delete")}
          >
            Delete page
          </RoughControlButton>
        </div>
        <div className="zoom-group">
          <RoughControlButton
            identity="fit"
            onClick={() => setZoom({ pageId, mode: FIT })}
            aria-pressed={mode.kind === "fit"}
            data-testid="fit"
          >
            Fit
          </RoughControlButton>
          <RoughControlButton
            identity="zoom-out"
            className="icon"
            aria-label="Zoom out"
            onClick={() => setZoom({ pageId, mode: zoomOut(mode) })}
            data-testid="zoom-out"
          >
            −
          </RoughControlButton>
          <span className="zoom-label" data-testid="zoom-label">
            {percent}%
          </span>
          <RoughControlButton
            identity="zoom-in"
            className="icon"
            aria-label="Zoom in"
            onClick={() => setZoom({ pageId, mode: zoomIn(mode) })}
            data-testid="zoom-in"
          >
            +
          </RoughControlButton>
        </div>
        <AppVersion />
      </footer>
      {toast && (
        <div key={toast.n} className="update-toast" role="status" data-testid="sort-toast">
          {toast.text}
        </div>
      )}
      {pageConfirm === "delete" && (
        <div className="dialog-scrim" onClick={() => setPageConfirm(null)}>
          <div className="dialog" role="dialog" aria-modal="true" aria-label="Delete page" data-testid="page-dialog" onClick={(e) => e.stopPropagation()}>
            <h2 className="dialog-heading">Delete this page?</h2>
            <p className="dialog-text">This page and its ink are removed from the notebook. This cannot be undone here.</p>
            <div className="dialog-actions">
              <button className="btn subtle" onClick={() => setPageConfirm(null)}>
                Cancel
              </button>
              <button className="btn primary" data-testid="confirm-delete-page" onClick={() => void onDeletePage()}>
                Delete
              </button>
            </div>
          </div>
        </div>
      )}
    </div>
  );
}
