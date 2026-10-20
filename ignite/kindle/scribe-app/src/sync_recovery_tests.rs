//! Import-fault recovery: journal batching, bulk-import backpressure, and startup recovery of
//! history lost while sync was faulted. Also hosts the copied-device dry run (`#[ignore]`,
//! driven by `SCRIBE_RECOVERY_SRC`), which never opens the source for writing.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, Instant};

use notebook_core::{new_stroke_id, Point, Stroke};
use sync_protocol::snapshot_sha256;

use super::tests::{
    append_stroke, env, geo, local_snapshot, nb_dir, pending_seqs, replay_local, server, stop,
    wait_until, write, write_config, Env,
};
use super::*;
use crate::pressure_curve::STYLE_PRESSURE_WIDTH_V1;
use crate::store::{load_existing_page, PageStore, PersistedReplacementV2};
use crate::sync_local::build_snapshot;

// ------------------------------------------------------------------ copied-device dry run

fn copy_tree(src: &Path, dst: &Path) {
    fs::create_dir_all(dst).unwrap();
    for e in fs::read_dir(src).unwrap() {
        let e = e.unwrap();
        let to = dst.join(e.file_name());
        if e.file_type().unwrap().is_dir() {
            copy_tree(&e.path(), &to);
        } else {
            fs::copy(e.path(), &to).unwrap();
        }
    }
}

/// Expected = ServerBaseState + accepted bridge + committed outbox (what reconcile replays).
fn expected_state(sync_dir: &Path) -> (SyncStateV1, LibraryState, ParsedJournal) {
    let state: SyncStateV1 = read_json(&sync_dir.join(STATE_FILE)).unwrap();
    let (base, _) = read_confirmed_state(sync_dir, state.library_id).unwrap();
    let accepted = load_accepted(
        sync_dir,
        state.library_id,
        state.replica_id,
        state.server_cursor,
    )
    .unwrap();
    let parsed = parse_journal(&fs::read(sync_dir.join(OUTBOX_FILE)).unwrap()).unwrap();
    let mut overlay: Vec<PendingOp> = accepted.iter().map(AcceptedOp::as_pending).collect();
    overlay.extend(
        parsed
            .committed
            .iter()
            .filter(|o| o.seq > state.acked_client_seq)
            .cloned(),
    );
    let expected = apply_all(&base, &overlay).unwrap();
    (state, expected, parsed)
}

fn describe_diff(expected: &SyncSnapshotV1, actual: &SyncSnapshotV1, identity: &IdentityFileV1) {
    let local_nb = |id: NotebookId| {
        identity
            .notebooks
            .iter()
            .find(|(_, g)| **g == id)
            .map_or_else(|| id.to_string(), |(l, _)| l.clone())
    };
    for en in &expected.notebooks {
        if !actual
            .notebooks
            .iter()
            .any(|a| a.notebook_id == en.notebook_id)
        {
            println!(
                "  GONE locally: {} ({} pages)",
                local_nb(en.notebook_id),
                en.pages.len()
            );
        }
    }
    for an in &actual.notebooks {
        let strokes: usize = an.pages.iter().map(|p| p.strokes.len()).sum();
        let Some(en) = expected
            .notebooks
            .iter()
            .find(|e| e.notebook_id == an.notebook_id)
        else {
            println!(
                "  NEW locally: {} {:?} pages={} strokes={}",
                local_nb(an.notebook_id),
                an.title,
                an.pages.len(),
                strokes
            );
            continue;
        };
        if en == an {
            continue;
        }
        println!("  CHANGED: {} {:?}", local_nb(an.notebook_id), an.title);
        if en.title != an.title {
            println!("    title {:?} -> {:?}", en.title, an.title);
        }
        let ep: Vec<_> = en.pages.iter().map(|p| p.page_id).collect();
        let ap: Vec<_> = an.pages.iter().map(|p| p.page_id).collect();
        if ep != ap {
            let gone = ep.iter().filter(|p| !ap.contains(p)).count();
            let new = ap.iter().filter(|p| !ep.contains(p)).count();
            let kept_e: Vec<_> = ep.iter().filter(|p| ap.contains(p)).collect();
            let kept_a: Vec<_> = ap.iter().filter(|p| ep.contains(p)).collect();
            println!(
                "    pages {} -> {}: removed={gone} added={new} reordered={}",
                ep.len(),
                ap.len(),
                kept_e != kept_a
            );
        }
        for a in &an.pages {
            let Some(e) = en.pages.iter().find(|p| p.page_id == a.page_id) else {
                println!("    + page {} strokes={}", a.page_id, a.strokes.len());
                continue;
            };
            if e == a {
                continue;
            }
            let prefix = e.strokes.len() <= a.strokes.len()
                && e.strokes.iter().zip(&a.strokes).all(|(x, y)| x == y);
            println!(
                "    ~ page {} strokes {} -> {} append_only={prefix} bg_same={} render_same={}",
                a.page_id,
                e.strokes.len(),
                a.strokes.len(),
                e.background == a.background,
                e.render == a.render
            );
        }
    }
}

