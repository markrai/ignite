//! S2A acceptance through the real HTTP interface (ephemeral loopback ports, real SQLite).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier};
use std::time::{SystemTime, UNIX_EPOCH};

use scribe_sync_server::{ServerHandle, Store};
use serde_json::Value;
use sync_engine::testkit::{self, genesis, line, nb, page_spec, pg, replica, sk, Author};
use sync_engine::{replay, LibraryState};
use sync_protocol::{
    global_stroke_id, snapshot_sha256, to_canonical_json, LineageReplacement, SyncEnvelopeV1,
    SyncOperationV1 as Op, SyncPoint, SyncSnapshotV1,
};

// ------------------------------------------------------------------ harness

static N: AtomicU64 = AtomicU64::new(0);

fn tmp(label: &str) -> PathBuf {
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = std::env::temp_dir().join(format!(
        "scribe-s2a-{label}-{n}-{}",
        N.fetch_add(1, Ordering::SeqCst)
    ));
    let _ = std::fs::remove_dir_all(&p);
    p
}

fn start(dir: &Path) -> ServerHandle {
    ServerHandle::start("127.0.0.1:0".parse().unwrap(), Store::open(dir).unwrap()).unwrap()
}

/// HTTP client over a keep-alive connection pool (one fresh TCP connection per request would
/// exhaust Windows' ephemeral ports under the concurrency tests).
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
        let req = self.agent.request(method, &format!("{}{path}", self.base));
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
        // Plain canonical serialization (no client-side validation), so tests can also send
        // semantically invalid envelopes and observe the server's refusal.
        let raws: Vec<Vec<u8>> = envs.iter().map(|e| to_canonical_json(e).unwrap()).collect();
        self.submit_raw(&testkit::library().to_string(), &raws)
    }

    /// Entire tail via pagination (`limit` per page) as (server_seq, raw envelope bytes).
    fn tail(&self, limit: usize) -> Vec<(i64, Vec<u8>)> {
        let lib = testkit::library();
        let mut after = 0;
        let mut out = Vec::new();
        loop {
            let (s, body) = self.call(
                "GET",
                &format!("/v1/libraries/{lib}/operations?after={after}&limit={limit}"),
                None,
            );
            assert_eq!(s, 200);
            // Parse envelopes as raw JSON to prove byte identity.
            #[derive(serde::Deserialize)]
            struct E<'a> {
                server_seq: i64,
                #[serde(borrow)]
                envelope: &'a serde_json::value::RawValue,
            }
            #[derive(serde::Deserialize)]
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

    /// Current snapshot: (through_server_seq, raw canonical bytes, reported sha).
    fn snapshot(&self) -> (i64, Vec<u8>, String) {
        let lib = testkit::library();
        let (s, body) = self.call("GET", &format!("/v1/libraries/{lib}/snapshot"), None);
        assert_eq!(s, 200);
        #[derive(serde::Deserialize)]
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
        #[derive(serde::Deserialize)]
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

/// Independent replay of fetched genesis + fetched tail must equal the server snapshot.
fn assert_replay_matches(http: &Http) -> String {
    let genesis = SyncSnapshotV1::parse(&http.genesis()).unwrap();
    let tail = http.tail(3);
    for (i, (seq, _)) in tail.iter().enumerate() {
        assert_eq!(*seq, i as i64 + 1, "gap-free ascending server_seq");
    }
    let envs: Vec<SyncEnvelopeV1> = tail
        .iter()
        .map(|(_, b)| SyncEnvelopeV1::parse(b).unwrap())
        .collect();
    for ((_, raw), env) in tail.iter().zip(&envs) {
        assert_eq!(
            raw,
            &env.to_canonical_bytes().unwrap(),
            "byte-identical envelope"
        );
    }
    let replayed = replay(genesis, &envs).unwrap();
    let (through, server_bytes, server_sha) = http.snapshot();
    assert_eq!(through, tail.len() as i64);
    let replay_bytes = replayed.snapshot.to_canonical_bytes().unwrap();
    assert_eq!(
        replay_bytes, server_bytes,
        "genesis + tail == server current snapshot"
    );
    let h = snapshot_sha256(&replayed.snapshot).unwrap().to_string();
    assert_eq!(h, server_sha);
    assert_eq!(
        sync_protocol::sha256_hex(&server_bytes).to_string(),
        server_sha,
        "returned hash matches returned bytes"
    );
    h
}

/// Operations covering every operation type (replica A).
fn every_op(a: &mut Author) -> Vec<SyncEnvelopeV1> {
    let legacy_root = global_stroke_id(pg(1), "s-1727000000000-2").unwrap();
    vec![
        a.env(Op::CreateNotebook {
            notebook_id: nb(10),
            title: "New".into(),
            after_notebook_id: Some(nb(1)),
            first_page: page_spec(pg(10)),
        }),
        a.env(Op::RenameNotebook {
            notebook_id: nb(2),
            title: "Work (renamed)".into(),
        }),
        a.env(Op::CreatePage {
            notebook_id: nb(1),
            page: page_spec(pg(11)),
            after_page_id: Some(pg(1)),
        }),
        a.env(Op::MovePage {
            notebook_id: nb(1),
            page_id: pg(2),
            after_page_id: None,
        }),
        a.env(Op::AddStroke {
            notebook_id: nb(1),
            page_id: pg(1),
            stroke: line(sk(20), sk(20), (2000.0, 7000.0), (8000.0, 7100.0), 11, true),
        }),
        a.env(Op::EraseRegion {
            notebook_id: nb(1),
            page_id: pg(1),
            observed_roots: vec![sk(20)],
            path: vec![
                SyncPoint::new(5000.0, 6800.0, None),
                SyncPoint::new(5000.0, 7300.0, None),
            ],
            radius: 240.0,
        }),
        a.env(Op::MoveStrokeLineages {
            notebook_id: nb(1),
            page_id: pg(1),
            root_ids: vec![sk(20)],
            dx: 100.0,
            dy: 200.0,
        }),
        a.env(Op::DeleteStrokeLineage {
            notebook_id: nb(1),
            page_id: pg(1),
            root_id: legacy_root,
        }),
        a.env(Op::DeletePage {
            notebook_id: nb(1),
            page_id: pg(11),
        }),
        a.env(Op::DeleteNotebook { notebook_id: nb(3) }),
    ]
}

// ------------------------------------------------------------------ 7 genesis

#[test]
fn genesis_bootstrap_is_strict_idempotent_and_never_replaced() {
    let dir = tmp("genesis");
    let h = start(&dir);
    let http = Http::of(&h);
    // Referenced blob missing → fail closed (422) and nothing stored.
    let (s, v) = http.bootstrap(&genesis());
    assert_eq!(s, 422, "{v}");
    assert_eq!(v["error"], "missing_blobs");
    let lib = testkit::library();
    assert_eq!(
        http.call("GET", &format!("/v1/libraries/{lib}"), None).0,
        404
    );
    put_genesis_blob(&http);
    let (s, v) = http.bootstrap(&genesis());
    assert_eq!(s, 201);
    assert_eq!(v["head_server_seq"], 0);
    assert_eq!(v["library_id"], lib.to_string());
    let sha = v["genesis_sha256"].clone();
    // Identical → idempotent success.
    let (s, v) = http.bootstrap(&genesis());
    assert_eq!(
        (s, v["genesis_sha256"].clone(), v["head_server_seq"].clone()),
        (200, sha.clone(), 0.into())
    );
    // Different genesis for the same library → conflict, never replaced.
    let mut other = genesis();
    other.notebooks[0].title = "Different".into();
    let (s, v) = http.bootstrap(&other);
    assert_eq!(s, 409);
    assert_eq!(v["error"], "genesis_conflict");
    assert_eq!(
        http.json("GET", &format!("/v1/libraries/{lib}"), None).1["genesis_sha256"],
        sha
    );
    // Non-canonical, unknown fields, wrong version, bad JSON → 400.
    let canon = to_canonical_json(&genesis()).unwrap();
    let pretty =
        serde_json::to_vec_pretty(&serde_json::from_slice::<Value>(&canon).unwrap()).unwrap();
    let text = String::from_utf8(canon).unwrap();
    for (body, code) in [
        (pretty, "non_canonical"),
        (
            text.replacen("{\"v\":1,", "{\"v\":1,\"extra\":1,", 1)
                .into_bytes(),
            "invalid_snapshot",
        ),
        (
            text.replacen("{\"v\":1,", "{\"v\":2,", 1).into_bytes(),
            "unsupported_version",
        ),
        (b"{not json".to_vec(), "invalid_snapshot"),
    ] {
        let (s, v) = http.json("POST", "/v1/libraries", Some(&body));
        assert_eq!((s, v["error"].as_str()), (400, Some(code)), "{v}");
    }
    // Restart after genesis: state survives.
    drop(h);
    let h = start(&dir);
    let http = Http::of(&h);
    assert_eq!(
        http.json("GET", &format!("/v1/libraries/{lib}"), None).1["genesis_sha256"],
        sha
    );
    assert_eq!(http.genesis(), to_canonical_json(&genesis()).unwrap());
    assert_eq!(http.snapshot().0, 0);
}

// ------------------------------------------------------------------ 13 replay + restart

#[test]
fn genesis_plus_tail_replays_to_the_server_snapshot_across_restart() {
    let (dir, h, http) = bootstrapped("replay");
    let mut a = Author::new(replica(1));
    let envs = every_op(&mut a);
    // First half one by one, second half as one batch.
    for e in &envs[..4] {
        let (s, v) = http.submit(std::slice::from_ref(e));
        assert_eq!(s, 200, "{v}");
    }
    let (s, v) = http.submit(&envs[4..]);
    assert_eq!(s, 200, "{v}");
    let seqs: Vec<i64> = v["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["server_seq"].as_i64().unwrap())
        .collect();
    assert_eq!(seqs, vec![5, 6, 7, 8, 9, 10]);
    assert_eq!(v["head_server_seq"], 10);
    let hash = assert_replay_matches(&http);
    assert!(h
        .store
        .verify_replay(&testkit::library().to_string())
        .unwrap());
    // The server snapshot really reflects every operation type.
    let snap = SyncSnapshotV1::parse(&http.snapshot().1).unwrap();
    let ids: Vec<_> = snap.notebooks.iter().map(|n| n.notebook_id).collect();
    assert_eq!(ids, vec![nb(1), nb(10), nb(2)]);
    assert_eq!(snap.notebooks[2].title, "Work (renamed)");
    assert_eq!(
        snap.notebooks[0]
            .pages
            .iter()
            .map(|p| p.page_id)
            .collect::<Vec<_>>(),
        vec![pg(2), pg(1)]
    );
    let moved: Vec<_> = snap.notebooks[0].pages[1]
        .strokes
        .iter()
        .filter(|s| s.root_id == sk(20))
        .collect();
    assert_eq!(moved.len(), 2);
    assert!(moved
        .iter()
        .flat_map(|stroke| &stroke.points)
        .all(|point| point.y >= 7200.0));
    // Restart: identical results.
    drop(h);
    let h2 = start(&dir);
    let http2 = Http::of(&h2);
    assert_eq!(assert_replay_matches(&http2), hash);
    assert!(h2
        .store
        .verify_replay(&testkit::library().to_string())
        .unwrap());
}

// ------------------------------------------------------------------ 9 dedup / sequencing

#[test]
fn retries_dedup_and_sequence_conflicts() {
    let (_dir, _h, http) = bootstrapped("dedup");
    let lib = testkit::library().to_string();
    let mut a = Author::new(replica(1));
    let e1 = a.env(Op::RenameNotebook {
        notebook_id: nb(1),
        title: "One".into(),
    });
    let (s, v) = http.submit(std::slice::from_ref(&e1));
    assert_eq!((s, v["results"][0]["server_seq"].as_i64()), (200, Some(1)));
    // Client lost the ACK and retries: same server_seq, nothing advances.
    for _ in 0..3 {
        let (s, v) = http.submit(std::slice::from_ref(&e1));
        assert_eq!(s, 200);
        assert_eq!(v["results"][0]["server_seq"], 1);
        assert_eq!(v["results"][0]["duplicate"], true);
        assert_eq!(v["head_server_seq"], 1);
    }
    // Same OperationId, different bytes → conflict.
    let mut forged = e1.clone();
    forged.operation = Op::RenameNotebook {
        notebook_id: nb(1),
        title: "Other".into(),
    };
    let (s, v) = http.submit(&[forged]);
    assert_eq!(
        (s, v["error"].as_str()),
        (409, Some("operation_id_conflict"))
    );
    // Same (replica, client_seq), different OperationId → conflict.
    let mut clash = e1.clone();
    clash.operation_id = testkit::Author::new(replica(9)).op_id(1);
    let (s, v) = http.submit(&[clash]);
    assert_eq!((s, v["error"].as_str()), (409, Some("client_seq_conflict")));
    // Gap → refused; no gap filling.
    let skipped = {
        a.next_seq += 1;
        a.env(Op::RenameNotebook {
            notebook_id: nb(1),
            title: "Gap".into(),
        })
    };
    let (s, v) = http.submit(&[skipped]);
    assert_eq!(
        (s, v["error"].as_str()),
        (409, Some("client_seq_out_of_order")),
        "{v}"
    );
    // First envelope of a new replica must be client_seq 1.
    let mut b = Author::new(replica(2));
    b.next_seq = 2;
    let (s, _) = http.submit(&[b.env(Op::RenameNotebook {
        notebook_id: nb(1),
        title: "B".into(),
    })]);
    assert_eq!(s, 409);
    // Semantic refusal → 422, nothing persisted (a rolled-back txn consumes no sequence).
    let mut c = Author::new(replica(3));
    let bad = c.env(Op::DeletePage {
        notebook_id: nb(3),
        page_id: pg(4),
    });
    let (s, v) = http.submit(&[bad]);
    assert_eq!((s, v["semantic_code"].as_str()), (422, Some("sole_page")));
    // Batch with a refusal inside → whole batch refused atomically.
    let mut d = Author::new(replica(4));
    let ok = d.env(Op::RenameNotebook {
        notebook_id: nb(2),
        title: "fine".into(),
    });
    let unk = d.env(Op::RenameNotebook {
        notebook_id: nb(99),
        title: "x".into(),
    });
    let (s, v) = http.submit(&[ok.clone(), unk]);
    assert_eq!(
        (s, v["semantic_code"].as_str(), v["envelope_index"].as_i64()),
        (422, Some("unknown_notebook"), Some(1))
    );
    assert_eq!(http.tail(100).len(), 1);
    assert_eq!(http.snapshot().0, 1);
    // The refused replica can continue at its unchanged expected client_seq.
    let (s, v) = http.submit(&[ok]);
    assert_eq!((s, v["results"][0]["server_seq"].as_i64()), (200, Some(2)));
    let (s, v) = http.submit(&[c.env(Op::RenameNotebook {
        notebook_id: nb(3),
        title: "c2".into(),
    })]);
    assert_eq!(
        (s, v["error"].as_str()),
        (409, Some("client_seq_out_of_order")),
        "c skipped seq 1"
    );
    let _ = lib;
    assert_replay_matches(&http);
}

// ------------------------------------------------------------------ 17 HTTP surface

#[test]
fn http_surface_status_codes_limits_and_pagination() {
    let (_dir, _h, http) = bootstrapped("http");
    let lib = testkit::library().to_string();
    let mut a = Author::new(replica(1));
    let e = a.env(Op::RenameNotebook {
        notebook_id: nb(1),
        title: "T".into(),
    });
    let canon = String::from_utf8(e.to_canonical_bytes().unwrap()).unwrap();
    let cases: Vec<(Vec<u8>, u16, &str)> = vec![
        (b"{nope".to_vec(), 400, "invalid_json"),
        (b"{\"envelopes\":[],\"x\":1}".to_vec(), 400, "invalid_json"),
        (b"{\"envelopes\":[]}".to_vec(), 400, "invalid_batch_size"),
        (
            format!(
                "{{\"envelopes\":[{}]}}",
                canon.replacen("{\"v\":1", "{\"v\":2", 1)
            )
            .into_bytes(),
            400,
            "unsupported_version",
        ),
        (
            format!(
                "{{\"envelopes\":[{}]}}",
                canon.replacen("\"client_seq\":1", "\"client_seq\":1,\"server_seq\":5", 1)
            )
            .into_bytes(),
            400,
            "invalid_envelope",
        ),
        (
            format!("{{\"envelopes\":[{}]}}", canon.replace(",\"", ", \"")).into_bytes(),
            400,
            "non_canonical",
        ),
    ];
    for (body, status, code) in cases {
        let (s, v) = http.json(
            "POST",
            &format!("/v1/libraries/{lib}/operations"),
            Some(&body),
        );
        assert_eq!((s, v["error"].as_str()), (status, Some(code)), "{v}");
    }
    // Wrong library in the route vs the envelope.
    let other = testkit::uuid7(1, 77).to_string();
    let (s, v) = http.submit_raw(&other, &[canon.clone().into_bytes()]);
    assert_eq!((s, v["error"].as_str()), (400, Some("wrong_library")));
    let (s, _) = http.submit_raw("not-a-uuid", &[canon.clone().into_bytes()]);
    assert_eq!(s, 400);
    // Oversized batch and oversized body.
    let many: Vec<Vec<u8>> = (0..101).map(|_| canon.clone().into_bytes()).collect();
    let (s, v) = http.submit_raw(&lib, &many);
    assert_eq!((s, v["error"].as_str()), (400, Some("invalid_batch_size")));
    // Oversized declared body: refused with 413 *before* the body is read (the server never
    // buffers it). Raw socket: send only the headers, then read the immediate response.
    {
        use std::io::{Read as _, Write as _};
        let mut sock =
            std::net::TcpStream::connect(http.base.trim_start_matches("http://")).unwrap();
        let too_big = scribe_sync_server::http::MAX_JSON_BODY + 10;
        write!(
            sock,
            "POST /v1/libraries/{lib}/operations HTTP/1.1
Host: x
Content-Type: application/json
Content-Length: {too_big}

"
        )
        .unwrap();
        sock.set_read_timeout(Some(std::time::Duration::from_secs(10)))
            .unwrap();
        let mut head = [0u8; 12];
        sock.read_exact(&mut head).unwrap();
        assert_eq!(&head, b"HTTP/1.1 413", "{}", String::from_utf8_lossy(&head));
    }
    // Invalid operation (semantic) → 422.
    let mut fresh = Author::new(replica(3));
    let bad = fresh.env(Op::MovePage {
        notebook_id: nb(1),
        page_id: pg(1),
        after_page_id: Some(pg(1)),
    });
    let (s, v) = http.submit(&[bad]);
    assert_eq!(
        (s, v["semantic_code"].as_str()),
        (422, Some("invalid_operation"))
    );
    // Unknown library / route / method.
    let unknown = testkit::uuid7(1, 78).to_string();
    assert_eq!(
        http.call("GET", &format!("/v1/libraries/{unknown}/snapshot"), None)
            .0,
        404
    );
    assert_eq!(http.call("GET", "/v2/nothing", None).0, 404);
    assert_eq!(
        http.call("DELETE", &format!("/v1/libraries/{lib}"), None).0,
        405
    );
    // Pagination: 25 ops, pages of 7, stable across repeated requests.
    let mut b = Author::new(replica(2));
    for i in 0..25 {
        let (s, _) = http.submit(&[b.env(Op::RenameNotebook {
            notebook_id: nb(1),
            title: format!("t{i}"),
        })]);
        assert_eq!(s, 200);
    }
    let first = http.tail(7);
    assert_eq!(first.len(), 25);
    assert_eq!(first, http.tail(7));
    assert_eq!(first, http.tail(500));
    let (s, v) = http.json(
        "GET",
        &format!("/v1/libraries/{lib}/operations?after=20&limit=2"),
        None,
    );
    assert_eq!(s, 200);
    assert_eq!(v["operations"].as_array().unwrap().len(), 2);
    assert_eq!(v["next_after"], 22);
    for q in ["after=-1", "limit=0", "limit=501", "after=x"] {
        assert_eq!(
            http.call("GET", &format!("/v1/libraries/{lib}/operations?{q}"), None)
                .0,
            400,
            "{q}"
        );
    }
    // Non-loopback bind is refused outright.
    assert!(ServerHandle::start(
        "0.0.0.0:0".parse().unwrap(),
        Store::open(&tmp("nl")).unwrap()
    )
    .is_err());
}

// ------------------------------------------------------------------ 15 blobs

#[test]
fn blobs_are_content_addressed_and_verified() {
    let dir = tmp("blobs");
    let h = start(&dir);
    let http = Http::of(&h);
    let bytes = b"scribe background bytes".to_vec();
    let sha = sync_protocol::sha256_hex(&bytes).to_string();
    let (s, v) = http.json("PUT", &format!("/v1/blobs/{sha}"), Some(&bytes));
    assert_eq!((s, v["created"].as_bool()), (201, Some(true)));
    let (s, v) = http.json("PUT", &format!("/v1/blobs/{sha}"), Some(&bytes));
    assert_eq!(
        (s, v["created"].as_bool()),
        (200, Some(false)),
        "idempotent"
    );
    let (s, body) = http.call("GET", &format!("/v1/blobs/{sha}"), None);
    assert_eq!((s, body), (200, bytes.clone()));
    // URL/body mismatch; malformed hash; unknown blob.
    let other = sync_protocol::sha256_hex(b"other").to_string();
    let (s, v) = http.json("PUT", &format!("/v1/blobs/{other}"), Some(&bytes));
    assert_eq!((s, v["error"].as_str()), (400, Some("hash_mismatch")));
    assert_eq!(http.call("PUT", "/v1/blobs/ABC", Some(&bytes)).0, 400);
    assert_eq!(http.call("GET", &format!("/v1/blobs/{other}"), None).0, 404);
    // Corrupted stored bytes: a re-PUT is refused and a GET fails closed.
    let path = dir.join("blobs").join(&sha[..2]).join(&sha);
    std::fs::write(&path, b"tampered").unwrap();
    assert_eq!(
        http.call("PUT", &format!("/v1/blobs/{sha}"), Some(&bytes))
            .0,
        409
    );
    assert_eq!(http.call("GET", &format!("/v1/blobs/{sha}"), None).0, 500);
    // Operations that reference an absent blob are refused before sequencing.
    put_genesis_blob(&http);
    assert_eq!(http.bootstrap(&genesis()).0, 201);
    let (_, blob_ref) = {
        let b = b"never uploaded".to_vec();
        let r = sync_protocol::BackgroundRef::Blob {
            sha256: sync_protocol::sha256_hex(&b),
            byte_len: b.len() as u64,
            format: sync_protocol::BackgroundFormat {
                encoding: sync_protocol::BackgroundEncoding::Gray8,
                width: b.len() as u32,
                height: 1,
                stride: b.len() as u32,
            },
        };
        (b, r)
    };
    let mut spec = page_spec(pg(40));
    spec.background = blob_ref;
    let mut a = Author::new(replica(1));
    let (s, v) = http.submit(&[a.env(Op::CreatePage {
        notebook_id: nb(1),
        page: spec,
        after_page_id: None,
    })]);
    assert_eq!((s, v["error"].as_str()), (422, Some("missing_blobs")));
    assert_eq!(http.snapshot().0, 0);
}

// ------------------------------------------------------------------ 16 failures

#[test]
fn failed_transaction_changes_nothing_and_consumes_no_sequence() {
    let (_dir, h, http) = bootstrapped("txn");
    let mut a = Author::new(replica(1));
    let e1 = a.env(Op::RenameNotebook {
        notebook_id: nb(1),
        title: "x".into(),
    });
    let e2 = a.env(Op::RenameNotebook {
        notebook_id: nb(1),
        title: "y".into(),
    });
    assert_eq!(http.submit(std::slice::from_ref(&e1)).0, 200);
    let before = (http.tail(100), http.snapshot());
    h.store
        .hooks
        .fail_submit_before_commit
        .store(true, Ordering::SeqCst);
    let (s, v) = http.submit(std::slice::from_ref(&e2));
    assert_eq!((s, v["error"].as_str()), (500, Some("internal")));
    h.store
        .hooks
        .fail_submit_before_commit
        .store(false, Ordering::SeqCst);
    assert_eq!(
        (http.tail(100), http.snapshot()),
        before,
        "ledger/head/snapshot unchanged"
    );
    // The replica's sequence did not advance either: the same envelope now lands at seq 2.
    let (s, v) = http.submit(std::slice::from_ref(&e2));
    assert_eq!((s, v["results"][0]["server_seq"].as_i64()), (200, Some(2)));
    assert_eq!(v["results"][0]["duplicate"], false);
    assert_replay_matches(&http);
}

// ------------------------------------------------------------------ 10/16 concurrency

#[test]
fn concurrent_replicas_get_one_gap_free_order() {
    let (_dir, h, http) = bootstrapped("concurrent");
    let base = http.base.clone();
    const REPLICAS: u64 = 8;
    const PER: usize = 25;
    let barrier = Arc::new(Barrier::new(REPLICAS as usize));
    std::thread::scope(|s| {
        for r in 0..REPLICAS {
            let barrier = Arc::clone(&barrier);
            let base = base.clone();
            s.spawn(move || {
                let http = Http::at(base);
                let mut a = Author::new(replica(100 + r));
                barrier.wait();
                for i in 0..PER {
                    let stroke_n = 10_000 + r * 100 + i as u64;
                    let e = a.env(Op::AddStroke {
                        notebook_id: nb(1),
                        page_id: pg(2),
                        stroke: line(
                            sk(stroke_n),
                            sk(stroke_n),
                            (100.0 + i as f32 * 50.0, 100.0 + r as f32 * 900.0),
                            (140.0 + i as f32 * 50.0, 800.0 + r as f32 * 900.0),
                            3,
                            true,
                        ),
                    });
                    let (st, v) = http.submit(&[e]);
                    assert_eq!(st, 200, "{v}");
                }
            });
        }
    });
    let tail = http.tail(50);
    assert_eq!(tail.len(), REPLICAS as usize * PER);
    let seqs: Vec<i64> = tail.iter().map(|(s, _)| *s).collect();
    assert_eq!(
        seqs,
        (1..=tail.len() as i64).collect::<Vec<_>>(),
        "gap-free, no duplicates"
    );
    // Each replica's operations appear in its own client_seq order.
    let mut per: BTreeMap<String, Vec<u64>> = BTreeMap::new();
    for (_, raw) in &tail {
        let e = SyncEnvelopeV1::parse(raw).unwrap();
        per.entry(e.replica_id.to_string())
            .or_default()
            .push(e.client_seq);
    }
    for (r, seqs) in &per {
        assert_eq!(seqs, &(1..=PER as u64).collect::<Vec<_>>(), "replica {r}");
    }
    assert_replay_matches(&http);
    assert!(h
        .store
        .verify_replay(&testkit::library().to_string())
        .unwrap());
}

#[test]
fn concurrent_submissions_from_one_replica_and_retry_storms() {
    let (_dir, _h, http) = bootstrapped("same-replica");
    let base = http.base.clone();
    let mut a = Author::new(replica(1));
    let envs: Vec<SyncEnvelopeV1> = (0..20)
        .map(|i| {
            a.env(Op::RenameNotebook {
                notebook_id: nb(1),
                title: format!("r{i}"),
            })
        })
        .collect();
    // 20 threads submit client_seq 1..20 simultaneously; out-of-order ones retry until
    // accepted. Each envelope is also re-sent once after acceptance (lost ACK).
    let barrier = Arc::new(Barrier::new(envs.len()));
    let accepted: Vec<(u64, i64)> = std::thread::scope(|s| {
        let hs: Vec<_> = envs
            .iter()
            .map(|e| {
                let barrier = Arc::clone(&barrier);
                let base = base.clone();
                s.spawn(move || {
                    let http = Http::at(base);
                    barrier.wait();
                    loop {
                        let (st, v) = http.submit(std::slice::from_ref(e));
                        match st {
                            200 => {
                                let seq = v["results"][0]["server_seq"].as_i64().unwrap();
                                let (st2, v2) = http.submit(std::slice::from_ref(e));
                                assert_eq!(st2, 200);
                                assert_eq!(v2["results"][0]["server_seq"].as_i64().unwrap(), seq);
                                return (e.client_seq, seq);
                            }
                            409 => {
                                assert_eq!(v["error"], "client_seq_out_of_order", "{v}");
                                std::thread::yield_now();
                            }
                            other => panic!("{other}: {v}"),
                        }
                    }
                })
            })
            .collect();
        hs.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let mut accepted = accepted;
    accepted.sort();
    for (i, (cs, ss)) in accepted.iter().enumerate() {
        assert_eq!(
            (*cs, *ss),
            (i as u64 + 1, i as i64 + 1),
            "client order == server order"
        );
    }
    // A pure duplicate storm: 16 threads resend the same accepted envelope.
    let last = envs.last().unwrap().clone();
    let barrier = Arc::new(Barrier::new(16));
    std::thread::scope(|s| {
        for _ in 0..16 {
            let barrier = Arc::clone(&barrier);
            let base = base.clone();
            let last = last.clone();
            s.spawn(move || {
                let http = Http::at(base);
                barrier.wait();
                let (st, v) = http.submit(&[last]);
                assert_eq!(
                    (st, v["results"][0]["server_seq"].as_i64()),
                    (200, Some(20))
                );
            });
        }
    });
    assert_eq!(http.tail(100).len(), 20);
    assert_replay_matches(&http);
}

// ------------------------------------------------------------------ 14 multi-replica

/// Pinned: A's offline erase must not cut B's concurrent stroke that A never observed.
#[test]
fn multi_replica_order_and_observed_roots_rule() {
    let (dir, h, http) = bootstrapped("multi");
    let mut a = Author::new(replica(1));
    let mut b = Author::new(replica(2));
    // Both start from genesis. B draws across the area A will erase (A never sees it).
    let b_stroke = line(
        sk(500),
        sk(500),
        (4000.0, 2600.0),
        (6000.0, 3400.0),
        9,
        true,
    );
    let b_ops = [
        b.env(Op::AddStroke {
            notebook_id: nb(1),
            page_id: pg(1),
            stroke: b_stroke.clone(),
        }),
        b.env(Op::CreatePage {
            notebook_id: nb(1),
            page: page_spec(pg(60)),
            after_page_id: Some(pg(1)),
        }),
        b.env(Op::RenameNotebook {
            notebook_id: nb(2),
            title: "B named it".into(),
        }),
    ];
    let a_ops = [
        a.env(Op::AddStroke {
            notebook_id: nb(1),
            page_id: pg(1),
            stroke: line(
                sk(501),
                sk(501),
                (4500.0, 2900.0),
                (5500.0, 2950.0),
                5,
                true,
            ),
        }),
        // A observed only genesis sk(1) and its own sk(501) — never B's sk(500).
        a.env(Op::EraseRegion {
            notebook_id: nb(1),
            page_id: pg(1),
            observed_roots: {
                let mut r = vec![sk(1), sk(501)];
                r.sort();
                r
            },
            path: vec![
                SyncPoint::new(5000.0, 2000.0, None),
                SyncPoint::new(5000.0, 4000.0, None),
            ],
            radius: 240.0,
        }),
        a.env(Op::CreatePage {
            notebook_id: nb(1),
            page: page_spec(pg(61)),
            after_page_id: None,
        }),
        a.env(Op::MovePage {
            notebook_id: nb(1),
            page_id: pg(2),
            after_page_id: Some(pg(61)),
        }),
        a.env(Op::DeleteStrokeLineage {
            notebook_id: nb(1),
            page_id: pg(1),
            root_id: global_stroke_id(pg(1), "s-1727000000000-2").unwrap(),
        }),
    ];
    // B's first op is accepted first; the rest race concurrently.
    assert_eq!(http.submit(&b_ops[..1]).0, 200);
    let base = http.base.clone();
    std::thread::scope(|s| {
        let (ba, bb) = (base.clone(), base.clone());
        let a_ops = &a_ops;
        let b_rest = &b_ops[1..];
        s.spawn(move || {
            let http = Http::at(ba);
            for e in a_ops {
                assert_eq!(http.submit(std::slice::from_ref(e)).0, 200);
            }
        });
        s.spawn(move || {
            let http = Http::at(bb);
            for e in b_rest {
                assert_eq!(http.submit(std::slice::from_ref(e)).0, 200);
            }
        });
    });
    let tail = http.tail(4);
    assert_eq!(tail.len(), 8);
    let snap = SyncSnapshotV1::parse(&http.snapshot().1).unwrap();
    let p1 = &snap.notebooks[0]
        .pages
        .iter()
        .find(|p| p.page_id == pg(1))
        .unwrap()
        .strokes;
    assert!(
        p1.contains(&b_stroke),
        "B's unobserved stroke survives byte-for-byte"
    );
    assert!(
        !p1.iter()
            .any(|s| s.stroke_id == sk(1) || s.stroke_id == sk(501)),
        "A's observed roots were cut"
    );
    assert_eq!(p1.iter().filter(|s| s.root_id == sk(1)).count(), 2);
    assert_eq!(p1.iter().filter(|s| s.root_id == sk(501)).count(), 2);
    assert_eq!(snap.notebooks[1].title, "B named it");
    let pages: Vec<_> = snap.notebooks[0].pages.iter().map(|p| p.page_id).collect();
    assert!(pages.contains(&pg(60)) && pages.contains(&pg(61)));
    // Complete replay reproduces the server state; restart too.
    let hash = assert_replay_matches(&http);
    drop(h);
    let h2 = start(&dir);
    assert_eq!(assert_replay_matches(&Http::of(&h2)), hash);
    // Sanity: the engine alone (same canonical order) agrees.
    let envs: Vec<_> = tail
        .iter()
        .map(|(_, r)| SyncEnvelopeV1::parse(r).unwrap())
        .collect();
    let direct = replay(genesis(), &envs).unwrap();
    let _: &LibraryState = &direct;
    assert_eq!(snapshot_sha256(&direct.snapshot).unwrap().to_string(), hash);
}

// ------------------------------------------------------------------ one process per ledger

#[test]
fn a_ledger_is_owned_by_one_server_process_at_a_time() {
    let dir = tmp("owner");
    let first = Store::open(&dir).unwrap();
    let second = Store::open(&dir);
    assert!(
        matches!(&second, Err(e) if e.to_string().contains("in use by another server process")),
        "second opener must be refused"
    );
    drop(first);
    // Released on drop (and by the kernel if the process dies).
    Store::open(&dir).unwrap();
}

#[test]
fn replace_stroke_lineages_is_not_accepted_on_this_server() {
    let (_dir, _h, http) = bootstrapped("no-replace");
    let mut a = Author::new(replica(1));
    let env = a.env(Op::ReplaceStrokeLineages {
        notebook_id: nb(1),
        page_id: pg(1),
        replacements: vec![LineageReplacement {
            root_id: sk(1),
            expected: vec![line(
                sk(1),
                sk(1),
                (1000.0, 3000.0),
                (9000.0, 3000.0),
                17,
                true,
            )],
            after: vec![],
        }],
    });
    let (s, v) = http.submit(&[env]);
    assert_eq!(s, 400, "{v}");
    assert_eq!(v["error"], "unsupported_operation");
    let (through, _, _) = http.snapshot();
    assert_eq!(through, 0, "head must not advance");
}
