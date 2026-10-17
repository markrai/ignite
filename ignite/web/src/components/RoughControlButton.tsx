// Decorative Rough.js capsule outline for selected chrome controls. Presentation
// only: the host <button> remains the interactive element. Geometry comes from
// the button's rendered box.

import { useEffect, useMemo, useRef, useState, type ButtonHTMLAttributes } from "react";
import rough from "roughjs";

export const ROUGH_BUTTON_FRAME_CLASS = "rough-button-frame";
export const ROUGH_BUTTON_FRAME_POINTER_EVENTS = "none";
export const ROUGH_CONTROL_INSET = 1.5;
export const ROUGH_CONTROL_RADIUS = 10;
export const ROUGH_CONTROL_STROKE_WIDTH = 1.2;

const gen = rough.generator();

export function seedFromIdentity(identity: string): number {
  let h = 2166136261;
  for (let i = 0; i < identity.length; i++) {
    h ^= identity.charCodeAt(i);
    h = Math.imul(h, 16777619);
  }
  return (h >>> 0) % 0x7fffffff || 1;
}

export function roundedRectSvgPath(width: number, height: number, radius: number, inset: number): string {
  const x = inset;
  const y = inset;
  const w = width - 2 * inset;
  const h = height - 2 * inset;
  if (!(w > 0) || !(h > 0)) return "";
  const r = Math.min(radius, w / 2, h / 2);
  if (!(r > 0)) {
    return `M${x} ${y} H${x + w} V${y + h} H${x} Z`;
  }
  return [
    `M${x + r} ${y}`,
    `H${x + w - r}`,
    `A${r} ${r} 0 0 1 ${x + w} ${y + r}`,
    `V${y + h - r}`,
    `A${r} ${r} 0 0 1 ${x + w - r} ${y + h}`,
    `H${x + r}`,
    `A${r} ${r} 0 0 1 ${x} ${y + h - r}`,
    `V${y + r}`,
    `A${r} ${r} 0 0 1 ${x + r} ${y}`,
    "Z",
  ].join(" ");
}

export function roughCapsulePath(width: number, height: number, seed: number, radius?: number): string {
  const r = radius ?? Math.min(height / 2, ROUGH_CONTROL_RADIUS);
  const d = roundedRectSvgPath(width, height, r, ROUGH_CONTROL_INSET);
  if (!d) return "";
  // Keep a light sketch character: lower roughness/bowing than the first pass so
  // outlines read as soft capsules, not jagged scribbles, at desktop zoom.
  const drawable = gen.path(d, {
    roughness: 0.55,
    bowing: 0.4,
    strokeWidth: ROUGH_CONTROL_STROKE_WIDTH,
    seed,
    disableMultiStroke: false,
  });
  return gen
    .toPaths(drawable)
    .filter((p) => p.stroke !== "none" && p.d)
    .map((p) => p.d)
    .join(" ");
}

export interface RoughButtonFrameProps {
  width: number;
  height: number;
  identity: string;
}

export function RoughButtonFrame({ width, height, identity }: RoughButtonFrameProps) {
  const seed = seedFromIdentity(identity);
  const d = useMemo(() => roughCapsulePath(width, height, seed), [width, height, seed]);
  if (!(width > 0) || !(height > 0) || !d) return null;
  return (
    <svg
      className={ROUGH_BUTTON_FRAME_CLASS}
      data-testid="rough-button-frame"
      aria-hidden
      pointerEvents={ROUGH_BUTTON_FRAME_POINTER_EVENTS}
      width={width}
      height={height}
      viewBox={`0 0 ${width} ${height}`}
    >
      <path d={d} />
    </svg>
  );
}

export interface RoughControlButtonProps extends ButtonHTMLAttributes<HTMLButtonElement> {
  identity: string;
}

export function RoughControlButton({ identity, className, children, ...props }: RoughControlButtonProps) {
  const ref = useRef<HTMLButtonElement>(null);
  const [size, setSize] = useState({ width: 0, height: 0 });

  useEffect(() => {
    const el = ref.current;
    if (!el) return;
    const update = () => {
      const next = { width: el.offsetWidth, height: el.offsetHeight };
      setSize((prev) => (prev.width === next.width && prev.height === next.height ? prev : next));
    };
    update();
    const obs = new ResizeObserver(update);
    obs.observe(el);
    return () => obs.disconnect();
  }, []);

  const extra = className ?? "";
  return (
    <button ref={ref} className={`btn rough-control-button ${extra}`.trim()} {...props}>
      <RoughButtonFrame width={size.width} height={size.height} identity={identity} />
      <span className="rough-control-content">{children}</span>
    </button>
  );
}
