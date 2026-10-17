//! Slice 18E.3: `ReplaceStrokeLineages` through the real HTTP ledger (disposable loopback).
//! Synthetic identities only. Default server gate stays off; tests that accept the operation
//! opt in explicitly.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier};
use std::time::{SystemTime, UNIX_EPOCH};

use scribe_sync_server::{generate_token, ServerConfig, ServerHandle, Store, TokenVerifier};
use serde::Deserialize;
use serde_json::Value;
use sync_engine::testkit::{self, genesis, line, nb, pg, replica, sk, Author};
use sync_engine::{replay, LibraryState};
use sync_protocol::{
    snapshot_sha256, to_canonical_json, BackgroundRef, LineageReplacement, NotebookId, PageId,
    PageSpace, PageSpec, SnapshotNotebook, SnapshotPage, StrokeId, SyncEnvelopeV1,
    SyncOperationV1 as Op, SyncSnapshotV1, SyncStroke,
};

static N: AtomicU64 = AtomicU64::new(0);

fn tmp(label: &str) -> PathBuf {
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = std::env::temp_dir().join(format!(
        "scribe-s2a-rsl-{label}-{n}-{}",
        N.fetch_add(1, Ordering::SeqCst)
    ));
    let _ = std::fs::remove_dir_all(&p);
    p
}

fn start(dir: &Path) -> ServerHandle {
    ServerHandle::start("127.0.0.1:0".parse().unwrap(), Store::open(dir).unwrap()).unwrap()
}

struct Http {
    base: String,
    agent: ureq::Agent,
}

impl Http {
    fn of(h: &ServerHandle) -> Self {
        Self::at(h.base_url())
    }
    fn at(base: String) -> Self {
        Self {
            base,
            agent: ureq::AgentBuilder::new().build(),
        }
    }
    fn call(&self, method: &str, path: &str, body: Option<&[u8]>) -> (u16, Vec<u8>) {
        self.call_auth(method, path, body, None)
    }
    fn call_auth(
        &self,
        method: &str,
        path: &str,
        body: Option<&[u8]>,
        auth: Option<&str>,
    ) -> (u16, Vec<u8>) {
        let mut req = self.agent.request(method, &format!("{}{path}", self.base));
        if let Some(a) = auth {
            req = req.set("Authorization", a);
        }
        let res = match body {
            Some(b) => req.send_bytes(b),
            None => req.call(),
        };
        let resp = match res {
            Ok(r) => r,
            Err(ureq::Error::Status(_, r)) => r,
            Err(e) => panic!("transport error: {e}"),
        };
        let status = resp.status();
        let mut out = Vec::new();
        std::io::Read::read_to_end(&mut resp.into_reader(), &mut out).unwrap();
        (status, out)
    }
    fn json(&self, method: &str, path: &str, body: Option<&[u8]>) -> (u16, Value) {
        let (s, b) = self.call(method, path, body);
        (s, serde_json::from_slice(&b).unwrap_or(Value::Null))
    }
    fn bootstrap(&self, snap: &SyncSnapshotV1) -> (u16, Value) {
        let bytes = to_canonical_json(snap).unwrap();
        self.json("POST", "/v1/libraries", Some(&bytes))
    }
    fn submit_raw(&self, lib: &str, envs: &[Vec<u8>]) -> (u16, Value) {
        let mut body = b"{\"envelopes\":[".to_vec();
        for (i, e) in envs.iter().enumerate() {
            if i > 0 {
                body.push(b',');
            }
            body.extend_from_slice(e);
        }
        body.extend_from_slice(b"]}");
        self.json(
            "POST",
            &format!("/v1/libraries/{lib}/operations"),
            Some(&body),
        )
    }
    fn submit(&self, envs: &[SyncEnvelopeV1]) -> (u16, Value) {
        let raws: Vec<Vec<u8>> = envs.iter().map(|e| to_canonical_json(e).unwrap()).collect();
        self.submit_raw(&testkit::library().to_string(), &raws)
    }
    fn tail(&self) -> Vec<(i64, Vec<u8>)> {
        let lib = testkit::library();
        let mut after = 0;
        let mut out = Vec::new();
        loop {
            let (s, body) = self.call(
                "GET",
                &format!("/v1/libraries/{lib}/operations?after={after}&limit=50"),
                None,
            );
            assert_eq!(s, 200);
            #[derive(Deserialize)]
            struct E<'a> {
                server_seq: i64,
                #[serde(borrow)]
                envelope: &'a serde_json::value::RawValue,
            }
            #[derive(Deserialize)]
            struct T<'a> {
                #[serde(borrow)]
                operations: Vec<E<'a>>,
                next_after: i64,
            }
            let t: T<'_> = serde_json::from_slice(&body).unwrap();
            if t.operations.is_empty() {
                return out;
            }
            for e in &t.operations {
                out.push((e.server_seq, e.envelope.get().as_bytes().to_vec()));
            }
            after = t.next_after;
        }
    }
    fn snapshot(&self) -> (i64, Vec<u8>, String) {
        let lib = testkit::library();
        let (s, body) = self.call("GET", &format!("/v1/libraries/{lib}/snapshot"), None);
        assert_eq!(s, 200);
        #[derive(Deserialize)]
        struct S<'a> {
            through_server_seq: i64,
            snapshot_sha256: String,
            #[serde(borrow)]
            snapshot: &'a serde_json::value::RawValue,
        }
        let v: S<'_> = serde_json::from_slice(&body).unwrap();
        (
            v.through_server_seq,
            v.snapshot.get().as_bytes().to_vec(),
            v.snapshot_sha256,
        )
    }
    fn genesis(&self) -> Vec<u8> {
        let lib = testkit::library();
        let (s, body) = self.call("GET", &format!("/v1/libraries/{lib}/genesis"), None);
        assert_eq!(s, 200);
        #[derive(Deserialize)]
        struct G<'a> {
            #[serde(borrow)]
            snapshot: &'a serde_json::value::RawValue,
        }
        let g: G<'_> = serde_json::from_slice(&body).unwrap();
        g.snapshot.get().as_bytes().to_vec()
    }
}