/// Phase timings of startup reconciliation on an already-recovered copy
/// (`SCRIBE_RECOVERY_RUN=<dry-run temp dir>`); read-only.
#[test]
#[ignore = "manual: needs SCRIBE_RECOVERY_RUN (a recovered dry-run copy)"]
fn recovery_profile_reconcile_phases() {
    let run = PathBuf::from(std::env::var("SCRIBE_RECOVERY_RUN").expect("SCRIBE_RECOVERY_RUN"));
    let (lib, sync) = (run.join("scribe-data"), run.join("sync"));
    let t = std::time::Instant::now();
    let raw = fs::read(sync.join(OUTBOX_FILE)).unwrap();
    println!(
        "read outbox {} MB: {} ms",
        raw.len() >> 20,
        t.elapsed().as_millis()
    );
    let t = std::time::Instant::now();
    let parsed = parse_journal(&raw).unwrap();
    println!(
        "parse_journal {} ops: {} ms",
        parsed.committed.len(),
        t.elapsed().as_millis()
    );
    let state: SyncStateV1 = read_json(&sync.join(STATE_FILE)).unwrap();
    let t = std::time::Instant::now();
    let base: BaseStateV2 = read_json(&sync.join(BASE_FILE)).unwrap();
    let base = LibraryState {
        snapshot: base.snapshot,
        history: base.history,
    };
    println!("read base: {} ms", t.elapsed().as_millis());
    let t = std::time::Instant::now();
    let accepted = load_accepted(
        &sync,
        state.library_id,
        state.replica_id,
        state.server_cursor,
    )
    .unwrap();
    println!(
        "load_accepted {}: {} ms",
        accepted.len(),
        t.elapsed().as_millis()
    );
    let mut overlay: Vec<PendingOp> = accepted.iter().map(AcceptedOp::as_pending).collect();
    overlay.extend(parsed.committed.iter().cloned());
    let t = std::time::Instant::now();
    let expected = apply_all(&base, &overlay).unwrap();
    println!(
        "apply_all {} ops: {} ms",
        overlay.len(),
        t.elapsed().as_millis()
    );
    let t = std::time::Instant::now();
    let (_, plan) = crate::sync_local::plan_sync_identity(&lib).unwrap();
    let actual = build_snapshot(&lib, &plan.identity).unwrap();
    println!("identity + build_snapshot: {} ms", t.elapsed().as_millis());
    let t = std::time::Instant::now();
    let trashed = |_: NotebookId, _: PageId| false;
    let ops = derive_lost_ops(&expected.snapshot, &actual, &trashed).unwrap();
    println!(
        "derive ({} ops) + compare: {} ms",
        ops.len(),
        t.elapsed().as_millis()
    );
}

/// Base-state persistence cost on a real ServerBaseState (`SCRIBE_RECOVERY_RUN`), both paths.
#[test]
#[ignore = "manual: needs SCRIBE_RECOVERY_RUN (a recovered dry-run copy)"]
fn recovery_profile_base_state_write() {
    let run = PathBuf::from(std::env::var("SCRIBE_RECOVERY_RUN").expect("SCRIBE_RECOVERY_RUN"));
    let base = read_base_state(&run.join("sync").join(BASE_FILE)).unwrap();
    let state = LibraryState {
        snapshot: base.snapshot,
        history: base.history,
    };
    let out = std::env::temp_dir().join("scribe-base-write-profile");
    let t = Instant::now();
    write_base_state(&out, 1, &state).unwrap();
    println!("streamed write_base_state: {} ms", t.elapsed().as_millis());
    let t = Instant::now();
    let bytes = to_canonical_json(&BaseStateRef {
        v: 2,
        through_server_seq: 1,
        snapshot: &state.snapshot,
        history: &state.history,
    })
    .unwrap();
    println!(
        "to_vec only ({} MB): {} ms",
        bytes.len() >> 20,
        t.elapsed().as_millis()
    );
    let t = Instant::now();
    write_atomic(&out, "x.json", &bytes).unwrap();
    println!("buffered write_atomic: {} ms", t.elapsed().as_millis());
}

/// Canonical snapshot hash + duplicate census of a copied library (`SCRIBE_RECOVERY_RUN`,
/// `scribe-data/` inside); read-only.
#[test]
#[ignore = "manual: needs SCRIBE_RECOVERY_RUN"]
fn recovery_local_snapshot_census() {
    let run = PathBuf::from(std::env::var("SCRIBE_RECOVERY_RUN").expect("SCRIBE_RECOVERY_RUN"));
    let lib = run.join("scribe-data");
    let snap = build_snapshot(&lib, &read_identity(&lib).unwrap().unwrap()).unwrap();
    println!(
        "local_snapshot_sha256={}",
        sync_protocol::snapshot_sha256(&snap).unwrap()
    );
    let strokes: Vec<&SyncStroke> = snap
        .notebooks
        .iter()
        .flat_map(|n| &n.pages)
        .flat_map(|p| &p.strokes)
        .collect();
    let mut geom = std::collections::HashMap::new();
    for s in &strokes {
        *geom
            .entry(serde_json::to_string(&s.points).unwrap())
            .or_insert(0usize) += 1;
    }
    println!(
        "notebooks={} pages={} strokes={} identical_geometry_repeats={}",
        snap.notebooks.len(),
        snap.notebooks.iter().map(|n| n.pages.len()).sum::<usize>(),
        strokes.len(),
        geom.values()
            .filter(|c| **c > 1)
            .map(|c| c - 1)
            .sum::<usize>()
    );
}

/// Files (relative path → (len, mtime)) under `root`.
fn file_stamps(root: &Path) -> std::collections::BTreeMap<PathBuf, (u64, std::time::SystemTime)> {
    let mut out = std::collections::BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(&d).unwrap().flatten() {
            let m = e.metadata().unwrap();
            if m.is_dir() {
                stack.push(e.path());
            } else {
                out.insert(e.path(), (m.len(), m.modified().unwrap()));
            }
        }
    }
    out
}

/// Upper bound of bytes written: sizes of files created or modified since `before`.
fn bytes_written(
    before: &std::collections::BTreeMap<PathBuf, (u64, std::time::SystemTime)>,
    root: &Path,
) -> (u64, Vec<String>) {
    let mut total = 0;
    let mut files = Vec::new();
    for (p, (len, t)) in file_stamps(root) {
        if before.get(&p).is_none_or(|b| b.1 != t || b.0 != len) {
            let delta = before.get(&p).map_or(len, |b| {
                // Appended files count only their growth (page journals, the confirmed log).
                if len >= b.0 {
                    len - b.0
                } else {
                    len
                }
            });
            total += delta;
            files.push(format!(
                "{}={}",
                p.strip_prefix(root).unwrap().display(),
                delta
            ));
        }
    }
    (total, files)
}

