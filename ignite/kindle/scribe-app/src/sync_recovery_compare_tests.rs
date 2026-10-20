//! Slice 18E.6C: read-only comparison proofs. Synthetic fixtures only for ordinary
//! runs. No Kindle or production host.

use super::*;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use kindle_platform::FbGeometry;
use sync_engine::testkit::{library, line, nb, pg, render, replica, sk, Author};
use sync_engine::LibraryState;
use sync_protocol::{
    to_canonical_json, BackgroundRef, IdentityFileV1, PageSpace, Sha256Hex, SnapshotNotebook,
    SnapshotPage, SyncOperationV1, SyncSnapshotV1, SyncStroke, IDENTITY_FILE_VERSION,
};

use crate::ink::InkRequest;
use crate::library::open_or_create_library;
use crate::sync_local::{ensure_sync_identity, identity_path, read_identity};
use crate::sync_outbound::SyncStateV1;
use crate::sync_recovery_extract::file_sha256;
use crate::sync_recovery_plan::{
    plan_recovery, OutboxInput, PlanResult, RecoveryBlock, RecoveryInputs, SequenceAuthority,
};
use crate::ui_gfx::paper_background;

fn orig(n: u64) -> SyncStroke {
    line(
        sk(n),
        sk(n),
        (200.0, 200.0 + n as f32),
        (800.0, 200.0 + n as f32),
        4,
        false,
    )
}

fn frag(id: u64, root: u64, y: f32) -> SyncStroke {
    line(sk(id), sk(root), (200.0, y), (400.0, y), 3, false)
}

fn blank(id: sync_protocol::PageId, strokes: Vec<SyncStroke>) -> SnapshotPage {
    SnapshotPage {
        page_id: id,
        background: BackgroundRef::Blank,
        render: render(false),
        strokes,
        texts: Vec::new(),
        links: Vec::new(),
    }
}

fn notebook(
    id: sync_protocol::NotebookId,
    title: &str,
    pages: Vec<SnapshotPage>,
) -> SnapshotNotebook {
    SnapshotNotebook {
        notebook_id: id,
        title: title.into(),
        pages,
        folder_id: None,
    }
}

fn snap(notebooks: Vec<SnapshotNotebook>) -> SyncSnapshotV1 {
    SyncSnapshotV1 {
        v: 1,
        library_id: library(),
        page_space: PageSpace::CANONICAL,
        notebooks,
        folders: Vec::new(),
    }
}

fn genesis(notebooks: Vec<SnapshotNotebook>) -> LibraryState {
    LibraryState::genesis(snap(notebooks)).unwrap()
}

fn tmp(label: &str) -> PathBuf {
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = std::env::temp_dir().join(format!("scribe-compare-{label}-{n}"));
    let _ = std::fs::remove_dir_all(&p);
    p
}

fn geo() -> FbGeometry {
    FbGeometry {
        xres: 200,
        yres: 160,
        xres_virtual: 208,
        yres_virtual: 160,
        xoffset: 0,
        yoffset: 0,
        bits_per_pixel: 8,
        line_length: 208,
        smem_len: 208 * 160,
    }
}

fn dummy_state(cursor: u64, acked: u64, next: u64) -> SyncStateV1 {
    SyncStateV1 {
        v: 2,
        library_id: library(),
        replica_id: replica(1),
        genesis_sha256: Sha256Hex::from_digest(&[0u8; 32]),
        genesis_uploaded: true,
        acked_client_seq: acked,
        next_client_seq: next,
        server_base_url: None,
        server_cursor: cursor,
        server_id: None,
    }
}

fn identity_maps_equal(a: &IdentityFileV1, b: &IdentityFileV1) -> bool {
    a.library_id == b.library_id && a.notebooks == b.notebooks && a.pages == b.pages
}

#[test]
fn wrong_phrase_never_authorizes_device_read() {
    let err = KindleReadGrant::explicit(
        "not the phrase",
        "root@kindle".into(),
        "ssh".into(),
        PathBuf::from("C:/tmp"),
    )
    .unwrap_err();
    assert!(err.contains("not authorized"));
}

#[test]
fn phrase_in_source_is_not_a_grant() {
    assert_eq!(
        KINDLE_READ_AUTHORIZATION_PHRASE,
        "I AUTHORIZE READ-ONLY KINDLE STATE ACQUISITION"
    );
    let err = KindleReadGrant::explicit(
        KINDLE_READ_AUTHORIZATION_PHRASE,
        String::new(),
        "ssh".into(),
        PathBuf::from("C:/tmp"),
    )
    .unwrap_err();
    assert!(err.contains("explicit device"));
}

