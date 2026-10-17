//! Compact monochrome link icons in canonical page space, painted after text and before chrome.
//!
//! Icons are display metadata: never baked into `background.raw`.

use kindle_platform::{DirtyRegion, FbGeometry};
use notebook_core::{PagePoint, ScribeDisplayTransform};
use sync_protocol::SyncPageLink;

use crate::content::CanonicalRect;
use crate::profile::RenderProfile;
use crate::ui_gfx::{Canvas, Rect, BLACK};

/// Half-extent of the icon in canonical units (~24 panel pixels at scale 8.4).
pub const LINK_ICON_HALF_CANONICAL: f32 = 200.0;
const LINK_ICON_HALF_PANEL: i32 = 24;

pub fn icon_canonical_rect(link: &SyncPageLink) -> CanonicalRect {
    CanonicalRect {
        x: link.source_x - LINK_ICON_HALF_CANONICAL,
        y: link.source_y - LINK_ICON_HALF_CANONICAL,
        width: LINK_ICON_HALF_CANONICAL * 2.0,
        height: LINK_ICON_HALF_CANONICAL * 2.0,
    }
}

pub fn contains_point(link: &SyncPageLink, x: f32, y: f32) -> bool {
    icon_canonical_rect(link).contains(notebook_core::Point::new(x, y))
}

pub fn panel_dirty(transform: ScribeDisplayTransform, link: &SyncPageLink) -> Option<DirtyRegion> {
    let c = transform.to_panel_pixel(PagePoint::new(link.source_x, link.source_y))?;
    let x = c.0 as i32 - LINK_ICON_HALF_PANEL - 2;
    let y = c.1 as i32 - LINK_ICON_HALF_PANEL - 2;
    let w = (LINK_ICON_HALF_PANEL * 2 + 4) as u32;
    Some(DirtyRegion {
        x,
        y,
        width: w,
        height: w,
    })
}

/// Paint link icons after text and before chrome.
///
/// `clip == None` paints every icon. `Some` paints only icons whose panel box
/// intersects the clip, and restores pixels of that box that fall outside the clip
/// so a regional clean does not rewrite the rest of the page.
pub fn paint_page_links(
    fb: &mut [u8],
    geo: &FbGeometry,
    profile: &RenderProfile,
    links: &[SyncPageLink],
    clip: Option<DirtyRegion>,
) -> Result<(), String> {
    if links.is_empty() {
        return Ok(());
    }
    let Ok(transform) = profile.transform() else {
        return Ok(());
    };
    paint_link_icons(fb, geo, transform, links, clip)?;
    Ok(())
}

/// Paint link icons. Returns the union dirty region in panel pixels.
pub fn paint_link_icons(
    fb: &mut [u8],
    geo: &FbGeometry,
    transform: ScribeDisplayTransform,
    links: &[SyncPageLink],
    clip: Option<DirtyRegion>,
) -> Result<Option<DirtyRegion>, String> {
    if links.is_empty() {
        return Ok(None);
    }
    let mut dirty: Option<DirtyRegion> = None;
    for link in links {
        let Some(icon) = panel_dirty(transform, link) else {
            continue;
        };
        if clip.is_some_and(|c| !icon.intersects(c)) {
            continue;
        }
        let saved = clip.and_then(|c| snapshot_outside(fb, geo, icon, c));
        let Some((px, py)) = transform.to_panel_pixel(PagePoint::new(link.source_x, link.source_y))
        else {
            continue;
        };
        {
            let mut c = Canvas::new(fb, geo)?;
            paint_one(&mut c, px as i32, py as i32);
        }
        if let Some(saved) = saved {
            restore_outside(fb, &saved);
        }
        dirty = Some(match dirty {
            None => icon,
            Some(acc) => acc.union(icon),
        });
    }
    Ok(dirty)
}

struct SavedPixel {
    offset: usize,
    value: u8,
}

fn snapshot_outside(
    fb: &[u8],
    geo: &FbGeometry,
    icon: DirtyRegion,
    clip: DirtyRegion,
) -> Option<Vec<SavedPixel>> {
    let box_ = icon.clamp_to(geo.xres, geo.yres)?;
    let mut saved = Vec::new();
    for y in box_.y as u32..box_.y as u32 + box_.height {
        for x in box_.x as u32..box_.x as u32 + box_.width {
            if clip_contains(clip, x, y) {
                continue;
            }
            let Some(off) = geo.byte_offset_visible_8bpp(x, y) else {
                continue;
            };
            if off < fb.len() {
                saved.push(SavedPixel {
                    offset: off,
                    value: fb[off],
                });
            }
        }
    }
    Some(saved)
}

fn restore_outside(fb: &mut [u8], saved: &[SavedPixel]) {
    for px in saved {
        if px.offset < fb.len() {
            fb[px.offset] = px.value;
        }
    }
}

fn clip_contains(clip: DirtyRegion, x: u32, y: u32) -> bool {
    let x = x as i32;
    let y = y as i32;
    x >= clip.x && y >= clip.y && x < clip.right() && y < clip.bottom()
}

fn paint_one(c: &mut Canvas<'_>, cx: i32, cy: i32) {
    let r = LINK_ICON_HALF_PANEL as u32;
    let x = cx.saturating_sub(LINK_ICON_HALF_PANEL).max(0) as u32;
    let y = cy.saturating_sub(LINK_ICON_HALF_PANEL).max(0) as u32;
    let boxr = Rect::new(x, y, r * 2, r * 2);
    c.frame(boxr, 3, BLACK);
    // Arrow pointing up-right: two legs.
    let x0 = x + 8;
    let y0 = y + r * 2 - 10;
    let x1 = x + r * 2 - 8;
    let y1 = y + 8;
    for i in 0..r {
        let t = i as f32 / r.max(1) as f32;
        let px = x0 as f32 + (x1 as f32 - x0 as f32) * t;
        let py = y0 as f32 + (y1 as f32 - y0 as f32) * t;
        c.put(px.round() as u32, py.round() as u32, BLACK);
        c.put(px.round() as u32 + 1, py.round() as u32, BLACK);
    }
    // Arrow head.
    for k in 0..10 {
        c.put(x1 - k, y1, BLACK);
        c.put(x1, y1 + k, BLACK);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sync_engine::testkit::{nb, page_link, pg};

    #[test]
    fn icon_contains_source_point() {
        let link = page_link(1, nb(1), pg(1), nb(2), pg(2));
        assert!(contains_point(&link, link.source_x, link.source_y));
        assert!(!contains_point(
            &link,
            link.source_x + 5000.0,
            link.source_y
        ));
    }
}
