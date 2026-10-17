//! Offline authoritative-tail verifier proofs. Synthetic identities and disposable
//! in-process servers only — never a production host. An ignored env-gated
//! test may re-validate a previously saved GET body; it does not open sockets.

use super::*;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::OptionalExtension;
use scribe_sync_server::http::{route, ServerConfig, ServerHandle};
use scribe_sync_server::store::{Store, DB_FILE};
use scribe_sync_server::{generate_token, TokenVerifier};
use sync_engine::testkit::{library, line, nb, pg, render, replica, sk, Author};
use sync_engine::LibraryState;
use sync_protocol::{
    BackgroundRef, PageSpace, SnapshotNotebook, SnapshotPage, SyncEnvelopeV1, SyncOperationV1,
    SyncSnapshotV1, SyncStroke,
};

use crate::sync_recovery_plan::{OutboxInput, PlanResult, SequenceAuthority};

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

fn spec(cursor: u64, acked: u64) -> TailVerifySpec {
    TailVerifySpec {
        library_id: library(),
        replica_id: replica(1),
        checkpoint_cursor: cursor,
        acked_client_seq: acked,
        page_limit: 2,
    }
}

fn env_bytes(e: &SyncEnvelopeV1) -> Vec<u8> {
    e.to_canonical_bytes().unwrap()
}

fn page_body(head: u64, after: u64, ops: &[(u64, SyncEnvelopeV1)]) -> Vec<u8> {
    let mut s = format!(
        "{{\"library_id\":\"{}\",\"head_server_seq\":{head},\"operations\":[",
        library()
    );
    for (i, (seq, env)) in ops.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&format!(
            "{{\"server_seq\":{seq},\"envelope\":{}}}",
            String::from_utf8(env_bytes(env)).unwrap()
        ));
    }
    let next = ops.last().map(|(q, _)| *q).unwrap_or(after);
    s.push_str(&format!("],\"next_after\":{next}}}"));
    s.into_bytes()
}

fn add_env(author: &mut Author, n: u64) -> SyncEnvelopeV1 {
    author.env(SyncOperationV1::AddStroke {
        notebook_id: nb(1),
        page_id: pg(1),
        stroke: orig(n),
    })
}

fn tmp(label: &str) -> PathBuf {
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("scribe-tail-{label}-{n}"))
}

#[test]
fn empty_tail_at_checkpoint_is_complete() {
    let cursor = 37928;
    let body = page_body(cursor, cursor, &[]);
    let src = InjectedTailSource {
        pages: vec![(cursor, body)],
    };
    let tail = acquire_verified_tail(&src, &spec(cursor, 37865)).unwrap();
    assert!(tail.items.is_empty());
    assert!(tail.complete);
    assert_eq!(tail.claimed_head, cursor);
    assert_eq!(tail.observed_through, cursor);
    assert!(!tail.upload_authorized);
    assert!(!tail.live_fetch_used);
}

#[test]
fn contiguous_unrelated_originals_assemble() {
    let mut remote = Author::new(replica(2));
    remote.library = library();
    let a = add_env(&mut remote, 70);
    let b = add_env(&mut remote, 71);
    let s = TailVerifySpec {
        page_limit: 1,
        ..spec(10, 0)
    };
    let src = InjectedTailSource {
        pages: vec![
            (10, page_body(12, 10, &[(11, a.clone())])),
            (11, page_body(12, 11, &[(12, b.clone())])),
        ],
    };
    let tail = acquire_verified_tail(&src, &s).unwrap();
    assert_eq!(
        tail.items.iter().map(|i| i.server_seq).collect::<Vec<_>>(),
        vec![11, 12]
    );
    assert_eq!(tail.observed_through, 12);
    assert_eq!(tail.replica_ops_in_tail, 0);
}

#[test]
fn gap_in_server_seq_is_refused() {
    let mut remote = Author::new(replica(2));
    remote.library = library();
    let a = add_env(&mut remote, 70);
    let body = page_body(12, 10, &[(12, a)]);
    let err = validate_operations_page(&body, library(), 10, 2).unwrap_err();
    assert!(matches!(err, TailVerifyError::CursorDiscontinuity { .. }));
}

