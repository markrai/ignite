// Display layout for snapshot page links. The browser does not author links; it only paints
// them and jumps. Size matches Kindle `link_paint::LINK_ICON_HALF_CANONICAL` (200).

import type { SyncPageLink } from "../model/types";
import type { ScreenTransform } from "./viewport";

/** Half-extent of the icon in canonical units (~24 panel pixels at scale 8.4). */
export const LINK_ICON_HALF_CANONICAL = 200;

export function linkIconLayout(
  link: SyncPageLink,
  t: ScreenTransform,
): { left: number; top: number; width: number; height: number } {
  const size = LINK_ICON_HALF_CANONICAL * 2 * t.scale;
  return {
    left: t.offsetX + (link.source_x - LINK_ICON_HALF_CANONICAL) * t.scale,
    top: t.offsetY + (link.source_y - LINK_ICON_HALF_CANONICAL) * t.scale,
    width: size,
    height: size,
  };
}