fn put_genesis_blob(http: &Http) {
    let (bytes, _) = testkit::blob_background();
    let sha = sync_protocol::sha256_hex(&bytes);
    let (s, _) = http.call("PUT", &format!("/v1/blobs/{sha}"), Some(&bytes));
    assert!(s == 201 || s == 200, "{s}");
}

fn bootstrapped(label: &str) -> (PathBuf, ServerHandle, Http) {
    let dir = tmp(label);
    let h = start(&dir);
    let http = Http::of(&h);
    put_genesis_blob(&http);
    let (s, v) = http.bootstrap(&genesis());
    assert_eq!(s, 201, "{v}");
    (dir, h, http)
}

fn enable_replace(h: &ServerHandle) {
    h.store
        .accept_replace_stroke_lineages
        .store(true, Ordering::SeqCst);
}

fn bootstrapped_accepting(label: &str) -> (PathBuf, ServerHandle, Http) {
    let t = bootstrapped(label);
    enable_replace(&t.1);
    t
}

fn assert_unchanged(http: &Http, before_head: i64, before_snap: &[u8]) {
    let (through, bytes, _) = http.snapshot();
    assert_eq!(through, before_head, "head must not advance on refusal");
    assert_eq!(bytes, before_snap, "snapshot must not change on refusal");
}

fn assert_replay_matches(http: &Http) -> String {
    let genesis = SyncSnapshotV1::parse(&http.genesis()).unwrap();
    let tail = http.tail();
    for (i, (seq, _)) in tail.iter().enumerate() {
        assert_eq!(*seq, i as i64 + 1);
    }
    let envs: Vec<SyncEnvelopeV1> = tail
        .iter()
        .map(|(_, b)| SyncEnvelopeV1::parse(b).unwrap())
        .collect();
    let replayed = replay(genesis, &envs).unwrap();
    let (through, server_bytes, server_sha) = http.snapshot();
    assert_eq!(through, tail.len() as i64);
    assert_eq!(
        replayed.snapshot.to_canonical_bytes().unwrap(),
        server_bytes
    );
    let h = snapshot_sha256(&replayed.snapshot).unwrap().to_string();
    assert_eq!(h, server_sha);
    h
}

fn sk1() -> SyncStroke {
    genesis().notebooks[0].pages[0].strokes[1].clone()
}