#[test]
fn inspect_does_not_write_identity() {
    let lib = tmp("id-ro").join("library");
    let g = geo();
    open_or_create_library(&lib, InkRequest::Default, &g, &paper_background(&g)).unwrap();
    ensure_sync_identity(&lib).unwrap();
    let path = identity_path(&lib);
    let before = std::fs::read(&path).unwrap();
    let insp = inspect_identity_read_only(&lib).unwrap();
    assert!(insp.complete());
    assert_eq!(insp.live_pages, 1);
    assert_eq!(insp.unbound_pages.len(), 0);
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert_eq!(
        file_sha256(&path).unwrap().unwrap(),
        sync_protocol::sha256_hex(&before).to_string()
    );
}

#[test]
fn inspect_reports_unbound_page_without_minting() {
    let lib = tmp("unbound").join("library");
    let g = geo();
    open_or_create_library(&lib, InkRequest::Default, &g, &paper_background(&g)).unwrap();
    ensure_sync_identity(&lib).unwrap();
    let mut id = read_identity(&lib).unwrap().unwrap();
    id.pages
        .get_mut("notebook-1")
        .unwrap()
        .remove("page-1")
        .unwrap();
    let bytes = id.to_canonical_bytes().unwrap();
    let mut out = bytes.clone();
    out.push(b'\n');
    std::fs::write(identity_path(&lib), &out).unwrap();
    let before = std::fs::read(identity_path(&lib)).unwrap();
    let insp = inspect_identity_read_only(&lib).unwrap();
    assert!(!insp.complete());
    assert_eq!(
        insp.unbound_pages,
        vec![("notebook-1".into(), "page-1".into())]
    );
    assert_eq!(std::fs::read(identity_path(&lib)).unwrap(), before);
    let (again, snap) = bound_snapshot_read_only(&lib).unwrap();
    assert_eq!(again.unbound_pages.len(), 1);
    let snap = snap.expect("bound-only snapshot");
    assert!(snap.notebooks.is_empty());
}

#[test]
fn compare_detects_stroke_id_root_geometry_and_order() {
    let base = vec![orig(1), orig(2)];
    let hist = snap(vec![notebook(nb(1), "N", vec![blank(pg(1), base.clone())])]);
    let mut id_changed = base.clone();
    id_changed[0] = orig(9);
    let d = compare_snapshots(
        &hist,
        &snap(vec![notebook(nb(1), "N", vec![blank(pg(1), id_changed)])]),
    );
    assert!(!d.exact_canonical_match);
    assert!(d.stroke_id_changed >= 1);

    let mut root_changed = vec![orig(1), orig(2)];
    root_changed[0] = frag(1, 99, 200.0);
    let d = compare_snapshots(
        &hist,
        &snap(vec![notebook(nb(1), "N", vec![blank(pg(1), root_changed)])]),
    );
    assert!(d.root_lineage_changed >= 1);

    let mut geo = vec![orig(1), orig(2)];
    geo[1] = line(sk(2), sk(2), (10.0, 10.0), (20.0, 20.0), 4, false);
    let d = compare_snapshots(
        &hist,
        &snap(vec![notebook(nb(1), "N", vec![blank(pg(1), geo)])]),
    );
    assert!(d.geometry_changed >= 1);

    let swapped = vec![orig(2), orig(1)];
    let d = compare_snapshots(
        &hist,
        &snap(vec![notebook(nb(1), "N", vec![blank(pg(1), swapped)])]),
    );
    assert!(d.render_order_changed >= 1);
}

#[test]
fn compare_detects_missing_and_added_pages() {
    let hist = snap(vec![notebook(
        nb(1),
        "N",
        vec![blank(pg(1), vec![orig(1)]), blank(pg(2), vec![orig(2)])],
    )]);
    let missing = snap(vec![notebook(
        nb(1),
        "N",
        vec![blank(pg(1), vec![orig(1)])],
    )]);
    let d = compare_snapshots(&hist, &missing);
    assert_eq!(d.removed_pages, 1);
    assert!(d.page_changes.iter().any(|c| c.kind == "missing_page"));

    let added = snap(vec![notebook(
        nb(1),
        "N",
        vec![
            blank(pg(1), vec![orig(1)]),
            blank(pg(2), vec![orig(2)]),
            blank(pg(3), vec![orig(3)]),
        ],
    )]);
    let d = compare_snapshots(&hist, &added);
    assert_eq!(d.added_pages, 1);
}