/// Inbound cost of remote strokes on a copy of real device state
/// (`SCRIBE_RECOVERY_SRC=<dir with scribe-data/ and scribe-app/state/sync/>`, only read).
/// `SCRIBE_PROFILE_BATCHES="1,50,500"` (default) = remote strokes per measured transaction.
#[test]
#[ignore = "manual: needs SCRIBE_RECOVERY_SRC"]
fn recovery_profile_inbound_remote_strokes() {
    let src = PathBuf::from(std::env::var("SCRIBE_RECOVERY_SRC").expect("SCRIBE_RECOVERY_SRC"));
    let run = std::env::temp_dir().join(format!(
        "scribe-inbound-profile-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis()
    ));
    let (lib, sync) = (run.join("scribe-data"), run.join("sync"));
    copy_tree(&src.join("scribe-data"), &lib);
    copy_tree(&src.join("scribe-app/state/sync"), &sync);
    let batches: Vec<usize> = std::env::var("SCRIBE_PROFILE_BATCHES")
        .unwrap_or_else(|_| "1,50,500".into())
        .split(',')
        .map(|x| x.trim().parse().unwrap())
        .collect();
    let t = Instant::now();
    let s = OutboundSync::open(&lib, &sync).unwrap();
    println!("open ms={} fault={:?}", t.elapsed().as_millis(), s.fault());
    assert!(s.fault().is_none());
    let (library_id, nb, page) = {
        let g = s.shared.lock().unwrap();
        let n = &g.base.snapshot.notebooks[0];
        (g.state.library_id, n.notebook_id, n.pages[0].clone())
    };
    let replica = ReplicaId::from_uuid(Uuid::now_v7()).unwrap();
    let mut client_seq = 0u64;
    for k in batches {
        {
            let mut g = s.shared.lock().unwrap();
            let mut seq = g.state.server_cursor;
            for i in 0..k {
                client_seq += 1;
                seq += 1;
                let id = StrokeId::from_uuid(Uuid::now_v7()).unwrap();
                let y = 500.0 + 37.0 * (client_seq % 500) as f32;
                let env = SyncEnvelopeV1 {
                    v: ENVELOPE_VERSION,
                    operation_id: OperationId::from_uuid(Uuid::now_v7()).unwrap(),
                    library_id,
                    replica_id: replica,
                    client_seq,
                    operation: SyncOperationV1::AddStroke {
                        notebook_id: nb,
                        page_id: page.page_id,
                        stroke: SyncStroke {
                            stroke_id: id,
                            root_id: id,
                            style: StrokeStyle::LegacyFixedV1,
                            points: (0..40)
                                .map(|j| {
                                    SyncPoint::new(800.0 + 150.0 * j as f32, y + i as f32, None)
                                })
                                .collect(),
                        },
                    },
                };
                let bytes = env.to_canonical_bytes().unwrap();
                g.inbox.push(inbound::InboxItem {
                    server_seq: seq,
                    envelope: env,
                    bytes,
                });
            }
        }
        let before = file_stamps(&run);
        let t = Instant::now();
        let mut tx = 0;
        let mut phases = inbound::ApplyTimings::default();
        while s.inbound_ready() {
            let r = s.apply_inbound().unwrap().expect("applied");
            tx += 1;
            phases.reducer_us += r.timings.reducer_us;
            phases.rebase_us += r.timings.rebase_us;
            phases.materialize_us += r.timings.materialize_us;
            phases.verify_us += r.timings.verify_us;
            phases.persist_us += r.timings.persist_us;
        }
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        let (written, files) = bytes_written(&before, &run);
        println!(
            "remote strokes={k}: transactions={tx} main_thread_ms={ms:.1} ops_per_s={:.1} bytes_written={written} \
             reducer_ms={} rebase_ms={} materialize_ms={} verify_ms={} persist_ms={}",
            k as f64 / (ms / 1000.0),
            phases.reducer_us / 1000,
            phases.rebase_us / 1000,
            phases.materialize_us / 1000,
            phases.verify_us / 1000,
            phases.persist_us / 1000
        );
        println!("  files: {}", files.join(" "));
        assert!(s.fault().is_none(), "{:?}", s.fault());
    }
    let stage = |s: &OutboundSync, client_seq: &mut u64, n: usize, y0: f32| {
        let mut g = s.shared.lock().unwrap();
        let mut seq = g
            .inbox
            .last()
            .map_or(g.state.server_cursor, |i| i.server_seq);
        for i in 0..n {
            *client_seq += 1;
            seq += 1;
            let id = StrokeId::from_uuid(Uuid::now_v7()).unwrap();
            let env = SyncEnvelopeV1 {
                v: ENVELOPE_VERSION,
                operation_id: OperationId::from_uuid(Uuid::now_v7()).unwrap(),
                library_id,
                replica_id: replica,
                client_seq: *client_seq,
                operation: SyncOperationV1::AddStroke {
                    notebook_id: nb,
                    page_id: page.page_id,
                    stroke: SyncStroke {
                        stroke_id: id,
                        root_id: id,
                        style: StrokeStyle::LegacyFixedV1,
                        points: (0..40)
                            .map(|j| {
                                SyncPoint::new(
                                    800.0 + 150.0 * j as f32,
                                    y0 + (i % 900) as f32,
                                    None,
                                )
                            })
                            .collect(),
                    },
                },
            };
            let bytes = env.to_canonical_bytes().unwrap();
            g.inbox.push(inbound::InboxItem {
                server_seq: seq,
                envelope: env,
                bytes,
            });
        }
    };
    // A checkpoint of the realistic state, with inbound traffic applied while it is written.
    let shared = Arc::clone(&s.shared);
    let t = Instant::now();
    let ck = std::thread::spawn(move || run_checkpoint(&shared, true));
    let mut during = Vec::new();
    while !ck.is_finished() {
        stage(&s, &mut client_seq, 10, 900.0);
        let t1 = Instant::now();
        while s.inbound_ready() {
            s.apply_inbound().unwrap();
        }
        during.push(t1.elapsed().as_millis());
    }
    let r = ck.join().unwrap();
    println!(
        "checkpoint {:?}: wall_ms={} capture_lock_us={} | while it ran: {} x 10 remote strokes, per-transaction ms {:?}",
        r,
        t.elapsed().as_millis(),
        s.stats().checkpoint_capture_max_us,
        during.len(),
        during
    );
    println!(
        "confirmed (checkpoint_through, log records) = {:?}",
        s.confirmed_state()
    );
    // Sustained inbound, then startup replay of the resulting log.
    stage(&s, &mut client_seq, 5000, 1500.0);
    let t = Instant::now();
    while s.inbound_ready() {
        s.apply_inbound().unwrap();
    }
    let ms = t.elapsed().as_secs_f64() * 1000.0;
    println!(
        "sustained: 5000 remote strokes in {ms:.0} ms = {:.0} ops/s",
        5000.0 / (ms / 1000.0)
    );
    println!(
        "confirmed (checkpoint_through, log records) = {:?}",
        s.confirmed_state()
    );
    s.shutdown(Duration::from_secs(30));
    let t = Instant::now();
    let s = OutboundSync::open(&lib, &sync).unwrap();
    println!(
        "reopen (checkpoint load + log replay) ms={} fault={:?}",
        t.elapsed().as_millis(),
        s.fault()
    );
    s.shutdown(Duration::from_secs(5));
    let _ = fs::remove_dir_all(&run);
}

