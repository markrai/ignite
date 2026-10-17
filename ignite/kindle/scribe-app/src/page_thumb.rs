//! Lazy, replica-local page thumbnails for the link-target picker.
//!
//! Derived cache only: never snapshot authority, never `background.raw`. Generation is
//! proportional to the visible grid, not to every page in the notebook.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::Path;
use std::time::SystemTime;

use notebook_core::{Stroke, CANONICAL_HEIGHT, CANONICAL_WIDTH};

use crate::store::{load_page_journals_read_only, load_page_texts, page_dir};
use crate::text_paint::paint_texts_8bpp;
use crate::ui_gfx::WHITE;

pub const THUMB_W: u32 = 186;
pub const THUMB_H: u32 = 248;
const CACHE_NAME: &str = ".scribe-thumb";

fn file_stamp(path: &Path) -> String {
    match fs::metadata(path) {
        Ok(m) => {
            let t = m
                .modified()
                .ok()
                .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            format!("{}:{}", m.len(), t)
        }
        Err(_) => "0:0".into(),
    }
}

fn fingerprint(dir: &Path) -> String {
    let mut s = String::new();
    for name in [
        "page.meta",
        "strokes.jsonl",
        "edits.jsonl",
        "texts.jsonl",
        "links.jsonl",
        "background.raw",
    ] {
        s.push_str(name);
        s.push('=');
        s.push_str(&file_stamp(&dir.join(name)));
        s.push(';');
    }
    s
}

fn cache_path(dir: &Path) -> std::path::PathBuf {
    dir.join(CACHE_NAME)
}

fn write_cache(dir: &Path, fp: &str, pixels: &[u8]) -> Result<(), String> {
    let tmp = dir.join(".scribe-thumb.tmp");
    {
        let mut f = File::create(&tmp).map_err(|e| e.to_string())?;
        writeln!(f, "SCRIBE_THUMB_v1").map_err(|e| e.to_string())?;
        writeln!(f, "{fp}").map_err(|e| e.to_string())?;
        writeln!(f, "{THUMB_W} {THUMB_H}").map_err(|e| e.to_string())?;
        f.write_all(pixels).map_err(|e| e.to_string())?;
    }
    let dest = cache_path(dir);
    #[cfg(windows)]
    if dest.exists() {
        let _ = fs::remove_file(&dest);
    }
    fs::rename(&tmp, dest).map_err(|e| e.to_string())
}

fn read_cache(dir: &Path, fp: &str) -> Option<Vec<u8>> {
    let mut f = File::open(cache_path(dir)).ok()?;
    let mut header = String::new();
    let mut buf = [0u8; 1];
    // Read three newline-terminated header lines.
    let mut lines = Vec::new();
    for _ in 0..3 {
        header.clear();
        loop {
            f.read_exact(&mut buf).ok()?;
            if buf[0] == b'\n' {
                break;
            }
            header.push(buf[0] as char);
        }
        lines.push(header.clone());
    }
    if lines[0] != "SCRIBE_THUMB_v1" || lines[1] != fp {
        return None;
    }
    let mut dims = lines[2].split_whitespace();
    let w: u32 = dims.next()?.parse().ok()?;
    let h: u32 = dims.next()?.parse().ok()?;
    if w != THUMB_W || h != THUMB_H {
        return None;
    }
    let mut pixels = vec![0u8; (THUMB_W * THUMB_H) as usize];
    f.read_exact(&mut pixels).ok()?;
    Some(pixels)
}

fn subsample_background(full: &[u8], fw: u32, fh: u32) -> Vec<u8> {
    let mut out = vec![WHITE; (THUMB_W * THUMB_H) as usize];
    if fw == 0 || fh == 0 || full.is_empty() {
        return out;
    }
    for y in 0..THUMB_H {
        let sy = y * fh / THUMB_H;
        for x in 0..THUMB_W {
            let sx = x * fw / THUMB_W;
            let i = (sy * fw + sx) as usize;
            if i < full.len() {
                out[(y * THUMB_W + x) as usize] = full[i];
            }
        }
    }
    out
}

