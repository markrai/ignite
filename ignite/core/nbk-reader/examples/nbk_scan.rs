//! Read-only diagnostic: decode every notebook directory under ROOT and report what the
//! importer would do. Never writes anything.
//!
//! `nbk_scan ROOT [CATALOG_DB]`   e.g. `nbk_scan /mnt/us/.notebooks /var/local/ksdk.content.db`

use std::collections::BTreeMap;
use std::path::Path;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let root = Path::new(args.first().expect("usage: nbk_scan ROOT [CATALOG_DB]"));
    let catalog = args
        .get(1)
        .map(|p| {
            nbk_reader::catalog::read_catalog(&std::fs::read(p).expect("read catalog"))
                .expect("catalog")
        })
        .unwrap_or_default();
    let mut dirs: Vec<_> = std::fs::read_dir(root)
        .expect("read root")
        .filter_map(|e| e.ok())
        .filter(|e| e.path().join("nbk").is_file())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    dirs.sort();
    for id in dirs {
        let entry = catalog.iter().find(|c| c.uuid == id);
        let kind = entry
            .and_then(|e| e.node_type.clone())
            .unwrap_or_else(|| "-".into());
        let title = entry
            .and_then(|e| e.title.clone())
            .unwrap_or_else(|| "-".into());
        let t0 = std::time::Instant::now();
        let result =
            nbk_reader::read_source(&root.join(&id)).and_then(|s| nbk_reader::decode_snapshot(&s));
        let ms = t0.elapsed().as_millis();
        match result {
            Ok(nb) => {
                let mut brushes: BTreeMap<String, usize> = BTreeMap::new();
                let mut diag = 0;
                let mut canvases = BTreeMap::new();
                for p in &nb.pages {
                    *canvases.entry((p.width, p.height)).or_insert(0) += 1;
                    for s in &p.strokes {
                        *brushes.entry(format!("{:?}", s.brush_type)).or_insert(0) += 1;
                        diag += usize::from(!s.diagnostics.is_empty());
                    }
                }
                println!(
                    "OK   {id} [{kind}] {title:?} pages={} strokes={} canvases={canvases:?} brushes={brushes:?} stroke_diags={diag} {ms}ms",
                    nb.pages.len(),
                    nb.stroke_count()
                );
            }
            Err(e) => println!("FAIL {id} [{kind}] {title:?} {e} {ms}ms"),
        }
    }
}