/// Just the startup path on an already-recovered copy (for timing / peak-memory measurement).
#[test]
#[ignore = "manual: needs SCRIBE_RECOVERY_RUN (a recovered dry-run copy)"]
fn recovery_open_recovered_copy() {
    let run = PathBuf::from(std::env::var("SCRIBE_RECOVERY_RUN").expect("SCRIBE_RECOVERY_RUN"));
    let t = std::time::Instant::now();
    let s = OutboundSync::open(&run.join("scribe-data"), &run.join("sync")).unwrap();
    println!(
        "open ms={} fault={:?} pending={}",
        t.elapsed().as_millis(),
        s.fault(),
        s.pending()
    );
    s.shutdown(std::time::Duration::from_secs(5));
}

/// Dry run against a copy of real device state:
/// `SCRIBE_RECOVERY_SRC=<dir with scribe-data/ and scribe-app/state/sync/>`.
/// The source is only read; everything happens in a fresh temp copy.
#[test]
#[ignore = "manual: needs SCRIBE_RECOVERY_SRC (copied device state)"]
fn recovery_dry_run_on_copied_device_state() {
    let src = PathBuf::from(std::env::var("SCRIBE_RECOVERY_SRC").expect("SCRIBE_RECOVERY_SRC"));
    let run = std::env::temp_dir().join(format!(
        "scribe-recovery-dryrun-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis()
    ));
    let lib = run.join("scribe-data");
    let sync = run.join("sync");
    copy_tree(&src.join("scribe-data"), &lib);
    copy_tree(&src.join("scribe-app/state/sync"), &sync);
    println!("dry run copy: {}", run.display());

    let (state, expected, parsed) = expected_state(&sync);
    println!(
        "before: acked={} next={} server_cursor={} outbox_committed={} trailing_prep={:?} torn={}",
        state.acked_client_seq,
        state.next_client_seq,
        state.server_cursor,
        parsed.committed.len(),
        parsed
            .trailing_prep
            .as_ref()
            .map(|p| (p.seq, op_kind(&p.envelope.operation))),
        parsed.torn_tail
    );
    let (_, plan) = crate::sync_local::plan_sync_identity(&lib).unwrap();
    let actual = build_snapshot(&lib, &plan.identity).unwrap();
    println!(
        "expected notebooks={} actual notebooks={}",
        expected.snapshot.notebooks.len(),
        actual.notebooks.len()
    );
    describe_diff(&expected.snapshot, &actual, &plan.identity);
    let old_outbox = fs::read(sync.join(OUTBOX_FILE)).unwrap();
    let old_identity = fs::read(lib.join(".scribe-sync/identity.json")).unwrap();

    // The real startup path (offline: no config.json → nothing leaves this machine).
    assert!(!sync.join(CONFIG_FILE).exists());
    let t0 = std::time::Instant::now();
    let s = OutboundSync::open(&lib, &sync).unwrap();
    println!("open+reconcile ms={}", t0.elapsed().as_millis());
    let fault = s.fault();
    let st = s.state();
    let pending = s.pending();
    s.shutdown(std::time::Duration::from_secs(5));
    if let Some(f) = fault {
        assert_eq!(
            fs::read(lib.join(".scribe-sync/identity.json")).unwrap(),
            old_identity,
            "a refused recovery must not publish identities"
        );
        assert_eq!(fs::read(sync.join(OUTBOX_FILE)).unwrap(), old_outbox);
        panic!("RECOVERY REFUSED (nothing written): {f}");
    }
    println!(
        "after: acked={} next_client_seq={} pending={pending}",
        st.acked_client_seq, st.next_client_seq
    );

    // Every previously journaled envelope is preserved byte-for-byte at its client_seq.
    let new_parsed = parse_journal(&fs::read(sync.join(OUTBOX_FILE)).unwrap()).unwrap();
    assert!(new_parsed.trailing_prep.is_none() && !new_parsed.torn_tail);
    let by_seq: std::collections::BTreeMap<u64, &PendingOp> =
        new_parsed.committed.iter().map(|o| (o.seq, o)).collect();
    let mut old_ops: Vec<&PendingOp> = parsed
        .committed
        .iter()
        .filter(|o| o.seq > state.acked_client_seq)
        .collect();
    if let Some(p) = &parsed.trailing_prep {
        if by_seq.get(&p.seq).is_some_and(|n| n.bytes == p.bytes) {
            println!("trailing prep {} resolved as committed (same bytes)", p.seq);
            old_ops.push(p);
        } else {
            println!("trailing prep {} resolved as aborted", p.seq);
        }
    }
    for o in &old_ops {
        let n = by_seq.get(&o.seq).expect("existing op dropped");
        assert_eq!(n.bytes, o.bytes, "existing op {} changed", o.seq);
    }
    let first_new = old_ops
        .last()
        .map_or(state.acked_client_seq + 1, |o| o.seq + 1);
    let derived: Vec<&PendingOp> = new_parsed
        .committed
        .iter()
        .filter(|o| o.seq >= first_new)
        .collect();
    let mut kinds: std::collections::BTreeMap<&str, u64> = Default::default();
    for o in &derived {
        *kinds.entry(op_kind(&o.envelope.operation)).or_default() += 1;
    }
    println!(
        "preserved existing ops={} derived ops={} (client_seq {first_new}..) {kinds:?}",
        old_ops.len(),
        derived.len()
    );

    // Replay proof, independently of reconcile: base + accepted + journal == local files.
    let (_, replayed, _) = expected_state(&sync);
    let identity = read_identity(&lib).unwrap().unwrap();
    let local = build_snapshot(&lib, &identity).unwrap();
    assert!(replayed.snapshot == local, "replay != local after recovery");
    for n in &local.notebooks {
        let strokes: usize = n.pages.iter().map(|p| p.strokes.len()).sum();
        let local_id = identity
            .notebooks
            .iter()
            .find(|(_, g)| **g == n.notebook_id)
            .map(|(l, _)| l.as_str())
            .unwrap_or("?");
        println!(
            "  replay == local: {local_id} {:?} pages={} strokes={strokes}",
            n.title,
            n.pages.len()
        );
    }

    // Idempotent: a second start derives nothing and rewrites nothing semantic.
    let outbox_after = fs::read(sync.join(OUTBOX_FILE)).unwrap();
    let s = OutboundSync::open(&lib, &sync).unwrap();
    assert!(s.fault().is_none(), "{:?}", s.fault());
    assert_eq!(s.state().next_client_seq, st.next_client_seq);
    s.shutdown(std::time::Duration::from_secs(5));
    assert_eq!(fs::read(sync.join(OUTBOX_FILE)).unwrap(), outbox_after);
    println!("second open: no new derivation, outbox unchanged");
}