fn frag(id: u64, root: u64, y: f32) -> SyncStroke {
    line(sk(id), sk(root), (200.0, y), (400.0, y), 3, false)
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

fn replace_env(
    a: &mut Author,
    notebook_id: NotebookId,
    page_id: PageId,
    replacements: Vec<LineageReplacement>,
) -> SyncEnvelopeV1 {
    a.env(Op::ReplaceStrokeLineages {
        notebook_id,
        page_id,
        replacements,
    })
}

fn repl(root: StrokeId, expected: Vec<SyncStroke>, after: Vec<SyncStroke>) -> LineageReplacement {
    LineageReplacement {
        root_id: root,
        expected,
        after,
    }
}

fn blank_page(id: PageId, strokes: Vec<SyncStroke>) -> SnapshotPage {
    SnapshotPage {
        page_id: id,
        background: BackgroundRef::Blank,
        render: testkit::render(false),
        strokes,
        texts: Vec::new(),
        links: Vec::new(),
    }
}

fn page_spec_plain(id: PageId) -> PageSpec {
    PageSpec {
        page_id: id,
        background: BackgroundRef::Blank,
        render: testkit::render(false),
    }
}

fn notebook(id: NotebookId, title: &str, pages: Vec<SnapshotPage>) -> SnapshotNotebook {
    SnapshotNotebook {
        notebook_id: id,
        title: title.into(),
        pages,
        folder_id: None,
    }
}

fn custom_snap(notebooks: Vec<SnapshotNotebook>) -> SyncSnapshotV1 {
    SyncSnapshotV1 {
        v: 1,
        library_id: testkit::library(),
        page_space: PageSpace::CANONICAL,
        notebooks,
        folders: Vec::new(),
    }
}

/// Pre-18E.2 operation set: unknown `type` (including `replace_stroke_lineages`) fails closed.
#[derive(Deserialize)]
struct LegacyEnvelope {
    #[allow(dead_code)]
    operation: LegacyOp,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum LegacyOp {
    CreateNotebook {
        #[serde(flatten)]
        _r: Value,
    },
    RenameNotebook {
        #[serde(flatten)]
        _r: Value,
    },
    DeleteNotebook {
        #[serde(flatten)]
        _r: Value,
    },
    CreatePage {
        #[serde(flatten)]
        _r: Value,
    },
    MovePage {
        #[serde(flatten)]
        _r: Value,
    },
    DeletePage {
        #[serde(flatten)]
        _r: Value,
    },
    AddStroke {
        #[serde(flatten)]
        _r: Value,
    },
    EraseRegion {
        #[serde(flatten)]
        _r: Value,
    },
    DeleteStrokeLineage {
        #[serde(flatten)]
        _r: Value,
    },
    MoveStrokeLineages {
        #[serde(flatten)]
        _r: Value,
    },
    AddText {
        #[serde(flatten)]
        _r: Value,
    },
    UpdateText {
        #[serde(flatten)]
        _r: Value,
    },
    DeleteText {
        #[serde(flatten)]
        _r: Value,
    },
    AddPageLink {
        #[serde(flatten)]
        _r: Value,
    },
    DeletePageLink {
        #[serde(flatten)]
        _r: Value,
    },
    CreateFolder {
        #[serde(flatten)]
        _r: Value,
    },
    RenameFolder {
        #[serde(flatten)]
        _r: Value,
    },
    DeleteFolder {
        #[serde(flatten)]
        _r: Value,
    },
    SetNotebookFolder {
        #[serde(flatten)]
        _r: Value,
    },
}

fn legacy_cursor(tail: &[(i64, Vec<u8>)]) -> Result<i64, String> {
    let mut cursor = 0i64;
    for (seq, bytes) in tail {
        serde_json::from_slice::<LegacyEnvelope>(bytes)
            .map_err(|e| format!("TAIL_REFUSED at {seq}: {e}"))?;
        cursor = *seq;
    }
    Ok(cursor)
}

fn strokes_on(snap: &SyncSnapshotV1, notebook: NotebookId, page: PageId) -> Vec<SyncStroke> {
    snap.notebooks
        .iter()
        .find(|n| n.notebook_id == notebook)
        .unwrap()
        .pages
        .iter()
        .find(|p| p.page_id == page)
        .unwrap()
        .strokes
        .clone()
}

// ------------------------------------------------------------------ gate

#[test]
fn default_gate_refuses_new_replace_without_advancing_head() {
    let (_dir, _h, http) = bootstrapped("gate-off");
    let mut a = Author::new(replica(1));
    let env = replace_env(&mut a, nb(1), pg(1), vec![repl(sk(1), vec![sk1()], vec![])]);
    let (s, v) = http.submit(&[env]);
    assert_eq!(s, 400, "{v}");
    assert_eq!(v["error"], "unsupported_operation");
    assert_eq!(http.snapshot().0, 0);
}

#[test]
fn gate_on_accepts_well_formed_replacement_and_preserves_fragment_ids() {
    let (_dir, h, http) = bootstrapped_accepting("accept");
    let mut a = Author::new(replica(1));
    let after = vec![frag(60, 1, 300.0), frag(61, 1, 310.0)];
    let env = replace_env(
        &mut a,
        nb(1),
        pg(1),
        vec![repl(sk(1), vec![sk1()], after.clone())],
    );
    let (s, v) = http.submit(&[env]);
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["results"][0]["duplicate"], false);
    assert_eq!(v["results"][0]["outcome"], "applied");
    assert_eq!(v["head_server_seq"], 1);
    let snap = SyncSnapshotV1::parse(&http.snapshot().1).unwrap();
    let ids: Vec<_> = strokes_on(&snap, nb(1), pg(1))
        .into_iter()
        .filter(|s| s.root_id == sk(1))
        .map(|s| s.stroke_id)
        .collect();
    assert_eq!(ids, vec![sk(60), sk(61)]);
    assert!(h
        .store
        .verify_replay(&testkit::library().to_string())
        .unwrap());
    assert_replay_matches(&http);
}

// ------------------------------------------------------------------ auth

#[test]
fn authed_server_rejects_replace_without_bearer_and_accepts_with_token() {
    let dir = tmp("auth-rsl");
    let token = generate_token().unwrap();
    let store = Store::open(&dir).unwrap();
    store
        .accept_replace_stroke_lineages
        .store(true, Ordering::SeqCst);
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
    let http = Http::of(&h);
    let bearer = format!("Bearer {token}");
    let (bytes, _) = testkit::blob_background();
    let sha = sync_protocol::sha256_hex(&bytes);
    let (s, _) = http.call_auth(
        "PUT",
        &format!("/v1/blobs/{sha}"),
        Some(&bytes),
        Some(&bearer),
    );
    assert!(s == 201 || s == 200, "{s}");
    let gen = to_canonical_json(&genesis()).unwrap();
    let (s, _) = http.call_auth("POST", "/v1/libraries", Some(&gen), Some(&bearer));
    assert_eq!(s, 201);
    let mut a = Author::new(replica(1));
    let env = replace_env(
        &mut a,
        nb(1),
        pg(1),
        vec![repl(sk(1), vec![sk1()], vec![frag(9, 1, 1.0)])],
    );
    let raw = to_canonical_json(&env).unwrap();
    let mut body = b"{\"envelopes\":[".to_vec();
    body.extend_from_slice(&raw);
    body.extend_from_slice(b"]}");
    let lib = testkit::library().to_string();
    let path = format!("/v1/libraries/{lib}/operations");
    let (s, b) = http.call_auth("POST", &path, Some(&body), None);
    assert_eq!(s, 401, "{}", String::from_utf8_lossy(&b));
    let (s, b) = http.call_auth("POST", &path, Some(&body), Some(&bearer));
    let v: Value = serde_json::from_slice(&b).unwrap();
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["results"][0]["outcome"], "applied");
}

// ------------------------------------------------------------------ validation / conflicts

#[test]
fn malformed_and_oversized_payloads_are_refused_before_mutation() {
    let (_dir, _h, http) = bootstrapped_accepting("malformed");
    let before = http.snapshot();
    let mut a = Author::new(replica(1));
    let empty = replace_env(&mut a, nb(1), pg(1), vec![]);
    let (s, v) = http.submit(&[empty]);
    assert_eq!(s, 422, "{v}");
    assert_eq!(v["semantic_code"], "invalid_operation");
    assert_unchanged(&http, before.0, &before.1);

    let mut inf = frag(8, 1, 10.0);
    inf.points[0].x = f32::INFINITY;
    let mut b = Author::new(replica(2));
    let bad_geom = replace_env(
        &mut b,
        nb(1),
        pg(1),
        vec![repl(sk(1), vec![sk1()], vec![inf])],
    );
    let (s, v) = http.submit(&[bad_geom]);
    // Non-finite coordinates are not canonical JSON numbers; parse fails closed.
    assert_eq!(s, 400, "{v}");
    assert_eq!(v["error"], "invalid_envelope");
    assert_unchanged(&http, before.0, &before.1);
}

