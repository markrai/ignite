//! Diagnostic: where does opening a notebook spend its time?
//!
//! Run against a COPY of a notebook directory (the open path may write one-time
//! format migrations / chrome repairs). Prints per-phase timings.
//!
//!   open_bench <notebook-dir> [runs]
//!
//! Or synthesize a multi-page fixture at Kindle panel size:
//!
//!   open_bench --synth <out-dir> <page-count> [strokes-per-page] [runs]

use std::path::{Path, PathBuf};
use std::time::Instant;

use kindle_platform::FbGeometry;
use notebook_core::{Point, Stroke, StrokeId};
use scribe_app::ink::InkRequest;
use scribe_app::library::read_notebook_manifest_read_only;
use scribe_app::notebook::{
    allocate_page_id, commit_new_page_to_manifest, create_page_files_from_template,
    load_page_for_switch, open_or_create_notebook, persist_active_page, repair_background_chrome,
};
use scribe_app::store::{
    load_existing_page, migrate_all_pages, parse_stroke_journal, replay_edit_journal, PageStore,
};

const PANEL_W: u32 = 1860;
const PANEL_H: u32 = 2480;
const LINE_LENGTH: u32 = 1860;

fn ms(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1000.0
}

fn geo() -> FbGeometry {
    FbGeometry {
        xres: PANEL_W,
        yres: PANEL_H,
        xres_virtual: LINE_LENGTH,
        yres_virtual: PANEL_H,
        xoffset: 0,
        yoffset: 0,
        bits_per_pixel: 8,
        line_length: LINE_LENGTH,
        smem_len: LINE_LENGTH * PANEL_H,
    }
}

fn paper() -> Vec<u8> {
    vec![0xffu8; (LINE_LENGTH * PANEL_H) as usize]
}

fn stroke(id: &str, i: usize) -> Stroke {
    let x0 = 100.0 + (i % 50) as f32 * 20.0;
    let y0 = 100.0 + (i / 50) as f32 * 30.0;
    Stroke {
        id: StrokeId::new(id),
        points: vec![Point::new(x0, y0), Point::new(x0 + 40.0, y0 + 10.0)],
        brush: None,
        finished: true,
    }
}

fn synth_notebook(out: &Path, pages: usize, strokes_per_page: usize) {
    let _ = std::fs::remove_dir_all(out);
    std::fs::create_dir_all(out).expect("mkdir");
    let g = geo();
    let bg = paper();
    let open = open_or_create_notebook(out, InkRequest::Default, &g, &bg).expect("create");
    let mut man = open.manifest;
    let mut store = open.store;
    for i in 0..strokes_per_page {
        store
            .append_committed_stroke(&stroke(&format!("s-p1-{i}"), i))
            .unwrap();
    }
    drop(store);
    for p in 2..=pages {
        let id = allocate_page_id(&man);
        let (mut s, _) = create_page_files_from_template(out, &id, "page-1").unwrap();
        for i in 0..strokes_per_page {
            s.append_committed_stroke(&stroke(&format!("s-p{p}-{i}"), i))
                .unwrap();
        }
        commit_new_page_to_manifest(out, &mut man, &id).unwrap();
    }
    persist_active_page(out, &mut man, "page-1").unwrap();
    println!(
        "synth pages={pages} strokes_per_page={strokes_per_page} dir={}",
        out.display()
    );
}

fn bench_dir(dir: &Path, runs: usize) {
    let m = read_notebook_manifest_read_only(dir).expect("manifest");
    let (_, first) = load_existing_page(dir, &m.active_page_id).expect("active page");
    let h = &first.header;
    let geo = FbGeometry {
        xres: h.panel_width,
        yres: h.panel_height,
        xres_virtual: h.line_length,
        yres_virtual: h.panel_height,
        xoffset: 0,
        yoffset: 0,
        bits_per_pixel: 8,
        line_length: h.line_length,
        smem_len: h.line_length * h.panel_height,
    };
    let profile = first.render_profile.clone();
    println!(
        "pages={} active={} panel={}x{}",
        m.page_ids.len(),
        m.active_page_id,
        h.panel_width,
        h.panel_height
    );

    for run in 1..=runs {
        let t = Instant::now();
        open_or_create_notebook(dir, InkRequest::Default, &geo, &paper()).expect("open");
        let total = ms(t);

        let t = Instant::now();
        migrate_all_pages(dir, &m.page_ids).unwrap();
        let migrate = ms(t);

        let t = Instant::now();
        let mut bg_read = 0.0;
        let mut journals = 0.0;
        let mut strokes = 0usize;
        for pid in &m.page_ids {
            let (_s, mut lp) = load_existing_page(dir, pid).unwrap();
            repair_background_chrome(dir, pid, &mut lp).unwrap();
            strokes += lp.strokes.len();

            let pdir = dir.join(pid);

            let tb = Instant::now();
            let bg = std::fs::read(pdir.join("background.raw")).unwrap();
            std::hint::black_box(bg.iter().fold(0u32, |a, b| a.wrapping_add(*b as u32)));
            bg_read += ms(tb);

            let tj = Instant::now();
            let raw = std::fs::read(pdir.join("strokes.jsonl")).unwrap();
            let (s, _) = parse_stroke_journal(&raw).unwrap();
            let e = std::fs::read(pdir.join("edits.jsonl")).unwrap();
            std::hint::black_box(replay_edit_journal(s, &e).unwrap());
            journals += ms(tj);
        }
        let all_pages = ms(t);

        let t = Instant::now();
        load_page_for_switch(dir, &m.active_page_id, &profile, &geo).unwrap();
        let one_page = ms(t);

        println!(
            "run {run}: open_total={total:.0}ms | migrate_all={migrate:.0}ms load+repair_all_pages={all_pages:.0}ms (bg_read≈{bg_read:.0} journals≈{journals:.0}, {strokes} strokes) | one_page_load={one_page:.0}ms"
        );
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(|s| s.as_str()) == Some("--synth") {
        let out = PathBuf::from(args.get(2).expect("out-dir"));
        let pages: usize = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(22);
        let strokes: usize = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(50);
        let runs: usize = args.get(5).and_then(|s| s.parse().ok()).unwrap_or(3);
        synth_notebook(&out, pages, strokes);
        bench_dir(&out, runs);
        return;
    }
    let dir = Path::new(args.get(1).expect("notebook-dir or --synth"));
    let runs: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(3);
    bench_dir(dir, runs);
}