// ------------------------------------------------------------------ journal durability

fn gated(s: &OutboundSync) -> Arc<JournalGate> {
    let gate = Arc::new(JournalGate::default());
    gate.close();
    s.set_journal_gate(Some(Arc::clone(&gate)));
    gate
}

fn wait_for(what: &str, mut f: impl FnMut() -> bool) {
    wait_until(what, Duration::from_secs(20), &mut f);
}

#[test]
fn a_durability_barrier_publishes_exactly_the_records_it_covers() {
    let e = env("barrier");
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    let gate = gated(&s);
    // Record 1 is written and held at its barrier: nothing may be eligible for upload yet.
    write(
        &s,
        &e.lib,
        "notebook-1",
        "page-1",
        &[(1.0, 1.0), (9.0, 9.0)],
    );
    gate.wait_held();
    assert_eq!(s.pending(), 0, "written but not durable ⇒ not eligible");
    assert_eq!(s.stats().journal_records_durable, 0);
    // Records 2–5 queue behind it.
    for i in 0..4 {
        let y = 100.0 * (i + 2) as f32;
        write(&s, &e.lib, "notebook-1", "page-1", &[(1.0, y), (9.0, y)]);
    }
    gate.release_one();
    wait_for("first barrier", || s.pending() == 1);
    gate.wait_held(); // records 2–5 are one group, held at the second barrier
    assert_eq!(
        s.pending(),
        1,
        "only the record covered by the completed barrier"
    );
    let st = s.stats();
    assert_eq!((st.journal_records_durable, st.journal_syncs), (1, 1));
    gate.release_one();
    wait_for("second barrier", || s.pending() == 5);
    let st = s.stats();
    assert_eq!(
        (st.journal_records_durable, st.journal_syncs),
        (5, 2),
        "one barrier covered the four queued records"
    );
    s.set_journal_gate(None);
    gate.open();
    stop(s);
    assert_eq!(pending_seqs(&e.sync), [1, 2, 3, 4, 5]);
}

#[test]
fn a_bulk_backlog_is_made_durable_under_few_barriers() {
    let e = env("bulk");
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    // 5000 local strokes (one durable page append), then their captures.
    let strokes: Vec<Stroke> = (0..5000)
        .map(|i| Stroke {
            id: new_stroke_id(),
            points: vec![
                Point::new(10.0, i as f32 + 10.0).with_pressure(0.5),
                Point::new(900.0, i as f32 + 10.0).with_pressure(0.5),
            ],
            brush: Some(STYLE_PRESSURE_WIDTH_V1.to_string()),
            finished: true,
        })
        .collect();
    let (mut store, _) = load_existing_page(&nb_dir(&e.lib, "notebook-1"), "page-1").unwrap();
    store.append_committed_strokes(&strokes).unwrap();
    let gate = gated(&s);
    for st in &strokes {
        s.stroke_committed("notebook-1", "page-1", st);
    }
    gate.wait_held();
    s.set_journal_gate(None);
    gate.open();
    let t0 = Instant::now();
    s.wait_journal_durable(&AtomicBool::new(false)).unwrap();
    let st = s.stats();
    println!(
        "5000 records durable in {} ms under {} barriers",
        t0.elapsed().as_millis(),
        st.journal_syncs
    );
    assert_eq!(st.journal_records_durable, 5000);
    // 1 (the held first record) + ⌈4999 / JOURNAL_GROUP_MAX_RECORDS⌉.
    assert!(st.journal_syncs <= 1 + 4999_u64.div_ceil(JOURNAL_GROUP_MAX_RECORDS as u64));
    assert_eq!(s.pending(), 5000);
    stop(s);
    // Restart: the whole backlog replays (in place) to exactly the local page.
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    assert!(s.fault().is_none(), "{:?}", s.fault());
    assert_eq!(s.pending(), 5000);
    stop(s);
    assert_eq!(replay_local(&e.sync).snapshot, local_snapshot(&e.lib));
}