#[test]
fn same_lineage_conflict_refuses_and_does_not_advance() {
    let (_dir, _h, http) = bootstrapped_accepting("conflict");
    let mut remote = Author::new(replica(2));
    let del = remote.env(Op::DeleteStrokeLineage {
        notebook_id: nb(1),
        page_id: pg(1),
        root_id: sk(1),
    });
    assert_eq!(http.submit(&[del]).0, 200);
    let before = http.snapshot();
    let mut local = Author::new(replica(1));
    let env = replace_env(
        &mut local,
        nb(1),
        pg(1),
        vec![repl(sk(1), vec![sk1()], vec![frag(60, 1, 300.0)])],
    );
    let (s, v) = http.submit(&[env]);
    assert_eq!(s, 422, "{v}");
    assert_eq!(v["semantic_code"], "conflict_same_lineage");
    assert_unchanged(&http, before.0, &before.1);
}

#[test]
fn unrelated_lineage_is_preserved() {
    let (_dir, _h, http) = bootstrapped_accepting("unrelated");
    let mut remote = Author::new(replica(2));
    let extra = orig(50);
    let add = remote.env(Op::AddStroke {
        notebook_id: nb(1),
        page_id: pg(1),
        stroke: extra.clone(),
    });
    assert_eq!(http.submit(&[add]).0, 200);
    let mut local = Author::new(replica(1));
    let env = replace_env(
        &mut local,
        nb(1),
        pg(1),
        vec![repl(sk(1), vec![sk1()], vec![frag(60, 1, 300.0)])],
    );
    assert_eq!(http.submit(&[env]).0, 200);
    let snap = SyncSnapshotV1::parse(&http.snapshot().1).unwrap();
    let ids: Vec<_> = strokes_on(&snap, nb(1), pg(1))
        .into_iter()
        .map(|s| s.stroke_id)
        .collect();
    assert!(ids.contains(&extra.stroke_id));
    assert!(ids.contains(&sk(60)));
    assert_replay_matches(&http);
}

#[test]
fn identity_collision_is_refused() {
    let (_dir, _h, http) = bootstrapped_accepting("collide");
    let before = http.snapshot();
    let other = genesis().notebooks[1].pages[0].strokes[0].clone(); // sk(2) on another page
    let mut colliding = frag(3, 1, 10.0);
    colliding.stroke_id = other.stroke_id;
    colliding.root_id = sk(1);
    let mut a = Author::new(replica(1));
    let env = replace_env(
        &mut a,
        nb(1),
        pg(1),
        vec![repl(sk(1), vec![sk1()], vec![colliding])],
    );
    let (s, v) = http.submit(&[env]);
    assert_eq!(s, 422, "{v}");
    assert_eq!(v["semantic_code"], "duplicate_stroke");
    assert_unchanged(&http, before.0, &before.1);
}

#[test]
fn multi_root_conflict_refuses_entire_operation() {
    let (_dir, _h, http) = bootstrapped_accepting("multi");
    let extra = orig(80);
    let mut setup = Author::new(replica(9));
    assert_eq!(
        http.submit(&[setup.env(Op::AddStroke {
            notebook_id: nb(1),
            page_id: pg(1),
            stroke: extra.clone(),
        })])
        .0,
        200
    );
    let mut remote = Author::new(replica(2));
    assert_eq!(
        http.submit(&[remote.env(Op::DeleteStrokeLineage {
            notebook_id: nb(1),
            page_id: pg(1),
            root_id: extra.root_id,
        })])
        .0,
        200
    );
    let before = http.snapshot();
    let mut local = Author::new(replica(1));
    let env = replace_env(
        &mut local,
        nb(1),
        pg(1),
        vec![
            repl(sk(1), vec![sk1()], vec![frag(10, 1, 1.0)]),
            repl(extra.root_id, vec![extra.clone()], vec![frag(11, 80, 2.0)]),
        ],
    );
    let (s, v) = http.submit(&[env]);
    assert_eq!(s, 422, "{v}");
    assert_eq!(v["semantic_code"], "conflict_same_lineage");
    assert_unchanged(&http, before.0, &before.1);
}

#[test]
fn deleted_page_new_submit_is_refused() {
    let (_dir, _h, http) = bootstrapped_accepting("del-page");
    let mut remote = Author::new(replica(2));
    assert_eq!(
        http.submit(&[remote.env(Op::DeletePage {
            notebook_id: nb(1),
            page_id: pg(1),
        })])
        .0,
        200
    );
    let before = http.snapshot();
    let mut local = Author::new(replica(1));
    let env = replace_env(
        &mut local,
        nb(1),
        pg(1),
        vec![repl(sk(1), vec![sk1()], vec![frag(9, 1, 1.0)])],
    );
    let (s, v) = http.submit(&[env]);
    assert_eq!(s, 422, "{v}");
    assert_eq!(v["semantic_code"], "invalid_operation");
    assert!(v["message"]
        .as_str()
        .unwrap_or("")
        .contains("deleted page or notebook"));
    assert_unchanged(&http, before.0, &before.1);
}

#[test]
fn deleted_notebook_new_submit_is_refused() {
    let (_dir, _h, http) = bootstrapped_accepting("del-nb");
    let mut remote = Author::new(replica(2));
    assert_eq!(
        http.submit(&[remote.env(Op::DeleteNotebook { notebook_id: nb(3) })])
            .0,
        200
    );
    let before = http.snapshot();
    let mut local = Author::new(replica(1));
    let env = replace_env(
        &mut local,
        nb(3),
        pg(4),
        vec![repl(sk(1), vec![], vec![frag(9, 1, 1.0)])],
    );
    let (s, v) = http.submit(&[env]);
    assert_eq!(s, 422, "{v}");
    assert_eq!(v["semantic_code"], "invalid_operation");
    assert_unchanged(&http, before.0, &before.1);
}

// ------------------------------------------------------------------ identity / sequence

