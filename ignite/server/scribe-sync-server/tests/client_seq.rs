//! The ledger preserves the full S1 `client_seq: u64` contract (exact +1, exhaustion only
//! after `u64::MAX`) across accept, persist, reload, dedup, compare and advance — including
//! values above SQLite's signed `i64::MAX`.

use std::path::{Path, PathBuf};

use scribe_sync_server::{ServerHandle, Store};
use serde_json::Value;
use sync_engine::testkit::{self, nb, replica, Author};
use sync_protocol::{to_canonical_json, SyncEnvelopeV1, SyncOperationV1 as Op};

fn tmp() -> PathBuf {
    std::env::temp_dir().join(format!(
        "scribe-s2a-seq-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

fn start(dir: &Path) -> ServerHandle {
    ServerHandle::start("127.0.0.1:0".parse().unwrap(), Store::open(dir).unwrap()).unwrap()
}

fn post(h: &ServerHandle, path: &str, body: &[u8]) -> (u16, Value) {
    let res = ureq::post(&format!("{}{path}", h.base_url())).send_bytes(body);
    let resp = match res {
        Ok(r) => r,
        Err(ureq::Error::Status(_, r)) => r,
        Err(e) => panic!("{e}"),
    };
    let s = resp.status();
    (s, resp.into_json().unwrap_or(Value::Null))
}

fn put(h: &ServerHandle, path: &str, body: &[u8]) -> u16 {
    match ureq::put(&format!("{}{path}", h.base_url())).send_bytes(body) {
        Ok(r) => r.status(),
        Err(ureq::Error::Status(s, _)) => s,
        Err(e) => panic!("{e}"),
    }
}

fn submit(h: &ServerHandle, env: &SyncEnvelopeV1) -> (u16, Value) {
    let lib = testkit::library();
    let raw = String::from_utf8(to_canonical_json(env).unwrap()).unwrap();
    post(
        h,
        &format!("/v1/libraries/{lib}/operations"),
        format!("{{\"envelopes\":[{raw}]}}").as_bytes(),
    )
}

fn rename(a: &mut Author, seq: u64) -> SyncEnvelopeV1 {
    a.next_seq = seq;
    a.env(Op::RenameNotebook {
        notebook_id: nb(1),
        title: format!("seq {seq}"),
    })
}

fn seed(h: &ServerHandle, r: &Author, last: u64) {
    h.store
        .seed_replica_sequence_for_test(
            &testkit::library().to_string(),
            &r.replica.to_string(),
            last,
        )
        .unwrap();
}

#[test]
fn client_seq_round_trips_the_full_u64_range() {
    let dir = tmp();
    let h = start(&dir);
    let (bytes, _) = testkit::blob_background();
    assert_eq!(
        put(
            &h,
            &format!("/v1/blobs/{}", sync_protocol::sha256_hex(&bytes)),
            &bytes
        ),
        201
    );
    assert_eq!(
        post(
            &h,
            "/v1/libraries",
            &to_canonical_json(&testkit::genesis()).unwrap()
        )
        .0,
        201
    );
    const I: u64 = i64::MAX as u64;

    // None → 1.
    let mut r1 = Author::new(replica(1));
    let (s, v) = submit(&h, &rename(&mut r1, 1));
    assert_eq!(
        (s, v["results"][0]["server_seq"].as_i64()),
        (200, Some(1)),
        "{v}"
    );

    // i64::MAX − 1 → i64::MAX → i64::MAX + 1 (crosses SQLite's signed range).
    let mut r2 = Author::new(replica(2));
    seed(&h, &r2, I - 1);
    assert_eq!(submit(&h, &rename(&mut r2, I)).0, 200);
    let past_i64 = rename(&mut r2, I + 1);
    let (s, v) = submit(&h, &past_i64);
    assert_eq!(s, 200, "i64::MAX + 1 must be accepted, not narrowed: {v}");

    // u64::MAX − 1 → u64::MAX, then exhausted; retry of the accepted u64::MAX op is idempotent.
    let mut r3 = Author::new(replica(3));
    seed(&h, &r3, u64::MAX - 1);
    let at_max = rename(&mut r3, u64::MAX);
    let (s, v) = submit(&h, &at_max);
    assert_eq!(s, 200, "{v}");
    let max_seq = v["results"][0]["server_seq"].as_i64().unwrap();
    let (s, v) = submit(&h, &at_max);
    assert_eq!(
        (
            s,
            v["results"][0]["server_seq"].as_i64(),
            v["results"][0]["duplicate"].as_bool()
        ),
        (200, Some(max_seq), Some(true)),
        "lost-ACK retry at u64::MAX"
    );
    // Nothing can follow u64::MAX: a new op (any client_seq) is refused as exhausted/conflict.
    let after = {
        let mut e = rename(&mut r3, 1);
        e.operation_id = Author::new(replica(99)).op_id(7);
        e
    };
    let (s, v) = submit(&h, &after);
    assert_eq!(s, 409, "{v}");
    assert!(v["message"].as_str().unwrap().contains("exhausted"), "{v}");
    let mut clash = at_max.clone();
    clash.operation_id = Author::new(replica(99)).op_id(8);
    assert_eq!(submit(&h, &clash).1["error"], "client_seq_conflict");

    // Restart: values reload exactly; the sequences continue exactly where they were.
    drop(h);
    let h = start(&dir);
    assert_eq!(submit(&h, &past_i64).1["results"][0]["duplicate"], true);
    let (s, v) = submit(&h, &rename(&mut r2, I + 2));
    assert_eq!(s, 200, "continues after reload: {v}");
    let (s, v) = submit(&h, &at_max);
    assert_eq!(
        (s, v["results"][0]["server_seq"].as_i64()),
        (200, Some(max_seq))
    );
    let (s, v) = submit(&h, &after);
    assert_eq!(s, 409, "still exhausted after reload: {v}");
    // Out-of-order is still exact (no saturation / wrap).
    assert_eq!(
        submit(&h, &rename(&mut r1, 3)).1["error"],
        "client_seq_out_of_order"
    );

    // The ledger stores the exact decimal text; the tail returns envelopes byte-identical.
    let lib = testkit::library();
    let tail: Value = ureq::get(&format!(
        "{}/v1/libraries/{lib}/operations?limit=500",
        h.base_url()
    ))
    .call()
    .unwrap()
    .into_json()
    .unwrap();
    let seqs: Vec<u64> = tail["operations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["envelope"]["client_seq"].as_u64().unwrap())
        .collect();
    assert_eq!(seqs, vec![1, I, I + 1, u64::MAX, I + 2]);
    let conn = rusqlite::Connection::open(dir.join("ledger.sqlite3")).unwrap();
    let stored: Vec<String> = conn
        .prepare("SELECT client_seq FROM operations ORDER BY server_seq")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        stored,
        vec![
            "00000000000000000001",
            "09223372036854775807",
            "09223372036854775808",
            "18446744073709551615",
            "09223372036854775809",
        ]
    );
    assert!(h.store.verify_replay(&lib.to_string()).unwrap());
    drop(h);
    let _ = std::fs::remove_dir_all(&dir);
}