#[test]
fn compare_detects_altered_identity_binding() {
    let mut a = IdentityFileV1 {
        v: IDENTITY_FILE_VERSION,
        library_id: library(),
        notebooks: Default::default(),
        pages: Default::default(),
        retired_notebooks: Default::default(),
        retired_pages: Default::default(),
    };
    a.notebooks.insert("notebook-1".into(), nb(1));
    a.pages
        .entry("notebook-1".into())
        .or_default()
        .insert("page-1".into(), pg(1));
    let mut b = a.clone();
    b.pages
        .get_mut("notebook-1")
        .unwrap()
        .insert("page-1".into(), pg(99));
    assert!(identity_maps_equal(&a, &a));
    assert!(!identity_maps_equal(&a, &b));
}

#[test]
fn outbox_gap_is_malformed_not_repaired() {
    let rec = |seq: u64, stroke: u64| {
        let mut remote = Author::new(replica(1));
        remote.library = library();
        remote.next_seq = seq;
        let env = remote.env(SyncOperationV1::AddStroke {
            notebook_id: nb(1),
            page_id: pg(1),
            stroke: orig(stroke),
        });
        let text = String::from_utf8(to_canonical_json(&env).unwrap()).unwrap();
        format!(
            "{}\n",
            serde_json::json!({"v":1,"kind":"op","seq":seq,"envelope":text})
        )
    };
    let raw = format!("{}{}", rec(1, 1), rec(3, 3));
    match classify_outbox(raw.as_bytes()) {
        OutboxClass::Malformed { detail } => {
            assert!(
                detail.contains("seq 3 after 1") || detail.contains("client_seq"),
                "{detail}"
            );
        }
        other => panic!("{other:?}"),
    }
    assert!(matches!(classify_outbox(b""), OutboxClass::EmptyValid));
}

#[test]
fn pending_dependency_is_a_blocker_not_uncaptured() {
    let cp = genesis(vec![notebook(
        nb(1),
        "N",
        vec![blank(pg(1), vec![orig(1), orig(2)])],
    )]);
    let local = snap(vec![notebook(
        nb(1),
        "N",
        vec![blank(
            pg(1),
            vec![orig(1), orig(2), orig(50), frag(501, 50, 400.0)],
        )],
    )]);
    let mut later = Author::new(replica(1));
    later.library = library();
    later.next_seq = 37866;
    let pending = vec![later.env(SyncOperationV1::ReplaceStrokeLineages {
        notebook_id: nb(1),
        page_id: pg(1),
        replacements: vec![sync_protocol::LineageReplacement {
            root_id: sk(50),
            expected: vec![orig(50), frag(501, 50, 400.0)],
            after: vec![frag(502, 50, 410.0)],
        }],
    })];
    let result = plan_recovery(RecoveryInputs {
        library_id: library(),
        replica_id: replica(1),
        checkpoint: &cp,
        checkpoint_cursor: 0,
        acked_client_seq: 37865,
        accepted: &[],
        outbox: OutboxInput::Valid {
            envelopes: &pending,
        },
        local: &local,
        server_tail: &[],
        sequence_authority: SequenceAuthority::Known {
            next_client_seq: 37867,
        },
    });
    assert!(
        matches!(
            result,
            PlanResult::Blocked(RecoveryBlock::PendingOperationDependency { .. })
        ),
        "{result:?}"
    );
}

#[test]
fn changed_confirmed_cursor_is_sync_state_category() {
    let a = dummy_state(37928, 37865, 37866);
    let mut b = a.clone();
    b.server_cursor = 37929;
    let d = compare_sync_state(&a, &b);
    assert!(d.cursor_changed);
    assert_eq!(d.category, DiffCategory::SyncState);
    assert_eq!(compare_sync_state(&a, &a).category, DiffCategory::Unchanged);
}

#[test]
fn review_94_tail_is_labeled_and_cursor_gated() {
    assert_eq!(observed_tail_label(), REVIEW_94_TAIL_LABEL);
    assert!(review_94_tail_applicable(37928));
    assert!(!review_94_tail_applicable(37929));
}