#[test]
fn duplicate_operation_id_retries_without_duplicating_history() {
    let (_dir, _h, http) = bootstrapped_accepting("dup-id");
    let mut a = Author::new(replica(1));
    let env = replace_env(
        &mut a,
        nb(1),
        pg(1),
        vec![repl(sk(1), vec![sk1()], vec![frag(60, 1, 300.0)])],
    );
    assert_eq!(http.submit(std::slice::from_ref(&env)).0, 200);
    let (s, v) = http.submit(std::slice::from_ref(&env));
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["results"][0]["duplicate"], true);
    assert_eq!(v["results"][0]["server_seq"], 1);
    assert_eq!(http.tail().len(), 1);
}

#[test]
fn same_operation_id_different_payload_conflicts() {
    let (_dir, _h, http) = bootstrapped_accepting("id-clash");
    let mut a = Author::new(replica(1));
    let env = replace_env(
        &mut a,
        nb(1),
        pg(1),
        vec![repl(sk(1), vec![sk1()], vec![frag(60, 1, 300.0)])],
    );
    assert_eq!(http.submit(std::slice::from_ref(&env)).0, 200);
    let mut forged = env.clone();
    if let Op::ReplaceStrokeLineages { replacements, .. } = &mut forged.operation {
        replacements[0].after = vec![frag(61, 1, 400.0)];
    }
    let (s, v) = http.submit(&[forged]);
    assert_eq!(
        (s, v["error"].as_str()),
        (409, Some("operation_id_conflict"))
    );
    assert_eq!(http.tail().len(), 1);
}

#[test]
fn same_client_seq_different_operation_conflicts() {
    let (_dir, _h, http) = bootstrapped_accepting("seq-clash");
    let mut a = Author::new(replica(1));
    let env = replace_env(
        &mut a,
        nb(1),
        pg(1),
        vec![repl(sk(1), vec![sk1()], vec![frag(60, 1, 300.0)])],
    );
    assert_eq!(http.submit(std::slice::from_ref(&env)).0, 200);
    let mut clash = env.clone();
    clash.operation_id = Author::new(replica(9)).op_id(1);
    let (s, v) = http.submit(&[clash]);
    assert_eq!((s, v["error"].as_str()), (409, Some("client_seq_conflict")));
    assert_eq!(http.tail().len(), 1);
}

#[test]
fn matching_after_does_not_bypass_client_seq() {
    let (_dir, _h, http) = bootstrapped_accepting("after-seq");
    let mut a = Author::new(replica(1));
    let first = replace_env(
        &mut a,
        nb(1),
        pg(1),
        vec![repl(sk(1), vec![sk1()], vec![frag(60, 1, 300.0)])],
    );
    assert_eq!(http.submit(&[first]).0, 200);
    let mut b = Author::new(replica(1));
    b.next_seq = 3; // skip 2
    let skipped = replace_env(
        &mut b,
        nb(1),
        pg(1),
        vec![repl(sk(1), vec![sk1()], vec![frag(60, 1, 300.0)])],
    );
    let (s, v) = http.submit(&[skipped]);
    assert_eq!(
        (s, v["error"].as_str()),
        (409, Some("client_seq_out_of_order"))
    );
    assert_eq!(http.snapshot().0, 1);
}

#[test]
fn new_operation_with_matching_after_is_accepted_as_ledger_noop() {
    // Existing protocol: NoOp still occupies one history slot after identity/seq checks.
    // State equality alone did not skip those checks (see matching_after_does_not_bypass_client_seq).
    let (_dir, _h, http) = bootstrapped_accepting("after-noop");
    let mut a = Author::new(replica(1));
    let first = replace_env(
        &mut a,
        nb(1),
        pg(1),
        vec![repl(sk(1), vec![sk1()], vec![frag(60, 1, 300.0)])],
    );
    assert_eq!(http.submit(&[first]).0, 200);
    let snap = http.snapshot().1;
    let mut b = Author::new(replica(2));
    let second = replace_env(
        &mut b,
        nb(1),
        pg(1),
        vec![repl(sk(1), vec![sk1()], vec![frag(60, 1, 300.0)])],
    );
    let (s, v) = http.submit(&[second]);
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["results"][0]["outcome"], "noop");
    assert_eq!(v["results"][0]["duplicate"], false);
    assert_eq!(http.snapshot().0, 2);
    assert_eq!(http.snapshot().1, snap);
}

#[test]
fn empty_after_clears_only_the_named_lineage() {
    let (_dir, _h, http) = bootstrapped_accepting("empty-after");
    let mut a = Author::new(replica(1));
    let env = replace_env(&mut a, nb(1), pg(1), vec![repl(sk(1), vec![sk1()], vec![])]);
    assert_eq!(http.submit(&[env]).0, 200);
    let snap = SyncSnapshotV1::parse(&http.snapshot().1).unwrap();
    let roots: Vec<_> = strokes_on(&snap, nb(1), pg(1))
        .into_iter()
        .map(|s| s.root_id)
        .collect();
    assert!(!roots.contains(&sk(1)));
    assert!(!roots.is_empty());
}

// ------------------------------------------------------------------ durability

#[test]
fn injected_commit_failure_consumes_no_sequence_then_retry_succeeds() {
    let (_dir, h, http) = bootstrapped_accepting("crash");
    let mut a = Author::new(replica(1));
    let env = replace_env(
        &mut a,
        nb(1),
        pg(1),
        vec![repl(sk(1), vec![sk1()], vec![frag(60, 1, 300.0)])],
    );
    h.store
        .hooks
        .fail_submit_before_commit
        .store(true, Ordering::SeqCst);
    let (s, v) = http.submit(std::slice::from_ref(&env));
    assert_eq!((s, v["error"].as_str()), (500, Some("internal")));
    h.store
        .hooks
        .fail_submit_before_commit
        .store(false, Ordering::SeqCst);
    assert_eq!(http.snapshot().0, 0);
    let (s, v) = http.submit(std::slice::from_ref(&env));
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["results"][0]["duplicate"], false);
    assert_eq!(v["head_server_seq"], 1);
    assert_replay_matches(&http);
}

