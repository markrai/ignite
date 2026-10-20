//! Slice 18E.10M: disposable recovery executor over supervised direct stream.
//! Synthetic credentials, local servers, and a stub `dropbearmulti` only.

use super::*;
use crate::ink::InkRequest;
use crate::library::{
    open_library_read_only, open_or_create_library, read_notebook_manifest_read_only,
};
use crate::notebook::{insert_page_after_durable, InsertPageHooks};
use crate::pressure_curve::STYLE_PRESSURE_WIDTH_V1;
use crate::store::{load_existing_page, PageStore, PersistedReplacementV2};
use crate::sync_local::{build_snapshot_read_only, ensure_sync_identity, read_identity};
use crate::sync_outbound::{
    parse_journal, OutboundSync, SyncConfigV1, CONFIG_FILE, OUTBOX_FILE, STATE_FILE,
};
use crate::sync_recovery_attest::bind_attestation_to_recovery_policy;
use crate::sync_recovery_policy::{RecoveryGate, LIVE_NON_LOOPBACK_POST_ENABLED};
use crate::sync_recovery_stream::spawn_recovery_stream_child;
use crate::sync_recovery_tunnel::{
    fingerprint_of_b64, HostKeyTrustInput, DROPBEAR_CLIENT_APPLET, KINDLE_DROPBEARMULTI,
};
use crate::ui_gfx::paper_background;
use kindle_platform::FbGeometry;
use notebook_core::{new_stroke_id, Point, Stroke};
use scribe_sync_server::{generate_token, ServerConfig, ServerHandle, Store, TokenVerifier};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use sync_engine::testkit::{self, genesis, line, nb, replica, sk, Author};
use sync_protocol::{
    to_canonical_json, BackgroundRef, LineageReplacement, OperationId, PageId, SyncEnvelopeV1,
    SyncOperationV1, SyncSnapshotV1, SyncStroke, ENVELOPE_VERSION,
};
use uuid::Uuid;

const PIN_BLOB: &[u8] = b"ssh-ed25519\0synthetic-host-key-blob-115";

fn tmp(label: &str) -> PathBuf {
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("scribe-exec-18e7-115-{label}-{n}"))
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

fn b64_encode(data: &[u8]) -> String {
    const B64: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    let mut i = 0;
    while i < data.len() {
        let b0 = data[i];
        let b1 = if i + 1 < data.len() { data[i + 1] } else { 0 };
        let b2 = if i + 2 < data.len() { data[i + 2] } else { 0 };
        out.push(B64[(b0 >> 2) as usize] as char);
        out.push(B64[(((b0 & 3) << 4) | (b1 >> 4)) as usize] as char);
        if i + 1 < data.len() {
            out.push(B64[(((b1 & 15) << 2) | (b2 >> 6)) as usize] as char);
        } else {
            out.push('=');
        }
        if i + 2 < data.len() {
            out.push(B64[(b2 & 63) as usize] as char);
        } else {
            out.push('=');
        }
        i += 3;
    }
    out
}

