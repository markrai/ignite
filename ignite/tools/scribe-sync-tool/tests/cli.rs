//! End-to-end CLI tests on a real (host-built) Scribe library.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use kindle_platform::FbGeometry;
use notebook_core::{new_stroke_id, Point, Stroke, StrokeId};
use scribe_app::ink::InkRequest;
use scribe_app::library::open_or_create_library;
use scribe_app::store::{load_existing_page, PageStore};
use scribe_app::ui_gfx::paper_background;
use scribe_sync_tool::run;

fn tmp(label: &str) -> PathBuf {
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = std::env::temp_dir().join(format!("scribe-sync-tool-{label}-{n}"));
    let _ = fs::remove_dir_all(&p);
    p
}

fn geo() -> FbGeometry {
    FbGeometry {
        xres: 200,
        yres: 160,
        xres_virtual: 200,
        yres_virtual: 160,
        xoffset: 0,
        yoffset: 0,
        bits_per_pixel: 8,
        line_length: 200,
        smem_len: 200 * 160,
    }
}

fn library(root: &Path) {
    let g = geo();
    let lib = open_or_create_library(root, InkRequest::Default, &g, &paper_background(&g)).unwrap();
    let dir = lib.repository.notebook_dir("notebook-1");
    let (mut store, _) = load_existing_page(&dir, "page-1").unwrap();
    for id in [StrokeId::new("s-1727000000000-1"), new_stroke_id()] {
        store
            .append_committed_stroke(&Stroke {
                id,
                points: vec![Point::new(10.0, 20.0), Point::new(300.5, 400.25)],
                brush: None,
                finished: true,
            })
            .unwrap();
    }
}

fn tree(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn walk(dir: &Path, out: &mut BTreeMap<PathBuf, Vec<u8>>) {
        for e in fs::read_dir(dir).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, out);
            } else {
                out.insert(p.clone(), fs::read(&p).unwrap());
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, &mut out);
    out
}

fn args(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

fn line<'a>(out: &'a str, key: &str) -> &'a str {
    out.lines()
        .find_map(|l| l.strip_prefix(key))
        .unwrap_or_else(|| panic!("no {key} in:\n{out}"))
}

