// The page viewport. A spacer sized to the zoomed page gives native scrolling/panning; a
// viewport-sized canvas on top is redrawn from vectors for the visible region (memory stays
// bounded at any zoom).
//
// Three layers (S3B):
//   1. server canonical strokes (the displayed snapshot)       ┐ main canvas, one draw: the
//   2. durable pending overlay (IndexedDB, client_seq order)   ┘ snapshot projected through
//      the pending AddStroke/EraseRegion ops by the reducer (`display`, from the Viewer)
//   3. wet ink (the stroke under the pen, and strokes whose commit has not yet surfaced in the
//      overlay) — a separate transparent canvas, cleared only once layer 2 has the stroke, so
//      ink never blinks during the hand-off.
// An erase gesture previews on the main canvas: the shown strokes projected through the
// in-progress sweep by the reducer's erase (`previewErase`). The preview stays until the
// committed EraseRegion is part of `display` (same geometry, same fragment IDs), so erased ink
// never reappears during the hand-off.
//
// Outside write mode every pointer only scrolls — nothing can create or erase ink. In write
// mode pen and mouse draw (or erase with the Eraser tool / the pen's eraser end); touch pans
// (unless "Draw with touch" is on).
//
// Touch gestures (every mode): one finger pans after a small tap slop; two fingers pinch-zoom
// around the page point between them. A second finger always means a gesture: with "Draw with
// touch" on, a finger stroke in progress is dropped (never authored) and becomes the pinch.
// In type mode a touch that armed as a pan or pinch never places/edits text on pointerup.
// This component never talks to the network or storage: completed gestures go to `onStroke` /
// `onErase` / text commit, which commit them durably.

import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState, useSyncExternalStore, type PointerEvent as ReactPointerEvent } from "react";
import { currentTheme, subscribeTheme } from "../app/theme";
import { EraseCapture, StrokeCapture, insidePage, toCanonical, type FinishedStroke, type InkPointerType, type Sample } from "../authoring/capture";
import { BLUR_SUPPRESS_MS, keepEditorInVisualViewport, onVisualViewportChange } from "../authoring/editorViewport";
import { advancePan, beginPan, type PanTracker } from "../authoring/gesture";
import { fitTextEditorHeight } from "../authoring/textEditorLayout";
import {
  advanceTextGrab,
  beginTextGrab,
  clearSuppressedRelease,
  consumeSuppressedRelease,
  dragCanonicalDelta,
  dragPreviewDelta,
  suppressPointerRelease,
  type TextGrab,
} from "../authoring/textDrag";
import {
  DEFAULT_TEXT_FONT_SIZE,
  buildMoveUpdate,
  editDraft,
  newDraft,
  normalizeLineEndings,
  runDelete,
  runFinish,
  sameText,
  type TextDraft,
  type TextPersistIntent,
} from "../authoring/textDraft";
import { TextFinalizationGate, type TextFinalizeResult } from "../authoring/textSession";
import { ERASER_RADIUS as ERASE_RADIUS } from "../model/generated/protocolConstants";
import { LinkIcon, TrashIcon } from "./Icons";
import type { SnapshotPage, SyncPageLink, SyncStroke, SyncText } from "../model/types";
import type { BlobCache } from "../rendering/background";
import { linkIconLayout } from "../rendering/pageLinks";
import { DARK_DESK_COLOR, DESK_COLOR, paintInk, planPage, renderPage } from "../rendering/renderPage";
import { planStroke } from "../rendering/strokePlan";
import {
  canonicalAt,
  layout,
  pinchFactor,
  screenTransform,
  scrollForZoom,
  scrollToAnchor,
  type Layout,
  type ScreenTransform,
  type Size,
  type ZoomMode,
} from "../rendering/viewport";

export interface PageViewProps {
  page: SnapshotPage;
  /** Owning notebook: text commits are bound to the page where the edit/drag began. */
  notebookId: string;
  pageSize: Size;
  blobs: BlobCache;
  mode: ZoomMode;
  /** Bumps when a background finishes loading (repaint trigger). */
  version: number;
  onWheelZoom?: (direction: 1 | -1) => void;
  /** Touch pinch: set the zoom (the caller owns the zoom mode). */
  onZoom?: (mode: ZoomMode) => void;
  /** Layers 1+2: the page's strokes after pending ops (defaults to the snapshot's). */
  display?: readonly SyncStroke[];
  /** Snapshot texts after pending text ops. */
  displayTexts?: readonly SyncText[];
  /**
   * Persist one text intent. `notebook_id`/`page_id` bind the commit to the page
   * where the edit or drag began (never the newly current page after a switch).
   */
  onTextCommit?: (
    intent:
      | { kind: "add" | "update"; text: SyncText; notebook_id: string; page_id: string }
      | { kind: "delete"; text_id: string; notebook_id: string; page_id: string },
  ) => Promise<boolean>;
  /** Snapshot page links (Kindle-authored; the browser only paints and jumps). */
  onLinkClick?: (link: SyncPageLink) => void;
  /** Hover tooltip / accessible name: destination `"Title - Page N"`. */
  linkLabel?: (link: SyncPageLink) => string;
  typing?: boolean;
  /** Read-mode click on existing text: caller should enter Type mode. */
  onActivateText?: (text: SyncText) => void;
  /** Text-session preference used only when a new block is placed. */
  textFontSize?: number;
  /** Existing block activation seeds the session preference from stored state. */
  onTextFontSizeChange?: (fontSize: number) => void;
  mintTextId?: () => string;
  onTypeSession?: (session: TypeSession | null) => void;
  /** Number of pending ops affecting this page (diagnostic `data-pending`). */
  pendingCount?: number;
  /** OperationIds of the pending ops already reflected in `display`. */
  pendingOpIds?: ReadonlySet<string>;
  /** Write mode (pen/mouse ink). */
  writing?: boolean;
  drawWithTouch?: boolean;
  /** Active tool in write mode. A pen's eraser end always erases. */
  tool?: "pen" | "eraser";
  /** A fresh OperationId for an erase gesture (fixed at pointer down, so preview fragment IDs match). */
  mintOperationId?: () => string;
  /** Reducer projection of `base` through one erase; null if unavailable. */
  previewErase?: (base: readonly SyncStroke[], erase: EraseGesture) => { strokes: SyncStroke[]; changed: boolean } | null;
  /** A completed erase that changed the page. Resolves true once durably queued. */
  onErase?: (erase: EraseGesture) => Promise<boolean>;
  /**
   * A completed stroke. Resolves to its StrokeId once durably queued, or null if it could not
   * be saved (the wet ink is then removed).
   */
  onStroke?: (stroke: FinishedStroke) => Promise<string | null>;
  /** True while a stroke is in progress (navigation must wait). */
  onDrawingChange?: (drawing: boolean) => void;
  /** A stroke was refused before saving (e.g. too long). */
  onRefused?: (message: string) => void;
}

interface WetStroke {
  points: SyncStroke["points"];
  style: SyncStroke["style"];
  /** StrokeId once committed; the wet copy is dropped when the overlay contains it. */
  committedId: string | null;
}

interface Active {
  pointerId: number;
  capture: StrokeCapture;
  wet: WetStroke;
}