#[test]
fn restart_and_verify_replay_preserve_fragment_ids() {
    let (dir, h, http) = bootstrapped_accepting("restart");
    let mut a = Author::new(replica(1));
    let env = replace_env(
        &mut a,
        nb(1),
        pg(1),
        vec![repl(
            sk(1),
            vec![sk1()],
            vec![frag(60, 1, 300.0), frag(61, 1, 310.0)],
        )],
    );
    assert_eq!(http.submit(&[env]).0, 200);
    let hash = assert_replay_matches(&http);
    drop(h);
    let h2 = start(&dir);
    enable_replace(&h2);
    let http2 = Http::of(&h2);
    assert_eq!(assert_replay_matches(&http2), hash);
    assert!(h2
        .store
        .verify_replay(&testkit::library().to_string())
        .unwrap());
    let snap = SyncSnapshotV1::parse(&http2.snapshot().1).unwrap();
    let ids: Vec<_> = strokes_on(&snap, nb(1), pg(1))
        .into_iter()
        .filter(|s| s.root_id == sk(1))
        .map(|s| s.stroke_id)
        .collect();
    assert_eq!(ids, vec![sk(60), sk(61)]);
}

#[test]
fn accepted_replace_then_delete_page_still_replays() {
    let (_dir, h, http) = bootstrapped_accepting("replay-del");
    let mut a = Author::new(replica(1));
    let env = replace_env(
        &mut a,
        nb(1),
        pg(1),
        vec![repl(sk(1), vec![sk1()], vec![frag(60, 1, 300.0)])],
    );
    assert_eq!(http.submit(&[env]).0, 200);
    assert_eq!(
        http.submit(&[a.env(Op::DeletePage {
            notebook_id: nb(1),
            page_id: pg(1),
        })])
        .0,
        200
    );
    assert!(h
        .store
        .verify_replay(&testkit::library().to_string())
        .unwrap());
    assert_replay_matches(&http);
}

// ------------------------------------------------------------------ concurrency

#[test]
fn concurrent_same_lineage_at_most_one_succeeds() {
    let (_dir, _h, http) = bootstrapped_accepting("race-same");
    let base = http.base.clone();
    let barrier = Arc::new(Barrier::new(2));
    let mut a = Author::new(replica(1));
    let mut b = Author::new(replica(2));
    let e1 = replace_env(
        &mut a,
        nb(1),
        pg(1),
        vec![repl(sk(1), vec![sk1()], vec![frag(10, 1, 1.0)])],
    );
    let e2 = replace_env(
        &mut b,
        nb(1),
        pg(1),
        vec![repl(sk(1), vec![sk1()], vec![frag(11, 1, 2.0)])],
    );
    let outcomes = std::thread::scope(|s| {
        let b1 = Arc::clone(&barrier);
        let base1 = base.clone();
        let t1 = s.spawn(move || {
            let http = Http::at(base1);
            b1.wait();
            http.submit(&[e1])
        });
        let t2 = s.spawn(move || {
            let http = Http::at(base);
            barrier.wait();
            http.submit(&[e2])
        });
        vec![t1.join().unwrap(), t2.join().unwrap()]
    });
    let oks: Vec<_> = outcomes.iter().filter(|(s, _)| *s == 200).collect();
    let conflicts: Vec<_> = outcomes
        .iter()
        .filter(|(s, v)| *s == 422 && v["semantic_code"] == "conflict_same_lineage")
        .collect();
    assert_eq!(oks.len(), 1, "{outcomes:?}");
    assert_eq!(conflicts.len(), 1, "{outcomes:?}");
    assert_eq!(http.snapshot().0, 1);
    assert_eq!(http.tail().len(), 1);
    assert_replay_matches(&http);
}

#[test]
fn concurrent_unrelated_lineage_and_replace_both_succeed() {
    let (_dir, _h, http) = bootstrapped_accepting("race-unrel");
    let base = http.base.clone();
    let barrier = Arc::new(Barrier::new(2));
    let mut a = Author::new(replica(1));
    let mut b = Author::new(replica(2));
    let replace = replace_env(
        &mut a,
        nb(1),
        pg(1),
        vec![repl(sk(1), vec![sk1()], vec![frag(60, 1, 300.0)])],
    );
    let add = b.env(Op::AddStroke {
        notebook_id: nb(1),
        page_id: pg(1),
        stroke: orig(50),
    });
    std::thread::scope(|s| {
        let b1 = Arc::clone(&barrier);
        let base1 = base.clone();
        s.spawn(move || {
            let http = Http::at(base1);
            b1.wait();
            assert_eq!(http.submit(&[replace]).0, 200);
        });
        s.spawn(move || {
            let http = Http::at(base);
            barrier.wait();
            assert_eq!(http.submit(&[add]).0, 200);
        });
    });
    assert_eq!(http.snapshot().0, 2);
    let snap = SyncSnapshotV1::parse(&http.snapshot().1).unwrap();
    let ids: Vec<_> = strokes_on(&snap, nb(1), pg(1))
        .into_iter()
        .map(|s| s.stroke_id)
        .collect();
    assert!(ids.contains(&sk(60)));
    assert!(ids.contains(&sk(50)));
    assert_replay_matches(&http);
}