#[test]
fn wrong_library_is_refused() {
    let mut remote = Author::new(replica(2));
    remote.library = library();
    let a = add_env(&mut remote, 70);
    let mut body = String::from_utf8(page_body(1, 0, &[(1, a)])).unwrap();
    body = body.replace(
        &library().to_string(),
        &sync_engine::testkit::uuid7(1, 9).to_string(),
    );
    let err = validate_operations_page(body.as_bytes(), library(), 0, 2).unwrap_err();
    assert!(matches!(
        err,
        TailVerifyError::ForeignLibrary { .. } | TailVerifyError::Malformed { .. }
    ));
}

#[test]
fn malformed_json_is_refused() {
    let err = validate_operations_page(b"{not json", library(), 0, 2).unwrap_err();
    assert!(matches!(err, TailVerifyError::Malformed { .. }));
}

#[test]
fn next_after_mismatch_is_refused() {
    let mut remote = Author::new(replica(2));
    remote.library = library();
    let a = add_env(&mut remote, 70);
    let mut body = String::from_utf8(page_body(1, 0, &[(1, a)])).unwrap();
    body = body.replace("\"next_after\":1", "\"next_after\":9");
    let err = validate_operations_page(body.as_bytes(), library(), 0, 2).unwrap_err();
    assert!(matches!(err, TailVerifyError::CursorDiscontinuity { .. }));
}

#[test]
fn claimed_head_without_rows_on_short_page_is_refused() {
    let mut remote = Author::new(replica(2));
    remote.library = library();
    let a = add_env(&mut remote, 70);
    let body = page_body(99, 0, &[(1, a)]);
    let err = validate_operations_page(&body, library(), 0, 2).unwrap_err();
    assert!(matches!(err, TailVerifyError::IncompleteOrGapped { .. }));
}

#[test]
fn empty_page_while_head_ahead_is_refused() {
    let body = page_body(5, 0, &[]);
    let err = validate_operations_page(&body, library(), 0, 2).unwrap_err();
    assert!(matches!(err, TailVerifyError::IncompleteOrGapped { .. }));
}

#[test]
fn mixed_operation_types_are_accepted() {
    let mut remote = Author::new(replica(2));
    remote.library = library();
    let add = add_env(&mut remote, 80);
    let repl = remote.env(SyncOperationV1::ReplaceStrokeLineages {
        notebook_id: nb(1),
        page_id: pg(1),
        replacements: vec![sync_protocol::LineageReplacement {
            root_id: sk(1),
            expected: vec![orig(1)],
            after: vec![orig(1)],
        }],
    });
    let src = InjectedTailSource {
        pages: vec![(0, page_body(2, 0, &[(1, add), (2, repl)]))],
    };
    let tail = acquire_verified_tail(&src, &spec(0, 0)).unwrap();
    assert_eq!(tail.items.len(), 2);
    assert!(matches!(
        tail.items[0].envelope.operation,
        SyncOperationV1::AddStroke { .. }
    ));
    assert!(matches!(
        tail.items[1].envelope.operation,
        SyncOperationV1::ReplaceStrokeLineages { .. }
    ));
}

#[test]
fn replica_client_seq_skip_is_refused() {
    let mut author = Author::new(replica(1));
    author.library = library();
    author.next_seq = 37867;
    let env = add_env(&mut author, 90);
    let src = InjectedTailSource {
        pages: vec![(37928, page_body(37929, 37928, &[(37929, env)]))],
    };
    let err = acquire_verified_tail(&src, &spec(37928, 37865)).unwrap_err();
    assert!(matches!(err, TailVerifyError::ReplicaSequence { .. }));
}