fn draw_strokes(out: &mut [u8], strokes: &[Stroke]) {
    for s in strokes {
        let pts = &s.points;
        if pts.is_empty() {
            continue;
        }
        let mut prev = pts[0];
        for p in pts.iter().skip(1) {
            draw_line(out, prev.x, prev.y, p.x, p.y);
            prev = *p;
        }
        plot(out, prev.x, prev.y);
    }
}

fn plot(out: &mut [u8], x: f32, y: f32) {
    let px = (x / CANONICAL_WIDTH * THUMB_W as f32).round() as i32;
    let py = (y / CANONICAL_HEIGHT * THUMB_H as f32).round() as i32;
    if px < 0 || py < 0 {
        return;
    }
    let (px, py) = (px as u32, py as u32);
    if px >= THUMB_W || py >= THUMB_H {
        return;
    }
    out[(py * THUMB_W + px) as usize] = 0x00;
}

fn draw_line(out: &mut [u8], x0: f32, y0: f32, x1: f32, y1: f32) {
    let n = ((x1 - x0).abs().max((y1 - y0).abs()) / 40.0)
        .ceil()
        .max(1.0) as i32;
    for i in 0..=n {
        let t = i as f32 / n as f32;
        plot(out, x0 + (x1 - x0) * t, y0 + (y1 - y0) * t);
    }
}

/// Load or build a thumbnail for one page. Missing journals yield plain paper.
pub fn load_or_build(notebook_dir: &Path, page_local: &str, panel_w: u32, panel_h: u32) -> Vec<u8> {
    let dir = page_dir(notebook_dir, page_local);
    let fp = fingerprint(&dir);
    if let Some(cached) = read_cache(&dir, &fp) {
        return cached;
    }
    let mut pixels = vec![WHITE; (THUMB_W * THUMB_H) as usize];
    let bg_path = dir.join("background.raw");
    if let Ok(raw) = fs::read(&bg_path) {
        let packed = (panel_w as usize).saturating_mul(panel_h as usize);
        if raw.len() == packed {
            pixels = subsample_background(&raw, panel_w, panel_h);
        }
    }
    if let Ok(journals) = load_page_journals_read_only(notebook_dir, page_local) {
        draw_strokes(&mut pixels, &journals.strokes);
    }
    if let Ok(texts) = load_page_texts(&dir) {
        paint_texts_8bpp(&mut pixels, THUMB_W, THUMB_H, &texts);
    }
    let _ = write_cache(&dir, &fp, &pixels);
    pixels
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subsample_is_thumb_sized() {
        let full = vec![0x80u8; 1860 * 2480];
        let t = subsample_background(&full, 1860, 2480);
        assert_eq!(t.len(), (THUMB_W * THUMB_H) as usize);
        assert!(t.iter().all(|b| *b == 0x80));
    }

    #[test]
    fn text_only_page_thumbnail_is_not_blank() {
        use sync_protocol::{TextFace, TextId};
        use uuid::Uuid;

        let dir = std::env::temp_dir().join(format!(
            "scribe-thumb-text-{}-{}",
            std::process::id(),
            "only"
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let page = dir.join("page-1");
        fs::create_dir_all(&page).unwrap();
        let mut b = [0u8; 16];
        b[0] = 0x01;
        b[6] = 0x70;
        b[8] = 0x80;
        b[15] = 1;
        let text = sync_protocol::SyncText {
            text_id: TextId::from_uuid(Uuid::from_bytes(b)).unwrap(),
            x: 200.0,
            y: 400.0,
            width: 8000.0,
            font_size: 340.0,
            face: TextFace::DefaultSansV1,
            content: "Hello".into(),
        };
        crate::store::write_page_texts(&page, std::slice::from_ref(&text)).unwrap();
        let pixels = load_or_build(&dir, "page-1", 1860, 2480);
        assert!(
            pixels.iter().any(|p| *p != WHITE),
            "typed text must darken the thumbnail"
        );
        let cached = load_or_build(&dir, "page-1", 1860, 2480);
        assert_eq!(cached, pixels);
        let _ = fs::remove_dir_all(&dir);
    }
}
