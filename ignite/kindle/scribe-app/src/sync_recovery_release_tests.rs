//! Slice 18E.9B: disposable confirmed-state reconciliation and verified hold release.

use super::*;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use kindle_platform::FbGeometry;
use notebook_core::{new_stroke_id, Point, Stroke};
use scribe_sync_server::{generate_token, ServerConfig, ServerHandle, Store, TokenVerifier};
use sync_engine::testkit::{line, replica, sk};
use sync_protocol::{
    to_canonical_json, BackgroundRef, OperationId, SyncEnvelopeV1, SyncOperationV1,
    ENVELOPE_VERSION,
};

use crate::ink::InkRequest;
use crate::library::{
    open_library_read_only, open_or_create_library, read_notebook_manifest_read_only,
};
use crate::notebook::{insert_page_after_durable, InsertPageHooks};
use crate::pressure_curve::STYLE_PRESSURE_WIDTH_V1;
use crate::store::{load_existing_page, PageStore, PersistedReplacementV2};
use crate::sync_local::{build_snapshot_read_only, ensure_sync_identity, read_identity};
use crate::sync_outbound::{
    parse_journal, OutboundSync, SyncConfigV1, APPLY_FILE, CONFIG_FILE, OUTBOX_FILE, STATE_FILE,
};
use crate::ui_gfx::paper_background;

fn tmp(label: &str) -> PathBuf {
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("scribe-rel-{label}-{n}"))
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

fn wait(what: &str, mut f: impl FnMut() -> bool) {
    let t0 = Instant::now();
    while !f() {
        assert!(
            t0.elapsed() < Duration::from_secs(60),
            "timed out waiting for {what}"
        );
        std::thread::sleep(Duration::from_millis(15));
    }
}

fn write_config(sync: &Path, url: &str, token: &str) {
    fs::create_dir_all(sync).unwrap();
    let cfg = SyncConfigV1 {
        v: 1,
        server_base_url: url.to_string(),
        token: token.to_string(),
        debug_drop_first_ack: false,
        debug_crash_point: None,
        server_id: None,
    };
    let mut bytes = to_canonical_json(&cfg).unwrap();
    bytes.push(b'\n');
    fs::write(sync.join(CONFIG_FILE), bytes).unwrap();
}

fn health_of(s: &OutboundSync) -> ReleaseHealth {
    ReleaseHealth {
        journal_error: s.journal_error(),
        seq_ready: s.seq_ready(),
        seq_block: s.seq_block(),
        sync_fault: s.fault(),
    }
}

fn local_append_stroke(lib: &Path) -> Stroke {
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
    let (mut store, _) = load_existing_page(&nb, "page-1").unwrap();
    store.append_committed_stroke(&stroke).unwrap();
    stroke
}

