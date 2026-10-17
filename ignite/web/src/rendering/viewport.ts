// Canonical page → browser viewport transform. Pure math; no DOM.
//
// Zoom is relative to Fit (100% = the whole page fits the available area). The page keeps its
// exact canonical aspect ratio (one uniform scale); canonical coordinates are never altered.

export interface Size {
  width: number;
  height: number;
}

export const ZOOM_MIN = 0.25;
export const ZOOM_MAX = 4;
export const ZOOM_STEP = 1.25;
/** Breathing room around the page in CSS px (each side). */
export const PAGE_MARGIN = 16;

export type ZoomMode = { kind: "fit" } | { kind: "zoom"; factor: number };

export function clampZoom(f: number): number {
  return Math.min(ZOOM_MAX, Math.max(ZOOM_MIN, f));
}

export function zoomIn(mode: ZoomMode): ZoomMode {
  const f = mode.kind === "fit" ? 1 : mode.factor;
  return { kind: "zoom", factor: snap(clampZoom(f * ZOOM_STEP)) };
}

export function zoomOut(mode: ZoomMode): ZoomMode {
  const f = mode.kind === "fit" ? 1 : mode.factor;
  return { kind: "zoom", factor: snap(clampZoom(f / ZOOM_STEP)) };
}

function snap(f: number): number {
  // Keep repeated steps from drifting (1.25^n) and make 100% reachable again.
  const r = Math.round(f * 1000) / 1000;
  return Math.abs(r - 1) < 0.01 ? 1 : r;
}

/** CSS px per canonical unit so the whole page fits `avail` (minus margins). */
export function fitScale(avail: Size, page: Size): number {
  const w = Math.max(1, avail.width - 2 * PAGE_MARGIN);
  const h = Math.max(1, avail.height - 2 * PAGE_MARGIN);
  return Math.min(w / page.width, h / page.height);
}

export interface Layout {
  /** CSS px per canonical unit. */
  scale: number;
  /** Zoom relative to Fit. */
  factor: number;
  /** Scrollable content size (≥ the viewport). */
  content: Size;
  /** Page rectangle within the content, CSS px. */
  pageX: number;
  pageY: number;
  pageWidth: number;
  pageHeight: number;
}

export function layout(avail: Size, page: Size, mode: ZoomMode): Layout {
  const fit = fitScale(avail, page);
  const factor = mode.kind === "fit" ? 1 : clampZoom(mode.factor);
  const scale = fit * factor;
  const pageWidth = page.width * scale;
  const pageHeight = page.height * scale;
  const content = {
    width: Math.max(avail.width, pageWidth + 2 * PAGE_MARGIN),
    height: Math.max(avail.height, pageHeight + 2 * PAGE_MARGIN),
  };
  return {
    scale,
    factor,
    content,
    pageX: (content.width - pageWidth) / 2,
    pageY: (content.height - pageHeight) / 2,
    pageWidth,
    pageHeight,
  };
}

/** Screen transform for a viewport scrolled to (scrollLeft, scrollTop). */
export interface ScreenTransform {
  scale: number;
  offsetX: number;
  offsetY: number;
}

export function screenTransform(l: Layout, scrollLeft: number, scrollTop: number): ScreenTransform {
  return { scale: l.scale, offsetX: l.pageX - scrollLeft, offsetY: l.pageY - scrollTop };
}

export function toScreen(t: ScreenTransform, x: number, y: number): [number, number] {
  return [t.offsetX + x * t.scale, t.offsetY + y * t.scale];
}

/**
 * Scroll position that keeps the canonical point at the viewport centre fixed across a zoom
 * change (clamped to the new content).
 */
export function scrollForZoom(
  before: Layout,
  after: Layout,
  avail: Size,
  scrollLeft: number,
  scrollTop: number,
): { left: number; top: number } {
  const cx = (scrollLeft + avail.width / 2 - before.pageX) / before.scale;
  const cy = (scrollTop + avail.height / 2 - before.pageY) / before.scale;
  const left = after.pageX + cx * after.scale - avail.width / 2;
  const top = after.pageY + cy * after.scale - avail.height / 2;
  return {
    left: Math.min(Math.max(0, left), Math.max(0, after.content.width - avail.width)),
    top: Math.min(Math.max(0, top), Math.max(0, after.content.height - avail.height)),
  };
}

/** Canonical page point under viewport point (`fx`, `fy`) (CSS px from the viewport's corner). */
export function canonicalAt(l: Layout, scrollLeft: number, scrollTop: number, fx: number, fy: number): { x: number; y: number } {
  return { x: (scrollLeft + fx - l.pageX) / l.scale, y: (scrollTop + fy - l.pageY) / l.scale };
}

/** Pinch: `startFactor` scaled by how far the fingers spread (`dist / startDist`), clamped. */
export function pinchFactor(startFactor: number, startDist: number, dist: number): number {
  if (!(startDist > 0) || !(dist > 0)) return clampZoom(startFactor);
  return clampZoom((startFactor * dist) / startDist);
}

/**
 * Scroll position that puts canonical point `anchor` under viewport point `focal` in layout `l`
 * (clamped to the content) — the pinch keeps the page point between the fingers under them.
 */
export function scrollToAnchor(l: Layout, avail: Size, anchor: { x: number; y: number }, focal: { x: number; y: number }): { left: number; top: number } {
  const left = l.pageX + anchor.x * l.scale - focal.x;
  const top = l.pageY + anchor.y * l.scale - focal.y;
  return {
    left: Math.min(Math.max(0, left), Math.max(0, l.content.width - avail.width)),
    top: Math.min(Math.max(0, top), Math.max(0, l.content.height - avail.height)),
  };
}
