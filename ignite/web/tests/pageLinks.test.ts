import { describe, expect, it } from "vitest";
import { LINK_ICON_HALF_CANONICAL, linkIconLayout } from "../src/rendering/pageLinks";
import type { SyncPageLink } from "../src/model/types";

const link: SyncPageLink = {
  link_id: "01a0c450-6c14-7401-8203-04050607081a",
  source_notebook_id: "01a0c450-6c04-7401-8203-040506070804",
  source_page_id: "01a0c450-6c05-7501-8203-040506070805",
  source_x: 1000,
  source_y: 2000,
  target_notebook_id: "01a0c450-6c04-7401-8203-040506070804",
  target_page_id: "01a0c450-6c05-7501-8203-040506070806",
};

describe("page-link icon layout", () => {
  it("matches the Kindle 400×400 canonical box centred on the source point", () => {
    expect(LINK_ICON_HALF_CANONICAL).toBe(200);
    const box = linkIconLayout(link, { scale: 1, offsetX: 0, offsetY: 0 });
    expect(box).toEqual({ left: 800, top: 1800, width: 400, height: 400 });
  });

  it("applies the screen transform (scale + scroll offset)", () => {
    const box = linkIconLayout(link, { scale: 0.5, offsetX: 10, offsetY: 20 });
    expect(box.left).toBeCloseTo(10 + 800 * 0.5);
    expect(box.top).toBeCloseTo(20 + 1800 * 0.5);
    expect(box.width).toBeCloseTo(200);
    expect(box.height).toBeCloseTo(200);
  });
});
