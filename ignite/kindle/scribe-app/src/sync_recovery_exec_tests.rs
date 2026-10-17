//! Slice 18E.7: disposable recovery-execution proofs. Synthetic identities and loopback server only.

use super::*;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::{SystemTime, UNIX_EPOCH};

use kindle_platform::FbGeometry;
use scribe_sync_server::{generate_token, ServerConfig, ServerHandle, Store, TokenVerifier};
use sync_engine::testkit::{self, genesis, line, nb, replica, sk, Author};
use sync_engine::LibraryState;
use sync_protocol::{
    to_canonical_json, BackgroundRef, LineageReplacement, OperationId, PageId, SyncEnvelopeV1,
    SyncOperationV1, SyncSnapshotV1, SyncStroke, ENVELOPE_VERSION,
};
use uuid::Uuid;

use crate::ink::InkRequest;
use crate::library::{
    open_library_read_only, open_or_create_library, read_notebook_manifest_read_only,
};
use crate::notebook::{insert_page_after_durable, InsertPageHooks};
use crate::pressure_curve::STYLE_PRESSURE_WIDTH_V1;
use crate::store::{load_existing_page, PageStore};
use crate::sync_local::{
    build_snapshot_read_only, ensure_sync_identity, plan_sync_identity, read_identity,
};
use crate::sync_outbound::{SyncStateV1, STATE_FILE};
use crate::sync_recovery_bind::checkpoint_collision;
use crate::sync_recovery_compare::inspect_identity_read_only;
use crate::sync_verified::read_mutation_gen;
use crate::ui_gfx::paper_background;
use notebook_core::{new_stroke_id, Point, Stroke};
use std::collections::BTreeSet;
use std::sync::Mutex;
use std::thread;
use sync_protocol::binding::RetiredPage;
use sync_protocol::{from_json, sha256_hex, LibraryId};

fn tmp(label: &str) -> PathBuf {
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("scribe-exec-{label}-{n}"))
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

fn dummy_loopback() -> SubmitTarget {
    SubmitTarget {
        base_url: "http://127.0.0.1:1".into(),
        token: "t".into(),
    }
}

fn assert_halted_contains(out: ExecOutcome, needle: &str) {
    match out {
        ExecOutcome::Halted {
            reason,
            accepted_prefix,
        } => {
            assert!(reason.contains(needle), "{reason}");
            assert_eq!(accepted_prefix, 0);
        }
        other => panic!("expected Halted containing {needle:?}, got {other:?}"),
    }
}

fn local_append_stroke(lib: &Path) {
    try_append_stroke(lib).unwrap();
}

fn extra_pages_lib(label: &str) -> (PathBuf, PathBuf, LibraryState) {
    let lib = tmp(label).join("library");
    let sync = tmp(&format!("{label}-sync"));
    let g = geo();
    open_or_create_library(&lib, InkRequest::Default, &g, &paper_background(&g)).unwrap();
    ensure_sync_identity(&lib).unwrap();
    let repo = open_library_read_only(&lib).unwrap();
    let dir = repo.notebook_dir("notebook-1");
    let mut m = read_notebook_manifest_read_only(&dir).unwrap();
    insert_page_after_durable(&dir, &mut m, "page-1", &InsertPageHooks::default()).unwrap();
    let after = m.page_ids.last().unwrap().clone();
    insert_page_after_durable(&dir, &mut m, &after, &InsertPageHooks::default()).unwrap();
    fs::create_dir_all(&sync).unwrap();
    let identity = read_identity(&lib).unwrap().unwrap();
    let bound = crate::sync_local::build_snapshot_bound_only(&lib, &identity).unwrap();
    let cp = LibraryState::genesis(bound).unwrap();
    (lib, sync, cp)
}

fn extra_pages_spec<'a>(lib: &'a Path, sync: &'a Path, cp: &'a LibraryState) -> PrepareSpec<'a> {
    PrepareSpec {
        library_root: lib,
        sync_dir: sync,
        checkpoint: cp,
        checkpoint_cursor: 0,
        acked_client_seq: 0,
        next_client_seq: 1,
        replica_id: replica(1),
        genesis_sha256: "g".into(),
        server_tail: &[],
        require_disposable: false,
    }
}

fn dummy_sync_state(library_id: LibraryId, next: u64, cursor: u64) -> SyncStateV1 {
    SyncStateV1 {
        v: 2,
        library_id,
        replica_id: replica(1),
        genesis_sha256: sha256_hex(b"g"),
        genesis_uploaded: false,
        acked_client_seq: 0,
        next_client_seq: next,
        server_base_url: None,
        server_cursor: cursor,
    }
}

static AFTER_MUTATE_LIB: Mutex<Option<PathBuf>> = Mutex::new(None);

fn mutate_after_revalidate_hook() {
    let lib = AFTER_MUTATE_LIB.lock().unwrap().clone().unwrap();
    let err = try_append_stroke(&lib).expect_err("ordinary write must refuse during POST");
    assert!(err.contains("RECOVERY MAINTENANCE"), "{err}");
}

fn try_append_stroke(lib: &Path) -> Result<Stroke, String> {
    let stroke = Stroke {
        id: new_stroke_id(),
        points: vec![
            Point::new(500.0, 500.0).with_pressure(0.5),
            Point::new(4000.0, 4000.0).with_pressure(0.5),
        ],
        brush: Some(STYLE_PRESSURE_WIDTH_V1.to_string()),
        finished: true,
    };
    let nb = lib.join("notebooks").join("notebook-1");
    let (mut store, _) = load_existing_page(&nb, "page-1")?;
    store.append_committed_stroke(&stroke)?;
    Ok(stroke)
}

struct Srv {
    h: ServerHandle,
    token: String,
    _dir: PathBuf,
}

impl Srv {
    fn url(&self) -> String {
        self.h.base_url()
    }
    fn target(&self) -> SubmitTarget {
        SubmitTarget {
            base_url: self.url(),
            token: self.token.clone(),
        }
    }
}

fn server(accept_replace: bool) -> Srv {
    let dir = tmp("server");
    let token = generate_token().unwrap();
    let store = Store::open(&dir).unwrap();
    store
        .accept_replace_stroke_lineages
        .store(accept_replace, Ordering::SeqCst);
    let h = ServerHandle::start_with(
        "127.0.0.1:0".parse().unwrap(),
        store,
        ServerConfig {
            auth: Some(TokenVerifier::for_token(&token)),
            web_root: None,
            control: None,
        },
    )
    .unwrap();
    Srv {
        h,
        token,
        _dir: dir,
    }
}

fn put_genesis_blob(srv: &Srv) {
    let (bytes, _) = testkit::blob_background();
    let sha = sync_protocol::sha256_hex(&bytes);
    let req = ureq::request("PUT", &format!("{}/v1/blobs/{sha}", srv.url()))
        .set("Authorization", &format!("Bearer {}", srv.token));
    let r = req.send_bytes(&bytes).unwrap();
    assert!(r.status() == 200 || r.status() == 201);
}

fn bootstrap(srv: &Srv, snap: &SyncSnapshotV1) {
    put_genesis_blob(srv);
    let bytes = to_canonical_json(snap).unwrap();
    let req = ureq::request("POST", &format!("{}/v1/libraries", srv.url()))
        .set("Authorization", &format!("Bearer {}", srv.token));
    let r = req.send_bytes(&bytes).unwrap();
    assert!(r.status() == 200 || r.status() == 201, "{}", r.status());
}

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