#[test]
fn a_bulk_wait_tracks_journal_health_not_upload_health() {
    use crate::nbk_import::ImportSync;
    let e = env("bulkwait");
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    let no = AtomicBool::new(false);
    write(
        &s,
        &e.lib,
        "notebook-1",
        "page-1",
        &[(1.0, 1.0), (9.0, 9.0)],
    );
    assert_eq!(s.wait_journal_durable(&no), Ok(()));
    assert_eq!(ImportSync::flush(&s, &no), Ok(()), "healthy ⇒ durable");
    assert_eq!(s.pending(), 1);

    // Cancelled while the journal is held: returns promptly, not success.
    let gate = gated(&s);
    write(
        &s,
        &e.lib,
        "notebook-1",
        "page-1",
        &[(2.0, 2.0), (9.0, 9.0)],
    );
    gate.wait_held();
    let cancel = AtomicBool::new(true);
    assert_eq!(
        s.wait_journal_durable(&cancel),
        Err(DurableWaitError::Cancelled)
    );
    s.set_journal_gate(None);
    gate.open();
    assert_eq!(s.wait_journal_durable(&no), Ok(()));

    // An upload/reconcile fault must not fail a healthy local journal barrier.
    s.fail_external("simulated reconcile fault".into());
    assert!(!s.upload_permitted());
    write(
        &s,
        &e.lib,
        "notebook-1",
        "page-1",
        &[(3.0, 3.0), (9.0, 9.0)],
    );
    assert_eq!(s.wait_journal_durable(&no), Ok(()));
    assert_eq!(ImportSync::flush(&s, &no), Ok(()));
    assert_eq!(s.pending(), 3);

    // A real local journal failure does fail the barrier.
    s.fail_next_journal_write("simulated disk stall");
    write(
        &s,
        &e.lib,
        "notebook-1",
        "page-1",
        &[(4.0, 4.0), (9.0, 9.0)],
    );
    assert!(matches!(
        s.wait_journal_durable(&no),
        Err(DurableWaitError::Faulted(f)) if f.contains("simulated")
    ));
    assert!(matches!(
        ImportSync::flush(&s, &no),
        Err(DurableWaitError::Faulted(_))
    ));
    stop(s);
}

#[test]
fn compaction_waits_until_the_unacknowledged_rest_is_small() {
    let e = env("compact");
    let srv = server("compact");
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    let strokes: Vec<Stroke> = (0..COMPACT_MAX_REMAINING + 300)
        .map(|i| Stroke {
            id: new_stroke_id(),
            points: vec![
                Point::new(10.0, i as f32 + 10.0).with_pressure(0.5),
                Point::new(90.0, i as f32 + 10.0).with_pressure(0.5),
            ],
            brush: Some(STYLE_PRESSURE_WIDTH_V1.to_string()),
            finished: true,
        })
        .collect();
    let (mut store, _) = load_existing_page(&nb_dir(&e.lib, "notebook-1"), "page-1").unwrap();
    store.append_committed_strokes(&strokes).unwrap();
    for st in &strokes {
        s.stroke_committed("notebook-1", "page-1", st);
    }
    s.wait_journal_durable(&AtomicBool::new(false)).unwrap();
    stop(s);
    let total = strokes.len();
    write_config(&e.sync, &srv.url(), &srv.token, false);
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    wait_for("drain", || s.pending() == 0);
    wait_for("final compaction", || pending_seqs(&e.sync).is_empty());
    assert!(s.fault().is_none(), "{:?}", s.fault());
    // Batches acknowledged while more than COMPACT_MAX_REMAINING stayed pending rewrote
    // nothing (the acknowledged prefix waited in the file; startup skips it).
    let st = s.stats();
    let batches = total.div_ceil(MAX_BATCH_OPS) as u64;
    assert_eq!(st.batches, batches);
    assert!(
        st.journal_compactions <= 1 + (COMPACT_MAX_REMAINING / MAX_BATCH_OPS) as u64,
        "{} compactions for {batches} batches",
        st.journal_compactions
    );
    let lib_id = s.state().library_id;
    stop(s);
    assert_eq!(
        srv.snapshot_hash(&lib_id),
        snapshot_sha256(&local_snapshot(&e.lib))
            .unwrap()
            .to_string()
    );
}

// ------------------------------------------------------------------ recovery

/// Everything the S2C startup would replay (base + accepted + journal).
fn expected_now(sync_dir: &Path) -> SyncSnapshotV1 {
    expected_state(sync_dir).1.snapshot
}

fn journal_kinds(
    sync_dir: &Path,
    after_seq: u64,
) -> std::collections::BTreeMap<&'static str, usize> {
    let mut m = std::collections::BTreeMap::new();
    for o in parse_journal(&fs::read(sync_dir.join(OUTBOX_FILE)).unwrap())
        .unwrap()
        .committed
        .iter()
        .filter(|o| o.seq > after_seq)
    {
        *m.entry(op_kind(&o.envelope.operation)).or_default() += 1;
    }
    m
}

fn page_strokes(lib: &Path, nb: &str, page: &str, n: usize, y0: f32) {
    for i in 0..n {
        let y = y0 + 150.0 * i as f32;
        append_stroke(lib, nb, page, &[(1000.0, y), (6000.0, y + 40.0)]);
    }
}

fn library(lib: &Path) -> crate::library::LibraryRepository {
    crate::library::open_library_read_only(lib).unwrap()
}

fn nb_manifest(lib: &Path, nb: &str) -> crate::notebook::PersistedNotebookManifestV1 {
    crate::library::read_notebook_manifest_read_only(&nb_dir(lib, nb)).unwrap()
}

/// A notebook with `pages` pages (`strokes` each), created locally with **no** capture.
fn local_notebook(lib: &Path, title: &str, pages: usize, strokes: usize) -> String {
    let g = geo();
    let mut repo = library(lib);
    let c = crate::library::create_notebook_durable(
        &mut repo,
        &crate::profile::RenderProfile::sealed_v2_pressure(0x00).unwrap(),
        &g,
        &crate::ui_gfx::paper_background(&g),
        Some(title),
        &crate::library::CreateNotebookHooks::default(),
    )
    .unwrap();
    let dir = repo.notebook_dir(&c.notebook_id);
    let mut m = nb_manifest(lib, &c.notebook_id);
    for _ in 1..pages {
        let after = m.page_ids.last().unwrap().clone();
        crate::notebook::insert_page_after_durable(
            &dir,
            &mut m,
            &after,
            &crate::notebook::InsertPageHooks::default(),
        )
        .unwrap();
    }
    for p in m.page_ids.clone() {
        page_strokes(lib, &c.notebook_id, &p, strokes, 1000.0);
    }
    c.notebook_id
}