#[test]
fn plan_refuses_unbound_identity_without_minting() {
    let insp = IdentityInspection {
        live_notebooks: 1,
        live_pages: 2,
        bound_notebooks: 1,
        bound_pages: 1,
        unbound_notebooks: Vec::new(),
        unbound_pages: vec![("notebook-1".into(), "page-2".into())],
        retired_notebooks: 0,
        retired_pages: 0,
        library_bound: true,
    };
    let cp = genesis(vec![notebook(
        nb(1),
        "N",
        vec![blank(pg(1), vec![orig(1)])],
    )]);
    let local = cp.snapshot.clone();
    let err = plan_if_identity_complete(
        library(),
        replica(1),
        &cp,
        37928,
        37865,
        &local,
        &[],
        &[],
        SequenceAuthority::Known {
            next_client_seq: 37866,
        },
        &insp,
    )
    .unwrap_err();
    assert!(err.contains("identity_binding_prerequisite"));
}

#[test]
fn census_detects_journal_change() {
    let lib = tmp("census").join("library");
    let sync = tmp("census-sync");
    std::fs::create_dir_all(&sync).unwrap();
    let g = geo();
    open_or_create_library(&lib, InkRequest::Default, &g, &paper_background(&g)).unwrap();
    ensure_sync_identity(&lib).unwrap();
    let a = consistency_census(&lib, &sync).unwrap();
    let b = consistency_census(&lib, &sync).unwrap();
    assert!(census_equal(&a, &b));
    let strokes = lib
        .join("notebooks")
        .join("notebook-1")
        .join("page-1")
        .join("strokes.jsonl");
    let mut extra = std::fs::read(&strokes).unwrap();
    extra.extend_from_slice(b"\n");
    std::fs::write(&strokes, extra).unwrap();
    let c = consistency_census(&lib, &sync).unwrap();
    assert!(!census_equal(&a, &c));
}

#[test]
fn fragments_preserved_when_ids_unchanged() {
    let strokes = vec![orig(1), frag(10, 1, 300.0)];
    let hist = snap(vec![notebook(
        nb(1),
        "N",
        vec![blank(pg(1), strokes.clone())],
    )]);
    let curr = snap(vec![notebook(nb(1), "N", vec![blank(pg(1), strokes)])]);
    let d = compare_snapshots(&hist, &curr);
    assert!(d.fragment_ids_match);
    assert!(d.exact_canonical_match);
}

#[test]
#[ignore = "manual: set SCRIBE_REHEARSAL_LIB to inspect historical identity without minting"]
fn historical_identity_inspect_from_env() {
    let lib = PathBuf::from(std::env::var("SCRIBE_REHEARSAL_LIB").unwrap());
    let before = file_sha256(&identity_path(&lib)).unwrap();
    let insp = inspect_identity_read_only(&lib).unwrap();
    let after = file_sha256(&identity_path(&lib)).unwrap();
    assert_eq!(before, after, "historical identity.json must not change");
    assert_eq!(insp.live_notebooks, 24);
    assert_eq!(insp.live_pages, 73);
    assert_eq!(insp.unbound_pages.len(), 2);
    assert!(!insp.complete());
    let (i, snap) = bound_snapshot_read_only(&lib).unwrap();
    assert_eq!(i.unbound_pages.len(), 2);
    let snap = snap.expect("71 bound pages still snapshot");
    assert_eq!(i.live_pages, 73);
    assert_eq!(
        snap.notebooks.iter().map(|n| n.pages.len()).sum::<usize>(),
        71
    );
}

