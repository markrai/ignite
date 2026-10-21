//! Slice 19A: the `sync_startup_breakdown` line is coherent on the fast path, the full path, a
//! reconcile fault and a failed `open`, its counters match what was actually processed, and
//! collecting it writes nothing to the library or the sync directory.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use super::confirmed::{self, CHECKPOINT_DIR, CHECKPOINT_PAGES};
use super::confirmed_tests::{apply_all_staged, stage_remote};
use super::tests::{env, stop};
use super::*;
use crate::sync_startup_diag::{self as diag, field, take_last_line};

/// Open, and check the breakdown against the caller-side duration of the same call (the
/// runtime's `sync_reconcile_begin/end` interval minus its own work around `open`).
fn open_line(lib: &Path, sync: &Path) -> (OutboundSync, String) {
    let t = std::time::Instant::now();
    let s = OutboundSync::open(lib, sync).unwrap();
    let outer_ms = t.elapsed().as_secs_f64() * 1000.0;
    let line = take_last_line().expect("open emits a breakdown");
    let total = num(&line, "total_ms");
    // `total_ms` never exceeds the outer interval (0.05 = its rounding); the difference is the
    // call before `begin`, the line's formatting and its stdout write.
    assert!(total <= outer_ms + 0.05, "outer {outer_ms:.3} ms < {line}");
    println!(
        "diag_outer_check outer_ms={outer_ms:.3} total_ms={total} outside_ms={:.3} emit_format_ms={}",
        outer_ms - total,
        field(&line, "emit_format_ms").unwrap()
    );
    (s, line)
}

fn num(line: &str, key: &str) -> f64 {
    field(line, key)
        .unwrap_or_else(|| panic!("no {key} in {line}"))
        .parse()
        .unwrap_or_else(|_| panic!("{key} not numeric in {line}"))
}

/// Every phase/sub-timer the line names, and the accounting identity.
fn assert_coherent(line: &str) {
    let total = num(line, "total_ms");
    let attributed = num(line, "attributed_ms");
    let residual = num(line, "residual_ms");
    assert!(attributed <= total + 0.1, "{line}");
    assert!((total - attributed - residual).abs() <= 0.11, "{line}");
    // Structural, not a speed assertion: only the instants between `begin` and the first phase
    // and after the last are unattributed.
    assert!(residual <= (total * 0.05).max(1.0), "{line}");
    let phase_sum: f64 = line
        .split(' ')
        .filter_map(|kv| kv.split_once('='))
        .filter(|(k, _)| k.ends_with("_ms"))
        .filter(|(k, _)| {
            !matches!(
                *k,
                "total_ms"
                    | "attributed_ms"
                    | "residual_ms"
                    | "ckpt_other_ms"
                    | "emit_format_ms"
                    | "instr_overhead_est_ms"
            ) && !k.starts_with("ckpt_")
                && !k.starts_with("replay_")
                && !k.starts_with("validate_census")
                && !k.starts_with("stamp_census")
                && !k.starts_with("instr_")
        })
        .filter_map(|(_, v)| v.parse::<f64>().ok())
        .sum();
    // Printed values are rounded to 0.1 ms each.
    assert!(
        (phase_sum - attributed).abs() <= 0.1 * 40.0,
        "{phase_sum} vs {line}"
    );
}