export interface TypeSession {
  finalizeActive: () => Promise<TextFinalizeResult>;
  edit: (text: SyncText) => Promise<boolean>;
  setFontSize: (fontSize: number) => void;
}

type PressKind = "editor-control" | "link" | "text" | "paper" | "outside-paper";

interface PressIntent {
  pointerId: number;
  notebookId: string;
  pageId: string;
  kind: PressKind;
  textId?: string;
}

export interface EraseGesture {
  operation_id: string;
  observed_roots: string[];
  path: { x: number; y: number }[];
}

interface Erasing {
  pointerId: number;
  capture: EraseCapture;
  operationId: string;
  /** Strokes shown at pointer down (what the gesture erases). */
  base: readonly SyncStroke[];
  observedRoots: string[];
}

/** An erase preview on the main canvas, until the committed op reaches `display`. */
interface ErasePreview {
  strokes: SyncStroke[];
  operationId: string;
  committed: boolean;
}

function pointerKind(t: string): InkPointerType {
  return t === "pen" ? "pen" : t === "touch" ? "touch" : "mouse";
}

export function PageView({
  page,
  notebookId,
  pageSize,
  blobs,
  mode,
  version,
  onWheelZoom,
  onZoom,
  display,
  displayTexts,
  onLinkClick,
  linkLabel,
  pendingCount = 0,
  pendingOpIds,
  writing = false,
  typing = false,
  onActivateText,
  textFontSize = DEFAULT_TEXT_FONT_SIZE,
  onTextFontSizeChange,
  drawWithTouch = false,
  tool = "pen",
  mintOperationId,
  mintTextId,
  previewErase,
  onErase,
  onStroke,
  onTextCommit,
  onTypeSession,
  onDrawingChange,
  onRefused,
}: PageViewProps) {
  const scroller = useRef<HTMLDivElement>(null);
  // Gesture root: the text and link overlays are siblings of the scroller, so
  // type-mode and touch-gesture listeners attach here (their common ancestor).
  // Geometry and scrolling still use the scroller element itself.
  const areaRef = useRef<HTMLDivElement>(null);
  const canvas = useRef<HTMLCanvasElement>(null);
  const wetCanvas = useRef<HTMLCanvasElement>(null);
  const [avail, setAvail] = useState<Size>({ width: 0, height: 0 });
  const [draft, setDraft] = useState<TextDraft | null>(null);
  const draftRef = useRef<TextDraft | null>(null);
  const shownTexts = useMemo(() => displayTexts ?? page.texts ?? [], [displayTexts, page.texts]);
  /** Page binding captured when a draft opens: commits target this page, never the current one. */
  const draftBoundRef = useRef<{ notebookId: string; pageId: string } | null>(null);
  const pageBindingRef = useRef({ notebookId, pageId: page.page_id });
  useLayoutEffect(() => {
    pageBindingRef.current = { notebookId, pageId: page.page_id };
  }, [notebookId, page.page_id]);
  // Synchronous mirror of the projected texts: drop-time rebase must read the
  // same array this render projected. A layout effect (not a passive one) keeps
  // the mirror current before paint, so no committed render can drop onto a stale ref.
  const textsRef = useRef(shownTexts);
  useLayoutEffect(() => {
    textsRef.current = shownTexts;
  }, [shownTexts]);
  /** Active text grab: Idle (null) | Armed | Dragging. No Selected state exists. */
  const grabRef = useRef<{ pointerId: number; textId: string; grab: TextGrab; notebookId: string; pageId: string } | null>(null);
  /** Pointers whose cancelled grab consumed their terminal release. */
  const suppressedReleaseRef = useRef<Set<number>>(new Set());
  /** Semantic owner captured at press time; release targets cannot retarget it. */
  const pressIntentsRef = useRef(new Map<number, PressIntent>());
  /** The open draft's original, when it is known remotely deleted. */
  const draftGoneRef = useRef<string | null>(null);
  const [draggingId, setDraggingId] = useState<string | null>(null);
  const [scroll, setScroll] = useState({ left: 0, top: 0 });
  const prevLayout = useRef<Layout | null>(null);
  const wet = useRef<WetStroke[]>([]);
  const active = useRef<Active | null>(null);
  const pan = useRef<PanTracker | null>(null);
  /**
   * True once this touch sequence became a pan (slop exceeded) or a pinch.
   * Type-mode placement reads it on pointerup so a completed pan cannot mint/edit text.
   */
  const touchGestureConsumed = useRef(false);
  /** Ignore blur→commit briefly after visualViewport churn (virtual keyboard). */
  const suppressBlurUntil = useRef(0);
  const editorRef = useRef<HTMLTextAreaElement | null>(null);
  /** Touch points down on the page (client px), for pan/pinch. */
  const touches = useRef(new Map<number, { x: number; y: number }>());
  /** A pinch in progress: finger spread and zoom at its start, and the page point it holds. */
  const pinch = useRef<{ startDist: number; startFactor: number; anchor: { x: number; y: number } } | null>(null);
  /** Where the next layout must put the pinch's page point (consumed by the layout effect). */
  const focus = useRef<{ anchor: { x: number; y: number }; focal: { x: number; y: number } } | null>(null);
  const erasing = useRef<Erasing | null>(null);
  const preview = useRef<ErasePreview | null>(null);
  const eraseCursor = useRef<{ x: number; y: number } | null>(null);
  const pendingIds = useRef<ReadonlySet<string>>(new Set());
  useEffect(() => {
    pendingIds.current = pendingOpIds ?? new Set();
  }, [pendingOpIds]);

  // Layers 1+2 as one page (the reducer's projection of the pending ops; never reordered here).
  const shown = useMemo<SnapshotPage>(() => (display ? { ...page, strokes: [...display] } : page), [page, display]);

  // Plan once per page (profile problems become a visible page error, never partial ink).
  const planned = useMemo(() => {
    try {
      return { plans: planPage(shown), error: null as string | null };
    } catch (e) {
      return { plans: [], error: e instanceof Error ? e.message : "unsupported page" };
    }
  }, [shown]);

  const bg = page.background.kind === "blob" ? blobs.get(page.background.sha256) ?? null : null;
  const bgError = page.background.kind === "blob" && !bg ? blobs.error(page.background.sha256) ?? null : null;
  const bgLoading = page.background.kind === "blob" && !bg && !bgError;
  const pageError = planned.error ?? (bgError ? `Background unavailable: ${bgError}` : null);

  const lay = useMemo(() => (avail.width > 0 ? layout(avail, pageSize, mode) : null), [avail, pageSize, mode]);
  const overlayT = lay ? screenTransform(lay, scroll.left, scroll.top) : null;

  useLayoutEffect(() => {
    const el = scroller.current;
    if (!el) return;
    const measure = () => setAvail({ width: el.clientWidth, height: el.clientHeight });
    measure();
    const ro = new ResizeObserver(measure);
    ro.observe(el);
    return () => ro.disconnect();
  }, []);

  // Keep the viewport centre fixed across zoom changes; Fit resets to the top-left (no scroll).
  useLayoutEffect(() => {
    const el = scroller.current;
    const before = prevLayout.current;
    if (el && lay) {
      const f = focus.current;
      focus.current = null;
      if (f) {
        const s = scrollToAnchor(lay, avail, f.anchor, f.focal);
        el.scrollLeft = s.left;
        el.scrollTop = s.top;
      } else if (mode.kind === "fit") {
        el.scrollLeft = 0;
        el.scrollTop = 0;
      } else if (before && before.scale !== lay.scale) {
        const s = scrollForZoom(before, lay, avail, el.scrollLeft, el.scrollTop);
        el.scrollLeft = s.left;
        el.scrollTop = s.top;
      }
    }
    prevLayout.current = lay;
    if (el) setScroll({ left: el.scrollLeft, top: el.scrollTop });
  }, [lay, mode, avail]);

  const transform = useCallback((): { t: ScreenTransform; w: number; h: number; dpr: number } | null => {
    const el = scroller.current;
    if (!el || !lay) return null;
    return { t: screenTransform(lay, el.scrollLeft, el.scrollTop), w: el.clientWidth, h: el.clientHeight, dpr: Math.min(window.devicePixelRatio || 1, 3) };
  }, [lay]);

  const sizeCanvas = (c: HTMLCanvasElement, w: number, h: number, dpr: number) => {
    if (c.width !== Math.round(w * dpr) || c.height !== Math.round(h * dpr)) {
      c.width = Math.round(w * dpr);
      c.height = Math.round(h * dpr);
      c.style.width = `${w}px`;
      c.style.height = `${h}px`;
    }
  };

  /** Repaint layer 3 from scratch (after scroll/zoom or a hand-off). */
  const drawWet = useCallback(() => {
    const c = wetCanvas.current;
    const v = transform();
    if (!c || !v) return;
    sizeCanvas(c, v.w, v.h, v.dpr);
    const ctx = c.getContext("2d");
    if (!ctx) return;
    ctx.setTransform(1, 0, 0, 1, 0, 0);
    ctx.clearRect(0, 0, c.width, c.height);
    c.dataset.wet = String(wet.current.length);
    if (wet.current.length > 0) {
      const plans = wet.current.map((s) => planStroke({ stroke_id: "", root_id: "", style: s.style, points: s.points }, page.render));
      paintInk(ctx, { render: page.render, pageSize, transform: v.t, viewport: { width: v.w, height: v.h, dpr: v.dpr } }, plans);
    }
    const cur = eraseCursor.current;
    if (cur) {
      // The eraser's footprint: the Kindle capsule radius, in screen px.
      ctx.setTransform(v.dpr, 0, 0, v.dpr, 0, 0);
      ctx.beginPath();
      ctx.arc(v.t.offsetX + cur.x * v.t.scale, v.t.offsetY + cur.y * v.t.scale, Math.max(2, ERASE_RADIUS * v.t.scale), 0, Math.PI * 2);
      ctx.lineWidth = 1.5;
      ctx.strokeStyle = "#2f5d8a";
      ctx.stroke();
    }
  }, [transform, page.render, pageSize]);

  // Dark mode shows the page inverted (styles.css); the surround is painted a neutral grey there
  // so the inversion makes it a neutral dark grey instead of a tinted one.
  const deskColor = useSyncExternalStore(subscribeTheme, currentTheme) === "dark" ? DARK_DESK_COLOR : DESK_COLOR;

  const draw = useCallback(() => {
    const c = canvas.current;
    const v = transform();
    if (!c || !v || !lay) return;
    sizeCanvas(c, v.w, v.h, v.dpr);
    const ctx = c.getContext("2d");
    if (!ctx) return;
    const t = v.t;
    c.dataset.pageId = page.page_id;
    c.dataset.scale = String(lay.scale);
    c.dataset.zoom = String(lay.factor);
    c.dataset.pageRect = [t.offsetX, t.offsetY, lay.pageWidth, lay.pageHeight].map((x) => x.toFixed(2)).join(",");
    c.dataset.viewport = `${v.w},${v.h}`;
    c.dataset.background = page.background.kind;
    if (pageError || bgLoading) {
      ctx.setTransform(1, 0, 0, 1, 0, 0);
      ctx.clearRect(0, 0, c.width, c.height);
      c.dataset.strokes = "0";
      c.dataset.rendered = "false";
      return;
    }
    // Erase hand-off: a committed preview leaves once `display` includes its op (same result).
    const pv = preview.current;
    if (pv && pv.committed && pendingIds.current.has(pv.operationId)) preview.current = null;
    let target = shown;
    let plans = planned.plans;
    if (preview.current) {
      try {
        target = { ...shown, strokes: preview.current.strokes };
        plans = planPage(target);
      } catch {
        preview.current = null;
        target = shown;
      }
    }
    const stats = renderPage(
      ctx,
      { page: target, pageSize, background: bg, transform: t, viewport: { width: v.w, height: v.h, dpr: v.dpr }, deskColor },
      plans,
    );
    c.dataset.strokes = String(stats.strokes);
    c.dataset.pending = String(pendingCount);
    c.dataset.preview = preview.current ? "true" : "false";
    c.dataset.runs = String(stats.runs);
    c.dataset.rendered = "true";
    // Hand-off: committed wet strokes now painted by layer 2 (or 1) leave layer 3 in the
    // same task, so there is no frame without them.
    const have = new Set(shown.strokes.map((s) => s.stroke_id));
    const before = wet.current.length;
    wet.current = wet.current.filter((s) => s.committedId === null || !have.has(s.committedId));
    if (wet.current.length !== before) drawWet();
  }, [transform, lay, page, shown, pageSize, bg, planned, pageError, bgLoading, drawWet, pendingCount, deskColor]);

  useEffect(() => {
    draw();
    drawWet();
  }, [draw, drawWet, version]);

  const raf = useRef(0);
  const onScroll = () => {
    cancelAnimationFrame(raf.current);
    raf.current = requestAnimationFrame(() => {
      draw();
      drawWet();
      const s = scroller.current;
      if (s) setScroll({ left: s.scrollLeft, top: s.scrollTop });
    });
  };

  useEffect(() => {
    draftRef.current = draft;
  }, [draft]);

  /** Persist bound to the page where the draft opened, never the newly current page. */
  const persistText = useCallback(
    (intent: TextPersistIntent): Promise<boolean> => {
      const b = draftBoundRef.current;
      if (!b || !onTextCommit) return Promise.resolve(false);
      return intent.kind === "delete"
        ? onTextCommit({ kind: "delete", text_id: intent.text_id, notebook_id: b.notebookId, page_id: b.pageId })
        : onTextCommit({ kind: intent.kind, text: intent.text, notebook_id: b.notebookId, page_id: b.pageId });
    },
    [onTextCommit],
  );

  const clearDraft = useCallback((expected?: TextDraft) => {
    if (expected && draftRef.current !== expected) return;
    draftRef.current = null;
    draftBoundRef.current = null;
    draftGoneRef.current = null;
    setDraft(null);
  }, []);

  /** Whether a text id is still on this page (liveness gate for finalize paths). */
  const isTextLive = useCallback((textId: string) => textsRef.current.some((t) => t.text_id === textId), []);

  const finishExactDraft = useCallback(async (d: TextDraft) => {
    const outcome = await runFinish(d, persistText, isTextLive);
    if (outcome === "restored" && d.original !== null && !isTextLive(d.text_id)) {
      onRefused?.("That text was deleted elsewhere. Your edits are kept.");
    }
    return outcome;
  }, [persistText, isTextLive, onRefused]);

  const restoreExactDraft = useCallback((d: TextDraft) => {
    if (draftRef.current !== d) return;
    d.status = "editing";
    const restored = { ...d, status: "editing" as const };
    draftRef.current = restored;
    setDraft(restored);
  }, []);

  // The gate object is stable for the component lifetime. Its port delegates to
  // the latest render callbacks without replacing an in-flight promise.
  const finalizationPortRef = useRef({
    finish: finishExactDraft,
    clear: (d: TextDraft) => clearDraft(d),
    restore: restoreExactDraft,
  });
  const finalizationGateRef = useRef<TextFinalizationGate | null>(null);
  useLayoutEffect(() => {
    finalizationPortRef.current = {
      finish: finishExactDraft,
      clear: (d: TextDraft) => clearDraft(d),
      restore: restoreExactDraft,
    };
  }, [finishExactDraft, clearDraft, restoreExactDraft]);
  useLayoutEffect(() => {
    finalizationGateRef.current ??= new TextFinalizationGate({
      current: () => draftRef.current,
      finish: (d) => finalizationPortRef.current.finish(d),
      clear: (d) => finalizationPortRef.current.clear(d),
      restore: (d) => finalizationPortRef.current.restore(d),
    });
  }, []);
  const finalizeActive = useCallback((): Promise<TextFinalizeResult> => {
    return finalizationGateRef.current?.finalizeActive() ?? Promise.resolve("blocked");
  }, []);

  const beginEdit = useCallback(
    (text: SyncText) => {
      const next = editDraft(text);
      draftBoundRef.current = { notebookId, pageId: page.page_id };
      draftGoneRef.current = null;
      draftRef.current = next;
      setDraft(next);
      onTextFontSizeChange?.(text.font_size);
    },
    [notebookId, page.page_id, onTextFontSizeChange],
  );

  const setActiveFontSize = useCallback((fontSize: number) => {
    const current = draftRef.current;
    if (!current || current.status !== "editing" || current.current.font_size === fontSize) return;
    const next: TextDraft = { ...current, current: { ...current.current, font_size: fontSize } };
    draftRef.current = next;
    setDraft(next);
  }, []);

  const editAfterFinalize = useCallback(async (text: SyncText): Promise<boolean> => {
    if (draftRef.current?.text_id === text.text_id) return true;
    if ((await finalizeActive()) === "blocked") return false;
    const live = textsRef.current.find((candidate) => candidate.text_id === text.text_id);
    if (!live) return false;
    beginEdit(live);
    return true;
  }, [beginEdit, finalizeActive]);

  const deleteCurrentDraft = useCallback(async () => {
    const d = draftRef.current;
    if (!d || d.original === null) return;
    const outcome = await runDelete(d, persistText, isTextLive);
    if (outcome === "cleared") clearDraft(d);
    else restoreExactDraft(d);
  }, [persistText, clearDraft, isTextLive, restoreExactDraft]);

  const clearGrabPreview = useCallback((textId: string) => {
    const el = areaRef.current?.querySelector(`[data-text-id="${textId}"]`);
    if (el instanceof HTMLElement) el.style.transform = "";
  }, []);

  // Cancel an active grab: no operation, preview removed, and this pointer's
  // terminal release is consumed so it cannot fall through into edit/create.
  const cancelTextGrab = useCallback(() => {
    const g = grabRef.current;
    if (!g) return;
    grabRef.current = null;
    suppressPointerRelease(suppressedReleaseRef.current, g.pointerId);
    clearGrabPreview(g.textId);
    setDraggingId(null);
  }, [clearGrabPreview]);

  // Commit a completed drag: gesture position rebased onto the latest live object.
  // A text that disappeared before drop commits nothing (no stale UpdateText, and
  // no trip through the coordinator's global conflict state).
  const commitTextGrab = useCallback(
    async (
      g: { pointerId: number; textId: string; grab: TextGrab; notebookId: string; pageId: string },
      clientX: number,
      clientY: number,
    ) => {
      grabRef.current = null;
      clearGrabPreview(g.textId);
      setDraggingId(null);
      const delta = dragCanonicalDelta(g.grab, clientX, clientY);
      const latest = textsRef.current.find((t) => t.text_id === g.textId);
      if (!latest) {
        onRefused?.("That text is no longer on this page. The move was cancelled.");
        return;
      }
      const moved = buildMoveUpdate(latest, g.grab.originX, g.grab.originY, delta.dx, delta.dy);
      if (!moved) return;
      const ok = await onTextCommit?.({
        kind: "update",
        text: moved,
        notebook_id: g.notebookId,
        page_id: g.pageId,
      });
      if (!ok) onRefused?.("Couldn't move that text. Nothing was changed.");
    },
    [clearGrabPreview, onRefused, onTextCommit],
  );

  // Arm a press on existing text: release within slop edits, past slop drags.
  const armTextGrab = (e: ReactPointerEvent<HTMLElement>, t: SyncText) => {
    if (!typing || grabRef.current || draftRef.current) return;
    if (e.pointerType === "mouse" && e.button !== 0) return;
    const v = transform();
    if (!v) return;
    grabRef.current = {
      pointerId: e.pointerId,
      textId: t.text_id,
      grab: beginTextGrab(e.clientX, e.clientY, t.x, t.y, v.t.scale),
      notebookId,
      pageId: page.page_id,
    };
    try {
      e.currentTarget.setPointerCapture(e.pointerId);
    } catch {
      // capture unsupported (synthetic events): proceed without it
    }
  };

  // Advance an armed grab; once claimed, the page must not scroll from this pointer.
  const advanceGrabForMove = useCallback(
    (pointerId: number, clientX: number, clientY: number): void => {
      const g = grabRef.current;
      if (!g || g.pointerId !== pointerId) return;
      const v = transform();
      if (!v || v.t.scale !== g.grab.scale) {
        cancelTextGrab();
        return;
      }
      if (advanceTextGrab(g.grab, clientX, clientY)) {
        touchGestureConsumed.current = true;
        setDraggingId(g.textId);
      }
      if (g.grab.phase !== "dragging") return;
      const d = dragPreviewDelta(g.grab, clientX, clientY);
      const el = areaRef.current?.querySelector(`[data-text-id="${g.textId}"]`);
      if (el instanceof HTMLElement) el.style.transform = `translate(${d.dx}px, ${d.dy}px)`;
    },
    [transform, cancelTextGrab],
  );

  useLayoutEffect(() => {
    onTypeSession?.(typing ? { finalizeActive, edit: editAfterFinalize, setFontSize: setActiveFontSize } : null);
    return () => onTypeSession?.(null);
  }, [typing, finalizeActive, editAfterFinalize, setActiveFontSize, onTypeSession]);

  // The Viewer finalizes through the shared gate before it leaves Type mode.
  // Once the mode changes, only grab cancellation remains for this component.
  useEffect(() => {
    if (!typing) cancelTextGrab();
  }, [typing, cancelTextGrab]);

  // An edit or drag belongs to the page where it began: a page switch finalizes an
  // edit against its original page and cancels a drag with no operation. A draft
  // whose original is known deleted commits nothing (a refused UpdateText would
  // poison the coordinator). Normal UI navigation finalizes before this fires;
  // this is the defensive boundary for an external page rebind.
  const prevPageId = useRef(page.page_id);
  useEffect(() => {
    if (prevPageId.current === page.page_id) return;
    prevPageId.current = page.page_id;
    cancelTextGrab();
    void finalizeActive().then((result) => {
      if (result === "blocked") onRefused?.("Couldn't save that text on this page. Your edits are kept.");
    });
  }, [page.page_id, cancelTextGrab, finalizeActive, onRefused]);

  // Remote delete of the edited or dragged object: the drag is cancelled, a
  // pristine edit closes, and an edit with unsaved typing stays open (finalize is
  // liveness-gated, so no UpdateText for the deleted id can follow).
  useEffect(() => {
    const d = draftRef.current;
    if (d && d.original !== null && d.status === "editing" && !shownTexts.some((t) => t.text_id === d.text_id)) {
      draftGoneRef.current = d.text_id;
      const typed = normalizeLineEndings(d.current.content);
      if (sameText(d.original, { ...d.current, content: typed })) {
        clearDraft(d);
      } else {
        onRefused?.("That text was deleted elsewhere. Your edits are kept.");
      }
    }
    const g = grabRef.current;
    if (g && !shownTexts.some((t) => t.text_id === g.textId)) {
      cancelTextGrab();
      onRefused?.("That text is no longer on this page. The move was cancelled.");
    }
  }, [shownTexts, clearDraft, cancelTextGrab, onRefused]);

  // Teardown: a drag never commits; an in-flight edit gets one best-effort finalize
  // through the same single-flight gate used by every transition.
  useEffect(
    () => () => {
      grabRef.current = null;
      pressIntentsRef.current.clear();
      void finalizeActive();
    },
    [finalizeActive],
  );

  useEffect(() => {
    const el = scroller.current;
    const area = areaRef.current;
    if (!el || !area || writing) return;

    const classify = (e: PointerEvent): PressIntent => {
      const target = e.target as HTMLElement | null;
      const binding = pageBindingRef.current;
      if (target?.closest?.("textarea, [data-text-delete]")) {
        return { pointerId: e.pointerId, ...binding, kind: "editor-control" };
      }
      if (target?.closest?.("[data-link-id]")) {
        return { pointerId: e.pointerId, ...binding, kind: "link" };
      }
      const text = target?.closest?.("[data-text-id]");
      if (text instanceof HTMLElement && text.dataset.textId) {
        return { pointerId: e.pointerId, ...binding, kind: "text", textId: text.dataset.textId };
      }
      const v = transform();
      if (!v) return { pointerId: e.pointerId, ...binding, kind: "outside-paper" };
      const box = el.getBoundingClientRect();
      const p = toCanonical(v.t, e.clientX - box.left, e.clientY - box.top);
      const kind = Number.isFinite(p.x) && Number.isFinite(p.y) && insidePage(p, pageSize) ? "paper" : "outside-paper";
      return { pointerId: e.pointerId, ...binding, kind };
    };

    // Capture semantic ownership on press. A release over a different sibling
    // overlay cannot turn paper into text/link (or vice versa).
    const down = (e: PointerEvent) => {
      if (e.button !== 0 && e.pointerType !== "touch") return;
      clearSuppressedRelease(suppressedReleaseRef.current, e.pointerId);
      pressIntentsRef.current.set(e.pointerId, classify(e));
    };

    const up = (e: PointerEvent) => {
      if (e.button !== 0 && e.pointerType !== "touch") return;
      const press = pressIntentsRef.current.get(e.pointerId);
      pressIntentsRef.current.delete(e.pointerId);
      if (!press) return;
      const binding = pageBindingRef.current;
      if (press.notebookId !== binding.notebookId || press.pageId !== binding.pageId) return;
      if (consumeSuppressedRelease(suppressedReleaseRef.current, e.pointerId)) return;

      // A completed text drag commits here (native order: before React click).
      // An armed-but-unmoved press continues as the captured text intent.
      const g = grabRef.current;
      if (g && g.pointerId === e.pointerId) {
        grabRef.current = null;
        if (g.grab.phase === "dragging") {
          void commitTextGrab(g, e.clientX, e.clientY);
          return;
        }
      }
      if (e.pointerType === "touch" && touchGestureConsumed.current) return;

      if (press.kind === "text" && press.textId) {
        const text = textsRef.current.find((candidate) => candidate.text_id === press.textId);
        if (!text || draftRef.current?.text_id === text.text_id) return;
        if (typing) void editAfterFinalize(text);
        else onActivateText?.(text);
        return;
      }
      if (!typing || press.kind !== "paper" || !mintTextId) return;

      // Paper intent is necessary but not sufficient: the release must also be
      // on the canonical sheet. Desk/background releases never clamp to an edge.
      const v = transform();
      if (!v) return;
      const box = el.getBoundingClientRect();
      const p = toCanonical(v.t, e.clientX - box.left, e.clientY - box.top);
      if (!Number.isFinite(p.x) || !Number.isFinite(p.y) || !insidePage(p, pageSize) || p.x >= pageSize.width || p.y >= pageSize.height) return;
      void (async () => {
        if ((await finalizeActive()) === "blocked") return;
        const current = pageBindingRef.current;
        if (press.notebookId !== current.notebookId || press.pageId !== current.pageId) return;
        const id = mintTextId();
        if (!id) return;
        const next = newDraft(id, p.x, p.y, textFontSize);
        draftBoundRef.current = { notebookId: current.notebookId, pageId: current.pageId };
        draftGoneRef.current = null;
        draftRef.current = next;
        setDraft(next);
      })();
    };

    const move = (e: PointerEvent) => {
      if (typing) advanceGrabForMove(e.pointerId, e.clientX, e.clientY);
    };
    const cancel = (e: PointerEvent) => {
      pressIntentsRef.current.delete(e.pointerId);
      if (grabRef.current?.pointerId === e.pointerId) cancelTextGrab();
    };
    area.addEventListener("pointerdown", down);
    area.addEventListener("pointerup", up);
    area.addEventListener("pointermove", move);
    area.addEventListener("pointercancel", cancel);
    return () => {
      area.removeEventListener("pointerdown", down);
      area.removeEventListener("pointerup", up);
      area.removeEventListener("pointermove", move);
      area.removeEventListener("pointercancel", cancel);
    };
  }, [writing, typing, mintTextId, textFontSize, transform, pageSize, commitTextGrab, cancelTextGrab, advanceGrabForMove, editAfterFinalize, finalizeActive, onActivateText]);

  const autosizeEditor = useCallback(() => {
    const editor = editorRef.current;
    if (!editor || !draft || !overlayT) return;
    editor.style.height = "0px";
    editor.style.maxHeight = "none";
    editor.style.overflowY = "hidden";
    const lineHeight = draft.current.font_size * overlayT.scale * 1.2;
    const remaining = (pageSize.height - draft.current.y) * overlayT.scale;
    const fitted = fitTextEditorHeight(editor.scrollHeight, lineHeight, remaining);
    editor.style.height = `${fitted.height}px`;
    editor.style.maxHeight = `${fitted.maxHeight}px`;
    editor.style.overflowY = fitted.overflowY;
    editor.dataset.naturalHeight = String(editor.scrollHeight);
    editor.dataset.maxPaperHeight = String(fitted.maxHeight);
  }, [draft, overlayT, pageSize.height]);

  // Height is a derived editor concern: content, wrapping, font size, zoom, and
  // font availability all remeasure, but no height enters SyncText.
  useLayoutEffect(() => {
    if (!draft) return;
    autosizeEditor();
    const fonts = document.fonts;
    const loaded = () => autosizeEditor();
    void fonts.ready.then(loaded);
    fonts.addEventListener("loadingdone", loaded);
    return () => fonts.removeEventListener("loadingdone", loaded);
  }, [draft, autosizeEditor]);

  // Keep the active editor inside the visual viewport (virtual keyboard) without moving
  // canonical text coordinates. Suppress accidental blur→commit during viewport churn.
  useEffect(() => {
    if (!draft) return;
    const scrollerEl = scroller.current;
    const bump = () => {
      suppressBlurUntil.current = performance.now() + BLUR_SUPPRESS_MS;
      const ed = editorRef.current;
      if (ed && scrollerEl) keepEditorInVisualViewport(ed, scrollerEl);
    };
    bump();
    return onVisualViewportChange(bump);
  }, [draft]);

  useEffect(() => {
    const el = scroller.current;
    if (!el || !onWheelZoom) return;
    const onWheel = (e: WheelEvent) => {
      if (!e.ctrlKey && !e.metaKey) return; // plain wheel scrolls; ctrl/⌘ or pinch zooms
      e.preventDefault();
      if (active.current) return; // never re-map coordinates mid-stroke
      onWheelZoom(e.deltaY < 0 ? 1 : -1);
    };
    el.addEventListener("wheel", onWheel, { passive: false });
    return () => el.removeEventListener("wheel", onWheel);
  }, [onWheelZoom]);

  // ---- write mode -------------------------------------------------------------------------

  const sample = (e: PointerEvent): Sample => {
    const r = scroller.current!.getBoundingClientRect();
    return { localX: e.clientX - r.left, localY: e.clientY - r.top, pressure: e.pressure };
  };

  const setDrawing = useCallback((d: boolean) => onDrawingChange?.(d), [onDrawingChange]);

  /** Drop the in-progress stroke or erase without authoring anything (cancel / lost capture / exit). */
  const discard = useCallback(() => {
    const a = active.current;
    const er = erasing.current;
    if (!a && !er) return;
    if (a) {
      active.current = null;
      wet.current = wet.current.filter((s) => s !== a.wet);
    }
    if (er) {
      erasing.current = null;
      eraseCursor.current = null;
      if (preview.current && !preview.current.committed) preview.current = null;
      draw();
    }
    drawWet();
    setDrawing(false);
  }, [draw, drawWet, setDrawing]);

  // Only leaving write mode or an actual page change drops an in-progress stroke — never a
  // snapshot refresh (which re-creates these callbacks) arriving while the pen is down.
  const discardRef = useRef(discard);
  useEffect(() => {
    discardRef.current = discard;
  }, [discard]);
  useEffect(() => {
    if (!writing) discardRef.current();
  }, [writing]);
  // A page change mid-stroke (should not happen: navigation waits) must not author onto it.
  useEffect(() => () => discardRef.current(), [page.page_id]);

  useEffect(() => {
    const el = scroller.current;
    if (!el || !writing) return;

    const paintSegment = (a: Active, from: number) => {
      const c = wetCanvas.current;
      const v = transform();
      if (!c || !v) return;
      sizeCanvas(c, v.w, v.h, v.dpr);
      const ctx = c.getContext("2d");
      if (!ctx) return;
      const pts = a.capture.points.slice(Math.max(0, from - 1));
      const style = a.capture.pointerType === "pen" && page.render.pressure && pts.some((p) => (p.pressure ?? 0) > 0) ? "pressure-width-v1" : "legacy-fixed-v1";
      const plan = planStroke({ stroke_id: "", root_id: "", style, points: pts }, page.render);
      paintInk(ctx, { render: page.render, pageSize, transform: v.t, viewport: { width: v.w, height: v.h, dpr: v.dpr } }, [plan]);
      c.dataset.wet = String(wet.current.length);
    };

    // ---- erase gesture (reducer preview; commit on up only if it changed the page) ----
    let eraseRaf = 0;
    const refreshPreview = () => {
      eraseRaf = 0;
      const er = erasing.current;
      if (!er || !previewErase) return;
      const r = previewErase(er.base, { operation_id: er.operationId, observed_roots: er.observedRoots, path: [...er.capture.path] });
      preview.current = r ? { strokes: r.strokes, operationId: er.operationId, committed: false } : null;
      draw();
      drawWet();
    };
    const schedulePreview = () => {
      if (!eraseRaf) eraseRaf = requestAnimationFrame(refreshPreview);
    };

    const beginErase = (e: PointerEvent, v: { t: ScreenTransform }): boolean => {
      if (!mintOperationId || !previewErase || !onErase) return false;
      const cap = EraseCapture.begin(v.t, pageSize, sample(e));
      if (!cap) return false; // erases must start on the page
      // What the user sees now (including any earlier erase still handing off).
      const base = preview.current?.strokes ?? shown.strokes;
      const roots = [...new Set(base.map((s) => s.root_id))];
      if (roots.length === 0) return false; // nothing visible to erase
      e.preventDefault();
      try {
        el.setPointerCapture(e.pointerId);
      } catch {
        // capture unsupported (synthetic events): proceed without it
      }
      erasing.current = { pointerId: e.pointerId, capture: cap, operationId: mintOperationId(), base, observedRoots: roots };
      eraseCursor.current = cap.path[cap.path.length - 1]!;
      setDrawing(true);
      schedulePreview();
      return true;
    };

    const moveErase = (e: PointerEvent, er: Erasing) => {
      e.preventDefault();
      const v = transform();
      if (!v) return;
      const events = typeof e.getCoalescedEvents === "function" ? e.getCoalescedEvents() : [];
      for (const ce of events.length > 0 ? events : [e]) er.capture.add(v.t, sample(ce));
      eraseCursor.current = er.capture.path[er.capture.path.length - 1]!;
      schedulePreview();
    };

    const finishErase = (e: PointerEvent, er: Erasing) => {
      e.preventDefault();
      if (eraseRaf) cancelAnimationFrame(eraseRaf);
      eraseRaf = 0;
      erasing.current = null;
      eraseCursor.current = null;
      const v = transform();
      if (v) er.capture.add(v.t, sample(e));
      setDrawing(false);
      if (er.capture.tooLong) {
        preview.current = null;
        draw();
        drawWet();
        onRefused?.("That erase stroke is too long to save. Try shorter strokes.");
        return;
      }
      const gesture: EraseGesture = { operation_id: er.operationId, observed_roots: er.observedRoots, path: [...er.capture.path] };
      const r = previewErase?.(er.base, gesture) ?? null;
      if (!r || !r.changed) {
        // Nothing erased (like the Kindle, a clean gesture authors nothing).
        preview.current = null;
        draw();
        drawWet();
        return;
      }
      const pv: ErasePreview = { strokes: r.strokes, operationId: er.operationId, committed: false };
      preview.current = pv;
      draw();
      drawWet();
      void onErase!(gesture).then((ok) => {
        if (preview.current !== pv) return; // superseded by a later gesture
        if (ok) pv.committed = true;
        else preview.current = null;
        draw();
      });
    };

    const down = (e: PointerEvent) => {
      const kind = pointerKind(e.pointerType);
      // Touch pans/pinches (gesture handler) unless drawing with one finger.
      if (kind === "touch" && (!drawWithTouch || touches.current.size > 1 || pinch.current)) return;
      if (active.current || erasing.current || (kind === "mouse" && e.button !== 0)) return;
      const v = transform();
      if (!v) return;
      // The Eraser tool, or a pen's eraser end (buttons bit 5 = 32).
      if (tool === "eraser" || (kind === "pen" && (e.buttons & 32) !== 0)) {
        beginErase(e, v);
        return;
      }
      const cap = StrokeCapture.begin(kind, v.t, pageSize, sample(e), !!page.render.pressure);
      if (!cap) return; // strokes must start on the page
      e.preventDefault();
      try {
        el.setPointerCapture(e.pointerId);
      } catch {
        // capture unsupported (synthetic events): proceed without it
      }
      const w: WetStroke = { points: cap.points as SyncStroke["points"], style: "legacy-fixed-v1", committedId: null };
      const a: Active = { pointerId: e.pointerId, capture: cap, wet: w };
      active.current = a;
      wet.current = [...wet.current, w];
      setDrawing(true);
      paintSegment(a, 0);
    };

    const move = (e: PointerEvent) => {
      const er = erasing.current;
      if (er && er.pointerId === e.pointerId) {
        moveErase(e, er);
        return;
      }
      const a = active.current;
      if (!a || a.pointerId !== e.pointerId) return;
      e.preventDefault();
      const v = transform();
      if (!v) return;
      const from = a.capture.length;
      const events = typeof e.getCoalescedEvents === "function" ? e.getCoalescedEvents() : [];
      for (const ce of events.length > 0 ? events : [e]) a.capture.add(v.t, sample(ce));
      if (a.capture.length > from) paintSegment(a, from);
    };

    const up = (e: PointerEvent) => {
      const er = erasing.current;
      if (er && er.pointerId === e.pointerId) {
        finishErase(e, er);
        return;
      }
      const a = active.current;
      if (!a || a.pointerId !== e.pointerId) return;
      e.preventDefault();
      active.current = null;
      const v = transform();
      const res = v ? a.capture.finish(v.t, sample(e)) : ({ kind: "empty" } as const);
      setDrawing(false);
      if (res.kind !== "stroke" || !onStroke) {
        wet.current = wet.current.filter((s) => s !== a.wet);
        drawWet();
        if (res.kind === "too-long") onRefused?.("That stroke is too long to save. Try shorter strokes.");
        return;
      }
      a.wet.points = res.stroke.points;
      a.wet.style = res.stroke.style;
      drawWet();
      void onStroke(res.stroke).then((id) => {
        if (id === null) {
          wet.current = wet.current.filter((s) => s !== a.wet);
        } else {
          a.wet.committedId = id;
        }
        draw(); // hands the stroke off to layer 2 if the overlay already has it
        drawWet();
      });
    };

    const cancel = (e: PointerEvent) => {
      if (active.current?.pointerId === e.pointerId || erasing.current?.pointerId === e.pointerId) discard();
    };

    el.addEventListener("pointerdown", down);
    el.addEventListener("pointermove", move);
    el.addEventListener("pointerup", up);
    el.addEventListener("pointercancel", cancel);
    el.addEventListener("lostpointercapture", cancel);
    return () => {
      el.removeEventListener("pointerdown", down);
      el.removeEventListener("pointermove", move);
      el.removeEventListener("pointerup", up);
      el.removeEventListener("pointercancel", cancel);
      el.removeEventListener("lostpointercapture", cancel);
    };
  }, [writing, drawWithTouch, tool, transform, page.render, pageSize, shown, onStroke, onErase, previewErase, mintOperationId, onRefused, setDrawing, discard, draw, drawWet]);

  // ---- touch gestures (every mode) ---------------------------------------------------------

  const layRef = useRef(lay);
  const availRef = useRef(avail);
  const modeRef = useRef(mode);
  useEffect(() => {
    layRef.current = lay;
    availRef.current = avail;
    modeRef.current = mode;
  }, [lay, avail, mode]);

  useEffect(() => {
    const el = scroller.current;
    const area = areaRef.current;
    if (!el || !area) return;
    const local = (p: { x: number; y: number }) => {
      const r = el.getBoundingClientRect();
      return { x: p.x - r.left, y: p.y - r.top };
    };
    const two = () => {
      const [a, b] = [...touches.current.values()];
      return { dist: Math.hypot(a!.x - b!.x, a!.y - b!.y), mid: local({ x: (a!.x + b!.x) / 2, y: (a!.y + b!.y) / 2 }) };
    };
    const startPinch = () => {
      const l = layRef.current;
      if (!l || !onZoom) return;
      const { dist, mid } = two();
      pan.current = null;
      pinch.current = { startDist: dist, startFactor: l.factor, anchor: canonicalAt(l, el.scrollLeft, el.scrollTop, mid.x, mid.y) };
    };
    const drawingWithTouch = () => writing && drawWithTouch;

    const down = (e: PointerEvent) => {
      if (e.pointerType !== "touch") return;
      // Interactive overlay controls own their touches: a gesture beginning on the
      // text editor, the text delete control, or a link icon never arms page pan/pinch.
      if ((e.target as HTMLElement | null)?.closest?.("textarea, [data-text-delete], [data-link-id]")) return;
      touches.current.set(e.pointerId, { x: e.clientX, y: e.clientY });
      if (touches.current.size === 2) {
        // A second finger is a gesture: a finger stroke just begun is dropped, never authored.
        if (active.current?.capture.pointerType === "touch" || erasing.current) discard();
        // Pinch outranks an armed/active text drag: cancel it without authoring anything.
        cancelTextGrab();
        touchGestureConsumed.current = true;
        pan.current = null;
        if (!active.current) startPinch();
        e.stopImmediatePropagation();
      } else if (touches.current.size === 1 && !drawingWithTouch()) {
        // New primary touch: potential tap until it arms as a pan (or a pinch starts).
        touchGestureConsumed.current = false;
        pan.current = beginPan(e.pointerId, e.clientX, e.clientY);
      }
    };

    const move = (e: PointerEvent) => {
      if (e.pointerType !== "touch" || !touches.current.has(e.pointerId)) return;
      touches.current.set(e.pointerId, { x: e.clientX, y: e.clientY });
      const pc = pinch.current;
      const l = layRef.current;
      if (pc && l && touches.current.size >= 2) {
        touchGestureConsumed.current = true;
        e.stopImmediatePropagation();
        const { dist, mid } = two();
        const factor = pinchFactor(pc.startFactor, pc.startDist, dist);
        if (Math.abs(factor - l.factor) < 1e-6 && modeRef.current.kind === "zoom") {
          // Same zoom (e.g. at a limit): the fingers only pan.
          const s = scrollToAnchor(l, availRef.current, pc.anchor, mid);
          el.scrollLeft = s.left;
          el.scrollTop = s.top;
        } else {
          focus.current = { anchor: pc.anchor, focal: mid };
          onZoom?.({ kind: "zoom", factor });
        }
        return;
      }
      // A press on text claims the gesture past slop; the page must not scroll
      // from that pointer. PanTracker itself is untouched.
      const g = grabRef.current;
      if (g && g.pointerId === e.pointerId) {
        advanceGrabForMove(e.pointerId, e.clientX, e.clientY);
        if (g.grab.phase === "dragging") return;
      }
      const p = pan.current;
      if (p && p.pointerId === e.pointerId) {
        const step = advancePan(p, e.clientX, e.clientY);
        if (step.justArmed) touchGestureConsumed.current = true;
        if (step.scrollX !== 0 || step.scrollY !== 0) el.scrollBy(step.scrollX, step.scrollY);
      }
    };

    const end = (e: PointerEvent) => {
      if (e.pointerType !== "touch" || !touches.current.delete(e.pointerId)) return;
      if (pinch.current) {
        touchGestureConsumed.current = true;
        e.stopImmediatePropagation();
        if (touches.current.size < 2) {
          pinch.current = null;
          // The finger still down keeps panning (it never draws after a pinch).
          const [rest] = [...touches.current.entries()];
          pan.current = rest ? beginPan(rest[0], rest[1].x, rest[1].y) : null;
          if (pan.current) {
            // Already part of a multi-touch gesture — never a tap.
            pan.current.armed = true;
            touchGestureConsumed.current = true;
          }
        }
        return;
      }
      if (pan.current?.pointerId === e.pointerId) pan.current = null;
      // touchGestureConsumed stays until the next primary touch down so the type-mode
      // bubble pointerup (after this capture listener) can still see it.
    };

    // Capture phase on the gesture root: pan/pinch/text-claim are decided before
    // the write-mode and type-mode handlers see the event.
    area.addEventListener("pointerdown", down, true);
    area.addEventListener("pointermove", move, true);
    area.addEventListener("pointerup", end, true);
    area.addEventListener("pointercancel", end, true);
    return () => {
      area.removeEventListener("pointerdown", down, true);
      area.removeEventListener("pointermove", move, true);
      area.removeEventListener("pointerup", end, true);
      area.removeEventListener("pointercancel", end, true);
    };
  }, [writing, drawWithTouch, onZoom, discard, advanceGrabForMove, cancelTextGrab]);

  return (
    <div className="page-area" ref={areaRef}>
      <div
        className={`page-scroller ${writing ? (drawWithTouch ? "writing touch-ink" : "writing") : ""} ${typing ? "typing" : ""}`}
        ref={scroller}
        onScroll={onScroll}
        data-testid="page-scroller"
        data-writing={writing ? "true" : "false"}
        data-typing={typing ? "true" : "false"}
      >
        <div style={{ width: lay?.content.width ?? 0, height: lay?.content.height ?? 0 }} />
      </div>
      <canvas className="page-canvas" ref={canvas} data-testid="page-canvas" aria-label="Notebook page" />
      <canvas className="page-canvas wet" ref={wetCanvas} data-testid="wet-canvas" aria-hidden />
      {overlayT && (
          <div className={`page-texts ${typing ? "typing" : ""} ${writing ? "writing" : ""}`} data-testid="page-texts">
            {shownTexts.map((t) =>
              draft?.text_id === t.text_id ? null : (
                <div
                  key={t.text_id}
                  className={draggingId === t.text_id ? "page-text dragging" : "page-text"}
                  data-text-id={t.text_id}
                  data-testid="page-text"
                  data-font-size={t.font_size}
                  data-canonical-width={t.width}
                  onPointerDown={(e) => armTextGrab(e, t)}
                  style={{
                    left: overlayT.offsetX + t.x * overlayT.scale,
                    top: overlayT.offsetY + t.y * overlayT.scale,
                    width: t.width * overlayT.scale,
                    fontSize: t.font_size * overlayT.scale,
                  }}
                >
                  {t.content}
                </div>
              ),
            )}
            {draft && (
              <textarea
                ref={editorRef}
                className="page-text page-text-editor"
                data-testid="text-editor"
                data-font-size={draft.current.font_size}
                data-canonical-width={draft.current.width}
                autoFocus
                value={draft.current.content}
                onChange={(e) => {
                  const next = { ...draft, current: { ...draft.current, content: e.target.value } };
                  draftRef.current = next;
                  setDraft(next);
                }}
                onBlur={(e) => {
                  // Editor/session controls own their interaction; don't race
                  // their click with blur finalization.
                  const next = e.relatedTarget;
                  if (next instanceof HTMLElement && next.closest?.('[data-testid="text-delete"], [data-text-session-control]')) return;
                  // Explicit focus move into another control (Done, etc.) always finishes.
                  if (next instanceof Node && document.contains(next)) {
                    void finalizeActive();
                    return;
                  }
                  // Virtual keyboard / visualViewport resize can transiently blur with no relatedTarget.
                  if (performance.now() < suppressBlurUntil.current) {
                    e.currentTarget.focus({ preventScroll: true });
                    return;
                  }
                  void finalizeActive();
                }}
                style={{
                  left: overlayT.offsetX + draft.current.x * overlayT.scale,
                  top: overlayT.offsetY + draft.current.y * overlayT.scale,
                  width: draft.current.width * overlayT.scale,
                  fontSize: draft.current.font_size * overlayT.scale,
                  minHeight: draft.current.font_size * overlayT.scale * 1.2,
                }}
              />
            )}
            {draft?.original && (
              <button
                type="button"
                className="page-text-delete"
                data-testid="text-delete"
                data-text-delete
                title="Delete text"
                aria-label="Delete text"
                onPointerDownCapture={(e) => e.stopPropagation()}
                onPointerUpCapture={(e) => e.stopPropagation()}
                onClick={() => void deleteCurrentDraft()}
                disabled={draft.status !== "editing"}
                style={{
                  left: overlayT.offsetX + (draft.current.x + draft.current.width) * overlayT.scale + 6,
                  top: overlayT.offsetY + draft.current.y * overlayT.scale - 8,
                }}
              >
                <TrashIcon />
              </button>
            )}
          </div>
      )}
      {overlayT && page.links.length > 0 && (
        <div className={`page-links ${writing ? "writing" : ""}`} data-testid="page-links">
          {page.links.map((link) => {
            const box = linkIconLayout(link, overlayT);
            const label = linkLabel?.(link) ?? "Jump to linked page";
            return (
              <button
                key={link.link_id}
                type="button"
                className="page-link"
                data-testid="page-link"
                data-link-id={link.link_id}
                title={label}
                aria-label={label}
                style={{ left: box.left, top: box.top, width: box.width, height: box.height }}
                onClick={(e) => {
                  e.stopPropagation();
                  onLinkClick?.(link);
                }}
              >
                <LinkIcon />
              </button>
            );
          })}
        </div>
      )}
      {pageError && (
        <div className="page-message error" role="alert" data-testid="page-error">
          This page can’t be displayed: {pageError}
        </div>
      )}
      {!pageError && bgLoading && <div className="page-message">Loading background…</div>}
    </div>
  );
}