/// Healthy history (captured), then sync faults and the user keeps working (nothing
/// captured) — the device's situation. Returns the sync state as it stood at the fault.
fn faulted_history(label: &str) -> Env {
    let e = env(label);
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    write(
        &s,
        &e.lib,
        "notebook-1",
        "page-1",
        &[(100.0, 100.0), (900.0, 900.0)],
    );
    let nb1 = nb_dir(&e.lib, "notebook-1");
    let mut m = nb_manifest(&e.lib, "notebook-1");
    for (after, new) in [("page-1", "page-2"), ("page-2", "page-3")] {
        let c = crate::notebook::insert_page_after_durable(
            &nb1,
            &mut m,
            after,
            &crate::notebook::InsertPageHooks::default(),
        )
        .unwrap();
        assert_eq!(c.inserted_page_id, new);
        s.page_created(&nb1, "notebook-1", &c.inserted_page_id, Some(after));
    }
    write(
        &s,
        &e.lib,
        "notebook-1",
        "page-2",
        &[(100.0, 100.0), (900.0, 900.0)],
    );
    s.wait_journal_durable(&AtomicBool::new(false)).unwrap();
    s.fail_external("prepare client_seq 7: journal not durable within 5s (simulated)".into());
    stop(s);
    e
}

#[test]
fn recovery_rebuilds_history_made_while_sync_was_faulted_and_is_idempotent() {
    let e = faulted_history("recover");
    let before = parse_journal(&fs::read(e.sync.join(OUTBOX_FILE)).unwrap()).unwrap();
    let last_journaled = before.committed.last().unwrap().seq;

    // Unsynced work, exactly the device's kinds:
    // (a) a whole new multi-page, multi-stroke notebook (never seen by the server);
    let new_nb = local_notebook(&e.lib, "Rumi Math", 3, 7);
    // (b) new strokes on a known page;
    page_strokes(&e.lib, "notebook-1", "page-1", 5, 8000.0);
    // (c) a known page deleted through the F8 path (quarantined in .page-trash);
    let nb1 = nb_dir(&e.lib, "notebook-1");
    let mut m = nb_manifest(&e.lib, "notebook-1");
    crate::notebook::delete_page_durable(
        &nb1,
        &mut m,
        "page-3",
        &crate::notebook::DeletePageHooks::default(),
    )
    .unwrap();
    // (d) a new page with strokes appended to the known notebook.
    let c = crate::notebook::insert_page_after_durable(
        &nb1,
        &mut m,
        "page-2",
        &crate::notebook::InsertPageHooks::default(),
    )
    .unwrap();
    page_strokes(&e.lib, "notebook-1", &c.inserted_page_id, 4, 2000.0);

    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    assert!(s.fault().is_none(), "{:?}", s.fault());
    stop(s);

    // Existing journal: byte-for-byte, same client_seqs; derived ops only after it.
    let after = parse_journal(&fs::read(e.sync.join(OUTBOX_FILE)).unwrap()).unwrap();
    for (a, b) in before.committed.iter().zip(&after.committed) {
        assert_eq!((a.seq, &a.bytes), (b.seq, &b.bytes));
    }
    assert_eq!(
        journal_kinds(&e.sync, last_journaled),
        [
            ("add_stroke", 3 * 7 + 5 + 4),
            ("create_notebook", 1),
            ("create_page", 2 + 1),
            ("delete_page", 1),
        ]
        .into_iter()
        .collect()
    );
    assert_eq!(
        expected_now(&e.sync),
        local_snapshot(&e.lib),
        "replay == local files"
    );
    let ids = read_identity(&e.lib).unwrap().unwrap();
    assert!(ids.notebook_id(&new_nb).is_some());

    // Idempotent: a second (and third) start derives nothing and leaves the journal as is.
    let journal = fs::read(e.sync.join(OUTBOX_FILE)).unwrap();
    for _ in 0..2 {
        let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
        assert!(s.fault().is_none(), "{:?}", s.fault());
        stop(s);
        assert_eq!(fs::read(e.sync.join(OUTBOX_FILE)).unwrap(), journal);
        assert_eq!(read_identity(&e.lib).unwrap().unwrap(), ids);
    }

    // The server accepts all of it (no duplicate creates/strokes) and converges exactly.
    let srv = server("recover");
    write_config(&e.sync, &srv.url(), &srv.token, false);
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    wait_for("upload", || s.pending() == 0);
    assert!(s.fault().is_none(), "{:?}", s.fault());
    let lib_id = s.state().library_id;
    stop(s);
    assert_eq!(
        srv.snapshot_hash(&lib_id),
        snapshot_sha256(&local_snapshot(&e.lib))
            .unwrap()
            .to_string()
    );
    // …and a restart after the upload re-derives nothing (no duplicates).
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    assert!(s.fault().is_none(), "{:?}", s.fault());
    assert_eq!(s.pending(), 0);
    stop(s);
}

#[test]
fn recovery_resumes_with_the_same_ids_after_a_crash_between_identity_and_journal() {
    let e = faulted_history("recover-crash");
    let new_nb = local_notebook(&e.lib, "Songs", 2, 3);
    // Crash window: identities published, derived ops not yet journaled.
    let (base, plan) = crate::sync_local::plan_sync_identity(&e.lib).unwrap();
    crate::sync_local::persist_planned_identity(&e.lib, base.as_ref(), &plan).unwrap();
    let bound = read_identity(&e.lib)
        .unwrap()
        .unwrap()
        .notebook_id(&new_nb)
        .unwrap();
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    assert!(s.fault().is_none(), "{:?}", s.fault());
    stop(s);
    let creates: Vec<NotebookId> = parse_journal(&fs::read(e.sync.join(OUTBOX_FILE)).unwrap())
        .unwrap()
        .committed
        .iter()
        .filter_map(|o| match &o.envelope.operation {
            SyncOperationV1::CreateNotebook { notebook_id, .. } => Some(*notebook_id),
            _ => None,
        })
        .collect();
    assert_eq!(
        creates,
        [bound],
        "exactly one create, with the published ID"
    );
    assert_eq!(expected_now(&e.sync), local_snapshot(&e.lib));
}