#[test]
fn concurrent_page_delete_versus_replace_no_resurrection() {
    let (_dir, _h, http) = bootstrapped_accepting("race-del");
    let base = http.base.clone();
    let barrier = Arc::new(Barrier::new(2));
    let mut a = Author::new(replica(1));
    let mut b = Author::new(replica(2));
    let replace = replace_env(
        &mut a,
        nb(1),
        pg(1),
        vec![repl(sk(1), vec![sk1()], vec![frag(60, 1, 300.0)])],
    );
    let del = b.env(Op::DeletePage {
        notebook_id: nb(1),
        page_id: pg(1),
    });
    let outcomes = std::thread::scope(|s| {
        let b1 = Arc::clone(&barrier);
        let base1 = base.clone();
        let t1 = s.spawn(move || {
            let http = Http::at(base1);
            b1.wait();
            http.submit(&[replace])
        });
        let t2 = s.spawn(move || {
            let http = Http::at(base);
            barrier.wait();
            http.submit(&[del])
        });
        (t1.join().unwrap(), t2.join().unwrap())
    });
    let (rs, rv) = &outcomes.0;
    let (ds, dv) = &outcomes.1;
    assert_eq!(*ds, 200, "{dv}");
    match *rs {
        200 => {
            assert_eq!(http.snapshot().0, 2);
            let snap = SyncSnapshotV1::parse(&http.snapshot().1).unwrap();
            assert!(snap.notebooks[0].pages.iter().all(|p| p.page_id != pg(1)));
        }
        422 => {
            assert_eq!(rv["semantic_code"], "invalid_operation");
            assert_eq!(http.snapshot().0, 1);
            let snap = SyncSnapshotV1::parse(&http.snapshot().1).unwrap();
            assert!(snap.notebooks[0].pages.iter().all(|p| p.page_id != pg(1)));
        }
        other => panic!("unexpected replace status {other} {rv}"),
    }
    assert_replay_matches(&http);
}

#[test]
fn concurrent_identical_retry_one_durable_row() {
    let (_dir, _h, http) = bootstrapped_accepting("race-dup");
    let base = http.base.clone();
    let barrier = Arc::new(Barrier::new(2));
    let mut a = Author::new(replica(1));
    let env = replace_env(
        &mut a,
        nb(1),
        pg(1),
        vec![repl(sk(1), vec![sk1()], vec![frag(60, 1, 300.0)])],
    );
    let outcomes = std::thread::scope(|s| {
        let b1 = Arc::clone(&barrier);
        let base1 = base.clone();
        let e1 = env.clone();
        let t1 = s.spawn(move || {
            let http = Http::at(base1);
            b1.wait();
            http.submit(&[e1])
        });
        let t2 = s.spawn(move || {
            let http = Http::at(base);
            barrier.wait();
            http.submit(&[env])
        });
        vec![t1.join().unwrap(), t2.join().unwrap()]
    });
    assert!(outcomes.iter().all(|(s, _)| *s == 200), "{outcomes:?}");
    let dups = outcomes
        .iter()
        .filter(|(_, v)| v["results"][0]["duplicate"] == true)
        .count();
    let news = outcomes
        .iter()
        .filter(|(_, v)| v["results"][0]["duplicate"] == false)
        .count();
    assert_eq!((news, dups), (1, 1), "{outcomes:?}");
    assert_eq!(http.tail().len(), 1);
    assert_eq!(http.snapshot().0, 1);
}

// ------------------------------------------------------------------ old client

#[test]
fn older_decoder_fails_closed_and_does_not_advance_past_replace() {
    let (_dir, _h, http) = bootstrapped_accepting("legacy");
    let mut a = Author::new(replica(1));
    let rename = a.env(Op::RenameNotebook {
        notebook_id: nb(2),
        title: "Work (renamed)".into(),
    });
    assert_eq!(http.submit(&[rename]).0, 200);
    let env = replace_env(
        &mut a,
        nb(1),
        pg(1),
        vec![repl(sk(1), vec![sk1()], vec![frag(60, 1, 300.0)])],
    );
    assert_eq!(http.submit(&[env]).0, 200);
    let tail = http.tail();
    assert_eq!(tail.len(), 2);
    assert!(legacy_cursor(&tail[..1]).is_ok());
    let err = legacy_cursor(&tail).unwrap_err();
    assert!(err.contains("TAIL_REFUSED"), "{err}");
    assert!(err.contains("2"), "{err}");
    // Current decoder still consumes the full tail.
    assert!(tail.iter().all(|(_, b)| SyncEnvelopeV1::parse(b).is_ok()));
}

// ------------------------------------------------------------------ existing ops still work with the gate on

#[test]
fn existing_add_stroke_and_erase_remain_accepted_with_gate_on() {
    let (_dir, _h, http) = bootstrapped_accepting("existing");
    let mut a = Author::new(replica(1));
    let add = a.env(Op::AddStroke {
        notebook_id: nb(1),
        page_id: pg(2),
        stroke: orig(40),
    });
    assert_eq!(http.submit(&[add]).0, 200);
    let erase = a.env(Op::EraseRegion {
        notebook_id: nb(1),
        page_id: pg(1),
        observed_roots: vec![sk(1)],
        path: vec![sync_protocol::SyncPoint::new(1000.0, 3000.0, None)],
        radius: 40.0,
    });
    let (s, v) = http.submit(&[erase]);
    assert_eq!(s, 200, "{v}");
    assert_replay_matches(&http);
}

// ------------------------------------------------------------------ Review 80 synthetic shape through the ledger