#[test]
#[ignore = "manual: set SCRIBE_COMPARE_HIST_LIB / SCRIBE_COMPARE_CURR_LIB"]
fn authorized_current_vs_historical_from_env() {
    use std::str::FromStr;

    use crate::sync_outbound::{parse_journal, SyncStateV1, OUTBOX_FILE, STATE_FILE};
    use crate::sync_recovery_plan::SequenceAuthority;
    use sync_protocol::{snapshot_sha256, LibraryId, ReplicaId};

    let hist_lib = PathBuf::from(std::env::var("SCRIBE_COMPARE_HIST_LIB").unwrap());
    let curr_lib = PathBuf::from(std::env::var("SCRIBE_COMPARE_CURR_LIB").unwrap());
    let hist_sync = PathBuf::from(std::env::var("SCRIBE_COMPARE_HIST_SYNC").unwrap());
    let curr_sync = PathBuf::from(std::env::var("SCRIBE_COMPARE_CURR_SYNC").unwrap());
    let out = PathBuf::from(std::env::var("SCRIBE_COMPARE_OUT").unwrap());

    let hist_id_before = file_sha256(&identity_path(&hist_lib)).unwrap();
    let curr_id_before = file_sha256(&identity_path(&curr_lib)).unwrap();

    let (hist_insp, hist_snap) = bound_snapshot_read_only(&hist_lib).unwrap();
    let (curr_insp, curr_snap) = bound_snapshot_read_only(&curr_lib).unwrap();
    assert_eq!(hist_insp.live_notebooks, curr_insp.live_notebooks);
    assert_eq!(hist_insp.live_pages, curr_insp.live_pages);
    assert_eq!(hist_insp.unbound_pages, curr_insp.unbound_pages);
    assert_eq!(hist_insp.unbound_pages.len(), 2);
    assert!(!curr_insp.complete());

    let hist_snap = hist_snap.expect("hist bound snap");
    let curr_snap = curr_snap.expect("curr bound snap");
    let lib_diff = compare_snapshots(&hist_snap, &curr_snap);
    assert!(lib_diff.exact_canonical_match);
    assert!(lib_diff.fragment_ids_match);

    let hist_st: SyncStateV1 =
        serde_json::from_slice(&std::fs::read(hist_sync.join(STATE_FILE)).unwrap()).unwrap();
    let curr_st: SyncStateV1 =
        serde_json::from_slice(&std::fs::read(curr_sync.join(STATE_FILE)).unwrap()).unwrap();
    let sync_diff = compare_sync_state(&hist_st, &curr_st);
    assert_eq!(sync_diff.category, DiffCategory::Unchanged);
    assert!(review_94_tail_applicable(curr_st.server_cursor));

    let curr_outbox = std::fs::read(curr_sync.join(OUTBOX_FILE)).unwrap_or_default();
    assert!(matches!(
        classify_outbox(&curr_outbox),
        OutboxClass::EmptyValid
    ));
    assert!(parse_journal(&curr_outbox).unwrap().committed.is_empty());

    let hist_census = consistency_census(&hist_lib, &hist_sync).unwrap();
    let curr_census = consistency_census(&curr_lib, &curr_sync).unwrap();
    assert!(census_equal(&hist_census, &curr_census));

    let err = plan_if_identity_complete(
        LibraryId::from_str(&std::env::var("SCRIBE_COMPARE_LIBRARY").unwrap()).unwrap(),
        ReplicaId::from_str(&std::env::var("SCRIBE_COMPARE_REPLICA").unwrap()).unwrap(),
        &sync_engine::LibraryState::genesis(curr_snap.clone()).unwrap(),
        curr_st.server_cursor,
        curr_st.acked_client_seq,
        &curr_snap,
        &[],
        &[],
        SequenceAuthority::Known {
            next_client_seq: curr_st.next_client_seq,
        },
        &curr_insp,
    )
    .unwrap_err();
    assert!(err.contains("identity_binding_prerequisite"));

    assert_eq!(
        file_sha256(&identity_path(&hist_lib)).unwrap(),
        hist_id_before
    );
    assert_eq!(
        file_sha256(&identity_path(&curr_lib)).unwrap(),
        curr_id_before
    );

    let report = serde_json::json!({
        "tail_label": observed_tail_label(),
        "review_94_tail_applicable": true,
        "upload_authorized": false,
        "exact_canonical_match": lib_diff.exact_canonical_match,
        "fragment_ids_match": lib_diff.fragment_ids_match,
        "sync_state_category": "Unchanged",
        "outbox": "EmptyValid",
        "live_notebooks": curr_insp.live_notebooks,
        "live_pages": curr_insp.live_pages,
        "bound_pages": curr_insp.bound_pages,
        "unbound_pages": curr_insp.unbound_pages.len(),
        "unbound_local_names": curr_insp.unbound_pages,
        "hist_bound_snapshot_sha256": snapshot_sha256(&hist_snap).map(|h| h.to_string()).unwrap_or_default(),
        "curr_bound_snapshot_sha256": snapshot_sha256(&curr_snap).map(|h| h.to_string()).unwrap_or_default(),
        "cursor": curr_st.server_cursor,
        "acked": curr_st.acked_client_seq,
        "next": curr_st.next_client_seq,
        "identity_binding_prerequisite": true,
        "device_identity_unchanged": true,
        "historical_identity_unchanged": true,
    });
    std::fs::write(
        out.join("comparison.json"),
        serde_json::to_vec_pretty(&report).unwrap(),
    )
    .unwrap();
}