#[test]
fn identity_snapshot_and_validate_end_to_end() {
    let root = tmp("lib");
    let state = tmp("state");
    let outdir = tmp("out");
    fs::create_dir_all(&outdir).unwrap();
    library(&root);
    let before = tree(&root);
    let (d, s) = (root.to_str().unwrap(), state.to_str().unwrap());

    let out = run(&args(&[
        "identity",
        "--data-dir",
        d,
        "--replica-state-dir",
        s,
    ]))
    .unwrap();
    assert!(
        out.contains(
            "identity_written=true allocated_library=true allocated_notebooks=1 allocated_pages=1"
        ),
        "{out}"
    );
    assert!(out.contains("notebook notebook-1 -> "));
    let library_id = line(&out, "library_id=").to_string();
    let replica_id = line(&out, "replica_id=").to_string();
    assert_ne!(library_id, replica_id);

    let again = run(&args(&[
        "identity",
        "--data-dir",
        d,
        "--replica-state-dir",
        s,
    ]))
    .unwrap();
    assert!(again.contains("identity_written=false"), "{again}");
    assert_eq!(line(&again, "library_id="), library_id);
    assert_eq!(line(&again, "replica_id="), replica_id);

    let snap_a = outdir.join("a.json");
    let snap_b = outdir.join("b.json");
    let o1 = run(&args(&[
        "snapshot",
        "--data-dir",
        d,
        "--replica-state-dir",
        s,
        "--out",
        snap_a.to_str().unwrap(),
    ]))
    .unwrap();
    let o2 = run(&args(&[
        "snapshot",
        "--data-dir",
        d,
        "--replica-state-dir",
        s,
        "--out",
        snap_b.to_str().unwrap(),
    ]))
    .unwrap();
    let h1 = line(&o1, "snapshot_sha256=").to_string();
    assert_eq!(h1, line(&o2, "snapshot_sha256="));
    assert!(o1.contains("notebooks=1 pages=1 visible_strokes=2"), "{o1}");
    let bytes = fs::read(&snap_a).unwrap();
    assert_eq!(bytes, fs::read(&snap_b).unwrap());
    assert_eq!(
        sync_protocol::sha256_hex(&bytes).as_str(),
        h1,
        "file hash == printed hash"
    );

    let v = run(&args(&["validate-snapshot", snap_a.to_str().unwrap()])).unwrap();
    assert!(
        v.contains("valid=true") && v.contains("canonical_bytes=true"),
        "{v}"
    );
    assert_eq!(line(&v, "snapshot_sha256="), h1);
    // Pretty-printed copy: valid, same hash, flagged non-canonical.
    let pretty = outdir.join("pretty.json");
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    fs::write(&pretty, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
    let v2 = run(&args(&["validate-snapshot", pretty.to_str().unwrap()])).unwrap();
    assert!(v2.contains("canonical_bytes=false"));
    assert_eq!(line(&v2, "snapshot_sha256="), h1);
    // Tampered: refused.
    let bad = outdir.join("bad.json");
    fs::write(
        &bad,
        String::from_utf8(bytes.clone())
            .unwrap()
            .replacen(r#""v":1"#, r#""v":9"#, 1),
    )
    .unwrap();
    assert!(run(&args(&["validate-snapshot", bad.to_str().unwrap()])).is_err());

    // Only .scribe-sync/{identity.json, identity.lock} were added; every other byte identical.
    let after = tree(&root);
    for (p, b) in &before {
        assert_eq!(after.get(p), Some(b), "{} changed", p.display());
    }
    let added: Vec<_> = after.keys().filter(|k| !before.contains_key(*k)).collect();
    assert_eq!(
        added,
        vec![
            &root.join(".scribe-sync").join("identity.json"),
            &root.join(".scribe-sync").join("identity.lock")
        ]
    );
    for d in [&root, &state, &outdir] {
        let _ = fs::remove_dir_all(d);
    }
}

#[test]
fn refusals() {
    let root = tmp("refuse");
    library(&root);
    let state = tmp("refuse-state");
    let d = root.to_str().unwrap();
    // Snapshot output inside the library.
    let inside = root.join("snap.json");
    assert!(run(&args(&[
        "snapshot",
        "--data-dir",
        d,
        "--replica-state-dir",
        state.to_str().unwrap(),
        "--out",
        inside.to_str().unwrap(),
    ]))
    .unwrap_err()
    .contains("inside the library"));
    assert!(!inside.exists());
    // Replica state inside the library.
    let bad_state = root.join("state");
    assert!(run(&args(&[
        "identity",
        "--data-dir",
        d,
        "--replica-state-dir",
        bad_state.to_str().unwrap()
    ]))
    .is_err());
    // A directory that is not a library is never created/migrated.
    let empty = tmp("not-a-library");
    fs::create_dir_all(&empty).unwrap();
    assert!(run(&args(&[
        "identity",
        "--data-dir",
        empty.to_str().unwrap(),
        "--replica-state-dir",
        state.to_str().unwrap()
    ]))
    .is_err());
    assert!(
        fs::read_dir(&empty).unwrap().next().is_none(),
        "nothing created"
    );
    assert!(run(&args(&["frobnicate"])).is_err());
    assert!(run(&args(&["identity", "--data-dir", d])).is_err());
    assert!(run(&args(&["identity", "--data-dir", d, "--bogus", "x"])).is_err());
    for p in [&root, &state, &empty] {
        let _ = fs::remove_dir_all(p);
    }
}

#[test]
fn compare_reports_match_then_differ_without_printing_the_token() {
    use scribe_app::sync_outbound::{OutboundSync, SyncConfigV1, CONFIG_FILE};
    use scribe_sync_server::{generate_token, ServerConfig, ServerHandle, Store, TokenVerifier};

    let root = tmp("cmp-lib");
    let sync = tmp("cmp-sync");
    let srv_dir = tmp("cmp-srv");
    library(&root);
    let d = root.to_str().unwrap();
    let sd = sync.to_str().unwrap();
    // Never synced: compare refuses rather than minting identity.
    let err = run(&args(&["compare", "--data-dir", d, "--sync-dir", sd])).unwrap_err();
    assert!(err.contains("no sync identity"), "{err}");
    assert!(!root.join(".scribe-sync").join("identity.json").exists());

    let token = generate_token().unwrap();
    let srv = ServerHandle::start_with(
        "127.0.0.1:0".parse().unwrap(),
        Store::open(&srv_dir).unwrap(),
        ServerConfig {
            auth: Some(TokenVerifier::for_token(&token)),
            web_root: None,
            control: None,
        },
    )
    .unwrap();
    fs::create_dir_all(&sync).unwrap();
    let cfg = SyncConfigV1 {
        v: 1,
        server_base_url: srv.base_url(),
        token: token.clone(),
        debug_drop_first_ack: false,
        debug_crash_point: None,
        server_id: None,
    };
    fs::write(sync.join(CONFIG_FILE), serde_json::to_vec(&cfg).unwrap()).unwrap();
    let s = OutboundSync::open(&root, &sync).unwrap();
    let t0 = std::time::Instant::now();
    while !s.state().genesis_uploaded {
        assert!(t0.elapsed().as_secs() < 20, "genesis upload");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    s.shutdown(std::time::Duration::from_secs(3));

    let before = tree(&root);
    let out = run(&args(&["compare", "--data-dir", d, "--sync-dir", sd])).unwrap();
    assert_eq!(line(&out, "result="), "MATCH");
    assert_eq!(line(&out, "server_seq="), "0");
    assert_eq!(
        line(&out, "local_snapshot_sha256="),
        line(&out, "server_snapshot_sha256=")
    );
    assert!(!out.contains(&token));
    assert_eq!(tree(&root), before, "compare is read-only");

    // A local change the server has not seen.
    let dir = root.join("notebooks").join("notebook-1");
    let (mut store, _) = load_existing_page(&dir, "page-1").unwrap();
    store
        .append_committed_stroke(&Stroke {
            id: new_stroke_id(),
            points: vec![Point::new(1.0, 2.0), Point::new(3.0, 4.0)],
            brush: None,
            finished: true,
        })
        .unwrap();
    drop(store);
    let err = run(&args(&["compare", "--data-dir", d, "--sync-dir", sd])).unwrap_err();
    assert_eq!(line(&err, "result="), "DIFFER");
    assert!(!err.contains(&token));
    for p in [&root, &sync, &srv_dir] {
        let _ = fs::remove_dir_all(p);
    }
}

#[test]
fn remote_op_submits_every_kind_as_a_second_replica() {
    use scribe_app::sync_outbound::{OutboundSync, SyncConfigV1, CONFIG_FILE};
    use scribe_sync_server::{generate_token, ServerConfig, ServerHandle, Store, TokenVerifier};

    let root = tmp("rop-lib");
    let sync = tmp("rop-sync");
    let dev = tmp("rop-dev");
    let srv_dir = tmp("rop-srv");
    library(&root);
    let token = generate_token().unwrap();
    let srv = ServerHandle::start_with(
        "127.0.0.1:0".parse().unwrap(),
        Store::open(&srv_dir).unwrap(),
        ServerConfig {
            auth: Some(TokenVerifier::for_token(&token)),
            web_root: None,
            control: None,
        },
    )
    .unwrap();
    fs::create_dir_all(&sync).unwrap();
    let cfg = SyncConfigV1 {
        v: 1,
        server_base_url: srv.base_url(),
        token: token.clone(),
        debug_drop_first_ack: false,
        debug_crash_point: None,
        server_id: None,
    };
    fs::write(sync.join(CONFIG_FILE), serde_json::to_vec(&cfg).unwrap()).unwrap();
    let s = OutboundSync::open(&root, &sync).unwrap();
    let t0 = std::time::Instant::now();
    while !s.state().genesis_uploaded {
        assert!(t0.elapsed().as_secs() < 20);
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let kindle_replica = s.state().replica_id.to_string();
    s.shutdown(std::time::Duration::from_secs(3));

    let sd = sync.to_str().unwrap();
    let dd = dev.to_str().unwrap();
    let base = [
        "--sync-dir",
        sd,
        "--replica-state-dir",
        dd,
        "--notebook-index",
        "0",
    ];
    let op = |extra: &[&str]| -> Result<String, String> {
        let mut v: Vec<&str> = vec!["remote-op"];
        v.extend_from_slice(extra);
        v.extend_from_slice(&base);
        run(&args(&v))
    };
    assert!(op(&["add-stroke", "--page-index", "0"])
        .unwrap_err()
        .contains("--confirm-write"));
    let mut outs = Vec::new();
    for extra in [
        vec!["add-stroke", "--page-index", "0", "--confirm-write"],
        vec!["erase", "--page-index", "0", "--confirm-write"],
        vec!["delete-lineage", "--page-index", "0", "--confirm-write"],
        vec!["rename", "--title", "Remote Renamed", "--confirm-write"],
        vec!["create-page", "--confirm-write"],
        vec![
            "move-page",
            "--page-index",
            "1",
            "--first",
            "--confirm-write",
        ],
        vec!["delete-page", "--page-index", "0", "--confirm-write"],
    ] {
        outs.push(op(&extra).unwrap_or_else(|e| panic!("{extra:?}: {e}")));
    }
    for (i, o) in outs.iter().enumerate() {
        let field = |k: &str| {
            o.split_whitespace()
                .find_map(|kv| kv.strip_prefix(k))
                .unwrap_or_else(|| panic!("no {k} in {o}"))
                .to_string()
        };
        assert_eq!(field("client_seq="), (i + 1).to_string());
        assert_eq!(field("server_seq="), (i + 1).to_string());
        assert!(!o.contains(&kindle_replica), "a DIFFERENT replica");
        assert!(!o.contains(&token));
    }
    let state = fs::read_to_string(dev.join("dev-replica.json")).unwrap();
    assert!(state.contains("\"next_client_seq\": 8"));
    for p in [&root, &sync, &dev, &srv_dir] {
        let _ = fs::remove_dir_all(p);
    }
}
