//! The streaming control-upload transport through real HTTP, with a minimal test control plane
//! whose sink records every write: chunks are bounded, ordered and hashed exactly.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use scribe_sync_server::http::{MAX_QUEUED_UPLOAD_BYTES, UPLOAD_CHUNK};
use scribe_sync_server::{
    ControlPlane, ControlReply, RequestInfo, ServerConfig, ServerHandle, Store, UploadSlot,
    UploadTicket, UploadedBody,
};

#[derive(Clone, Default)]
struct Recorder(Arc<Mutex<Vec<usize>>>, Arc<Mutex<Vec<u8>>>);

impl Write for Recorder {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().push(buf.len());
        self.1.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

struct TestPlane(Recorder);

fn reply(status: u16, body: serde_json::Value) -> ControlReply {
    ControlReply {
        status,
        content_type: "application/json",
        body: body.to_string().into_bytes(),
        headers: Vec::new(),
    }
}

impl ControlPlane for TestPlane {
    fn handle(&self, _: &Store, _: &RequestInfo, _: &[u8]) -> ControlReply {
        reply(404, serde_json::json!({}))
    }
    fn authorize_sync(&self, _: &RequestInfo) -> Result<(), (u16, &'static str, &'static str)> {
        Err((401, "unauthorized", "test plane"))
    }
    fn upload_route(&self, req: &RequestInfo) -> bool {
        req.method == "POST" && req.path() == "/control/up"
    }
    fn begin_upload(&self, _: &Store, _: &RequestInfo) -> Result<UploadSlot, ControlReply> {
        Ok(UploadSlot {
            sink: Box::new(self.0.clone()),
            max_bytes: 64 << 20,
            idle_timeout: Duration::from_secs(10),
            deadline: Duration::from_secs(60),
            ticket: Box::new(()),
        })
    }
    fn finish_upload(
        &self,
        _: &Store,
        _: &RequestInfo,
        _: UploadTicket,
        body: UploadedBody,
    ) -> ControlReply {
        reply(
            201,
            serde_json::json!({ "bytes": body.bytes, "sha256": body.sha256.as_str() }),
        )
    }
}

#[test]
fn large_frames_reach_the_sink_as_bounded_ordered_chunks() {
    assert_eq!(MAX_QUEUED_UPLOAD_BYTES, 8 * UPLOAD_CHUNK);
    let dir = std::env::temp_dir().join(format!(
        "scribe-upload-stream-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let rec = Recorder::default();
    let server = ServerHandle::start_with(
        "127.0.0.1:0".parse().unwrap(),
        Store::open(&dir).unwrap(),
        ServerConfig {
            control: Some(Arc::new(TestPlane(rec.clone()))),
            ..ServerConfig::default()
        },
    )
    .unwrap();
    let body: Vec<u8> = (0..(16 << 20)).map(|i: u32| (i % 251) as u8).collect();
    for framing in ["length", "chunked"] {
        rec.0.lock().unwrap().clear();
        rec.1.lock().unwrap().clear();
        let mut s = TcpStream::connect(server.addr()).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
        let head = if framing == "length" {
            format!("Content-Length: {}\r\n", body.len())
        } else {
            "Transfer-Encoding: chunked\r\n".into()
        };
        write!(
            s,
            "POST /control/up HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n{head}\r\n",
            server.addr()
        )
        .unwrap();
        if framing == "length" {
            s.write_all(&body).unwrap();
        } else {
            // HTTP chunks of 1 MiB: each arrives as (at least) one large frame.
            for c in body.chunks(1 << 20) {
                write!(s, "{:x}\r\n", c.len()).unwrap();
                s.write_all(c).unwrap();
                s.write_all(b"\r\n").unwrap();
            }
            s.write_all(b"0\r\n\r\n").unwrap();
        }
        let mut out = String::new();
        s.read_to_string(&mut out).unwrap();
        assert!(out.starts_with("HTTP/1.1 201"), "{framing}: {out}");
        let json: serde_json::Value =
            serde_json::from_str(out.split_once("\r\n\r\n").unwrap().1).unwrap();
        assert_eq!(json["bytes"], body.len(), "{framing}");
        assert_eq!(
            json["sha256"],
            sync_protocol::sha256_hex(&body).as_str(),
            "{framing}"
        );
        let sizes = rec.0.lock().unwrap().clone();
        let max = sizes.iter().copied().max().unwrap();
        assert!(max <= UPLOAD_CHUNK, "{framing}: a {max}-byte write");
        assert_eq!(
            *rec.1.lock().unwrap(),
            body,
            "{framing}: every byte, in order"
        );
    }
    server.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}