fn fake_dropbear() -> PathBuf {
    static BIN: OnceLock<PathBuf> = OnceLock::new();
    BIN.get_or_init(|| {
        let dir = std::env::temp_dir().join("ignite-115-fake-dropbear");
        fs::create_dir_all(&dir).unwrap();
        let src = dir.join("fake_dropbearmulti.rs");
        fs::write(&src, include_str!("testdata/fake_dropbearmulti.rs")).unwrap();
        let out = if cfg!(windows) {
            dir.join("dropbearmulti.exe")
        } else {
            dir.join("dropbearmulti")
        };
        let st = Command::new("rustc")
            .args(["--edition", "2021", "-C", "opt-level=1", "-o"])
            .arg(&out)
            .arg(&src)
            .status()
            .expect("rustc available to compile disposable dropbear stub");
        assert!(st.success(), "rustc failed to build dropbear stub");
        out
    })
    .clone()
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
    fn listen_hostport(&self) -> String {
        self.url()
            .trim_start_matches("http://")
            .trim_start_matches("https://")
            .to_string()
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

struct StreamHome {
    plan: DisposableDirectStreamPlan,
    home: PathBuf,
}

impl StreamHome {
    fn for_server(srv: &Srv, intent: &RecoveryIntentV1) -> Self {
        static SEQ: AtomicUsize = AtomicUsize::new(0);
        let home = tmp(&format!("ssh-{}", SEQ.fetch_add(1, Ordering::SeqCst)));
        fs::create_dir_all(home.join(".ssh")).unwrap();
        let identity = home.join("id_ed25519");
        fs::write(&identity, b"synthetic-dropbear-identity-115").unwrap();
        let exe = fake_dropbear();
        let b64 = b64_encode(PIN_BLOB);
        let fp = fingerprint_of_b64(&b64).unwrap();
        let kh = format!("ssh.example.test ssh-ed25519 {b64}\n");
        fs::write(home.join(".ssh").join("known_hosts"), &kh).unwrap();
        fs::write(home.join("forward_tcp"), srv.listen_hostport()).unwrap();
        Self {
            plan: DisposableDirectStreamPlan {
                executable: exe.to_string_lossy().into_owned(),
                identity_file: identity.to_string_lossy().into_owned(),
                known_hosts_home: home.to_string_lossy().into_owned(),
                known_hosts_text: kh,
                expected_key_b64: b64,
                pinned_host_key_sha256: fp,
                ssh_host: "ssh.example.test".into(),
                ssh_user: "syncfwd".into(),
                ssh_port: 22,
                remote_forward_host: "httptest.example.test".into(),
                remote_forward_port: 8080,
                library_id: intent.library_id.to_string(),
                replica_id: intent.replica_id.to_string(),
                genesis_sha256: intent.genesis_sha256.clone(),
                transaction_id: intent.transaction_id.clone(),
            },
            home,
        }
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
    library: sync_protocol::LibraryId,
    replica: sync_protocol::ReplicaId,
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
    let nbdir = lib.join("notebooks").join("notebook-1");
    let (mut store, _) = load_existing_page(&nbdir, "page-1").unwrap();
    store.append_committed_stroke(&stroke).unwrap();
    stroke
}

fn put_page_blob(srv: &Srv, lib: &Path, snap: &SyncSnapshotV1) {
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

fn dummy_attestation() -> crate::sync_recovery_attest::TunnelAttestation {
    crate::sync_recovery_attest::TunnelAttestation {
        generation: 1,
        identity: crate::sync_recovery_attest::ProcessStartIdentity {
            pid: 1,
            starttime: 1,
        },
        exe: KINDLE_DROPBEARMULTI.into(),
        listen_addr: "127.0.0.1".into(),
        listen_port: 1,
        listener_origin: "http://127.0.0.1:1".into(),
        remote_forward_host: "sync.example.test".into(),
        remote_forward_port: 8080,
        pinned_host_key_sha256: "aa".into(),
        observed_host_key_sha256: "aa".into(),
        host_key_algorithm: "ssh-ed25519".into(),
        library_id: "lib-aaaa".into(),
        replica_id: "rep-bbbb".into(),
        genesis_sha256: "gen-cccc".into(),
        transaction_id: "txn".into(),
        auth_source: crate::sync_recovery_policy::SshAuthEvidenceSource::PinnedForwardConjunction,
        lifecycle: crate::sync_recovery_policy::TunnelLifecycle::ReadyForAuthorizedRecovery,
        listener_present: true,
        listener_owner_observed: true,
        pid_recycled: false,
        unknown_host_key_accepted: false,
        gateway_ports: false,
        unrestricted_forwarding: false,
        agent_forwarding: false,
        interactive_shell: false,
        ssh_session_authenticated: true,
    }
}

#[test]
fn direct_stream_get_post_lost_ack_and_identities() {
    let srv = server(true);
    let snap = genesis();
    bootstrap(&srv, &snap);
    let genesis_sha = sync_protocol::snapshot_sha256(&snap).unwrap().to_string();
    let page = snap.notebooks[0].pages[0].page_id;
    let sync = tmp("lost");
    let intent = freeze_ops(
        &sync,
        testkit::library(),
        replica(1),
        &genesis_sha,
        vec![
            add(page, orig(10)),
            add(page, orig(11)),
            add(page, orig(12)),
        ],
        1,
    );
    let ids_before: Vec<_> = intent.ops.iter().map(|o| o.operation_id).collect();
    let bytes_before: Vec<_> = intent.ops.iter().map(|o| o.envelope.clone()).collect();
    let home = StreamHome::for_server(&srv, &intent);
    let first = submit_recovery_with_direct_stream(
        &sync,
        None,
        &srv.target(),
        &RecoveryExecHooks {
            halt_after_new_accepts: Some(1),
            ..Default::default()
        },
        home.plan.clone(),
    )
    .unwrap();
    match first {
        ExecOutcome::Progress {
            accepted_prefix, ..
        } => assert_eq!(accepted_prefix, 1),
        other => panic!("{other:?}"),
    }
    srv.h
        .store
        .hooks
        .fail_drop_response_after_commit
        .store(true, Ordering::SeqCst);
    let lost = submit_recovery_with_direct_stream(
        &sync,
        None,
        &srv.target(),
        &RecoveryExecHooks {
            halt_after_new_accepts: Some(1),
            ..Default::default()
        },
        home.plan.clone(),
    )
    .unwrap();
    match lost {
        ExecOutcome::Progress {
            accepted_prefix, ..
        } => assert!(accepted_prefix >= 1),
        ExecOutcome::Complete { accepted_prefix } => assert!(accepted_prefix >= 1),
        other => panic!("{other:?}"),
    }
    srv.h
        .store
        .hooks
        .fail_drop_response_after_commit
        .store(false, Ordering::SeqCst);
    let done = submit_recovery_with_direct_stream(
        &sync,
        None,
        &srv.target(),
        &Default::default(),
        home.plan.clone(),
    )
    .unwrap();
    assert!(matches!(done, ExecOutcome::Complete { accepted_prefix: 3 }));
    let after = read_intent(&sync).unwrap().unwrap();
    let ids_after: Vec<_> = after.ops.iter().map(|o| o.operation_id).collect();
    let bytes_after: Vec<_> = after.ops.iter().map(|o| o.envelope.clone()).collect();
    assert_eq!(ids_before, ids_after);
    assert_eq!(bytes_before, bytes_after);
    assert_eq!(after.transaction_id, intent.transaction_id);
    assert!(!after.upload_authorized);
}

#[test]
fn direct_stream_exact_retry_is_duplicate() {
    let srv = server(true);
    let snap = genesis();
    bootstrap(&srv, &snap);
    let genesis_sha = sync_protocol::snapshot_sha256(&snap).unwrap().to_string();
    let page = snap.notebooks[0].pages[0].page_id;
    let sync = tmp("dup");
    let intent = freeze_ops(
        &sync,
        testkit::library(),
        replica(1),
        &genesis_sha,
        vec![add(page, orig(3))],
        1,
    );
    let home = StreamHome::for_server(&srv, &intent);
    assert!(matches!(
        submit_recovery_with_direct_stream(
            &sync,
            None,
            &srv.target(),
            &Default::default(),
            home.plan.clone()
        )
        .unwrap(),
        ExecOutcome::Complete { accepted_prefix: 1 }
    ));
    assert!(matches!(
        submit_recovery_with_direct_stream(
            &sync,
            None,
            &srv.target(),
            &Default::default(),
            home.plan.clone()
        )
        .unwrap(),
        ExecOutcome::Complete { accepted_prefix: 1 }
    ));
    assert_eq!(
        read_intent(&sync).unwrap().unwrap().ops[0].operation_id,
        intent.ops[0].operation_id
    );
}

#[test]
fn direct_stream_cas_conflict_fails_closed() {
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
    let intent = freeze_ops(
        &sync,
        testkit::library(),
        replica(1),
        &genesis_sha,
        vec![add(page, orig(20)), recovery],
        1,
    );
    let home = StreamHome::for_server(&srv, &intent);
    match submit_recovery_with_direct_stream(
        &sync,
        None,
        &srv.target(),
        &Default::default(),
        home.plan,
    )
    .unwrap()
    {
        ExecOutcome::Halted {
            accepted_prefix,
            reason,
        } => {
            assert_eq!(accepted_prefix, 1);
            assert!(
                reason.contains("semantic")
                    || reason.contains("CAS")
                    || reason.contains("expected")
                    || reason.contains("gate"),
                "{reason}"
            );
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn direct_stream_auth_and_host_key_refuse_without_remint() {
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
    let home = StreamHome::for_server(&srv, &intent);
    let bad = SubmitTarget {
        base_url: srv.url(),
        token: "nope-not-the-server-token".into(),
    };
    match submit_recovery_with_direct_stream(
        &sync,
        None,
        &bad,
        &Default::default(),
        home.plan.clone(),
    )
    .unwrap()
    {
        ExecOutcome::Halted {
            accepted_prefix, ..
        } => assert_eq!(accepted_prefix, 0),
        other => panic!("{other:?}"),
    }
    assert_eq!(
        read_intent(&sync).unwrap().unwrap().ops[0].operation_id,
        intent.ops[0].operation_id
    );

    let sync = tmp("hk");
    let intent = freeze_ops(
        &sync,
        testkit::library(),
        replica(1),
        &genesis_sha,
        vec![add(page, orig(5))],
        1,
    );
    let mut home = StreamHome::for_server(&srv, &intent);
    fs::write(
        home.home.join(".ssh").join("known_hosts"),
        "ssh.example.test ssh-ed25519 AAAAWRONG_PIN\n",
    )
    .unwrap();
    home.plan.known_hosts_text = "ssh.example.test ssh-ed25519 AAAAWRONG_PIN\n".into();
    let err = submit_recovery_with_direct_stream(
        &sync,
        None,
        &srv.target(),
        &Default::default(),
        home.plan,
    );
    match err {
        Ok(ExecOutcome::Halted { .. }) | Err(_) => {}
        Ok(other) => panic!("wrong pin must not complete {other:?}"),
    }
    assert_eq!(
        read_intent(&sync).unwrap().unwrap().ops[0].operation_id,
        intent.ops[0].operation_id
    );
}

#[test]
fn direct_stream_crash_resume_keeps_frozen_identities() {
    let srv = server(true);
    let snap = genesis();
    bootstrap(&srv, &snap);
    let genesis_sha = sync_protocol::snapshot_sha256(&snap).unwrap().to_string();
    let page = snap.notebooks[0].pages[0].page_id;
    let sync = tmp("crash");
    let intent = freeze_ops(
        &sync,
        testkit::library(),
        replica(1),
        &genesis_sha,
        vec![add(page, orig(30)), add(page, orig(31))],
        1,
    );
    let ids = intent.ops.clone();
    let home = StreamHome::for_server(&srv, &intent);
    let halted = submit_recovery_with_direct_stream(
        &sync,
        None,
        &srv.target(),
        &RecoveryExecHooks {
            halt_after_new_accepts: Some(1),
            ..Default::default()
        },
        home.plan.clone(),
    )
    .unwrap();
    match halted {
        ExecOutcome::Progress {
            accepted_prefix, ..
        } => assert_eq!(accepted_prefix, 1),
        other => panic!("{other:?}"),
    }
    assert!(intent_path(&sync).exists());
    assert!(recovery_blocks_ordinary_push(&sync));
    let mid = read_intent(&sync).unwrap().unwrap();
    assert_eq!(mid.ops, ids);
    assert!(mid.submit_attempted);
    let done = submit_recovery_with_direct_stream(
        &sync,
        None,
        &srv.target(),
        &Default::default(),
        home.plan,
    )
    .unwrap();
    assert!(matches!(done, ExecOutcome::Complete { accepted_prefix: 2 }));
    assert_eq!(read_intent(&sync).unwrap().unwrap().ops, ids);
}

#[test]
fn public_submit_recovery_does_not_select_direct_stream() {
    let srv = server(true);
    let snap = genesis();
    bootstrap(&srv, &snap);
    let genesis_sha = sync_protocol::snapshot_sha256(&snap).unwrap().to_string();
    let page = snap.notebooks[0].pages[0].page_id;
    let sync = tmp("public");
    freeze_ops(
        &sync,
        testkit::library(),
        replica(1),
        &genesis_sha,
        vec![add(page, orig(8))],
        1,
    );
    assert!(matches!(
        submit_recovery(&sync, None, &srv.target(), &Default::default()).unwrap(),
        ExecOutcome::Complete { accepted_prefix: 1 }
    ));
}

#[test]
fn direct_stream_isolation_negatives() {
    assert!(!LIVE_NON_LOOPBACK_POST_ENABLED);
    assert!(require_loopback_url("http://host.example/v1").is_err());
    assert!(require_loopback_url("http://10.0.0.8:8080").is_err());
    // Refuse any path under this crate's product tree (portable vs hard-coded drive letters).
    let product = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let err = require_disposable_exec_root(product);
    assert!(err.unwrap_err().contains("protected"));
    let live = SubmitTarget {
        base_url: "http://192.168.1.9:8080".into(),
        token: "live-bearer-must-not-load".into(),
    };
    assert!(RecoveryHttp::from_target(&live).is_err());
    let src = include_str!("sync_recovery_exec.rs");
    assert!(!src.contains("allow_live"));
    assert!(!src.contains("skip_checks"));
    assert!(!src.contains("trusted_local"));
    let main = include_str!("main.rs");
    assert!(!main.contains("DirectStream"));
    assert!(!main.contains("submit_recovery"));
    assert!(!main.contains("SupervisedRecoveryStream"));
    assert!(!main.contains("kindle_supervisor_harness"));
    assert!(!main.contains("run_kindle_supervisor_harness"));
    let pkg = include_str!("../Cargo.toml");
    assert!(pkg.contains("kindle_supervisor_harness"));
    assert!(pkg.contains("required-features"));
    let err = spawn_recovery_stream_child(&crate::sync_recovery_tunnel::DropbearInvocation {
        program: "dropbearmulti".into(),
        argv: vec!["dropbearmulti".into()],
        home: "/tmp".into(),
    })
    .unwrap_err();
    assert!(err.contains("not enabled"));
    let d = bind_attestation_to_recovery_policy(
        &dummy_attestation(),
        RecoveryGate::DIrreversibleExecution,
    )
    .unwrap_err();
    assert!(d.iter().any(|e| e.contains("Gate D")));
    let e = bind_attestation_to_recovery_policy(
        &dummy_attestation(),
        RecoveryGate::EVerifiedFinalization,
    )
    .unwrap_err();
    assert!(e.iter().any(|e| e.contains("Gate E")));
    let store_dir = tmp("gate-default");
    let store = Store::open(&store_dir).unwrap();
    assert!(!store.accept_replace_stroke_lineages.load(Ordering::SeqCst));
}

#[test]
fn ssh_plan_does_not_grant_gates_after_successful_http() {
    let srv = server(true);
    let snap = genesis();
    bootstrap(&srv, &snap);
    let genesis_sha = sync_protocol::snapshot_sha256(&snap).unwrap().to_string();
    let page = snap.notebooks[0].pages[0].page_id;
    let sync = tmp("gates");
    let intent = freeze_ops(
        &sync,
        testkit::library(),
        replica(1),
        &genesis_sha,
        vec![add(page, orig(9))],
        1,
    );
    let home = StreamHome::for_server(&srv, &intent);
    assert!(matches!(
        submit_recovery_with_direct_stream(
            &sync,
            None,
            &srv.target(),
            &Default::default(),
            home.plan
        )
        .unwrap(),
        ExecOutcome::Complete { accepted_prefix: 1 }
    ));
    assert!(bind_attestation_to_recovery_policy(
        &dummy_attestation(),
        RecoveryGate::DIrreversibleExecution,
    )
    .is_err());
    assert!(bind_attestation_to_recovery_policy(
        &dummy_attestation(),
        RecoveryGate::EVerifiedFinalization,
    )
    .is_err());
    assert!(!LIVE_NON_LOOPBACK_POST_ENABLED);
}

#[test]
fn intent_file_does_not_launch_ssh() {
    let sync = tmp("noop");
    let intent = freeze_ops(
        &sync,
        testkit::library(),
        replica(1),
        "abc",
        vec![add(genesis().notebooks[0].pages[0].page_id, orig(1))],
        1,
    );
    assert!(intent_path(&sync).exists());
    let _ = intent;
}

#[test]
fn direct_stream_confirm_release_then_offline_edit() {
    let srv = server(true);
    let root = tmp("rel");
    let lib = root.join("library");
    let sync = root.join("sync");
    open_or_create_library(&lib, InkRequest::Default, &geo(), &paper_background(&geo())).unwrap();
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
    assert!(recovery_blocks_ordinary_push(&sync));
    let home = StreamHome::for_server(&srv, &prepared);
    assert!(matches!(
        submit_recovery_with_direct_stream(
            &sync,
            Some(&lib),
            &srv.target(),
            &Default::default(),
            home.plan.clone()
        )
        .unwrap(),
        ExecOutcome::Complete { .. }
    ));
    let s = OutboundSync::open(&lib, &sync).unwrap();
    wait("inbound confirm", || {
        if s.fault().is_some() {
            return true;
        }
        if s.inbound_ready() {
            let _ = s.apply_inbound();
        }
        let intent = read_intent(&sync).unwrap().unwrap();
        let tail = fetch_tail(&srv.target(), intent.library_id, cursor).unwrap();
        let head = tail.last().map(|(seq, _)| *seq as u64).unwrap_or(0);
        s.kick();
        s.server_cursor() == head && s.pending() == 0 && s.inbox_len() == 0
    });
    assert!(s.fault().is_none(), "{:?}", s.fault());
    wait("outbox retire", || {
        let acked = s.state().acked_client_seq;
        let raw = fs::read(sync.join(OUTBOX_FILE)).unwrap_or_default();
        let parsed = parse_journal(&raw).unwrap();
        parsed.committed.iter().all(|o| o.seq > acked)
    });
    let health = ReleaseHealth {
        journal_error: s.journal_error(),
        seq_ready: s.seq_ready(),
        seq_block: s.seq_block(),
        sync_fault: s.fault(),
    };
    let report = release_recovery_hold_with_direct_stream(
        &sync,
        &lib,
        &srv.target(),
        &health,
        &Default::default(),
        home.plan,
    )
    .unwrap();
    assert!(report.intent_removed);
    assert!(read_intent(&sync).unwrap().is_none());
    assert!(read_completion(&sync).unwrap().is_some());
    assert!(!recovery_blocks_ordinary_push(&sync));
    assert!(s.capture_ready());
    s.shutdown(Duration::from_secs(3));

    let dead = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let dead_url = format!("http://{}", dead.local_addr().unwrap());
    drop(dead);
    write_config(&sync, &dead_url, &srv.token);
    let s = OutboundSync::open(&lib, &sync).unwrap();
    assert!(s.capture_ready());
    let before = s.next_client_seq();
    let stroke = local_append_stroke(&lib);
    s.stroke_committed("notebook-1", "page-1", &stroke);
    wait("capture", || {
        s.pending() == 1 && s.next_client_seq() == before + 1
    });
    let (mut store, _) =
        load_existing_page(&lib.join("notebooks").join("notebook-1"), "page-1").unwrap();
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
}

#[test]
fn production_spawn_and_kindle_path_remain_unwired() {
    assert_eq!(DROPBEAR_CLIENT_APPLET, "dbclient");
    assert!(KINDLE_DROPBEARMULTI.contains("usbnetlite"));
    let err = spawn_recovery_stream_child(&crate::sync_recovery_tunnel::DropbearInvocation {
        program: KINDLE_DROPBEARMULTI.into(),
        argv: vec![KINDLE_DROPBEARMULTI.into(), "dbclient".into()],
        home: "/mnt/us".into(),
    })
    .unwrap_err();
    assert!(err.contains("not enabled"));
    let _ = HostKeyTrustInput {
        known_hosts_text: Some(""),
        expected_hostname: "ssh.example.test",
        expected_ssh_port: 22,
        planned_ssh_port: 22,
        expected_algorithm: "ssh-ed25519",
        expected_key_b64: "AAAA",
        expected_fingerprint_sha256: "00",
        tofu_permitted: false,
        accept_unknown_host: false,
        host_key_verification_disabled: false,
    };
}