fn add(page: PageId, s: SyncStroke) -> SyncOperationV1 {
    SyncOperationV1::AddStroke {
        notebook_id: nb(1),
        page_id: page,
        stroke: s,
    }
}

fn freeze_ops(
    sync_dir: &Path,
    library: LibraryId,
    replica: ReplicaId,
    genesis_sha: &str,
    ops: Vec<SyncOperationV1>,
    next: u64,
) -> RecoveryIntentV1 {
    fs::create_dir_all(sync_dir).unwrap();
    let mut seq = next;
    let mut frozen = Vec::new();
    for operation in ops {
        let env = SyncEnvelopeV1 {
            v: ENVELOPE_VERSION,
            operation_id: OperationId::from_uuid(Uuid::now_v7()).unwrap(),
            library_id: library,
            replica_id: replica,
            client_seq: seq,
            operation,
        };
        let bytes = env.to_canonical_bytes().unwrap();
        frozen.push(FrozenOp {
            client_seq: seq,
            operation_id: env.operation_id,
            envelope: String::from_utf8(bytes).unwrap(),
        });
        seq += 1;
    }
    let intent = RecoveryIntentV1 {
        v: RECOVERY_INTENT_VERSION,
        transaction_id: Uuid::now_v7().hyphenated().to_string(),
        phase: RecoveryPhase::SequencesAssigned,
        library_id: library,
        replica_id: replica,
        genesis_sha256: genesis_sha.into(),
        checkpoint_cursor: 0,
        expected_server_head: 0,
        acked_client_seq: next.saturating_sub(1),
        next_client_seq_after: seq,
        local_snapshot_sha256: "test".into(),
        identity_sha256: "test".into(),
        plan_fingerprint: "test".into(),
        ops: frozen,
        accepted_prefix: 0,
        upload_authorized: false,
        halt_reason: None,
        mutation_gen: 0,
        submit_attempted: false,
    };
    write_intent(sync_dir, &intent, &RecoveryExecHooks::default()).unwrap();
    ensure_outbox(sync_dir, &intent.ops).unwrap();
    intent
}

#[test]
fn non_loopback_and_protected_paths_are_refused() {
    assert!(require_loopback_url("http://127.0.0.1:9").is_ok());
    assert!(require_loopback_url("http://192.168.1.9:8080").is_err());
    assert!(require_loopback_url("http://host.example/v1").is_err());
    // Synthetic Windows-style fixture paths (no real user home directory).
    let err = require_disposable_exec_root(Path::new(
        r"C:\Users\example\Ignite-Backups-PRIVATE\Ignite-CurrentState-18E6C\x",
    ))
    .unwrap_err();
    assert!(err.contains("protected"));
    let err = require_disposable_exec_root(Path::new(
        r"C:\Users\example\Ignite-Backups-PRIVATE\Ignite-Divergence-Analysis-18D3\work\18E6D\copy",
    ))
    .unwrap_err();
    assert!(err.contains("protected"));
    require_disposable_exec_root(Path::new(
        r"C:\Users\example\Ignite-Backups-PRIVATE\Ignite-Divergence-Analysis-18D3\work\18E7\copy",
    ))
    .unwrap();
    require_disposable_exec_root(Path::new(
        r"C:\Users\example\Ignite-Backups-PRIVATE\Ignite-Divergence-Analysis-18D3\work\18E9A\copy",
    ))
    .unwrap();
    require_disposable_exec_root(Path::new(
        r"C:\Users\example\Ignite-Backups-PRIVATE\Ignite-Divergence-Analysis-18D3\work\18E9B\copy",
    ))
    .unwrap();
    require_disposable_exec_root(Path::new(
        r"C:\Users\example\Ignite-Backups-PRIVATE\Ignite-Divergence-Analysis-18D3\work\18E9C\copy",
    ))
    .unwrap();
}

#[test]
fn intent_file_blocks_ordinary_push() {
    let dir = tmp("block");
    fs::create_dir_all(&dir).unwrap();
    assert!(!recovery_blocks_ordinary_push(&dir));
    let intent = RecoveryIntentV1 {
        v: 1,
        transaction_id: "t".into(),
        phase: RecoveryPhase::Finalized,
        library_id: testkit::library(),
        replica_id: replica(1),
        genesis_sha256: "g".into(),
        checkpoint_cursor: 0,
        expected_server_head: 0,
        acked_client_seq: 0,
        next_client_seq_after: 1,
        local_snapshot_sha256: "l".into(),
        identity_sha256: "i".into(),
        plan_fingerprint: "p".into(),
        ops: Vec::new(),
        accepted_prefix: 0,
        upload_authorized: false,
        halt_reason: None,
        mutation_gen: 0,
        submit_attempted: false,
    };
    write_intent(&dir, &intent, &RecoveryExecHooks::default()).unwrap();
    assert!(recovery_blocks_ordinary_push(&dir));
}

#[test]
fn identity_crash_windows_do_not_submit() {
    let lib = tmp("idcrash").join("library");
    let g = geo();
    open_or_create_library(&lib, InkRequest::Default, &g, &paper_background(&g)).unwrap();
    ensure_sync_identity(&lib).unwrap();
    let repo = open_library_read_only(&lib).unwrap();
    let dir = repo.notebook_dir("notebook-1");
    let mut m = read_notebook_manifest_read_only(&dir).unwrap();
    insert_page_after_durable(&dir, &mut m, "page-1", &InsertPageHooks::default()).unwrap();
    let sync = tmp("idcrash-sync");
    fs::create_dir_all(&sync).unwrap();
    let identity = read_identity(&lib).unwrap().unwrap();
    let bound = crate::sync_local::build_snapshot_bound_only(&lib, &identity).unwrap();
    let cp = LibraryState::genesis(bound).unwrap();
    let spec = PrepareSpec {
        library_root: &lib,
        sync_dir: &sync,
        checkpoint: &cp,
        checkpoint_cursor: 0,
        acked_client_seq: 0,
        next_client_seq: 1,
        replica_id: replica(1),
        genesis_sha256: "g".into(),
        server_tail: &[],
        require_disposable: false,
    };
    let err = prepare_recovery(
        &spec.clone(),
        &RecoveryExecHooks {
            fail_before_identity: true,
            ..Default::default()
        },
    )
    .unwrap_err();
    assert!(err.contains("before identity"));
    assert!(!intent_path(&sync).exists());
    assert!(!inspect_identity_read_only(&lib).unwrap().complete());

    let err = prepare_recovery(
        &spec.clone(),
        &RecoveryExecHooks {
            fail_after_identity: true,
            ..Default::default()
        },
    )
    .unwrap_err();
    assert!(err.contains("after identity"));
    assert!(inspect_identity_read_only(&lib).unwrap().complete());
    assert!(!intent_path(&sync).exists());

    let err = prepare_recovery(
        &spec.clone(),
        &RecoveryExecHooks {
            fail_during_intent_write: true,
            ..Default::default()
        },
    )
    .unwrap_err();
    assert!(err.contains("intent write"));
    assert!(!intent_path(&sync).exists());

    let first = prepare_recovery(&spec, &Default::default()).unwrap();
    assert_eq!(first.ops.len(), 1);
    let again = prepare_recovery(&spec, &Default::default()).unwrap();
    assert_eq!(first.ops[0].operation_id, again.ops[0].operation_id);
    assert_eq!(first.ops[0].client_seq, again.ops[0].client_seq);
    assert!(!first.upload_authorized);
}

