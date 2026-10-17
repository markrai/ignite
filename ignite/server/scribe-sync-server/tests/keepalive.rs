//! Regression: idle keep-alive connections must never starve a fresh connection.
//!
//! This pins the exact starvation class observed with the original `tiny_http` transport, whose
//! finite connection worker pool could be fully occupied by long-lived keep-alive connections,
//! leaving a later accepted connection's request unread (observed on Linux as a client waiting
//! forever for its first response while the server kept answering everyone else). The hyper +
//! tokio transport runs one task per connection, so a fresh request must complete promptly no
//! matter how many idle keep-alive connections are held open.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::{Duration, Instant};

use scribe_sync_server::{ServerHandle, Store};
use sync_engine::testkit;
use sync_protocol::to_canonical_json;

const HELD: usize = 16;
const FRESH: usize = 5;
/// Generous for a loopback request, tiny compared with the old unbounded stall.
const DEADLINE: Duration = Duration::from_secs(5);

/// One HTTP/1.1 exchange on an existing connection; returns (status, body).
fn exchange(
    reader: &mut BufReader<TcpStream>,
    method: &str,
    path: &str,
    body: &[u8],
) -> (u16, Vec<u8>) {
    let started = Instant::now();
    {
        let sock = reader.get_mut();
        write!(
            sock,
            "{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: keep-alive\r\nContent-Length: {}\r\n\r\n",
            body.len()
        )
        .unwrap();
        sock.write_all(body).unwrap();
        sock.flush().unwrap();
    }
    let mut status_line = String::new();
    reader
        .read_line(&mut status_line)
        .unwrap_or_else(|e| panic!("no response to {method} {path} within {DEADLINE:?}: {e}"));
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("bad status line {status_line:?}"));
    let mut len = 0usize;
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            if k.eq_ignore_ascii_case("content-length") {
                len = v.trim().parse().unwrap();
            }
        }
    }
    let mut out = vec![0u8; len];
    reader.read_exact(&mut out).unwrap();
    assert!(
        started.elapsed() < DEADLINE,
        "{method} {path} took {:?}",
        started.elapsed()
    );
    (status, out)
}

fn connect(addr: SocketAddr) -> BufReader<TcpStream> {
    let s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(DEADLINE)).unwrap();
    s.set_write_timeout(Some(DEADLINE)).unwrap();
    s.set_nodelay(true).unwrap();
    BufReader::new(s)
}

#[test]
fn fresh_requests_complete_while_many_keepalive_connections_are_held_idle() {
    let dir = std::env::temp_dir().join(format!(
        "scribe-s2a-keepalive-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let h =
        ServerHandle::start("127.0.0.1:0".parse().unwrap(), Store::open(&dir).unwrap()).unwrap();
    let addr = h.addr();
    let lib = testkit::library();

    // A library to query (blob first: the genesis references one).
    let mut setup = connect(addr);
    let (bytes, _) = testkit::blob_background();
    let sha = sync_protocol::sha256_hex(&bytes);
    assert_eq!(
        exchange(&mut setup, "PUT", &format!("/v1/blobs/{sha}"), &bytes).0,
        201
    );
    let genesis = to_canonical_json(&testkit::genesis()).unwrap();
    assert_eq!(
        exchange(&mut setup, "POST", "/v1/libraries", &genesis).0,
        201
    );
    drop(setup);

    // 1–5. Many connections, each with a completed request, then held open and idle.
    let info = format!("/v1/libraries/{lib}");
    let mut held: Vec<BufReader<TcpStream>> = (0..HELD).map(|_| connect(addr)).collect();
    for conn in &mut held {
        let (status, body) = exchange(conn, "GET", &info, b"");
        assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    }

    // 6–9. Fresh connections must be served promptly while all of the above stay open.
    for i in 0..FRESH {
        let started = Instant::now();
        let mut fresh = connect(addr);
        let (status, body) = exchange(
            &mut fresh,
            "GET",
            &format!("/v1/libraries/{lib}/snapshot"),
            b"",
        );
        assert_eq!(
            status,
            200,
            "fresh #{i}: {}",
            String::from_utf8_lossy(&body)
        );
        assert!(
            started.elapsed() < DEADLINE,
            "fresh request #{i} took {:?} with {HELD} idle keep-alive connections held",
            started.elapsed()
        );
    }

    // The held connections are genuinely alive (keep-alive), not silently dropped.
    for conn in &mut held {
        assert_eq!(exchange(conn, "GET", &info, b"").0, 200);
    }

    // 10. Close everything and shut down normally (bounded).
    drop(held);
    let t = Instant::now();
    h.shutdown();
    assert!(
        t.elapsed() < Duration::from_secs(10),
        "shutdown took {:?}",
        t.elapsed()
    );
    let _ = std::fs::remove_dir_all(&dir);
}