#[test]
fn recovery_refuses_what_it_cannot_reproduce_exactly_and_writes_nothing() {
    // (label, unsynced local change, expected reason fragment)
    type Change = fn(&Path);
    let cases: [(&str, Change, &str); 3] = [
        (
            "no-trash",
            |lib| {
                let dir = nb_dir(lib, "notebook-1");
                let mut m = nb_manifest(lib, "notebook-1");
                crate::notebook::delete_page_durable(
                    &dir,
                    &mut m,
                    "page-3",
                    &crate::notebook::DeletePageHooks::default(),
                )
                .unwrap();
                fs::remove_dir_all(dir.join(crate::notebook::PAGE_TRASH_DIR)).unwrap();
            },
            "without deletion evidence",
        ),
        (
            "reorder",
            |lib| {
                let dir = nb_dir(lib, "notebook-1");
                let mut m = nb_manifest(lib, "notebook-1");
                crate::notebook::move_page_durable(
                    &dir,
                    &mut m,
                    "page-1",
                    crate::notebook::MovePageDirection::Right,
                    &crate::notebook::MovePageHooks::default(),
                )
                .unwrap();
            },
            "reordered",
        ),
        (
            "fragment",
            |lib| {
                // A new notebook whose page holds an erase fragment (lineage ≠ stroke).
                let nb = local_notebook(lib, "Erased", 1, 1);
                let dir = nb_dir(lib, &nb);
                let (mut store, loaded) = load_existing_page(&dir, "page-1").unwrap();
                let src = &loaded.strokes[0];
                let p = src.points[0];
                store
                    .append_replace_strokes(&[PersistedReplacementV2 {
                        target_stroke_id: src.id.as_str().to_string(),
                        fragments: vec![crate::store::PersistedFragmentV2 {
                            id: new_stroke_id().as_str().to_string(),
                            style: src.brush.clone(),
                            points: vec![crate::store::PersistedPointV1 {
                                x: p.x,
                                y: p.y,
                                pressure: p.pressure,
                            }],
                        }],
                    }])
                    .unwrap();
            },
            "lineage",
        ),
    ];
    for (label, change, why) in cases {
        let e = faulted_history(label);
        change(&e.lib);
        let journal = fs::read(e.sync.join(OUTBOX_FILE)).unwrap();
        let identity = fs::read(crate::sync_local::identity_path(&e.lib)).unwrap();
        let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
        let fault = s.fault().unwrap_or_default();
        stop(s);
        assert!(
            fault.contains("manual recovery") && fault.contains(why),
            "{label}: {fault}"
        );
        assert_eq!(
            fs::read(e.sync.join(OUTBOX_FILE)).unwrap(),
            journal,
            "{label}"
        );
        assert_eq!(
            fs::read(crate::sync_local::identity_path(&e.lib)).unwrap(),
            identity,
            "{label}: identities are published only after an exact replay"
        );
    }
}

#[test]
fn the_streamed_base_state_is_byte_identical_and_reads_back_exactly() {
    let e = env("base-stream");
    let s = OutboundSync::open(&e.lib, &e.sync).unwrap();
    write(
        &s,
        &e.lib,
        "notebook-1",
        "page-1",
        &[(1.0, 1.0), (9.0, 9.0)],
    );
    s.wait_journal_durable(&AtomicBool::new(false)).unwrap();
    stop(s);
    let state = replay_local(&e.sync);
    let (a, b) = (e.sync.join("a"), e.sync.join("b"));
    write_json(
        &a,
        BASE_FILE,
        &BaseStateV2 {
            v: 2,
            through_server_seq: 7,
            snapshot: state.snapshot.clone(),
            history: state.history.clone(),
        },
    )
    .unwrap();
    write_base_state(&b, 7, &state).unwrap();
    assert_eq!(
        fs::read(a.join(BASE_FILE)).unwrap(),
        fs::read(b.join(BASE_FILE)).unwrap()
    );
    let back = read_base_state(&b.join(BASE_FILE)).unwrap();
    assert_eq!(
        (back.through_server_seq, back.snapshot, back.history),
        (7, state.snapshot, state.history)
    );
}

/// The device wedge of 2026-09-27: the uploader reused a pooled connection whose peer vanished
/// while the Kindle was suspended, and blocked reading the reply forever. A server that answers
/// one request with keep-alive and then goes silent (connection left open) must make the next
/// exchange on the same agent fail within the request deadline — and never hang.
#[test]
fn an_exchange_whose_reply_never_comes_fails_within_the_deadline() {
    use std::io::{BufRead, BufReader, Write as _};
    use std::net::TcpListener;
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    // Accept every connection; answer only the very first request, then hold sockets open.
    std::thread::spawn(move || {
        let mut answered = false;
        let mut held = Vec::new();
        for conn in listener.incoming().flatten() {
            let mut r = BufReader::new(conn.try_clone().unwrap());
            let mut len = 0usize;
            loop {
                let mut line = String::new();
                if r.read_line(&mut line).unwrap_or(0) == 0 || line.trim_end().is_empty() {
                    break;
                }
                if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    len = v.trim().parse().unwrap_or(0);
                }
            }
            let mut body = vec![0u8; len];
            let _ = std::io::Read::read_exact(&mut r, &mut body);
            if !answered {
                answered = true;
                let mut w = conn.try_clone().unwrap();
                let _ = w.write_all(
                    concat!(
                        "HTTP/1.1 200 OK\r\n",
                        "Content-Length: 2\r\n",
                        "Connection: keep-alive\r\n",
                        "\r\n",
                        "{}"
                    )
                    .as_bytes(),
                );
            }
            held.push(conn); // silent from now on, never closed
        }
    });
    let cfg = SyncConfigV1 {
        v: 1,
        server_base_url: format!("http://{addr}"),
        token: "0".repeat(64),
        debug_drop_first_ack: false,
        debug_crash_point: None,
        server_id: None,
    };
    let agent = agent();
    let first = send(&agent, &cfg, "POST", "/x", b"{}");
    assert!(
        first.is_ok(),
        "first exchange answered: {:?}",
        first.err().map(|e| match e {
            PushError::Retry(s) | PushError::Fatal(s) | PushError::Unauthorized(s) => s,
        })
    );
    let t = Instant::now();
    let r = send(&agent, &cfg, "POST", "/x", b"{}");
    let took = t.elapsed();
    assert!(
        matches!(r, Err(PushError::Retry(_))),
        "silent server ⇒ retryable error"
    );
    assert!(
        took < HTTP_REQUEST_DEADLINE + Duration::from_secs(2),
        "bounded by the request deadline, took {took:?}"
    );
}