#[test]
fn loopback_submit_retry_and_prefix_resume() {
    let srv = server(true);
    let snap = genesis();
    bootstrap(&srv, &snap);
    let genesis_sha = sync_protocol::snapshot_sha256(&snap).unwrap().to_string();
    let lib = testkit::library();
    let page = snap.notebooks[0].pages[0].page_id;
    let ops: Vec<_> = (10..60).map(|n| add(page, orig(n))).collect();
    assert_eq!(ops.len(), 50);
    let sync = tmp("prefix");
    freeze_ops(&sync, lib, replica(1), &genesis_sha, ops, 1);
    let first = submit_recovery(
        &sync,
        None,
        &srv.target(),
        &RecoveryExecHooks {
            halt_after_new_accepts: Some(1),
            ..Default::default()
        },
    )
    .unwrap();
    match first {
        ExecOutcome::Progress {
            accepted_prefix, ..
        } => assert_eq!(accepted_prefix, 1),
        other => panic!("{other:?}"),
    }
    let mid = submit_recovery(
        &sync,
        None,
        &srv.target(),
        &RecoveryExecHooks {
            halt_after_new_accepts: Some(46),
            ..Default::default()
        },
    )
    .unwrap();
    match mid {
        ExecOutcome::Progress {
            accepted_prefix, ..
        } => assert_eq!(accepted_prefix, 47),
        other => panic!("{other:?}"),
    }
    let ids_before = read_intent(&sync).unwrap().unwrap().ops;
    srv.h
        .store
        .hooks
        .fail_drop_response_after_commit
        .store(true, Ordering::SeqCst);
    let lost = submit_recovery(
        &sync,
        None,
        &srv.target(),
        &RecoveryExecHooks {
            halt_after_new_accepts: Some(1),
            ..Default::default()
        },
    )
    .unwrap();
    match lost {
        ExecOutcome::Progress {
            accepted_prefix, ..
        } => assert_eq!(accepted_prefix, 48),
        ExecOutcome::Complete { accepted_prefix } => assert!(accepted_prefix >= 48),
        other => panic!("{other:?}"),
    }
    srv.h
        .store
        .hooks
        .fail_drop_response_after_commit
        .store(false, Ordering::SeqCst);
    let done = submit_recovery(&sync, None, &srv.target(), &Default::default()).unwrap();
    assert!(matches!(
        done,
        ExecOutcome::Complete {
            accepted_prefix: 50
        }
    ));
    let ids_after = read_intent(&sync).unwrap().unwrap().ops;
    assert_eq!(ids_before, ids_after);
    assert!(!read_intent(&sync).unwrap().unwrap().upload_authorized);
}

#[test]
fn exact_retry_after_accept_is_duplicate() {
    let srv = server(true);
    let snap = genesis();
    bootstrap(&srv, &snap);
    let genesis_sha = sync_protocol::snapshot_sha256(&snap).unwrap().to_string();
    let page = snap.notebooks[0].pages[0].page_id;
    let sync = tmp("dup");
    freeze_ops(
        &sync,
        testkit::library(),
        replica(1),
        &genesis_sha,
        vec![add(page, orig(3))],
        1,
    );
    assert!(matches!(
        submit_recovery(&sync, None, &srv.target(), &Default::default()).unwrap(),
        ExecOutcome::Complete { accepted_prefix: 1 }
    ));
    assert!(matches!(
        submit_recovery(&sync, None, &srv.target(), &Default::default()).unwrap(),
        ExecOutcome::Complete { accepted_prefix: 1 }
    ));
}