#[test]
fn replica_client_seq_contiguous_after_acked_is_ok() {
    let mut author = Author::new(replica(1));
    author.library = library();
    author.next_seq = 37866;
    let env = add_env(&mut author, 91);
    let src = InjectedTailSource {
        pages: vec![(37928, page_body(37929, 37928, &[(37929, env)]))],
    };
    let tail = acquire_verified_tail(&src, &spec(37928, 37865)).unwrap();
    assert_eq!(tail.replica_ops_in_tail, 1);
}

#[test]
fn wrong_authorization_phrase_does_not_connect() {
    let err = LiveReadGrant::explicit(
        "please",
        "http://127.0.0.1:1".into(),
        "token".into(),
        library(),
    )
    .unwrap_err();
    assert!(matches!(err, TailVerifyError::NotAuthorized { .. }));
}

#[test]
fn empty_url_is_not_authorized() {
    let err = LiveReadGrant::explicit(
        LIVE_READ_AUTHORIZATION_PHRASE,
        "  ".into(),
        "token".into(),
        library(),
    )
    .unwrap_err();
    assert!(matches!(err, TailVerifyError::NotAuthorized { .. }));
}

#[test]
fn default_source_has_no_embedded_endpoint() {
    let src = InjectedTailSource::default();
    let err = src.get_operations_page(0, 1).unwrap_err();
    assert!(matches!(err, TailVerifyError::Transport { .. }));
}

#[test]
fn verified_empty_tail_plans_local_originals() {
    let cp = genesis(vec![notebook(
        nb(1),
        "T",
        vec![blank(pg(1), vec![orig(1)])],
    )]);
    let local = snap(vec![notebook(
        nb(1),
        "T",
        vec![blank(pg(1), vec![orig(1), orig(2)])],
    )]);
    let cursor = 0;
    let src = InjectedTailSource {
        pages: vec![(cursor, page_body(cursor, cursor, &[]))],
    };
    let tail = acquire_verified_tail(&src, &spec(cursor, 0)).unwrap();
    let result = plan_from_verified(
        library(),
        replica(1),
        &cp,
        cursor,
        0,
        &[],
        OutboxInput::Valid { envelopes: &[] },
        &local,
        &tail,
        SequenceAuthority::Known { next_client_seq: 1 },
    );
    let PlanResult::Feasible(plan) = result else {
        panic!("{result:?}");
    };
    assert_eq!(plan.counts.extra_originals, 1);
    assert!(!plan.upload_authorized);
}

#[test]
fn stale_head_below_returned_rows_is_tolerated() {
    let mut remote = Author::new(replica(2));
    remote.library = library();
    let a = add_env(&mut remote, 70);
    let b = add_env(&mut remote, 71);
    // Head observed before a concurrent append; rows include the later seq.
    let body = page_body(1, 0, &[(1, a), (2, b)]);
    let page = validate_operations_page(&body, library(), 0, 2).unwrap();
    assert_eq!(page.items.len(), 2);
    assert_eq!(page.claimed_head, 1);
}

fn replica_last(dir: &std::path::Path, lib: &str) -> Option<String> {
    let conn = rusqlite::Connection::open(dir.join(DB_FILE)).unwrap();
    conn.query_row(
        "SELECT last_client_seq FROM replicas WHERE library_id = ?1",
        [lib],
        |r| r.get(0),
    )
    .optional()
    .unwrap()
}