/// (relative path → bytes) of every file under `root`.
fn tree(root: &Path) -> BTreeMap<String, Vec<u8>> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
        for e in fs::read_dir(dir).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(root, &p, out);
            } else {
                let rel = p.strip_prefix(root).unwrap().to_string_lossy().into_owned();
                out.insert(rel, fs::read(&p).unwrap());
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

#[test]
fn full_then_fast_path_breakdowns_are_coherent_and_distinguishable() {
    let e = env("diag-paths");
    let (s, full) = open_line(&e.lib, &e.sync);
    assert!(s.fault().is_none(), "{:?}", s.fault());
    stop(s);
    assert_eq!(field(&full, "outcome"), Some("ok"));
    assert_eq!(field(&full, "path"), Some("missing_stamp_fallback"));
    assert_eq!(field(&full, "last_phase"), Some("threads"));
    assert_eq!(field(&full, "failed_phase"), Some("-"));
    // A new epoch builds its genesis; the confirmed-state loaders do not run.
    assert_ne!(field(&full, "epoch_new_ms"), Some("-"));
    assert_eq!(field(&full, "checkpoint_ms"), Some("-"));
    assert_eq!(field(&full, "ckpt_pages"), Some("-"));
    for p in [
        "expected_ms",
        "identity_plan_ms",
        "snapshot_ms",
        "derive_ms",
        "proof_ms",
    ] {
        assert_ne!(field(&full, p), Some("-"), "{p}: {full}");
    }
    assert_eq!(field(&full, "fast_publish_ms"), Some("-"));
    assert_ne!(field(&full, "stamp_census_files"), Some("-"));
    assert_eq!(field(&full, "derived_ops"), Some("0"));
    assert_ne!(field(&full, "rss_kb_snapshot"), Some("-"));
    assert_coherent(&full);

    let (s, fast) = open_line(&e.lib, &e.sync);
    assert!(s.fault().is_none(), "{:?}", s.fault());
    stop(s);
    assert_eq!(field(&fast, "outcome"), Some("ok"));
    assert_eq!(field(&fast, "path"), Some("verified_fast_path"));
    assert_eq!(field(&fast, "epoch_new_ms"), Some("-"));
    // The fast path still pays for the confirmed-state load (Slice 19A measures it).
    assert_ne!(field(&fast, "checkpoint_ms"), Some("-"));
    assert_eq!(field(&fast, "ckpt_present"), Some("1"));
    assert_ne!(field(&fast, "replay_ms"), Some("-"));
    assert_ne!(field(&fast, "accepted_epoch_ms"), Some("-"));
    assert_ne!(field(&fast, "accepted_open_ms"), Some("-"));
    assert_ne!(field(&fast, "fast_publish_ms"), Some("-"));
    for p in [
        "expected_ms",
        "snapshot_ms",
        "derive_ms",
        "proof_ms",
        "stamp_ms",
    ] {
        assert_eq!(field(&fast, p), Some("-"), "{p}: {fast}");
    }
    assert!(num(&fast, "validate_census_files") >= 1.0, "{fast}");
    assert!(num(&fast, "validate_census_hashed") >= 1.0, "{fast}");
    assert_eq!(field(&fast, "stamp_census_files"), Some("-"));
    assert_eq!(field(&fast, "rss_kb_snapshot"), Some("-"));
    assert_ne!(field(&fast, "ckpt_other_ms"), Some("-"));
    assert_coherent(&fast);
}

#[test]
fn checkpoint_and_replay_counters_match_what_was_processed() {
    let e = env("diag-counters");
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    let replica = ReplicaId::from_uuid(Uuid::now_v7()).unwrap();
    let mut client = 0;
    stage_remote(&s, replica, &mut client, 3);
    apply_all_staged(&s);
    assert!(s.checkpoint_now().unwrap(), "a checkpoint is promoted");
    stage_remote(&s, replica, &mut client, 2);
    apply_all_staged(&s);
    stop(s);
    let _ = take_last_line();

    // Ground truth from disk: the manifest's page files and the log's records.
    let manifest: serde_json::Value = serde_json::from_slice(
        &fs::read(
            e.sync
                .join(CHECKPOINT_DIR)
                .join(confirmed::CHECKPOINT_MANIFEST),
        )
        .unwrap(),
    )
    .unwrap();
    let pages_dir = e.sync.join(CHECKPOINT_DIR).join(CHECKPOINT_PAGES);
    let (mut pages, mut bytes) = (0u64, 0u64);
    for n in manifest["notebooks"].as_array().unwrap() {
        for p in n["pages"].as_array().unwrap() {
            pages += 1;
            bytes += fs::metadata(pages_dir.join(p["sha256"].as_str().unwrap()))
                .unwrap()
                .len();
        }
    }
    let mut log_records = 0u64;
    let mut log_payload = 0u64;
    confirmed::scan_log(&e.sync.join(CONFIRMED_LOG_FILE), |_, p| {
        log_records += 1;
        log_payload += p.len() as u64;
        Ok(())
    })
    .unwrap();
    assert_eq!(log_records, 2, "only the post-checkpoint records remain");

    let (s, line) = open_line(&e.lib, &e.sync);
    stop(s);
    assert_eq!(num(&line, "ckpt_pages") as u64, pages, "{line}");
    assert_eq!(num(&line, "ckpt_page_bytes") as u64, bytes, "{line}");
    assert!(num(&line, "ckpt_manifest_bytes") > 0.0, "{line}");
    assert_eq!(num(&line, "replay_records") as u64, log_records, "{line}");
    assert_eq!(num(&line, "replay_applied") as u64, log_records, "{line}");
    assert_eq!(
        num(&line, "replay_payload_bytes") as u64,
        log_payload,
        "{line}"
    );
    // Both accepted-history loads ran; the bridge is empty for a foreign-only history.
    assert_eq!(field(&line, "accepted_records"), Some("0"), "{line}");
    assert_eq!(field(&line, "inbox_pending"), Some("0"), "{line}");
    assert_coherent(&line);
}

#[test]
fn a_failed_open_still_reports_where_it_stopped() {
    let e = env("diag-fail");
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    stop(s);
    let _ = take_last_line();
    // Disposable fixture: a cursor beyond the confirmed state is refused by `establish_epoch`.
    let state_path = e.sync.join(STATE_FILE);
    let mut st: serde_json::Value =
        serde_json::from_slice(&fs::read(&state_path).unwrap()).unwrap();
    st["server_cursor"] = serde_json::json!(st["server_cursor"].as_u64().unwrap() + 5);
    fs::write(&state_path, serde_json::to_vec(&st).unwrap()).unwrap();

    let err = OutboundSync::open(&e.lib, &e.sync).err().expect("refused");
    assert!(err.contains("sync state inconsistent"), "{err}");
    let line = take_last_line().expect("a failed open still emits a breakdown");
    assert_eq!(field(&line, "outcome"), Some("err"));
    assert_eq!(field(&line, "failed_phase"), Some("cursor"));
    assert_eq!(field(&line, "last_phase"), Some("ckpt_gc"));
    assert_eq!(field(&line, "path"), Some("-"));
    assert_eq!(field(&line, "ckpt_present"), Some("1"));
    assert_ne!(field(&line, "checkpoint_ms"), Some("-"));
    assert_eq!(field(&line, "validate_ms"), Some("-"));
    assert_eq!(field(&line, "rss_kb_end"), Some("-"));
    assert_eq!(field(&line, "rss_kb_epoch"), Some("-"), "not reached");
    assert_coherent(&line);

    // A configuration refusal fails in the first phase.
    let inside = e.lib.join("nested-sync");
    assert!(OutboundSync::open(&e.lib, &inside).is_err());
    let line = take_last_line().unwrap();
    assert_eq!(field(&line, "outcome"), Some("err"));
    assert_eq!(field(&line, "failed_phase"), Some("pre"));
    assert_eq!(field(&line, "last_phase"), Some("-"));
}

#[test]
fn a_reconcile_fault_is_reported_with_its_phase() {
    let e = env("diag-fault");
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    stop(s);
    let _ = take_last_line();
    // Disposable fixture: an unparsable durable outbox record faults reconcile (capture
    // continues, upload halts) — unchanged behaviour, now attributed.
    fs::write(e.sync.join(OUTBOX_FILE), b"{not json}\n").unwrap();
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    let line = take_last_line().unwrap();
    assert!(s.fault().is_some());
    stop(s);
    assert_eq!(field(&line, "outcome"), Some("fault"));
    assert_eq!(field(&line, "fault_phase"), Some("outbox"));
    assert_eq!(field(&line, "path"), Some("fault_divergence_fallback"));
    assert_eq!(field(&line, "last_phase"), Some("threads"));
    assert_eq!(num(&line, "outbox_file_bytes") as u64, 11);
    assert_eq!(field(&line, "validate_ms"), Some("-"));
    assert_coherent(&line);
}

#[test]
fn collecting_diagnostics_writes_nothing() {
    let e = env("diag-readonly");
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    let replica = ReplicaId::from_uuid(Uuid::now_v7()).unwrap();
    let mut client = 0;
    stage_remote(&s, replica, &mut client, 2);
    apply_all_staged(&s);
    stop(s);
    let _ = take_last_line();
    let (lib0, sync0) = (tree(&e.lib), tree(&e.sync));
    // Every instrumented loader, under an active collector, plus memory sampling.
    let d = diag::begin();
    diag::enter("checkpoint");
    let (mut state, through, _) = confirmed::load_checkpoint(&e.sync).unwrap().unwrap();
    diag::enter("replay");
    let lib_id = state.snapshot.library_id;
    let decode = |b: &[u8]| inbound::canonical_envelope(b, lib_id);
    confirmed::replay_log(&e.sync, &mut state, through, &decode).unwrap();
    diag::enter("accepted_open");
    let st: SyncStateV1 = read_json(&e.sync.join(STATE_FILE)).unwrap();
    load_accepted(&e.sync, st.library_id, st.replica_id, st.server_cursor).unwrap();
    load_inbox(&e.sync, st.library_id, st.server_cursor).unwrap();
    diag::mem("epoch");
    d.finish();
    let line = take_last_line().unwrap();
    assert_eq!(num(&line, "replay_applied"), 2.0, "{line}");
    assert_eq!(tree(&e.lib), lib0, "library unchanged");
    assert_eq!(tree(&e.sync), sync0, "sync dir unchanged");
}

// ------------------------------------------------------------------ Slice 19B.1: supplied geometry

/// The page-derived geometry of the `env` fixture (its pages are created from `tests::geo()`).
fn fixture_geometry() -> DeviceGeometry {
    let g = super::tests::geo();
    DeviceGeometry {
        width: g.xres,
        height: g.yres,
        line_length: g.line_length,
    }
}

fn device_of(s: &OutboundSync) -> Option<DeviceGeometry> {
    s.shared.lock().unwrap().device
}

#[test]
fn supplied_geometry_is_installed_and_skips_the_page_probe() {
    let e = env("geom-supplied");
    // Deliberately different from the pages' geometry, so its source is unambiguous.
    let panel = DeviceGeometry {
        width: 1860,
        height: 2480,
        line_length: 1872,
    };
    let s = OutboundSync::open_with_device(&e.lib, &e.sync, Some(panel)).unwrap();
    let line = take_last_line().unwrap();
    assert_eq!(device_of(&s), Some(panel));
    stop(s);
    assert_eq!(field(&line, "device_geom_probe"), Some("0"), "{line}");
    assert_eq!(field(&line, "device_geom_rejected"), Some("-"), "{line}");
    assert_ne!(
        field(&line, "device_geom_ms"),
        Some("-"),
        "phase still reported: {line}"
    );
}

#[test]
fn open_keeps_the_page_derived_fallback() {
    let e = env("geom-fallback");
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    let line = take_last_line().unwrap();
    assert_eq!(device_of(&s), Some(fixture_geometry()));
    stop(s);
    assert_eq!(field(&line, "device_geom_probe"), Some("1"), "{line}");
    let s = OutboundSync::open_with_device(&e.lib, &e.sync, None).unwrap();
    let line = take_last_line().unwrap();
    assert_eq!(device_of(&s), Some(fixture_geometry()), "None == open()");
    stop(s);
    assert_eq!(field(&line, "device_geom_probe"), Some("1"), "{line}");
}

#[test]
fn implausible_supplied_geometry_is_rejected_for_the_probe() {
    let e = env("geom-implausible");
    for bad in [
        DeviceGeometry {
            width: 0,
            height: 160,
            line_length: 208,
        },
        DeviceGeometry {
            width: 200,
            height: 0,
            line_length: 208,
        },
        DeviceGeometry {
            width: 200,
            height: 160,
            line_length: 199,
        },
    ] {
        let s = OutboundSync::open_with_device(&e.lib, &e.sync, Some(bad)).unwrap();
        let line = take_last_line().unwrap();
        assert_eq!(
            device_of(&s),
            Some(fixture_geometry()),
            "{bad:?} must not be used"
        );
        stop(s);
        assert_eq!(field(&line, "device_geom_rejected"), Some("1"), "{line}");
        assert_eq!(field(&line, "device_geom_probe"), Some("1"), "{line}");
    }
}

#[test]
fn supplied_geometry_changes_no_startup_decision_or_file() {
    // Twin fixtures: the same history opened with and without supplied geometry must take the
    // same reconcile paths and leave identical sync and library trees (modulo random ids, so
    // compare per fixture across a reopen rather than across fixtures).
    for supply in [false, true] {
        let e = env(if supply { "geom-same-a" } else { "geom-same-b" });
        let open = |e: &super::tests::Env| {
            if supply {
                OutboundSync::open_with_device(&e.lib, &e.sync, Some(fixture_geometry())).unwrap()
            } else {
                OutboundSync::open(&e.lib, &e.sync).unwrap()
            }
        };
        let s = open(&e);
        let first = take_last_line().unwrap();
        stop(s);
        let (lib0, sync0) = (tree(&e.lib), tree(&e.sync));
        let s = open(&e);
        let second = take_last_line().unwrap();
        stop(s);
        assert_eq!(
            field(&first, "path"),
            Some("missing_stamp_fallback"),
            "{first}"
        );
        assert_eq!(
            field(&second, "path"),
            Some("verified_fast_path"),
            "{second}"
        );
        assert_eq!(
            tree(&e.lib),
            lib0,
            "verified reopen leaves the library unchanged"
        );
        let mut sync1 = tree(&e.sync);
        let mut sync0 = sync0;
        for t in [&mut sync0, &mut sync1] {
            t.remove("sync.lock");
        }
        assert_eq!(sync1, sync0, "verified reopen leaves sync state unchanged");
    }
}