fn put_page_blob(srv: &Srv, lib: &Path, snap: &sync_protocol::SyncSnapshotV1) {
    for page in snap.notebooks.iter().flat_map(|n| &n.pages) {
        if let BackgroundRef::Blob { sha256, .. } = &page.background {
            let raw = crate::store::load_page_read_only(
                &open_library_read_only(lib)
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
}

struct Prepared {
    lib: PathBuf,
    sync: PathBuf,
    srv: Srv,
    spec_cursor: u64,
}

fn epoch_then_extra_page(label: &str) -> Prepared {
    let srv = server(true);
    let root = tmp(label);
    let lib = root.join("library");
    let sync = root.join("sync");
    let g = geo();
    open_or_create_library(&lib, InkRequest::Default, &g, &paper_background(&g)).unwrap();
    ensure_sync_identity(&lib).unwrap();
    write_config(&sync, &srv.url(), &srv.token);
    let s = OutboundSync::open(&lib, &sync).unwrap();
    wait("genesis", || s.state().genesis_uploaded);
    let identity = read_identity(&lib).unwrap().unwrap();
    let snap0 = build_snapshot_read_only(&lib, &identity).unwrap();
    put_page_blob(&srv, &lib, &snap0);
    s.shutdown(Duration::from_secs(3));
    let repo = open_library_read_only(&lib).unwrap();
    let nbdir = repo.notebook_dir("notebook-1");
    let mut m = read_notebook_manifest_read_only(&nbdir).unwrap();
    insert_page_after_durable(&nbdir, &mut m, "page-1", &InsertPageHooks::default()).unwrap();
    let (cp, cursor, _) = crate::sync_outbound::confirmed::load_checkpoint(&sync)
        .unwrap()
        .expect("checkpoint");
    let st: crate::sync_outbound::SyncStateV1 =
        serde_json::from_slice(&fs::read(sync.join(STATE_FILE)).unwrap()).unwrap();
    let spec = PrepareSpec {
        library_root: &lib,
        sync_dir: &sync,
        checkpoint: &cp,
        checkpoint_cursor: cursor,
        acked_client_seq: st.acked_client_seq,
        next_client_seq: st.next_client_seq,
        replica_id: st.replica_id,
        genesis_sha256: st.genesis_sha256.to_string(),
        server_tail: &[],
        require_disposable: false,
    };
    let prepared = prepare_recovery(&spec, &Default::default()).unwrap();
    assert!(!prepared.ops.is_empty());
    assert!(matches!(
        submit_recovery(&sync, Some(&lib), &srv.target(), &Default::default()).unwrap(),
        ExecOutcome::Complete { .. }
    ));
    Prepared {
        lib,
        sync,
        srv,
        spec_cursor: cursor,
    }
}

fn reopen_and_confirm(p: &Prepared) -> OutboundSync {
    let s = OutboundSync::open(&p.lib, &p.sync).unwrap();
    wait("inbound confirm", || {
        if s.fault().is_some() {
            return true;
        }
        if s.inbound_ready() {
            let _ = s.apply_inbound();
        }
        let intent = read_intent(&p.sync).unwrap().unwrap();
        let tail = fetch_tail(&p.srv.target(), intent.library_id, p.spec_cursor).unwrap();
        let head = tail.last().map(|(seq, _)| *seq as u64).unwrap_or(0);
        s.kick();
        s.server_cursor() == head && s.pending() == 0 && s.inbox_len() == 0
    });
    assert!(s.fault().is_none(), "{:?}", s.fault());
    wait("outbox retire", || {
        let acked = s.state().acked_client_seq;
        let raw = fs::read(p.sync.join(OUTBOX_FILE)).unwrap_or_default();
        let parsed = parse_journal(&raw).unwrap();
        parsed.committed.iter().all(|o| o.seq > acked)
    });
    s
}

#[test]
fn fully_accepted_confirmed_release_restores_offline_capture() {
    let p = epoch_then_extra_page("full");
    let s = reopen_and_confirm(&p);
    let health = health_of(&s);
    let report = release_recovery_hold(
        &p.sync,
        &p.lib,
        &p.srv.target(),
        &health,
        &Default::default(),
    )
    .unwrap();
    assert!(report.intent_removed);
    assert!(!report.already_released);
    assert!(read_intent(&p.sync).unwrap().is_none());
    assert!(read_completion(&p.sync).unwrap().is_some());
    assert!(!recovery_blocks_ordinary_push(&p.sync));
    assert!(s.capture_ready());
    assert!(s.upload_permitted());

    s.shutdown(Duration::from_secs(3));
    let dead = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let dead_url = format!("http://{}", dead.local_addr().unwrap());
    drop(dead);
    write_config(&p.sync, &dead_url, &p.srv.token);
    let s = OutboundSync::open(&p.lib, &p.sync).unwrap();
    assert!(s.capture_ready(), "offline capture after release");
    let before = s.next_client_seq();
    let stroke = local_append_stroke(&p.lib);
    s.stroke_committed("notebook-1", "page-1", &stroke);
    wait("capture", || {
        s.pending() == 1 && s.next_client_seq() == before + 1
    });
    let (mut store, _) =
        load_existing_page(&p.lib.join("notebooks").join("notebook-1"), "page-1").unwrap();
    store
        .append_replace_strokes(&[PersistedReplacementV2 {
            target_stroke_id: stroke.id.as_str().to_string(),
            fragments: Vec::new(),
        }])
        .unwrap();
    s.lineage_deleted("notebook-1", "page-1", stroke.id.as_str());
    wait("erase capture", || {
        s.pending() == 2 && s.next_client_seq() == before + 2
    });
    s.shutdown(Duration::from_secs(3));
    write_config(&p.sync, &p.srv.url(), &p.srv.token);
    let s = OutboundSync::open(&p.lib, &p.sync).unwrap();
    wait("ordinary upload", || {
        if s.inbound_ready() {
            let _ = s.apply_inbound();
        }
        s.kick();
        s.pending() == 0 && s.fault().is_none()
    });
    assert!(s.upload_permitted());
    let again = release_recovery_hold(
        &p.sync,
        &p.lib,
        &p.srv.target(),
        &health_of(&s),
        &Default::default(),
    )
    .unwrap();
    assert!(again.already_released);
    assert!(read_intent(&p.sync).unwrap().is_none());
}

#[test]
fn partial_acceptance_refuses_release() {
    let p = epoch_then_extra_page("partial-setup");
    let intent = read_intent(&p.sync).unwrap().unwrap();
    let mut halted = intent;
    halted.accepted_prefix = 0;
    halted.phase = RecoveryPhase::PartiallyAccepted;
    write_intent(&p.sync, &halted, &Default::default()).unwrap();
    let s = OutboundSync::open(&p.lib, &p.sync).unwrap();
    let err = release_recovery_hold(
        &p.sync,
        &p.lib,
        &p.srv.target(),
        &health_of(&s),
        &Default::default(),
    )
    .unwrap_err();
    assert!(err.contains("incomplete"), "{err}");
    assert!(read_intent(&p.sync).unwrap().is_some());
    assert!(recovery_blocks_ordinary_push(&p.sync));
}

#[test]
fn ambiguous_acceptance_refuses_release() {
    let srv = server(true);
    let root = tmp("amb");
    let lib = root.join("library");
    let sync = root.join("sync");
    let g = geo();
    open_or_create_library(&lib, InkRequest::Default, &g, &paper_background(&g)).unwrap();
    ensure_sync_identity(&lib).unwrap();
    write_config(&sync, &srv.url(), &srv.token);
    let s = OutboundSync::open(&lib, &sync).unwrap();
    wait("genesis", || s.state().genesis_uploaded);
    s.shutdown(Duration::from_secs(3));
    let repo = open_library_read_only(&lib).unwrap();
    let nbdir = repo.notebook_dir("notebook-1");
    let mut m = read_notebook_manifest_read_only(&nbdir).unwrap();
    insert_page_after_durable(&nbdir, &mut m, "page-1", &InsertPageHooks::default()).unwrap();
    let (cp, cursor, _) = crate::sync_outbound::confirmed::load_checkpoint(&sync)
        .unwrap()
        .unwrap();
    let st: crate::sync_outbound::SyncStateV1 =
        serde_json::from_slice(&fs::read(sync.join(STATE_FILE)).unwrap()).unwrap();
    let spec = PrepareSpec {
        library_root: &lib,
        sync_dir: &sync,
        checkpoint: &cp,
        checkpoint_cursor: cursor,
        acked_client_seq: st.acked_client_seq,
        next_client_seq: st.next_client_seq,
        replica_id: st.replica_id,
        genesis_sha256: st.genesis_sha256.to_string(),
        server_tail: &[],
        require_disposable: false,
    };
    prepare_recovery(&spec, &Default::default()).unwrap();
    srv.h
        .store
        .hooks
        .fail_submit_before_commit
        .store(true, Ordering::SeqCst);
    let _ = submit_recovery(&sync, Some(&lib), &srv.target(), &Default::default());
    let intent = read_intent(&sync).unwrap().unwrap();
    assert!(intent.submit_attempted);
    assert!(intent.accepted_prefix < intent.ops.len() || intent.accepted_prefix == 0);
    let s = OutboundSync::open(&lib, &sync).unwrap();
    let err = release_recovery_hold(
        &sync,
        &lib,
        &srv.target(),
        &health_of(&s),
        &Default::default(),
    )
    .unwrap_err();
    assert!(
        err.contains("incomplete") || err.contains("confirmed cursor"),
        "{err}"
    );
    assert!(read_intent(&sync).unwrap().is_some());
}

#[test]
fn incomplete_confirmed_state_refuses_release() {
    let p = epoch_then_extra_page("noconfirm");
    let s = OutboundSync::open(&p.lib, &p.sync).unwrap();
    let err = release_recovery_hold(
        &p.sync,
        &p.lib,
        &p.srv.target(),
        &health_of(&s),
        &Default::default(),
    )
    .unwrap_err();
    assert!(
        err.contains("confirmed cursor") || err.contains("acknowledged"),
        "{err}"
    );
    assert!(read_intent(&p.sync).unwrap().is_some());
}

#[test]
fn notebook_mismatch_refuses_release() {
    let p = epoch_then_extra_page("mismatch");
    let s = reopen_and_confirm(&p);
    crate::sync_verified::permit_sync_mutations(|| {
        local_append_stroke(&p.lib);
    });
    let err = release_recovery_hold(
        &p.sync,
        &p.lib,
        &p.srv.target(),
        &health_of(&s),
        &Default::default(),
    )
    .unwrap_err();
    assert!(err.contains("notebook"), "{err}");
    assert!(read_intent(&p.sync).unwrap().is_some());
}

#[test]
fn apply_marker_and_journal_error_refuse_release() {
    let p = epoch_then_extra_page("marker");
    let s = reopen_and_confirm(&p);
    fs::write(p.sync.join(APPLY_FILE), br#"{"v":1,"target_cursor":1}"#).unwrap();
    let err = release_recovery_hold(
        &p.sync,
        &p.lib,
        &p.srv.target(),
        &health_of(&s),
        &Default::default(),
    )
    .unwrap_err();
    assert!(err.contains("apply marker"), "{err}");
    fs::remove_file(p.sync.join(APPLY_FILE)).unwrap();
    let mut bad = health_of(&s);
    bad.journal_error = Some("planted".into());
    let err = release_recovery_hold(&p.sync, &p.lib, &p.srv.target(), &bad, &Default::default())
        .unwrap_err();
    assert!(err.contains("journal error"), "{err}");
    bad.journal_error = None;
    bad.seq_ready = false;
    let err = release_recovery_hold(&p.sync, &p.lib, &p.srv.target(), &bad, &Default::default())
        .unwrap_err();
    assert!(err.contains("sequence authority is not ready"), "{err}");
    bad.seq_ready = true;
    bad.sync_fault = Some("disk full".into());
    let err = release_recovery_hold(&p.sync, &p.lib, &p.srv.target(), &bad, &Default::default())
        .unwrap_err();
    assert!(err.contains("unrelated sync fault"), "{err}");
    assert!(read_intent(&p.sync).unwrap().is_some());
}

#[test]
fn sequence_and_identity_mismatch_refuse_release() {
    let p = epoch_then_extra_page("seqid");
    let s = reopen_and_confirm(&p);
    let mut st: crate::sync_outbound::SyncStateV1 =
        serde_json::from_slice(&fs::read(p.sync.join(STATE_FILE)).unwrap()).unwrap();
    st.next_client_seq += 3;
    fs::write(p.sync.join(STATE_FILE), to_canonical_json(&st).unwrap()).unwrap();
    let err = release_recovery_hold(
        &p.sync,
        &p.lib,
        &p.srv.target(),
        &health_of(&s),
        &Default::default(),
    )
    .unwrap_err();
    assert!(err.contains("next_client_seq"), "{err}");
    st.next_client_seq -= 3;
    fs::write(p.sync.join(STATE_FILE), to_canonical_json(&st).unwrap()).unwrap();
    let mut intent = read_intent(&p.sync).unwrap().unwrap();
    intent.identity_sha256 = "0".repeat(64);
    write_intent(&p.sync, &intent, &Default::default()).unwrap();
    let err = release_recovery_hold(
        &p.sync,
        &p.lib,
        &p.srv.target(),
        &health_of(&s),
        &Default::default(),
    )
    .unwrap_err();
    assert!(err.contains("identity"), "{err}");
}

#[test]
fn completion_proof_then_crash_releases_idempotently() {
    let p = epoch_then_extra_page("proofcrash");
    let s = reopen_and_confirm(&p);
    let err = release_recovery_hold(
        &p.sync,
        &p.lib,
        &p.srv.target(),
        &health_of(&s),
        &ReleaseHooks {
            fail_after_proof_before_release: true,
            ..Default::default()
        },
    )
    .unwrap_err();
    assert!(err.contains("completion proof"));
    assert!(read_intent(&p.sync).unwrap().is_some());
    assert!(read_completion(&p.sync).unwrap().is_some());
    let report = release_recovery_hold(
        &p.sync,
        &p.lib,
        &p.srv.target(),
        &health_of(&s),
        &Default::default(),
    )
    .unwrap();
    assert!(report.intent_removed);
    assert!(read_intent(&p.sync).unwrap().is_none());
}

#[test]
fn tail_verify_crash_keeps_hold() {
    let p = epoch_then_extra_page("tailcrash");
    let s = reopen_and_confirm(&p);
    let err = release_recovery_hold(
        &p.sync,
        &p.lib,
        &p.srv.target(),
        &health_of(&s),
        &ReleaseHooks {
            fail_after_tail_verify: true,
            ..Default::default()
        },
    )
    .unwrap_err();
    assert!(err.contains("accepted-tail"));
    assert!(read_intent(&p.sync).unwrap().is_some());
    release_recovery_hold(
        &p.sync,
        &p.lib,
        &p.srv.target(),
        &health_of(&s),
        &Default::default(),
    )
    .unwrap();
    assert!(read_intent(&p.sync).unwrap().is_none());
}

#[test]
fn unrelated_remote_edit_is_preserved_in_release() {
    let p = epoch_then_extra_page("remote");
    let intent = read_intent(&p.sync).unwrap().unwrap();
    let identity = read_identity(&p.lib).unwrap().unwrap();
    let page = *identity.pages["notebook-1"].get("page-1").unwrap();
    let mut remote = SyncEnvelopeV1 {
        v: ENVELOPE_VERSION,
        operation_id: OperationId::from_uuid(uuid::Uuid::now_v7()).unwrap(),
        library_id: intent.library_id,
        replica_id: replica(9),
        client_seq: 1,
        operation: SyncOperationV1::AddStroke {
            notebook_id: *identity.notebooks.get("notebook-1").unwrap(),
            page_id: page,
            stroke: line(sk(80), sk(80), (200.0, 300.0), (800.0, 300.0), 4, false),
        },
    };
    remote.library_id = intent.library_id;
    let body = format!(
        "{{\"envelopes\":[{}]}}",
        String::from_utf8(remote.to_canonical_bytes().unwrap()).unwrap()
    );
    let req = ureq::request(
        "POST",
        &format!(
            "{}/v1/libraries/{}/operations",
            p.srv.url(),
            intent.library_id
        ),
    )
    .set("Authorization", &format!("Bearer {}", p.srv.token));
    assert_eq!(req.send_bytes(body.as_bytes()).unwrap().status(), 200);
    let s = reopen_and_confirm(&p);
    let local = build_snapshot_read_only(&p.lib, &read_identity(&p.lib).unwrap().unwrap()).unwrap();
    let strokes: usize = local.notebooks[0].pages[0].strokes.len();
    assert!(strokes >= 1, "remote stroke must materialize");
    release_recovery_hold(
        &p.sync,
        &p.lib,
        &p.srv.target(),
        &health_of(&s),
        &Default::default(),
    )
    .unwrap();
    let after = build_snapshot_read_only(&p.lib, &read_identity(&p.lib).unwrap().unwrap()).unwrap();
    assert_eq!(after.notebooks[0].pages[0].strokes.len(), strokes);
}

#[test]
fn outbox_still_holding_recovery_ops_refuses_release() {
    let p = epoch_then_extra_page("outboxhold");
    let s = OutboundSync::open(&p.lib, &p.sync).unwrap();
    let mut st: crate::sync_outbound::SyncStateV1 =
        serde_json::from_slice(&fs::read(p.sync.join(STATE_FILE)).unwrap()).unwrap();
    let intent = read_intent(&p.sync).unwrap().unwrap();
    let tail = fetch_tail(&p.srv.target(), intent.library_id, p.spec_cursor).unwrap();
    let verified = verify_accepted_history(&intent, &tail).unwrap();
    st.server_cursor = verified.through_server_seq;
    st.acked_client_seq = intent.ops.last().unwrap().client_seq;
    st.next_client_seq = intent.next_client_seq_after;
    fs::write(p.sync.join(STATE_FILE), to_canonical_json(&st).unwrap()).unwrap();
    let err = release_recovery_hold(
        &p.sync,
        &p.lib,
        &p.srv.target(),
        &health_of(&s),
        &Default::default(),
    )
    .unwrap_err();
    assert!(
        err.contains("outbox") || err.contains("confirmed log") || err.contains("notebook"),
        "{err}"
    );
    assert!(read_intent(&p.sync).unwrap().is_some());
}