#[test]
fn in_process_get_operations_does_not_mutate_replica_or_head() {
    let dir = tmp("ro-get");
    let store = Store::open(&dir).unwrap();
    let bytes = snap(vec![notebook(
        nb(1),
        "G",
        vec![blank(pg(1), vec![orig(1)])],
    )])
    .to_canonical_bytes()
    .unwrap();
    let (info, _) = store.bootstrap(&bytes).unwrap();
    let lib = info.library_id.to_string();
    let mut author = Author::new(replica(1));
    author.library = info.library_id;
    let env = add_env(&mut author, 2);
    let body = format!(
        "{{\"envelopes\":[{}]}}",
        String::from_utf8(env_bytes(&env)).unwrap()
    );
    let post = route(
        &store,
        "POST",
        &format!("/v1/libraries/{lib}/operations"),
        body.as_bytes(),
    );
    assert_eq!(post.status, 200, "{}", String::from_utf8_lossy(&post.body));
    let head_before = store.library(&lib).unwrap().head_server_seq;
    let seq_before = replica_last(&dir, &lib);
    let get = route(
        &store,
        "GET",
        &format!("/v1/libraries/{lib}/operations?after=0&limit=100"),
        b"",
    );
    assert_eq!(get.status, 200, "{}", String::from_utf8_lossy(&get.body));
    let page = validate_operations_page(&get.body, info.library_id, 0, 100).unwrap();
    assert_eq!(page.items.len(), 1);
    assert_eq!(store.library(&lib).unwrap().head_server_seq, head_before);
    assert_eq!(replica_last(&dir, &lib), seq_before);
    let get2 = route(&store, "GET", &format!("/v1/libraries/{lib}"), b"");
    assert_eq!(get2.status, 200);
    assert_eq!(store.library(&lib).unwrap().head_server_seq, head_before);
    assert_eq!(replica_last(&dir, &lib), seq_before);
}

#[test]
fn disposable_http_get_does_not_post_or_advance_sequences() {
    let dir = tmp("http-get");
    let store = Store::open(&dir).unwrap();
    let bytes = snap(vec![notebook(
        nb(1),
        "H",
        vec![blank(pg(1), vec![orig(1)])],
    )])
    .to_canonical_bytes()
    .unwrap();
    let (info, _) = store.bootstrap(&bytes).unwrap();
    let mut author = Author::new(replica(2));
    author.library = info.library_id;
    let env = add_env(&mut author, 5);
    store
        .submit(&info.library_id.to_string(), &[env_bytes(&env)])
        .unwrap();
    let token = generate_token().unwrap();
    let handle = ServerHandle::start_with(
        "127.0.0.1:0".parse().unwrap(),
        store,
        ServerConfig {
            auth: Some(TokenVerifier::for_token(&token)),
            web_root: None,
            control: None,
        },
    )
    .unwrap();
    let grant = LiveReadGrant::explicit(
        LIVE_READ_AUTHORIZATION_PHRASE,
        handle.base_url(),
        token,
        info.library_id,
    )
    .unwrap();
    let tail = acquire_verified_tail_http(
        grant,
        &TailVerifySpec {
            library_id: info.library_id,
            replica_id: replica(1),
            checkpoint_cursor: 0,
            acked_client_seq: 0,
            page_limit: 100,
        },
    )
    .unwrap();
    assert!(tail.live_fetch_used);
    assert_eq!(tail.items.len(), 1);
    assert_eq!(tail.observed_through, 1);
    assert!(!tail.upload_authorized);
    assert_eq!(
        handle
            .store
            .library(&info.library_id.to_string())
            .unwrap()
            .head_server_seq,
        1
    );
}

#[test]
fn checkpoint_cursor_is_an_input_not_a_constant() {
    let mut remote = Author::new(replica(2));
    remote.library = library();
    let a = add_env(&mut remote, 70);
    for cursor in [0u64, 7, 37928] {
        let src = InjectedTailSource {
            pages: vec![(
                cursor,
                page_body(cursor + 1, cursor, &[(cursor + 1, a.clone())]),
            )],
        };
        let tail = acquire_verified_tail(&src, &spec(cursor, 0)).unwrap();
        assert_eq!(tail.items[0].server_seq, cursor + 1);
    }
}

#[test]
fn duplicate_server_seq_is_refused() {
    let mut remote = Author::new(replica(2));
    remote.library = library();
    let a = add_env(&mut remote, 70);
    let b = add_env(&mut remote, 71);
    let body = page_body(2, 0, &[(1, a), (1, b)]);
    let err = validate_operations_page(&body, library(), 0, 2).unwrap_err();
    assert!(matches!(err, TailVerifyError::CursorDiscontinuity { .. }));
}