#[test]
fn gate_off_halts_without_accepting_replacement() {
    let srv = server(false);
    let snap = genesis();
    bootstrap(&srv, &snap);
    let genesis_sha = sync_protocol::snapshot_sha256(&snap).unwrap().to_string();
    let page = snap.notebooks[0].pages[0].page_id;
    let root = orig(1);
    let op = SyncOperationV1::ReplaceStrokeLineages {
        notebook_id: nb(1),
        page_id: page,
        replacements: vec![LineageReplacement {
            root_id: root.root_id,
            expected: vec![root.clone()],
            after: vec![line(
                sk(9),
                root.root_id,
                (200.0, 400.0),
                (400.0, 400.0),
                4,
                true,
            )],
        }],
    };
    let sync = tmp("gate");
    freeze_ops(
        &sync,
        testkit::library(),
        replica(1),
        &genesis_sha,
        vec![op],
        1,
    );
    match submit_recovery(&sync, None, &srv.target(), &Default::default()).unwrap() {
        ExecOutcome::Halted {
            accepted_prefix,
            reason,
        } => {
            assert_eq!(accepted_prefix, 0);
            assert!(reason.contains("gate"), "{reason}");
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn auth_refusal_does_not_remint() {
    let srv = server(true);
    let snap = genesis();
    bootstrap(&srv, &snap);
    let genesis_sha = sync_protocol::snapshot_sha256(&snap).unwrap().to_string();
    let page = snap.notebooks[0].pages[0].page_id;
    let sync = tmp("auth");
    let intent = freeze_ops(
        &sync,
        testkit::library(),
        replica(1),
        &genesis_sha,
        vec![add(page, orig(4))],
        1,
    );
    let bad = SubmitTarget {
        base_url: srv.url(),
        token: "nope".into(),
    };
    match submit_recovery(&sync, None, &bad, &Default::default()).unwrap() {
        ExecOutcome::Halted {
            accepted_prefix, ..
        } => assert_eq!(accepted_prefix, 0),
        other => panic!("{other:?}"),
    }
    assert_eq!(
        read_intent(&sync).unwrap().unwrap().ops[0].operation_id,
        intent.ops[0].operation_id
    );
}

#[test]
fn cas_conflict_preserves_accepted_prefix() {
    let srv = server(true);
    let snap = genesis();
    bootstrap(&srv, &snap);
    let genesis_sha = sync_protocol::snapshot_sha256(&snap).unwrap().to_string();
    let page = snap.notebooks[0].pages[0].page_id;
    let mut remote = Author::new(replica(2));
    remote.library = testkit::library();
    let first = snap.notebooks[0].pages[0].strokes[1].clone();
    let steal = remote.env(SyncOperationV1::ReplaceStrokeLineages {
        notebook_id: nb(1),
        page_id: page,
        replacements: vec![LineageReplacement {
            root_id: first.root_id,
            expected: vec![first.clone()],
            after: vec![line(
                sk(8),
                first.root_id,
                (200.0, 300.0),
                (400.0, 300.0),
                4,
                true,
            )],
        }],
    });
    let body = format!(
        "{{\"envelopes\":[{}]}}",
        String::from_utf8(steal.to_canonical_bytes().unwrap()).unwrap()
    );
    let req = ureq::request(
        "POST",
        &format!(
            "{}/v1/libraries/{}/operations",
            srv.url(),
            testkit::library()
        ),
    )
    .set("Authorization", &format!("Bearer {}", srv.token));
    assert_eq!(req.send_bytes(body.as_bytes()).unwrap().status(), 200);

    let recovery = SyncOperationV1::ReplaceStrokeLineages {
        notebook_id: nb(1),
        page_id: page,
        replacements: vec![LineageReplacement {
            root_id: first.root_id,
            expected: vec![first.clone()],
            after: vec![line(
                sk(9),
                first.root_id,
                (200.0, 400.0),
                (400.0, 400.0),
                4,
                true,
            )],
        }],
    };
    let sync = tmp("cas");
    freeze_ops(
        &sync,
        testkit::library(),
        replica(1),
        &genesis_sha,
        vec![add(page, orig(20)), recovery],
        1,
    );
    match submit_recovery(&sync, None, &srv.target(), &Default::default()).unwrap() {
        ExecOutcome::Halted {
            accepted_prefix,
            reason,
        } => {
            assert_eq!(accepted_prefix, 1);
            assert!(
                reason.contains("semantic")
                    || reason.contains("CAS")
                    || reason.contains("expected"),
                "{reason}"
            );
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(read_intent(&sync).unwrap().unwrap().accepted_prefix, 1);
}

#[test]
fn concurrent_unrelated_remote_stroke_is_kept() {
    let srv = server(true);
    let snap = genesis();
    bootstrap(&srv, &snap);
    let genesis_sha = sync_protocol::snapshot_sha256(&snap).unwrap().to_string();
    let p1 = snap.notebooks[0].pages[0].page_id;
    let p2 = snap.notebooks[0].pages[1].page_id;
    let sync = tmp("merge");
    freeze_ops(
        &sync,
        testkit::library(),
        replica(1),
        &genesis_sha,
        vec![add(p1, orig(21))],
        1,
    );
    let mut remote = Author::new(replica(2));
    remote.library = testkit::library();
    let foreign = remote.env(add(p2, orig(22)));
    let body = format!(
        "{{\"envelopes\":[{}]}}",
        String::from_utf8(foreign.to_canonical_bytes().unwrap()).unwrap()
    );
    let req = ureq::request(
        "POST",
        &format!(
            "{}/v1/libraries/{}/operations",
            srv.url(),
            testkit::library()
        ),
    )
    .set("Authorization", &format!("Bearer {}", srv.token));
    assert_eq!(req.send_bytes(body.as_bytes()).unwrap().status(), 200);
    assert!(matches!(
        submit_recovery(&sync, None, &srv.target(), &Default::default()).unwrap(),
        ExecOutcome::Complete { accepted_prefix: 1 }
    ));
    let req = ureq::request(
        "GET",
        &format!("{}/v1/libraries/{}/snapshot", srv.url(), testkit::library()),
    )
    .set("Authorization", &format!("Bearer {}", srv.token));
    let mut bytes = Vec::new();
    std::io::Read::read_to_end(&mut req.call().unwrap().into_reader(), &mut bytes).unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let got: SyncSnapshotV1 = serde_json::from_value(v["snapshot"].clone()).unwrap();
    assert_eq!(got.notebooks[0].pages[0].strokes.len(), 5);
    assert_eq!(got.notebooks[0].pages[1].strokes.len(), 1);
}

#[test]
fn payload_mismatch_on_same_operation_id_refuses() {
    let srv = server(true);
    let snap = genesis();
    bootstrap(&srv, &snap);
    let genesis_sha = sync_protocol::snapshot_sha256(&snap).unwrap().to_string();
    let page = snap.notebooks[0].pages[0].page_id;
    let sync = tmp("mismatch");
    let mut intent = freeze_ops(
        &sync,
        testkit::library(),
        replica(1),
        &genesis_sha,
        vec![add(page, orig(5))],
        1,
    );
    submit_recovery(&sync, None, &srv.target(), &Default::default()).unwrap();
    let mut env: SyncEnvelopeV1 = serde_json::from_str(&intent.ops[0].envelope).unwrap();
    env.operation = add(page, orig(6));
    intent.ops[0].envelope = String::from_utf8(env.to_canonical_bytes().unwrap()).unwrap();
    intent.phase = RecoveryPhase::SequencesAssigned;
    intent.accepted_prefix = 0;
    write_intent(&sync, &intent, &Default::default()).unwrap();
    fs::write(sync.join(OUTBOX_FILE), b"").unwrap();
    ensure_outbox(&sync, &intent.ops).unwrap();
    match submit_recovery(&sync, None, &srv.target(), &Default::default()).unwrap() {
        ExecOutcome::Halted { reason, .. } => {
            assert!(
                reason.contains("conflict")
                    || reason.contains("409")
                    || reason.contains("different"),
                "{reason}"
            );
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn finalize_keeps_upload_unauthorized() {
    let dir = tmp("fin");
    fs::create_dir_all(&dir).unwrap();
    let intent = RecoveryIntentV1 {
        v: 1,
        transaction_id: "t".into(),
        phase: RecoveryPhase::FullyAccepted,
        library_id: testkit::library(),
        replica_id: replica(1),
        genesis_sha256: "g".into(),
        checkpoint_cursor: 0,
        expected_server_head: 0,
        acked_client_seq: 0,
        next_client_seq_after: 1,
        local_snapshot_sha256: "l".into(),
        identity_sha256: "i".into(),
        plan_fingerprint: "p".into(),
        ops: Vec::new(),
        accepted_prefix: 0,
        upload_authorized: false,
        halt_reason: None,
        mutation_gen: 0,
        submit_attempted: false,
    };
    write_intent(&dir, &intent, &Default::default()).unwrap();
    mark_verified(&dir).unwrap();
    let done = mark_finalized(&dir).unwrap();
    assert_eq!(done.phase, RecoveryPhase::Finalized);
    assert!(!done.upload_authorized);
    assert!(recovery_blocks_ordinary_push(&dir));
}

#[test]
fn incomplete_prefix_refuses_reconcile_and_finalize() {
    let dir = tmp("incomplete");
    fs::create_dir_all(&dir).unwrap();
    let intent = RecoveryIntentV1 {
        v: 1,
        transaction_id: "t".into(),
        phase: RecoveryPhase::PartiallyAccepted,
        library_id: testkit::library(),
        replica_id: replica(1),
        genesis_sha256: "g".into(),
        checkpoint_cursor: 0,
        expected_server_head: 0,
        acked_client_seq: 0,
        next_client_seq_after: 2,
        local_snapshot_sha256: "l".into(),
        identity_sha256: "i".into(),
        plan_fingerprint: "p".into(),
        ops: vec![
            FrozenOp {
                client_seq: 1,
                operation_id: OperationId::from_uuid(Uuid::now_v7()).unwrap(),
                envelope: "{}".into(),
            },
            FrozenOp {
                client_seq: 2,
                operation_id: OperationId::from_uuid(Uuid::now_v7()).unwrap(),
                envelope: "{}".into(),
            },
        ],
        accepted_prefix: 1,
        upload_authorized: false,
        halt_reason: None,
        mutation_gen: 0,
        submit_attempted: false,
    };
    write_intent(&dir, &intent, &Default::default()).unwrap();
    let err = mark_verified(&dir).unwrap_err();
    assert!(err.contains("incomplete"), "{err}");
    assert_eq!(
        read_intent(&dir).unwrap().unwrap().phase,
        RecoveryPhase::PartiallyAccepted
    );
}

#[test]
fn prefix_persist_crash_reconstructs_from_tail() {
    let srv = server(true);
    let snap = genesis();
    bootstrap(&srv, &snap);
    let genesis_sha = sync_protocol::snapshot_sha256(&snap).unwrap().to_string();
    let page = snap.notebooks[0].pages[0].page_id;
    let sync = tmp("persistcrash");
    freeze_ops(
        &sync,
        testkit::library(),
        replica(1),
        &genesis_sha,
        vec![add(page, orig(30)), add(page, orig(31))],
        1,
    );
    let first = submit_recovery(
        &sync,
        None,
        &srv.target(),
        &RecoveryExecHooks {
            halt_after_new_accepts: Some(1),
            ..Default::default()
        },
    )
    .unwrap();
    match first {
        ExecOutcome::Progress {
            accepted_prefix, ..
        } => assert_eq!(accepted_prefix, 1),
        other => panic!("{other:?}"),
    }
    let err = submit_recovery(
        &sync,
        None,
        &srv.target(),
        &RecoveryExecHooks {
            fail_during_intent_write: true,
            halt_after_new_accepts: Some(1),
            ..Default::default()
        },
    )
    .unwrap_err();
    assert!(err.contains("intent write"), "{err}");
    assert_eq!(read_intent(&sync).unwrap().unwrap().accepted_prefix, 1);
    let done = submit_recovery(&sync, None, &srv.target(), &Default::default()).unwrap();
    assert!(matches!(done, ExecOutcome::Complete { accepted_prefix: 2 }));
}

#[test]
fn same_seq_different_operation_id_refuses() {
    let srv = server(true);
    let snap = genesis();
    bootstrap(&srv, &snap);
    let genesis_sha = sync_protocol::snapshot_sha256(&snap).unwrap().to_string();
    let page = snap.notebooks[0].pages[0].page_id;
    let sync = tmp("seqcollision");
    let mut intent = freeze_ops(
        &sync,
        testkit::library(),
        replica(1),
        &genesis_sha,
        vec![add(page, orig(40))],
        1,
    );
    submit_recovery(&sync, None, &srv.target(), &Default::default()).unwrap();
    let mut env: SyncEnvelopeV1 = serde_json::from_str(&intent.ops[0].envelope).unwrap();
    env.operation_id = OperationId::from_uuid(Uuid::now_v7()).unwrap();
    intent.ops[0].operation_id = env.operation_id;
    intent.ops[0].envelope = String::from_utf8(env.to_canonical_bytes().unwrap()).unwrap();
    intent.phase = RecoveryPhase::SequencesAssigned;
    intent.accepted_prefix = 0;
    write_intent(&sync, &intent, &Default::default()).unwrap();
    fs::write(sync.join(OUTBOX_FILE), b"").unwrap();
    ensure_outbox(&sync, &intent.ops).unwrap();
    match submit_recovery(&sync, None, &srv.target(), &Default::default()).unwrap() {
        ExecOutcome::Halted { reason, .. } => {
            assert!(
                reason.contains("conflict") || reason.contains("client_seq"),
                "{reason}"
            );
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn http_500_before_commit_does_not_advance_prefix() {
    let srv = server(true);
    let snap = genesis();
    bootstrap(&srv, &snap);
    let genesis_sha = sync_protocol::snapshot_sha256(&snap).unwrap().to_string();
    let page = snap.notebooks[0].pages[0].page_id;
    let sync = tmp("precommit");
    freeze_ops(
        &sync,
        testkit::library(),
        replica(1),
        &genesis_sha,
        vec![add(page, orig(7))],
        1,
    );
    srv.h
        .store
        .hooks
        .fail_submit_before_commit
        .store(true, Ordering::SeqCst);
    match submit_recovery(&sync, None, &srv.target(), &Default::default()).unwrap() {
        ExecOutcome::Halted {
            accepted_prefix, ..
        }
        | ExecOutcome::Progress {
            accepted_prefix, ..
        } => assert_eq!(accepted_prefix, 0),
        other => panic!("{other:?}"),
    }
    srv.h
        .store
        .hooks
        .fail_submit_before_commit
        .store(false, Ordering::SeqCst);
    assert!(matches!(
        submit_recovery(&sync, None, &srv.target(), &Default::default()).unwrap(),
        ExecOutcome::Complete { accepted_prefix: 1 }
    ));
}

#[test]
fn library_prepare_then_loopback_submit_create_page() {
    let srv = server(true);
    let lib = tmp("libe2e").join("library");
    let sync = tmp("libe2e-sync");
    let g = geo();
    open_or_create_library(&lib, InkRequest::Default, &g, &paper_background(&g)).unwrap();
    ensure_sync_identity(&lib).unwrap();
    let identity = read_identity(&lib).unwrap().unwrap();
    let snap0 = build_snapshot_read_only(&lib, &identity).unwrap();
    let cp = LibraryState::genesis(snap0.clone()).unwrap();
    let bytes = snap0.to_canonical_bytes().unwrap();
    for page in snap0.notebooks.iter().flat_map(|n| &n.pages) {
        if let BackgroundRef::Blob { sha256, .. } = &page.background {
            let raw = crate::store::load_page_read_only(
                &open_library_read_only(&lib)
                    .unwrap()
                    .notebook_dir("notebook-1"),
                "page-1",
            )
            .unwrap()
            .background;
            let req = ureq::request("PUT", &format!("{}/v1/blobs/{sha256}", srv.url()))
                .set("Authorization", &format!("Bearer {}", srv.token));
            let _ = req.send_bytes(&raw);
        }
    }
    let req = ureq::request("POST", &format!("{}/v1/libraries", srv.url()))
        .set("Authorization", &format!("Bearer {}", srv.token));
    let st = req.send_bytes(&bytes).unwrap().status();
    assert!(st == 200 || st == 201, "{st}");
    let repo = open_library_read_only(&lib).unwrap();
    let nbdir = repo.notebook_dir("notebook-1");
    let mut m = read_notebook_manifest_read_only(&nbdir).unwrap();
    insert_page_after_durable(&nbdir, &mut m, "page-1", &InsertPageHooks::default()).unwrap();
    fs::create_dir_all(&sync).unwrap();
    let spec = PrepareSpec {
        library_root: &lib,
        sync_dir: &sync,
        checkpoint: &cp,
        checkpoint_cursor: 0,
        acked_client_seq: 0,
        next_client_seq: 1,
        replica_id: replica(1),
        genesis_sha256: sync_protocol::snapshot_sha256(&snap0).unwrap().to_string(),
        server_tail: &[],
        require_disposable: false,
    };
    let prepared = prepare_recovery(&spec, &Default::default()).unwrap();
    assert_eq!(prepared.ops.len(), 1);
    let local = build_snapshot_read_only(&lib, &read_identity(&lib).unwrap().unwrap()).unwrap();
    let replayed = replay_frozen(&cp, &prepared).unwrap();
    assert_eq!(replayed.snapshot, local);
    let out = submit_recovery(&sync, Some(&lib), &srv.target(), &Default::default()).unwrap();
    assert!(matches!(out, ExecOutcome::Complete { accepted_prefix: 1 }));
    let local_after =
        build_snapshot_read_only(&lib, &read_identity(&lib).unwrap().unwrap()).unwrap();
    assert_eq!(
        local, local_after,
        "acceptance must not duplicate local edits"
    );
    reconcile_confirmed(&sync, &cp, &local_after).unwrap();
    mark_verified(&sync).unwrap();
    mark_finalized(&sync).unwrap();
    assert!(!read_intent(&sync).unwrap().unwrap().upload_authorized);
    assert!(matches!(
        submit_recovery(&sync, Some(&lib), &srv.target(), &Default::default()).unwrap(),
        ExecOutcome::Complete { accepted_prefix: 1 }
    ));
    assert_eq!(read_intent(&sync).unwrap().unwrap().ops, prepared.ops);
}

#[test]
fn prepare_is_idempotent_across_reopen() {
    let lib = tmp("reopen").join("library");
    let sync = tmp("reopen-sync");
    let g = geo();
    open_or_create_library(&lib, InkRequest::Default, &g, &paper_background(&g)).unwrap();
    ensure_sync_identity(&lib).unwrap();
    let repo = open_library_read_only(&lib).unwrap();
    let dir = repo.notebook_dir("notebook-1");
    let mut m = read_notebook_manifest_read_only(&dir).unwrap();
    insert_page_after_durable(&dir, &mut m, "page-1", &InsertPageHooks::default()).unwrap();
    let after = m.page_ids.last().unwrap().clone();
    insert_page_after_durable(&dir, &mut m, &after, &InsertPageHooks::default()).unwrap();
    fs::create_dir_all(&sync).unwrap();
    let identity = read_identity(&lib).unwrap().unwrap();
    let bound = crate::sync_local::build_snapshot_bound_only(&lib, &identity).unwrap();
    let cp = LibraryState::genesis(bound).unwrap();
    let spec = PrepareSpec {
        library_root: &lib,
        sync_dir: &sync,
        checkpoint: &cp,
        checkpoint_cursor: 0,
        acked_client_seq: 0,
        next_client_seq: 1,
        replica_id: replica(1),
        genesis_sha256: "g".into(),
        server_tail: &[],
        require_disposable: false,
    };
    let a = prepare_recovery(&spec, &Default::default()).unwrap();
    assert_eq!(a.ops.len(), 2);
    let b = prepare_recovery(&spec, &Default::default()).unwrap();
    assert_eq!(a.transaction_id, b.transaction_id);
    assert_eq!(a.ops, b.ops);
}

#[test]
fn checkpoint_namespace_includes_live_bindings_and_refuses_empty_stand_in() {
    let (lib, _sync, cp) = extra_pages_lib("ckns");
    let identity = read_identity(&lib).unwrap().unwrap();
    let ns = checkpoint_namespace(&cp, identity.library_id, &[]).unwrap();
    assert!(!ns.is_empty());
    for pages in identity.pages.values() {
        for pid in pages.values() {
            assert!(ns.contains(pid));
        }
    }
}

#[test]
fn invalid_checkpoint_library_mismatch_refuses_prepare() {
    let (lib, sync, mut cp) = extra_pages_lib("ckbad");
    cp.snapshot.library_id = LibraryId::from_uuid(Uuid::now_v7()).unwrap();
    let spec = extra_pages_spec(&lib, &sync, &cp);
    let err = prepare_recovery(&spec, &Default::default()).unwrap_err();
    assert!(
        err.contains("checkpoint library") || err.contains("invalid checkpoint"),
        "{err}"
    );
    assert!(read_intent(&sync).unwrap().is_none());
}

#[test]
fn checkpoint_page_id_not_in_local_bindings_collides() {
    let (lib, _sync, cp) = extra_pages_lib("ckcol");
    let identity = read_identity(&lib).unwrap().unwrap();
    let (_from, plan) = plan_sync_identity(&lib).unwrap();
    assert_eq!(plan.allocated_pages.len(), 2);
    let mut ns = checkpoint_namespace(&cp, identity.library_id, &[]).unwrap();
    ns.insert(plan.allocated_pages[0].sync_id);
    let err = checkpoint_collision(&plan, &ns).unwrap_err();
    assert!(err.contains("collides with checkpoint"));
}

#[test]
fn retired_allocated_id_is_refused() {
    let (lib, _sync, _cp) = extra_pages_lib("retired");
    let (_from, mut plan) = plan_sync_identity(&lib).unwrap();
    let pid = plan.allocated_pages[0].sync_id;
    let nb = *plan.identity.notebooks.values().next().unwrap();
    plan.identity.retired_pages.insert(
        pid,
        RetiredPage {
            notebook_id: nb,
            local_id: "gone".into(),
        },
    );
    let err = checkpoint_collision(&plan, &BTreeSet::new()).unwrap_err();
    assert!(err.contains("retired"));
}

#[test]
fn prepare_with_no_unbound_pages_freezes_empty_plan() {
    let lib = tmp("nounbound").join("library");
    let sync = tmp("nounbound-sync");
    let g = geo();
    open_or_create_library(&lib, InkRequest::Default, &g, &paper_background(&g)).unwrap();
    ensure_sync_identity(&lib).unwrap();
    fs::create_dir_all(&sync).unwrap();
    let identity = read_identity(&lib).unwrap().unwrap();
    let snap = build_snapshot_read_only(&lib, &identity).unwrap();
    let cp = LibraryState::genesis(snap).unwrap();
    let spec = extra_pages_spec(&lib, &sync, &cp);
    let prepared = prepare_recovery(&spec, &Default::default()).unwrap();
    assert!(prepared.ops.is_empty());
    assert_eq!(prepared.phase, RecoveryPhase::SequencesAssigned);
}

#[test]
fn prepare_recovery_reuses_durable_bindings_and_frozen_envelopes() {
    let (lib, sync, cp) = extra_pages_lib("reuse");
    let spec = extra_pages_spec(&lib, &sync, &cp);
    let a = prepare_recovery(&spec, &Default::default()).unwrap();
    let ids_a = read_identity(&lib).unwrap().unwrap();
    let b = prepare_recovery(&spec, &Default::default()).unwrap();
    let ids_b = read_identity(&lib).unwrap().unwrap();
    assert_eq!(a.transaction_id, b.transaction_id);
    assert_eq!(a.ops, b.ops);
    assert_eq!(ids_a, ids_b);
    assert_eq!(a.ops.len(), 2);
}

#[test]
fn submit_refuses_stale_local_snapshot_stroke() {
    let (lib, sync, cp) = extra_pages_lib("stale-snap");
    let spec = extra_pages_spec(&lib, &sync, &cp);
    prepare_recovery(&spec, &Default::default()).unwrap();
    let err = try_append_stroke(&lib).unwrap_err();
    assert!(err.contains("RECOVERY MAINTENANCE"), "{err}");
    assert!(read_intent(&sync).unwrap().is_some());
}

#[test]
fn submit_refuses_page_insert_after_prepare() {
    let (lib, sync, cp) = extra_pages_lib("stale-page");
    let spec = extra_pages_spec(&lib, &sync, &cp);
    prepare_recovery(&spec, &Default::default()).unwrap();
    let repo = open_library_read_only(&lib).unwrap();
    let dir = repo.notebook_dir("notebook-1");
    let mut m = read_notebook_manifest_read_only(&dir).unwrap();
    let after = m.page_ids.last().unwrap().clone();
    let err =
        insert_page_after_durable(&dir, &mut m, &after, &InsertPageHooks::default()).unwrap_err();
    assert!(err.contains("RECOVERY MAINTENANCE"), "{err}");
    assert!(read_intent(&sync).unwrap().is_some());
}

#[test]
fn submit_refuses_mutation_gen_change() {
    let (lib, sync, cp) = extra_pages_lib("stale-gen");
    let spec = extra_pages_spec(&lib, &sync, &cp);
    prepare_recovery(&spec, &Default::default()).unwrap();
    let cur = read_mutation_gen(&lib).unwrap();
    fs::write(
        lib.join(crate::sync_local::SYNC_DIR)
            .join(crate::sync_verified::MUTATION_GEN_FILE),
        format!("{}", cur + 1),
    )
    .unwrap();
    assert_halted_contains(
        submit_recovery(&sync, Some(&lib), &dummy_loopback(), &Default::default()).unwrap(),
        "mutation generation",
    );
}

#[test]
fn submit_refuses_identity_sha_change() {
    let (lib, sync, cp) = extra_pages_lib("stale-id");
    let spec = extra_pages_spec(&lib, &sync, &cp);
    let prepared = prepare_recovery(&spec, &Default::default()).unwrap();
    let mut intent = prepared;
    intent.identity_sha256 = sha256_hex(b"tampered-identity").to_string();
    write_intent(&sync, &intent, &Default::default()).unwrap();
    assert_halted_contains(
        submit_recovery(&sync, Some(&lib), &dummy_loopback(), &Default::default()).unwrap(),
        "identity bindings",
    );
}

#[test]
fn submit_refuses_sequence_authority_change() {
    let (lib, sync, cp) = extra_pages_lib("stale-seq");
    fs::write(
        sync.join(STATE_FILE),
        to_canonical_json(&dummy_sync_state(cp.snapshot.library_id, 1, 0)).unwrap(),
    )
    .unwrap();
    let spec = extra_pages_spec(&lib, &sync, &cp);
    prepare_recovery(&spec, &Default::default()).unwrap();
    let mut st: SyncStateV1 =
        serde_json::from_slice(&fs::read(sync.join(STATE_FILE)).unwrap()).unwrap();
    st.next_client_seq = 99;
    fs::write(sync.join(STATE_FILE), to_canonical_json(&st).unwrap()).unwrap();
    assert_halted_contains(
        submit_recovery(&sync, Some(&lib), &dummy_loopback(), &Default::default()).unwrap(),
        "sequence authority",
    );
}

#[test]
fn submit_refuses_confirmed_cursor_change() {
    let (lib, sync, cp) = extra_pages_lib("stale-cur");
    fs::write(
        sync.join(STATE_FILE),
        to_canonical_json(&dummy_sync_state(cp.snapshot.library_id, 1, 0)).unwrap(),
    )
    .unwrap();
    let spec = extra_pages_spec(&lib, &sync, &cp);
    prepare_recovery(&spec, &Default::default()).unwrap();
    let mut st: SyncStateV1 =
        serde_json::from_slice(&fs::read(sync.join(STATE_FILE)).unwrap()).unwrap();
    st.server_cursor = 7;
    fs::write(sync.join(STATE_FILE), to_canonical_json(&st).unwrap()).unwrap();
    assert_halted_contains(
        submit_recovery(&sync, Some(&lib), &dummy_loopback(), &Default::default()).unwrap(),
        "confirmed cursor",
    );
}

#[test]
fn submit_refuses_outbox_payload_and_operation_id_changes() {
    let snap = genesis();
    let genesis_sha = sync_protocol::snapshot_sha256(&snap).unwrap().to_string();
    let page = snap.notebooks[0].pages[0].page_id;
    let library = testkit::library();

    let sync = tmp("stale-out");
    freeze_ops(
        &sync,
        library,
        replica(1),
        &genesis_sha,
        vec![add(page, orig(1))],
        1,
    );
    let extra = add(page, orig(2));
    let env = SyncEnvelopeV1 {
        v: ENVELOPE_VERSION,
        operation_id: OperationId::from_uuid(Uuid::now_v7()).unwrap(),
        library_id: library,
        replica_id: replica(1),
        client_seq: 2,
        operation: extra,
    };
    let bytes = env.to_canonical_bytes().unwrap();
    append_journal(
        &sync,
        &[PendingOp {
            seq: 2,
            envelope: env,
            bytes,
        }],
    )
    .unwrap();
    assert_halted_contains(
        submit_recovery(&sync, None, &dummy_loopback(), &Default::default()).unwrap(),
        "outside the frozen plan",
    );

    let sync = tmp("stale-pay");
    let intent = freeze_ops(
        &sync,
        library,
        replica(1),
        &genesis_sha,
        vec![add(page, orig(1))],
        1,
    );
    let mut env: SyncEnvelopeV1 = from_json(intent.ops[0].envelope.as_bytes()).unwrap();
    env.operation = add(page, orig(99));
    let bytes = env.to_canonical_bytes().unwrap();
    fs::write(sync.join(OUTBOX_FILE), b"").unwrap();
    append_journal(
        &sync,
        &[PendingOp {
            seq: env.client_seq,
            envelope: env,
            bytes,
        }],
    )
    .unwrap();
    assert_halted_contains(
        submit_recovery(&sync, None, &dummy_loopback(), &Default::default()).unwrap(),
        "does not match frozen envelope",
    );

    let sync = tmp("stale-opid");
    let mut intent = freeze_ops(
        &sync,
        library,
        replica(1),
        &genesis_sha,
        vec![add(page, orig(1))],
        1,
    );
    intent.ops[0].operation_id = OperationId::from_uuid(Uuid::now_v7()).unwrap();
    write_intent(&sync, &intent, &Default::default()).unwrap();
    assert_halted_contains(
        submit_recovery(&sync, None, &dummy_loopback(), &Default::default()).unwrap(),
        "frozen envelope identity mismatch",
    );
}

#[test]
fn concurrent_prepare_yields_one_durable_intent() {
    let (lib, sync, cp) = extra_pages_lib("conc");
    let lib_a = lib.clone();
    let lib_b = lib.clone();
    let sync_a = sync.clone();
    let sync_b = sync.clone();
    let cp_a = cp.clone();
    let cp_b = cp.clone();
    let h1 = thread::spawn(move || {
        let spec = extra_pages_spec(&lib_a, &sync_a, &cp_a);
        prepare_recovery(&spec, &Default::default())
    });
    let h2 = thread::spawn(move || {
        let spec = extra_pages_spec(&lib_b, &sync_b, &cp_b);
        prepare_recovery(&spec, &Default::default())
    });
    let r1 = h1.join().unwrap();
    let r2 = h2.join().unwrap();
    let wins = [&r1, &r2].iter().filter(|r| r.is_ok()).count();
    assert!(wins >= 1, "r1={r1:?} r2={r2:?}");
    let intent = read_intent(&sync).unwrap().unwrap();
    assert_eq!(intent.ops.len(), 2);
    if let (Ok(a), Ok(b)) = (&r1, &r2) {
        assert_eq!(a.transaction_id, b.transaction_id);
        assert_eq!(a.ops, b.ops);
    }
}

#[test]
fn abort_before_submit_retains_intent_and_does_not_rewind_sequences() {
    let (lib, sync, cp) = extra_pages_lib("abort-pre");
    fs::write(
        sync.join(STATE_FILE),
        to_canonical_json(&dummy_sync_state(cp.snapshot.library_id, 1, 0)).unwrap(),
    )
    .unwrap();
    let spec = extra_pages_spec(&lib, &sync, &cp);
    let prepared = prepare_recovery(&spec, &Default::default()).unwrap();
    let next_after = prepared.next_client_seq_after;
    let report = abort_prepared_recovery(&sync).unwrap();
    assert!(!report.cleanup_supported);
    assert!(report.intent_retained);
    let intent = read_intent(&sync).unwrap().unwrap();
    assert_eq!(intent.ops, prepared.ops);
    assert_eq!(intent.phase, RecoveryPhase::Halted);
    let st: SyncStateV1 =
        serde_json::from_slice(&fs::read(sync.join(STATE_FILE)).unwrap()).unwrap();
    assert_eq!(st.next_client_seq, next_after);
    match submit_recovery(&sync, Some(&lib), &dummy_loopback(), &Default::default()).unwrap() {
        ExecOutcome::Halted { .. } => {}
        other => panic!("{other:?}"),
    }
}

#[test]
fn abort_after_ambiguous_submit_retains_intent() {
    let srv = server(true);
    let snap = genesis();
    bootstrap(&srv, &snap);
    let genesis_sha = sync_protocol::snapshot_sha256(&snap).unwrap().to_string();
    let page = snap.notebooks[0].pages[0].page_id;
    let sync = tmp("abort-amb");
    freeze_ops(
        &sync,
        testkit::library(),
        replica(1),
        &genesis_sha,
        vec![add(page, orig(1)), add(page, orig(2))],
        1,
    );
    srv.h
        .store
        .hooks
        .fail_submit_before_commit
        .store(true, Ordering::SeqCst);
    match submit_recovery(&sync, None, &srv.target(), &Default::default()).unwrap() {
        ExecOutcome::Halted {
            accepted_prefix, ..
        }
        | ExecOutcome::Progress {
            accepted_prefix, ..
        } => assert_eq!(accepted_prefix, 0),
        other => panic!("{other:?}"),
    }
    let intent = read_intent(&sync).unwrap().unwrap();
    assert!(intent.submit_attempted);
    let report = abort_prepared_recovery(&sync).unwrap();
    assert!(!report.cleanup_supported);
    assert!(report.intent_retained);
    assert!(read_intent(&sync).unwrap().unwrap().submit_attempted);
    match submit_recovery(&sync, None, &srv.target(), &Default::default()).unwrap() {
        ExecOutcome::Halted { .. } => {}
        other => panic!("{other:?}"),
    }
}

#[test]
fn mutation_after_revalidate_is_caught_before_post() {
    let srv = server(true);
    let lib = tmp("mut-after").join("library");
    let sync = tmp("mut-after-sync");
    let g = geo();
    open_or_create_library(&lib, InkRequest::Default, &g, &paper_background(&g)).unwrap();
    ensure_sync_identity(&lib).unwrap();
    let identity = read_identity(&lib).unwrap().unwrap();
    let snap0 = build_snapshot_read_only(&lib, &identity).unwrap();
    let cp = LibraryState::genesis(snap0.clone()).unwrap();
    let bytes = snap0.to_canonical_bytes().unwrap();
    for page in snap0.notebooks.iter().flat_map(|n| &n.pages) {
        if let BackgroundRef::Blob { sha256, .. } = &page.background {
            let raw = crate::store::load_page_read_only(
                &open_library_read_only(&lib)
                    .unwrap()
                    .notebook_dir("notebook-1"),
                "page-1",
            )
            .unwrap()
            .background;
            let req = ureq::request("PUT", &format!("{}/v1/blobs/{sha256}", srv.url()))
                .set("Authorization", &format!("Bearer {}", srv.token));
            let _ = req.send_bytes(&raw);
        }
    }
    let req = ureq::request("POST", &format!("{}/v1/libraries", srv.url()))
        .set("Authorization", &format!("Bearer {}", srv.token));
    let st = req.send_bytes(&bytes).unwrap().status();
    assert!(st == 200 || st == 201, "{st}");
    let repo = open_library_read_only(&lib).unwrap();
    let nbdir = repo.notebook_dir("notebook-1");
    let mut m = read_notebook_manifest_read_only(&nbdir).unwrap();
    insert_page_after_durable(&nbdir, &mut m, "page-1", &InsertPageHooks::default()).unwrap();
    fs::create_dir_all(&sync).unwrap();
    let spec = PrepareSpec {
        library_root: &lib,
        sync_dir: &sync,
        checkpoint: &cp,
        checkpoint_cursor: 0,
        acked_client_seq: 0,
        next_client_seq: 1,
        replica_id: replica(1),
        genesis_sha256: sync_protocol::snapshot_sha256(&snap0).unwrap().to_string(),
        server_tail: &[],
        require_disposable: false,
    };
    prepare_recovery(&spec, &Default::default()).unwrap();
    *AFTER_MUTATE_LIB.lock().unwrap() = Some(lib.clone());
    let out = submit_recovery(
        &sync,
        Some(&lib),
        &srv.target(),
        &RecoveryExecHooks {
            after_revalidate: Some(mutate_after_revalidate_hook),
            ..Default::default()
        },
    )
    .unwrap();
    match out {
        ExecOutcome::Complete { accepted_prefix } => assert_eq!(accepted_prefix, 1),
        other => panic!("fence must keep POST valid: {other:?}"),
    }
}

#[test]
fn post_binding_fingerprint_is_stable_until_submit() {
    let (lib, sync, cp) = extra_pages_lib("match-fp");
    let spec = extra_pages_spec(&lib, &sync, &cp);
    let prepared = prepare_recovery(&spec, &Default::default()).unwrap();
    assert_eq!(prepared.mutation_gen, read_mutation_gen(&lib).unwrap());
    let identity = read_identity(&lib).unwrap().unwrap();
    let local = build_snapshot_read_only(&lib, &identity).unwrap();
    let sha = sync_protocol::snapshot_sha256(&local).unwrap().to_string();
    assert_eq!(prepared.local_snapshot_sha256, sha);
    revalidate_prepared(&sync, Some(&lib), &prepared).unwrap();
}

#[test]
#[ignore = "manual: historical 18E7 copy; no live server"]
fn historical_copy_prepare_from_env() {
    let lib = PathBuf::from(std::env::var("SCRIBE_EXEC_LIB").unwrap());
    let sync = PathBuf::from(std::env::var("SCRIBE_EXEC_SYNC").unwrap());
    require_disposable_exec_root(&lib).unwrap();
    require_disposable_exec_root(&sync).unwrap();
    let insp = inspect_identity_read_only(&lib).unwrap();
    assert_eq!(insp.unbound_pages.len(), 2);
    let (state, cursor) = crate::sync_outbound::confirmed::load_checkpoint(&sync)
        .unwrap()
        .map(|(s, c, _)| (s, c))
        .expect("checkpoint");
    let st: crate::sync_outbound::SyncStateV1 = serde_json::from_slice(
        &std::fs::read(sync.join(crate::sync_outbound::STATE_FILE)).unwrap(),
    )
    .unwrap();
    let spec = PrepareSpec {
        library_root: &lib,
        sync_dir: &sync,
        checkpoint: &state,
        checkpoint_cursor: cursor,
        acked_client_seq: st.acked_client_seq,
        next_client_seq: st.next_client_seq,
        replica_id: st.replica_id,
        genesis_sha256: st.genesis_sha256.to_string(),
        server_tail: &[],
        require_disposable: true,
    };
    let prepared = prepare_recovery(&spec, &Default::default()).unwrap();
    assert_eq!(prepared.ops.len(), 120);
    let replayed = replay_frozen(&state, &prepared).unwrap();
    let local = build_snapshot_read_only(&lib, &read_identity(&lib).unwrap().unwrap()).unwrap();
    assert_eq!(replayed.snapshot, local);
    assert!(!prepared.upload_authorized);
}