#[test]
fn fixture_e_review80_shape_through_server_replay() {
    let dir = tmp("fx-e");
    let h = start(&dir);
    enable_replace(&h);
    let http = Http::of(&h);

    let p1_exp: Vec<_> = (1..=3).map(orig).collect();
    let p1_extra: Vec<_> = (110..=117).map(orig).collect();
    let p2_exp: Vec<_> = (20..=22).map(orig).collect();
    let p2_extra: Vec<_> = (800..=895).map(orig).collect();
    let p2_frag = frag(900, 896, 400.0);
    let p3_keep: Vec<_> = (30..=33).map(orig).collect();
    let p3_g1 = orig(34);
    let p3_g2 = orig(35);
    let p3_f1 = frag(910, 34, 500.0);
    let p3_f2 = frag(911, 34, 510.0);
    let p3_f3 = frag(912, 35, 520.0);
    let p3_extra: Vec<_> = (920..=929).map(orig).collect();
    let p4_home = orig(40);
    assert_eq!(p1_extra.len() + p2_extra.len() + p3_extra.len(), 114);

    let genesis_snap = custom_snap(vec![
        notebook(nb(1), "E1", vec![blank_page(pg(1), p1_exp.clone())]),
        notebook(nb(2), "E2", vec![blank_page(pg(2), p2_exp.clone())]),
        notebook(
            nb(3),
            "E3",
            vec![blank_page(pg(3), {
                let mut v = p3_keep.clone();
                v.push(p3_g1.clone());
                v.push(p3_g2.clone());
                v
            })],
        ),
        notebook(nb(4), "E4", vec![blank_page(pg(4), vec![p4_home.clone()])]),
    ]);
    let (s, v) = http.bootstrap(&genesis_snap);
    assert_eq!(s, 201, "{v}");

    let mut a1 = p1_exp;
    a1.extend(p1_extra.iter().cloned());
    let mut a2 = p2_exp;
    a2.extend(p2_extra.iter().cloned());
    a2.push(p2_frag.clone());
    let mut a3 = p3_keep;
    a3.push(p3_f1.clone());
    a3.push(p3_f2.clone());
    a3.push(p3_f3.clone());
    a3.extend(p3_extra.iter().cloned());
    let actual = LibraryState::genesis(custom_snap(vec![
        notebook(nb(1), "E1", vec![blank_page(pg(1), a1)]),
        notebook(nb(2), "E2", vec![blank_page(pg(2), a2)]),
        notebook(nb(3), "E3", vec![blank_page(pg(3), a3)]),
        notebook(
            nb(4),
            "E4",
            vec![
                blank_page(pg(4), vec![p4_home]),
                blank_page(pg(5), vec![]),
                blank_page(pg(6), vec![orig(41), orig(42)]),
            ],
        ),
    ]))
    .unwrap();

    let mut author = Author::new(replica(1));
    let mut batch = Vec::new();
    for s in p1_extra {
        batch.push(author.env(Op::AddStroke {
            notebook_id: nb(1),
            page_id: pg(1),
            stroke: s,
        }));
    }
    for s in p2_extra {
        batch.push(author.env(Op::AddStroke {
            notebook_id: nb(2),
            page_id: pg(2),
            stroke: s,
        }));
    }
    batch.push(replace_env(
        &mut author,
        nb(2),
        pg(2),
        vec![repl(sk(896), vec![], vec![p2_frag.clone()])],
    ));
    batch.push(replace_env(
        &mut author,
        nb(3),
        pg(3),
        vec![
            repl(
                p3_g1.root_id,
                vec![p3_g1],
                vec![p3_f1.clone(), p3_f2.clone()],
            ),
            repl(p3_g2.root_id, vec![p3_g2], vec![p3_f3.clone()]),
        ],
    ));
    for s in p3_extra {
        batch.push(author.env(Op::AddStroke {
            notebook_id: nb(3),
            page_id: pg(3),
            stroke: s,
        }));
    }
    batch.push(author.env(Op::CreatePage {
        notebook_id: nb(4),
        page: page_spec_plain(pg(5)),
        after_page_id: Some(pg(4)),
    }));
    batch.push(author.env(Op::CreatePage {
        notebook_id: nb(4),
        page: page_spec_plain(pg(6)),
        after_page_id: Some(pg(5)),
    }));
    batch.push(author.env(Op::AddStroke {
        notebook_id: nb(4),
        page_id: pg(6),
        stroke: orig(41),
    }));
    batch.push(author.env(Op::AddStroke {
        notebook_id: nb(4),
        page_id: pg(6),
        stroke: orig(42),
    }));
    // Unrelated remote original on notebook 1, submitted in the same authorized path.
    let mut remote = Author::new(replica(2));
    let unrelated = orig(70);
    batch.push(remote.env(Op::AddStroke {
        notebook_id: nb(1),
        page_id: pg(1),
        stroke: unrelated.clone(),
    }));

    for chunk in batch.chunks(100) {
        let (s, v) = http.submit(chunk);
        assert_eq!(s, 200, "{v}");
    }

    let server = SyncSnapshotV1::parse(&http.snapshot().1).unwrap();
    let mut expect = actual.snapshot.clone();
    expect.notebooks[0].pages[0].strokes.push(unrelated.clone());
    assert_eq!(server, expect);
    let frags: Vec<_> = server
        .notebooks
        .iter()
        .flat_map(|n| &n.pages)
        .flat_map(|p| &p.strokes)
        .filter(|s| s.root_id != s.stroke_id)
        .map(|s| s.stroke_id)
        .collect();
    assert_eq!(frags, vec![sk(900), sk(910), sk(911), sk(912)]);
    assert!(h
        .store
        .verify_replay(&testkit::library().to_string())
        .unwrap());
    assert_replay_matches(&http);
}

#[test]
fn gate_can_be_disabled_after_accept_and_retries_still_dedup() {
    let (_dir, h, http) = bootstrapped_accepting("gate-later");
    let mut a = Author::new(replica(1));
    let env = replace_env(
        &mut a,
        nb(1),
        pg(1),
        vec![repl(sk(1), vec![sk1()], vec![frag(60, 1, 300.0)])],
    );
    assert_eq!(http.submit(std::slice::from_ref(&env)).0, 200);
    h.store
        .accept_replace_stroke_lineages
        .store(false, Ordering::SeqCst);
    let (s, v) = http.submit(std::slice::from_ref(&env));
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["results"][0]["duplicate"], true);
    let mut b = Author::new(replica(2));
    let fresh = replace_env(
        &mut b,
        nb(1),
        pg(1),
        vec![repl(sk(1), vec![sk1()], vec![frag(99, 1, 1.0)])],
    );
    let (s, v) = http.submit(&[fresh]);
    assert_eq!(s, 400, "{v}");
    assert_eq!(v["error"], "unsupported_operation");
    assert_eq!(http.tail().len(), 1);
}