#[test]
fn unknown_operation_type_fails_closed() {
    let mut remote = Author::new(replica(2));
    remote.library = library();
    let a = add_env(&mut remote, 70);
    let mut body = String::from_utf8(page_body(1, 0, &[(1, a)])).unwrap();
    body = body.replace("\"add_stroke\"", "\"not_a_real_op\"");
    let err = validate_operations_page(body.as_bytes(), library(), 0, 2).unwrap_err();
    assert!(matches!(
        err,
        TailVerifyError::Protocol { .. } | TailVerifyError::Malformed { .. }
    ));
}

#[test]
fn checkpoint_ahead_of_server_head_is_refused() {
    let body = page_body(100, 37928, &[]);
    let err = validate_operations_page(&body, library(), 37928, 100).unwrap_err();
    assert!(matches!(err, TailVerifyError::IncompleteOrGapped { .. }));
}

#[test]
fn unauthorized_http_never_reaches_the_socket() {
    assert!(matches!(
        LiveReadGrant::explicit(
            "not the phrase",
            "http://127.0.0.1:9".into(),
            "x".into(),
            library(),
        ),
        Err(TailVerifyError::NotAuthorized { .. })
    ));
}

#[test]
fn wrong_bearer_is_transport_refusal_on_disposable_server() {
    let dir = tmp("http-401");
    let store = Store::open(&dir).unwrap();
    let bytes = snap(vec![notebook(
        nb(1),
        "A",
        vec![blank(pg(1), vec![orig(1)])],
    )])
    .to_canonical_bytes()
    .unwrap();
    let (info, _) = store.bootstrap(&bytes).unwrap();
    let token = generate_token().unwrap();
    let handle = ServerHandle::start_with(
        "127.0.0.1:0".parse().unwrap(),
        store,
        ServerConfig {
            auth: Some(TokenVerifier::for_token(&token)),
            web_root: None,
            control: None,
        },
    )
    .unwrap();
    let grant = LiveReadGrant::explicit(
        LIVE_READ_AUTHORIZATION_PHRASE,
        handle.base_url(),
        "not-the-server-token".into(),
        info.library_id,
    )
    .unwrap();
    let err = acquire_verified_tail_http(
        grant,
        &TailVerifySpec {
            library_id: info.library_id,
            replica_id: replica(1),
            checkpoint_cursor: 0,
            acked_client_seq: 0,
            page_limit: 100,
        },
    )
    .unwrap_err();
    assert!(matches!(err, TailVerifyError::Transport { .. }), "{err:?}");
}

#[test]
fn concurrent_append_after_completing_page_is_outside_this_read() {
    let mut remote = Author::new(replica(2));
    remote.library = library();
    let a = add_env(&mut remote, 70);
    let b = add_env(&mut remote, 71);
    let s = TailVerifySpec {
        page_limit: 1,
        ..spec(10, 0)
    };
    // Completing page last==head=11. A later append (seq 12) is not part of this read's
    // high-water mark; a later authorized re-read would observe it.
    let src = InjectedTailSource {
        pages: vec![
            (10, page_body(11, 10, &[(11, a)])),
            (11, page_body(12, 11, &[(12, b)])),
        ],
    };
    let tail = acquire_verified_tail(&src, &s).unwrap();
    assert_eq!(tail.items.len(), 1);
    assert_eq!(tail.observed_through, 11);
    assert_eq!(tail.claimed_head, 11);
}

/// Validates a saved live GET body and plans against the historical acquisition.
/// Identities come from env; nothing is hardcoded.
#[test]
#[ignore = "manual: set SCRIBE_LIVE_TAIL_PAGE / SCRIBE_LIVE_LIBRARY / SCRIBE_REHEARSAL_LIB"]
fn authorized_saved_live_tail_page_plans() {
    use std::str::FromStr;

    use crate::sync_recovery_extract::extract_read_only;
    use sync_protocol::{LibraryId, ReplicaId};

    let page_path = PathBuf::from(std::env::var("SCRIBE_LIVE_TAIL_PAGE").unwrap());
    let lib = LibraryId::from_str(&std::env::var("SCRIBE_LIVE_LIBRARY").unwrap()).unwrap();
    let replica = ReplicaId::from_str(&std::env::var("SCRIBE_LIVE_REPLICA").unwrap()).unwrap();
    let cursor: u64 = std::env::var("SCRIBE_LIVE_CURSOR")
        .unwrap_or_else(|_| "37928".into())
        .parse()
        .unwrap();
    let acked: u64 = std::env::var("SCRIBE_LIVE_ACKED")
        .unwrap_or_else(|_| "37865".into())
        .parse()
        .unwrap();
    let body = std::fs::read(&page_path).unwrap();
    let page = validate_operations_page(&body, lib, cursor, 100).unwrap();
    assert_eq!(page.claimed_head, cursor);
    let src = InjectedTailSource {
        pages: vec![(cursor, body)],
    };
    let spec = TailVerifySpec {
        library_id: lib,
        replica_id: replica,
        checkpoint_cursor: cursor,
        acked_client_seq: acked,
        page_limit: 100,
    };
    let tail = acquire_verified_tail(&src, &spec).unwrap();
    assert!(tail.complete);
    assert!(!tail.upload_authorized);
    let lib_root = PathBuf::from(std::env::var("SCRIBE_REHEARSAL_LIB").unwrap());
    let sync_root = PathBuf::from(std::env::var("SCRIBE_REHEARSAL_SYNC").unwrap());
    let h = extract_read_only(&lib_root, &sync_root).expect("read-only extract");
    let result = plan_from_verified(
        lib,
        replica,
        &h.checkpoint,
        cursor,
        acked,
        &[],
        OutboxInput::Valid {
            envelopes: &h.pending,
        },
        &h.local,
        &tail,
        h.sequence_authority,
    );
    let PlanResult::Feasible(plan) = result else {
        panic!("live empty/validated tail blocked: {result:?}");
    };
    assert!(!plan.upload_authorized);
    let replace_ops = plan
        .candidates
        .iter()
        .filter(|c| matches!(c.operation, SyncOperationV1::ReplaceStrokeLineages { .. }))
        .count();
    let add_ops = plan
        .candidates
        .iter()
        .filter(|c| matches!(c.operation, SyncOperationV1::AddStroke { .. }))
        .count();
    let create_ops = plan
        .candidates
        .iter()
        .filter(|c| matches!(c.operation, SyncOperationV1::CreatePage { .. }))
        .count();
    let out = PathBuf::from(std::env::var("SCRIBE_LIVE_OUT").unwrap());
    let report = serde_json::json!({
        "feasible": true,
        "upload_authorized": false,
        "exact_state_proven": plan.exact_state_proven,
        "legal_submission_order": plan.sequencing.legal_submission_order,
        "extra_originals": plan.counts.extra_originals,
        "lineage_replacements": plan.counts.lineage_replacements,
        "extra_pages": plan.counts.extra_pages,
        "add_stroke_ops": add_ops,
        "replace_stroke_lineages_ops": replace_ops,
        "create_page_ops": create_ops,
        "candidate_total": plan.candidates.len(),
        "tail_ops": tail.items.len(),
        "claimed_head": tail.claimed_head,
        "observed_through": tail.observed_through,
        "replica_ops_in_tail": tail.replica_ops_in_tail,
        "identity_allocated_pages": h.identity_allocated_pages,
        "pending_count": h.pending.len(),
        "fingerprint": plan.fingerprint,
    });
    std::fs::write(
        out.join("planner-on-live-tail.json"),
        serde_json::to_vec_pretty(&report).unwrap(),
    )
    .unwrap();
    assert_eq!(tail.items.len(), 0);
    assert_eq!(plan.counts.extra_originals, 116);
    assert_eq!(plan.counts.lineage_replacements, 3);
    assert_eq!(plan.counts.extra_pages, 2);
    assert_eq!(add_ops, 116);
    assert_eq!(replace_ops, 2);
    assert_eq!(create_ops, 2);
    assert_eq!(plan.candidates.len(), 120);
    assert!(plan.exact_state_proven);
    assert!(plan.sequencing.legal_submission_order);
}
