//! Versioned HTTP API (`/v1`) over [`Store`]. Loopback only in S2A (no auth yet).
//!
//! ```text
//! POST /v1/libraries                                   canonical SyncSnapshotV1 genesis
//! GET  /v1/libraries/{id}                              {library_id, genesis_sha256, head_server_seq}
//! GET  /v1/libraries/{id}/genesis                      {library_id, genesis_sha256, snapshot}
//! POST /v1/libraries/{id}/operations                   {"envelopes":[SyncEnvelopeV1…]} (≤ MAX_BATCH)
//! GET  /v1/libraries/{id}/operations?after=N&limit=M   {library_id, head_server_seq, operations:[{server_seq, envelope}], next_after}
//! GET  /v1/libraries/{id}/snapshot                     {library_id, through_server_seq, snapshot_sha256, snapshot}
//! PUT  /v1/blobs/{sha256}                              raw bytes (content-addressed)
//! GET  /v1/blobs/{sha256}
//! ```
//!
//! Envelopes and snapshots are embedded as raw JSON: exactly the stored canonical bytes.
//!
//! Transport: `hyper` HTTP/1.1 on a `tokio` runtime. Every connection is its own task (keep-alive
//! connections can never starve one another), request bodies are size-limited while streaming,
//! and ledger work runs on `spawn_blocking`. (S2A first used `tiny_http`; its connection
//! `TaskPool` can strand a newly accepted keep-alive connection in its queue while every pool
//! thread serves a long-lived connection — observed on Linux as a client whose request was
//! never read. Routing below is transport-independent.)
//! Status codes: 200/201 success; 400 malformed/non-canonical/unknown fields/wrong version/
//! wrong library/oversized batch; 404 unknown library/blob/route; 405 method; 409 sequencing
//! or identity conflicts; 413 body too large; 422 semantic refusal or missing blobs; 500 storage.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use http_body_util::{BodyExt, Full, Limited};
use hyper::body::{Bytes, Incoming};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use tokio::sync::watch;

use crate::auth::TokenVerifier;
use crate::store::{Store, StoreError};
use crate::web::{StaticReply, WebRoot};

/// Largest JSON request body.
pub const MAX_JSON_BODY: usize = 4 * 1024 * 1024;
/// Largest blob body (Kindle backgrounds are ~4.6 MB).
pub const MAX_BLOB_BODY: usize = 32 * 1024 * 1024;
/// Most envelopes per submission.
pub const MAX_BATCH: usize = 100;
pub const DEFAULT_TAIL_LIMIT: i64 = 100;
pub const MAX_TAIL_LIMIT: i64 = 500;
/// Transport-independent response.
pub struct Reply {
    pub status: u16,
    pub content_type: &'static str,
    pub body: Vec<u8>,
}

fn json(status: u16, body: Vec<u8>) -> Reply {
    Reply {
        status,
        content_type: "application/json",
        body,
    }
}

fn error(status: u16, code: &str, message: impl std::fmt::Display) -> Reply {
    #[derive(Serialize)]
    struct E<'a> {
        error: &'a str,
        message: String,
    }
    json(
        status,
        serde_json::to_vec(&E {
            error: code,
            message: message.to_string(),
        })
        .unwrap(),
    )
}

fn store_error(e: StoreError) -> Reply {
    match e {
        StoreError::BadRequest { code, message } => error(400, code, message),
        StoreError::NotFound { code, message } => error(404, code, message),
        StoreError::Conflict { code, message } => error(409, code, message),
        StoreError::Semantic { index, error: se } => {
            #[derive(Serialize)]
            struct S<'a> {
                error: &'a str,
                semantic_code: &'a str,
                envelope_index: usize,
                message: String,
            }
            json(
                422,
                serde_json::to_vec(&S {
                    error: "semantic_refusal",
                    semantic_code: se.code.as_str(),
                    envelope_index: index,
                    message: se.message,
                })
                .unwrap(),
            )
        }
        StoreError::MissingBlobs(missing) => {
            #[derive(Serialize)]
            struct M {
                error: &'static str,
                missing_blobs: Vec<String>,
            }
            json(
                422,
                serde_json::to_vec(&M {
                    error: "missing_blobs",
                    missing_blobs: missing,
                })
                .unwrap(),
            )
        }
        StoreError::Internal(m) => error(500, "internal", m),
        StoreError::CommitUncertain(m) => error(500, "commit_uncertain", m),
    }
}

fn raw(bytes: Vec<u8>) -> Box<RawValue> {
    RawValue::from_string(String::from_utf8(bytes).expect("stored canonical JSON is UTF-8"))
        .expect("stored canonical JSON is valid")
}

/// Body limit for a request path.
fn body_limit(path: &str) -> usize {
    if path.starts_with("/v1/blobs/") {
        MAX_BLOB_BODY
    } else {
        MAX_JSON_BODY
    }
}

fn query_param(query: &str, key: &str) -> Option<String> {
    query
        .split('&')
        .filter_map(|kv| kv.split_once('='))
        .find(|(k, _)| *k == key)
        .map(|(_, v)| v.to_string())
}

/// Route one request (body already read and size-limited). Blocking: runs off the reactor.
pub fn route(store: &Store, method: &str, url: &str, body: &[u8]) -> Reply {
    let (path, query) = url.split_once('?').unwrap_or((url, ""));
    let segs: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    match (segs.as_slice(), method) {
        (["v1", "libraries"], "POST") => match store.bootstrap(body) {
            Ok((info, created)) => json(if created { 201 } else { 200 }, info_json(&info)),
            Err(e) => store_error(e),
        },
        (["v1", "libraries"], "GET") => match store.list_libraries() {
            Ok(libs) => {
                #[derive(Serialize)]
                struct L {
                    library_id: String,
                    head_server_seq: i64,
                    snapshot_sha256: String,
                    notebook_count: usize,
                    page_count: usize,
                }
                #[derive(Serialize)]
                struct Out {
                    v: u32,
                    libraries: Vec<L>,
                }
                json(
                    200,
                    serde_json::to_vec(&Out {
                        v: 1,
                        libraries: libs
                            .into_iter()
                            .map(|l| L {
                                library_id: l.library_id.to_string(),
                                head_server_seq: l.head_server_seq,
                                snapshot_sha256: l.snapshot_sha256,
                                notebook_count: l.notebook_count,
                                page_count: l.page_count,
                            })
                            .collect(),
                    })
                    .unwrap(),
                )
            }
            Err(e) => store_error(e),
        },
        (["v1", "libraries", id], "GET") => match (store.library(id), store.snapshot(id)) {
            (Ok(info), Ok(cur)) => json(
                200,
                serde_json::to_vec(&serde_json::json!({
                    "library_id": info.library_id.to_string(),
                    "genesis_sha256": info.genesis_sha256,
                    "head_server_seq": info.head_server_seq,
                    "snapshot_sha256": cur.snapshot_sha256,
                }))
                .unwrap(),
            ),
            (Err(e), _) | (_, Err(e)) => store_error(e),
        },
        (["v1", "libraries", id, "genesis"], "GET") => {
            match (store.library(id), store.genesis(id)) {
                (Ok(info), Ok(bytes)) => {
                    #[derive(Serialize)]
                    struct G {
                        library_id: String,
                        genesis_sha256: String,
                        snapshot: Box<RawValue>,
                    }
                    json(
                        200,
                        serde_json::to_vec(&G {
                            library_id: info.library_id.to_string(),
                            genesis_sha256: info.genesis_sha256,
                            snapshot: raw(bytes),
                        })
                        .unwrap(),
                    )
                }
                (Err(e), _) | (_, Err(e)) => store_error(e),
            }
        }
        (["v1", "libraries", id, "snapshot"], "GET") => match store.snapshot(id) {
            Ok(cur) => {
                #[derive(Serialize)]
                struct S {
                    library_id: String,
                    through_server_seq: i64,
                    snapshot_sha256: String,
                    snapshot: Box<RawValue>,
                }
                json(
                    200,
                    serde_json::to_vec(&S {
                        library_id: id.to_string(),
                        through_server_seq: cur.through_server_seq,
                        snapshot_sha256: cur.snapshot_sha256,
                        snapshot: raw(cur.snapshot_bytes),
                    })
                    .unwrap(),
                )
            }
            Err(e) => store_error(e),
        },
        (["v1", "libraries", id, "operations"], "POST") => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Submit<'a> {
                #[serde(borrow)]
                envelopes: Vec<&'a RawValue>,
            }
            let sub: Submit<'_> = match serde_json::from_slice(body) {
                Ok(s) => s,
                Err(e) => return error(400, "invalid_json", e),
            };
            if sub.envelopes.is_empty() || sub.envelopes.len() > MAX_BATCH {
                return error(
                    400,
                    "invalid_batch_size",
                    format!(
                        "need 1..={MAX_BATCH} envelopes, got {}",
                        sub.envelopes.len()
                    ),
                );
            }
            let raws: Vec<Vec<u8>> = sub
                .envelopes
                .iter()
                .map(|r| r.get().as_bytes().to_vec())
                .collect();
            match store.submit(id, &raws) {
                Ok((head, results)) => {
                    if store
                        .hooks
                        .fail_drop_response_after_commit
                        .load(std::sync::atomic::Ordering::SeqCst)
                    {
                        return error(500, "injected_dropped_ack", "response dropped after commit");
                    }
                    #[derive(Serialize)]
                    struct A {
                        operation_id: String,
                        server_seq: i64,
                        outcome: String,
                        duplicate: bool,
                    }
                    #[derive(Serialize)]
                    struct Out {
                        library_id: String,
                        head_server_seq: i64,
                        results: Vec<A>,
                    }
                    json(
                        200,
                        serde_json::to_vec(&Out {
                            library_id: id.to_string(),
                            head_server_seq: head,
                            results: results
                                .into_iter()
                                .map(|r| A {
                                    operation_id: r.operation_id,
                                    server_seq: r.server_seq,
                                    outcome: r.outcome,
                                    duplicate: r.duplicate,
                                })
                                .collect(),
                        })
                        .unwrap(),
                    )
                }
                Err(e) => store_error(e),
            }
        }
        (["v1", "libraries", id, "operations"], "GET") => {
            let after = match query_param(query, "after").map(|v| v.parse::<i64>()) {
                None => 0,
                Some(Ok(v)) if v >= 0 => v,
                _ => return error(400, "invalid_query", "after must be an integer >= 0"),
            };
            let limit = match query_param(query, "limit").map(|v| v.parse::<i64>()) {
                None => DEFAULT_TAIL_LIMIT,
                Some(Ok(v)) if (1..=MAX_TAIL_LIMIT).contains(&v) => v,
                _ => {
                    return error(
                        400,
                        "invalid_query",
                        format!("limit must be 1..={MAX_TAIL_LIMIT}"),
                    )
                }
            };
            match store.tail(id, after, limit) {
                Ok((head, entries)) => {
                    #[derive(Serialize)]
                    struct E {
                        server_seq: i64,
                        envelope: Box<RawValue>,
                    }
                    #[derive(Serialize)]
                    struct T {
                        library_id: String,
                        head_server_seq: i64,
                        operations: Vec<E>,
                        next_after: i64,
                    }
                    let next_after = entries.last().map_or(after, |e| e.server_seq);
                    json(
                        200,
                        serde_json::to_vec(&T {
                            library_id: id.to_string(),
                            head_server_seq: head,
                            operations: entries
                                .into_iter()
                                .map(|e| E {
                                    server_seq: e.server_seq,
                                    envelope: raw(e.envelope_bytes),
                                })
                                .collect(),
                            next_after,
                        })
                        .unwrap(),
                    )
                }
                Err(e) => store_error(e),
            }
        }
        (["v1", "blobs", sha], "PUT") => match store.put_blob(sha, body) {
            Ok(created) => {
                let out = serde_json::json!({
                    "sha256": sha, "byte_len": body.len(), "created": created
                });
                json(
                    if created { 201 } else { 200 },
                    serde_json::to_vec(&out).unwrap(),
                )
            }
            Err(e) => store_error(e),
        },
        (["v1", "blobs", sha], "GET") => match store.get_blob(sha) {
            Ok(bytes) => Reply {
                status: 200,
                content_type: "application/octet-stream",
                body: bytes,
            },
            Err(e) => store_error(e),
        },
        (["v1", "libraries"], _)
        | (["v1", "libraries", _], _)
        | (["v1", "libraries", _, "genesis" | "snapshot" | "operations"], _)
        | (["v1", "blobs", _], _) => error(405, "method_not_allowed", method),
        _ => error(404, "unknown_route", path),
    }
}

fn info_json(info: &crate::store::LibraryInfo) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "library_id": info.library_id.to_string(),
        "genesis_sha256": info.genesis_sha256,
        "head_server_seq": info.head_server_seq,
    }))
    .unwrap()
}

/// Server exposure + authentication (S2B).
#[derive(Clone, Default)]
pub struct ServerConfig {
    /// When set, every `/v1` request must carry `Authorization: Bearer <token>`.
    pub auth: Option<TokenVerifier>,
    /// S3A: serve the ignite-web bundle for non-`/v1` paths. `None` = no static behavior at all.
    pub web_root: Option<WebRoot>,
    /// D1 product mode: the control plane (`/control/*` routes + `/v1` authorization). When set
    /// it replaces `auth` for `/v1`; the sync routes themselves are unchanged.
    pub control: Option<Arc<dyn ControlPlane>>,
}

impl std::fmt::Debug for ServerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerConfig")
            .field("auth", &self.auth)
            .field("web_root", &self.web_root)
            .field("control", &self.control.as_ref().map(|_| "<control plane>"))
            .finish()
    }
}

/// Transport-independent view of one request for the control plane. Only the headers the
/// control plane needs are captured (never logged by this crate).
#[derive(Clone, Debug)]
pub struct RequestInfo {
    pub method: String,
    /// Path plus query, as received.
    pub url: String,
    pub peer: Option<SocketAddr>,
    pub authorization: Option<String>,
    pub cookie: Option<String>,
    pub origin: Option<String>,
    pub host: Option<String>,
    /// `X-Scribe-Request` (anti-CSRF marker: a custom header cross-origin pages cannot send
    /// without a CORS preflight, which this server never grants).
    pub scribe_request: Option<String>,
    pub user_agent: Option<String>,
    pub content_type: Option<String>,
    /// `Content-Length` as received (unparsed).
    pub content_length: Option<String>,
}

impl RequestInfo {
    fn from_parts(parts: &hyper::http::request::Parts, peer: Option<SocketAddr>) -> Self {
        let h = |name: &str| {
            parts
                .headers
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
        };
        Self {
            method: parts.method.as_str().to_string(),
            url: parts.uri.path_and_query().map_or_else(
                || parts.uri.path().to_string(),
                |pq| pq.as_str().to_string(),
            ),
            peer,
            authorization: h("authorization"),
            cookie: h("cookie"),
            origin: h("origin"),
            host: h("host"),
            scribe_request: h("x-scribe-request"),
            user_agent: h("user-agent"),
            content_type: h("content-type"),
            content_length: h("content-length"),
        }
    }

    pub fn path(&self) -> &str {
        self.url.split_once('?').map_or(&self.url, |(p, _)| p)
    }
}

/// A control-plane response (JSON by convention) with optional extra headers (`Set-Cookie`).
#[derive(Clone, Debug)]
pub struct ControlReply {
    pub status: u16,
    pub content_type: &'static str,
    pub body: Vec<u8>,
    pub headers: Vec<(&'static str, String)>,
}

/// D1 product control plane. Implementations are blocking (called off the reactor).
pub trait ControlPlane: Send + Sync + 'static {
    /// Handle a `/control/...` request. The body is already size-limited.
    fn handle(&self, store: &Store, req: &RequestInfo, body: &[u8]) -> ControlReply;
    /// Authorize one `/v1` request. `Err` carries the refusal (status + code + message).
    fn authorize_sync(&self, req: &RequestInfo) -> Result<(), (u16, &'static str, &'static str)>;
    /// D1.1 network boundary: may a TCP connection accepted on `local` from `peer` be served?
    /// `false` closes it before any HTTP is read. Default: accept (developer server unchanged).
    fn accept(&self, _local: SocketAddr, _peer: SocketAddr) -> bool {
        true
    }
    /// Is this `/control/...` request a streaming upload (instead of the 64 KiB in-memory
    /// path)? Headers only, no I/O. Default: no streaming routes.
    fn upload_route(&self, _req: &RequestInfo) -> bool {
        false
    }
    /// Authorize and prepare a streaming upload. Blocking; called before any body byte is
    /// read. An `Err` reply is sent with `Connection: close` and the body is never read (so an
    /// `Expect: 100-continue` client is never told to send it).
    fn begin_upload(&self, _store: &Store, _req: &RequestInfo) -> Result<UploadSlot, ControlReply> {
        Err(control_error(404, "not_found", "unknown control route"))
    }
    /// The whole body was written to the slot's sink and flushed; the sink is closed. Blocking.
    fn finish_upload(
        &self,
        _store: &Store,
        _req: &RequestInfo,
        _ticket: UploadTicket,
        _body: UploadedBody,
    ) -> ControlReply {
        control_error(404, "not_found", "unknown control route")
    }
}

/// Plane-owned state of one streaming upload (e.g. the private file's cleanup guard and the
/// concurrency permit). The transport drops it on every failure path, after the sink.
pub type UploadTicket = Box<dyn std::any::Any + Send>;

/// A streaming upload accepted by [`ControlPlane::begin_upload`].
pub struct UploadSlot {
    /// Where the body goes (written off the reactor, in arrival order).
    pub sink: Box<dyn std::io::Write + Send>,
    /// Refused (413) as soon as more bytes than this arrive, whatever `Content-Length` said.
    pub max_bytes: u64,
    /// Longest wait for the next body frame.
    pub idle_timeout: Duration,
    /// Longest whole transfer (including waiting for a slow sink).
    pub deadline: Duration,
    pub ticket: UploadTicket,
}

/// A completely received upload body.
#[derive(Clone, Debug)]
pub struct UploadedBody {
    pub bytes: u64,
    pub sha256: sync_protocol::Sha256Hex,
}

/// Chunks queued between the connection task and the blocking writer (backpressure: the
/// connection stops reading while the queue is full).
const UPLOAD_QUEUE: usize = 8;
/// Largest queued chunk. Each body frame (whatever size hyper hands over) is copied into
/// chunks of at most this size, so a queued chunk never keeps a larger frame buffer alive.
pub const UPLOAD_CHUNK: usize = 64 * 1024;
/// Most upload payload bytes ever waiting in the queue: 512 KiB. Besides the queue, one
/// upload holds at most the frame being split (bounded by hyper's read buffer), the chunk
/// the writer is writing, and the plane's sink buffer.
pub const MAX_QUEUED_UPLOAD_BYTES: usize = UPLOAD_QUEUE * UPLOAD_CHUNK;

/// `data` as owned chunks of at most `max` bytes, in order.
fn bounded_chunks(data: &[u8], max: usize) -> impl Iterator<Item = Bytes> + '_ {
    data.chunks(max).map(Bytes::copy_from_slice)
}

/// Largest control-plane request body.
pub const MAX_CONTROL_BODY: usize = 64 * 1024;

fn control_error(status: u16, code: &str, message: &str) -> ControlReply {
    ControlReply {
        status,
        content_type: "application/json",
        body: serde_json::to_vec(&serde_json::json!({ "error": code, "message": message }))
            .expect("serializable"),
        headers: Vec::new(),
    }
}

/// A reply sent before or while an upload body is unread: the connection is closed afterwards
/// instead of reading (draining) what is left of a possibly enormous body.
fn closing(mut reply: ControlReply) -> hyper::Response<Full<Bytes>> {
    reply.headers.push(("Connection", "close".into()));
    respond_control(reply)
}

enum Chunk {
    Data(Bytes),
    End,
}

enum StreamFailure {
    TooLarge,
    Body,
    Idle,
    Deadline,
    WriterGone,
}

/// Read body frames into the writer queue, enforcing the byte cap, idle timeout and deadline.
async fn stream_body(
    body: &mut Incoming,
    tx: &tokio::sync::mpsc::Sender<Chunk>,
    max: u64,
    idle: Duration,
    deadline: tokio::time::Instant,
) -> Result<(), StreamFailure> {
    let mut total = 0u64;
    loop {
        let now = tokio::time::Instant::now();
        let wait = idle.min(deadline.saturating_duration_since(now));
        let frame = match tokio::time::timeout(wait, body.frame()).await {
            Err(_) if tokio::time::Instant::now() >= deadline => {
                return Err(StreamFailure::Deadline)
            }
            Err(_) => return Err(StreamFailure::Idle),
            Ok(None) => break,
            Ok(Some(Err(_))) => return Err(StreamFailure::Body),
            Ok(Some(Ok(f))) => f,
        };
        let Ok(data) = frame.into_data() else {
            continue; // trailers carry no content
        };
        total = total.saturating_add(data.len() as u64);
        if total > max {
            return Err(StreamFailure::TooLarge);
        }
        for chunk in bounded_chunks(&data, UPLOAD_CHUNK) {
            match tokio::time::timeout_at(deadline, tx.send(Chunk::Data(chunk))).await {
                Err(_) => return Err(StreamFailure::Deadline),
                Ok(Err(_)) => return Err(StreamFailure::WriterGone),
                Ok(Ok(())) => {}
            }
        }
    }
    match tokio::time::timeout_at(deadline, tx.send(Chunk::End)).await {
        Err(_) => Err(StreamFailure::Deadline),
        Ok(Err(_)) => Err(StreamFailure::WriterGone),
        Ok(Ok(())) => Ok(()),
    }
}

/// Blocking writer: drains the queue into the sink while hashing. A closed queue without
/// `End` (connection dropped, limit, timeout) is an abort. The sink is always closed before
/// the ticket is released, so the plane's cleanup never races an open file.
fn write_upload(
    mut sink: Box<dyn std::io::Write + Send>,
    ticket: UploadTicket,
    mut rx: tokio::sync::mpsc::Receiver<Chunk>,
) -> Result<(UploadedBody, UploadTicket), Option<std::io::Error>> {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    let mut bytes = 0u64;
    let result = loop {
        match rx.blocking_recv() {
            Some(Chunk::Data(b)) => {
                if let Err(e) = sink.write_all(&b) {
                    break Err(Some(e));
                }
                h.update(&b);
                bytes += b.len() as u64;
            }
            Some(Chunk::End) => break sink.flush().map_err(Some),
            None => break Err(None),
        }
    };
    drop(rx);
    drop(sink);
    match result {
        Ok(()) => {
            let d: [u8; 32] = h.finalize().into();
            Ok((
                UploadedBody {
                    bytes,
                    sha256: sync_protocol::Sha256Hex::from_digest(&d),
                },
                ticket,
            ))
        }
        Err(e) => {
            drop(ticket);
            Err(e)
        }
    }
}

fn storage_full(e: &std::io::Error) -> bool {
    e.kind() == std::io::ErrorKind::StorageFull
        || e.kind() == std::io::ErrorKind::QuotaExceeded
        || matches!(e.raw_os_error(), Some(28) if cfg!(unix))
        || matches!(e.raw_os_error(), Some(39 | 112) if cfg!(windows))
}

/// A streaming control upload: authorize from headers, then stream the body to the plane's
/// sink through a bounded queue and a blocking writer, then let the plane finish it.
async fn serve_upload(
    control: Arc<dyn ControlPlane>,
    store: Arc<Store>,
    info: RequestInfo,
    mut body: Incoming,
) -> hyper::Response<Full<Bytes>> {
    let internal = || control_error(500, "internal", "upload failed");
    let begin = {
        let (control, store, info) = (Arc::clone(&control), Arc::clone(&store), info.clone());
        tokio::task::spawn_blocking(move || control.begin_upload(&store, &info)).await
    };
    let slot = match begin {
        Ok(Ok(slot)) => slot,
        Ok(Err(reply)) => return closing(reply),
        Err(_) => return closing(internal()),
    };
    let UploadSlot {
        sink,
        max_bytes,
        idle_timeout,
        deadline,
        ticket,
    } = slot;
    let deadline = tokio::time::Instant::now() + deadline;
    let (tx, rx) = tokio::sync::mpsc::channel(UPLOAD_QUEUE);
    // If this future is dropped (client gone), `tx` is dropped with it and the writer aborts
    // on its own: it never outlives the queue.
    let writer = tokio::task::spawn_blocking(move || write_upload(sink, ticket, rx));
    let streamed = stream_body(&mut body, &tx, max_bytes, idle_timeout, deadline).await;
    drop(tx);
    let written = writer.await;
    let failure = match (streamed, written) {
        (Ok(()), Ok(Ok((uploaded, ticket)))) => {
            let reply = tokio::task::spawn_blocking(move || {
                control.finish_upload(&store, &info, ticket, uploaded)
            })
            .await
            .unwrap_or_else(|_| internal());
            return respond_control(reply);
        }
        (_, Ok(Err(Some(e)))) if storage_full(&e) => control_error(
            507,
            "insufficient_storage",
            "not enough space to store the upload",
        ),
        (_, Ok(Err(Some(_)))) | (Err(StreamFailure::WriterGone), _) => internal(),
        (Err(StreamFailure::TooLarge), _) => {
            control_error(413, "body_too_large", &format!("limit {max_bytes} bytes"))
        }
        (Err(StreamFailure::Body), _) => {
            control_error(400, "bad_body", "the request body could not be read")
        }
        (Err(StreamFailure::Idle), _) => {
            control_error(408, "upload_idle_timeout", "no upload data arrived in time")
        }
        (Err(StreamFailure::Deadline), _) => {
            control_error(408, "upload_deadline", "the upload took too long")
        }
        _ => internal(),
    };
    closing(failure)
}

fn respond_control(reply: ControlReply) -> hyper::Response<Full<Bytes>> {
    let mut b = hyper::Response::builder()
        .status(reply.status)
        .header(hyper::header::CONTENT_TYPE, reply.content_type)
        .header(hyper::header::CACHE_CONTROL, "no-store")
        .header("X-Content-Type-Options", "nosniff")
        .header("Referrer-Policy", "no-referrer");
    for (k, v) in reply.headers {
        b = b.header(k, v);
    }
    b.body(Full::new(Bytes::from(reply.body)))
        .expect("control response parts are valid")
}

fn respond_static(reply: StaticReply, head: bool) -> hyper::Response<Full<Bytes>> {
    let mut b = hyper::Response::builder()
        .status(reply.status)
        .header(hyper::header::CONTENT_TYPE, reply.content_type)
        .header(hyper::header::CACHE_CONTROL, reply.cache_control)
        .header("X-Content-Type-Options", "nosniff")
        .header("Referrer-Policy", "no-referrer");
    if reply.content_type.starts_with("text/html") {
        b = b
            .header(
                "Content-Security-Policy",
                concat!(
                    "default-src 'self'; ",
                    // S3B: 'wasm-unsafe-eval' permits compiling the same-origin protocol bridge
                    // (WebAssembly only). Generic 'unsafe-eval' stays forbidden.
                    "script-src 'self' 'wasm-unsafe-eval'; ",
                    // Lifecycle-only /sw.js (update toast). No fetch handler; not an offline shell.
                    "worker-src 'self'; ",
                    "style-src 'self'; img-src 'self' data: blob:; connect-src 'self'; ",
                    "object-src 'none'; base-uri 'none'; frame-ancestors 'none'"
                ),
            )
            .header("X-Frame-Options", "DENY");
    }
    let body = if head { Vec::new() } else { reply.body };
    b.body(Full::new(Bytes::from(body)))
        .expect("static response parts are valid")
}

fn respond(reply: Reply) -> hyper::Response<Full<Bytes>> {
    let mut b = hyper::Response::builder()
        .status(reply.status)
        .header(hyper::header::CONTENT_TYPE, reply.content_type);
    if reply.status == 401 {
        b = b.header(hyper::header::WWW_AUTHENTICATE, "Bearer");
    }
    b.body(Full::new(Bytes::from(reply.body)))
        .expect("static response parts are valid")
}

async fn serve_request(
    store: Arc<Store>,
    config: Arc<ServerConfig>,
    peer: Option<SocketAddr>,
    req: hyper::Request<Incoming>,
) -> Result<hyper::Response<Full<Bytes>>, Infallible> {
    // D1: the product control plane owns `/control/*` and `/v1` authorization.
    if let Some(control) = &config.control {
        let path = req.uri().path();
        if path == "/control" || path.starts_with("/control/") {
            let (parts, body) = req.into_parts();
            let info = RequestInfo::from_parts(&parts, peer);
            if control.upload_route(&info) {
                let control = Arc::clone(control);
                return Ok(serve_upload(control, store, info, body).await);
            }
            let body = match Limited::new(body, MAX_CONTROL_BODY).collect().await {
                Ok(c) => c.to_bytes(),
                Err(_) => {
                    return Ok(respond(error(
                        413,
                        "body_too_large",
                        format!("limit {MAX_CONTROL_BODY} bytes"),
                    )))
                }
            };
            let control = Arc::clone(control);
            let store = Arc::clone(&store);
            let reply = tokio::task::spawn_blocking(move || control.handle(&store, &info, &body))
                .await
                .unwrap_or_else(|_| ControlReply {
                    status: 500,
                    content_type: "application/json",
                    body: br#"{"error":"internal","message":"control handler panicked"}"#.to_vec(),
                    headers: Vec::new(),
                });
            return Ok(respond_control(reply));
        }
    }
    // S3A: everything outside `/v1` is the public static bundle (only when configured).
    if let Some(web) = &config.web_root {
        let path = req.uri().path();
        if path != "/v1" && !path.starts_with("/v1/") {
            let method = req.method().as_str();
            return Ok(respond_static(web.serve(method, path), method == "HEAD"));
        }
    }
    if let Some(control) = &config.control {
        let (parts, body) = req.into_parts();
        let info = RequestInfo::from_parts(&parts, peer);
        let control = Arc::clone(control);
        let decision = tokio::task::spawn_blocking(move || control.authorize_sync(&info))
            .await
            .unwrap_or(Err((500, "internal", "authorization failed")));
        if let Err((status, code, message)) = decision {
            let limit = body_limit(parts.uri.path());
            let _ = Limited::new(body, limit).collect().await;
            return Ok(respond(error(status, code, message)));
        }
        return Ok(serve_authorized(store, hyper::Request::from_parts(parts, body)).await);
    }
    if let Some(verifier) = &config.auth {
        let header = req
            .headers()
            .get(hyper::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok());
        if !verifier.verify_header(header) {
            // Drain the (size-limited) body first: replying mid-upload would make the client
            // see a connection reset instead of a definitive 401 (then it would retry).
            let limit = body_limit(req.uri().path());
            let _ = Limited::new(req.into_body(), limit).collect().await;
            // Never echo the presented credential.
            return Ok(respond(error(
                401,
                "unauthorized",
                "missing or invalid bearer token",
            )));
        }
    }
    Ok(serve_authorized(store, req).await)
}

/// Read the (size-limited) body and route an already-authorized `/v1` request.
async fn serve_authorized(
    store: Arc<Store>,
    req: hyper::Request<Incoming>,
) -> hyper::Response<Full<Bytes>> {
    let method = req.method().as_str().to_string();
    let url = req.uri().path_and_query().map_or_else(
        || req.uri().path().to_string(),
        |pq| pq.as_str().to_string(),
    );
    let limit = body_limit(req.uri().path());
    let declared = req
        .headers()
        .get(hyper::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    let reply = if declared.is_some_and(|n| n > limit as u64) {
        error(413, "body_too_large", format!("limit {limit} bytes"))
    } else {
        match Limited::new(req.into_body(), limit).collect().await {
            Err(e)
                if e.downcast_ref::<http_body_util::LengthLimitError>()
                    .is_some() =>
            {
                error(413, "body_too_large", format!("limit {limit} bytes"))
            }
            Err(e) => error(400, "bad_body", e),
            Ok(collected) => {
                let body = collected.to_bytes();
                tokio::task::spawn_blocking(move || route(&store, &method, &url, &body))
                    .await
                    .unwrap_or_else(|e| error(500, "internal", format!("handler panicked: {e}")))
            }
        }
    };
    respond(reply)
}

/// A running server. Dropping it (or [`ServerHandle::shutdown`]) stops accepting, then shuts
/// the runtime down (bounded wait for in-flight ledger work) and releases the store.
pub struct ServerHandle {
    addr: SocketAddr,
    stop: Option<watch::Sender<bool>>,
    runtime: Option<tokio::runtime::Runtime>,
    pub store: Arc<Store>,
}

impl ServerHandle {
    /// Loopback, unauthenticated development server (S2A behavior).
    pub fn start(addr: SocketAddr, store: Store) -> Result<Self, String> {
        Self::start_with(addr, store, ServerConfig::default())
    }

    /// Bind `addr` and serve `store`. Invariant: a non-loopback bind **without** auth is refused
    /// (an unauthenticated ledger is never exposed to the LAN). With auth configured, every
    /// request — loopback or not — must authenticate.
    pub fn start_with(
        addr: SocketAddr,
        store: Store,
        config: ServerConfig,
    ) -> Result<Self, String> {
        if !addr.ip().is_loopback() && config.auth.is_none() && config.control.is_none() {
            return Err(format!(
                "REFUSE: non-loopback bind {addr} requires authentication (--token-verifier-file)"
            ));
        }
        let config = Arc::new(config);
        let std_listener =
            std::net::TcpListener::bind(addr).map_err(|e| format!("bind {addr}: {e}"))?;
        std_listener
            .set_nonblocking(true)
            .map_err(|e| format!("nonblocking: {e}"))?;
        let addr = std_listener
            .local_addr()
            .map_err(|e| format!("local addr: {e}"))?;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .thread_name("scribe-sync-server")
            .build()
            .map_err(|e| format!("runtime: {e}"))?;
        let store = Arc::new(store);
        let (stop_tx, mut stop_rx) = watch::channel(false);
        let listener = {
            let _guard = runtime.enter();
            tokio::net::TcpListener::from_std(std_listener).map_err(|e| format!("listener: {e}"))?
        };
        let accept_store = Arc::clone(&store);
        let accept_config = Arc::clone(&config);
        runtime.spawn(async move {
            loop {
                tokio::select! {
                    _ = stop_rx.changed() => break,
                    accepted = listener.accept() => {
                        let Ok((stream, peer)) = accepted else { continue };
                        // D1.1: product-mode network boundary (closes non-LAN connections).
                        if let Some(control) = &accept_config.control {
                            match stream.local_addr() {
                                Ok(local) if control.accept(local, peer) => {}
                                _ => continue,
                            }
                        }
                        let store = Arc::clone(&accept_store);
                        let config = Arc::clone(&accept_config);
                        tokio::spawn(async move {
                            let service = service_fn(move |req| {
                                serve_request(Arc::clone(&store), Arc::clone(&config), Some(peer), req)
                            });
                            let _ = http1::Builder::new()
                                .keep_alive(true)
                                .serve_connection(TokioIo::new(stream), service)
                                .await;
                        });
                    }
                }
            }
        });
        Ok(Self {
            addr,
            stop: Some(stop_tx),
            runtime: Some(runtime),
            store,
        })
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub fn shutdown(mut self) {
        self.stop();
    }

    fn stop(&mut self) {
        if let Some(tx) = self.stop.take() {
            let _ = tx.send(true);
        }
        if let Some(rt) = self.runtime.take() {
            rt.shutdown_timeout(Duration::from_secs(10));
        }
    }

    /// Block the calling thread until the process is terminated (binary use).
    pub fn join(self) -> ! {
        loop {
            std::thread::park();
        }
    }
}

impl Drop for ServerHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod upload_tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Mutex;

    /// Records every write the upload writer makes.
    #[derive(Clone, Default)]
    struct Recorder(Arc<Mutex<Vec<Vec<u8>>>>);

    impl std::io::Write for Recorder {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().push(buf.to_vec());
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    struct DropFlag(Arc<AtomicBool>);

    impl Drop for DropFlag {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    fn data(n: usize) -> Vec<u8> {
        (0..n).map(|i| (i % 251) as u8).collect()
    }

    #[test]
    fn frames_are_split_into_bounded_chunks_in_order() {
        assert_eq!(MAX_QUEUED_UPLOAD_BYTES, 512 * 1024);
        for n in [
            0,
            1,
            UPLOAD_CHUNK - 1,
            UPLOAD_CHUNK,
            UPLOAD_CHUNK + 1,
            3 * UPLOAD_CHUNK + 17,
        ] {
            let d = data(n);
            let chunks: Vec<Bytes> = bounded_chunks(&d, UPLOAD_CHUNK).collect();
            assert_eq!(chunks.len(), n.div_ceil(UPLOAD_CHUNK), "{n}");
            assert!(chunks
                .iter()
                .all(|c| !c.is_empty() && c.len() <= UPLOAD_CHUNK));
            assert_eq!(chunks.concat(), d, "{n}: order and content preserved");
        }
    }

    /// Run the blocking writer on its own thread and feed it `frames`, split as the connection
    /// task splits them; `end` sends the end marker, otherwise the queue is just dropped.
    type Written = Result<(UploadedBody, UploadTicket), Option<std::io::Error>>;

    fn run_writer(frames: &[Vec<u8>], end: bool) -> (Written, Recorder, Arc<AtomicBool>) {
        let rec = Recorder::default();
        let released = Arc::new(AtomicBool::new(false));
        let (tx, rx) = tokio::sync::mpsc::channel(UPLOAD_QUEUE);
        let sink = Box::new(rec.clone());
        let ticket: UploadTicket = Box::new(DropFlag(Arc::clone(&released)));
        let writer = std::thread::spawn(move || write_upload(sink, ticket, rx));
        for f in frames {
            for c in bounded_chunks(f, UPLOAD_CHUNK) {
                tx.blocking_send(Chunk::Data(c)).ok().unwrap();
            }
        }
        if end {
            tx.blocking_send(Chunk::End).ok().unwrap();
        }
        drop(tx);
        (writer.join().unwrap(), rec, released)
    }

    #[test]
    fn the_writer_receives_bounded_chunks_and_hashes_exactly_the_body() {
        // One frame far larger than a chunk, then small and odd-sized ones.
        let frames = vec![data(5 * UPLOAD_CHUNK + 3), data(10), data(UPLOAD_CHUNK + 1)];
        let body = frames.concat();
        let (r, rec, released) = run_writer(&frames, true);
        let (uploaded, ticket) = r.unwrap();
        assert_eq!(uploaded.bytes, body.len() as u64);
        assert_eq!(uploaded.sha256, sync_protocol::sha256_hex(&body));
        let writes = rec.0.lock().unwrap();
        assert!(writes.iter().all(|w| w.len() <= UPLOAD_CHUNK));
        assert_eq!(writes.concat(), body, "every byte, in order");
        assert!(
            !released.load(Ordering::SeqCst),
            "the ticket goes to finish_upload"
        );
        drop(ticket);
        assert!(released.load(Ordering::SeqCst));
    }

    #[test]
    fn a_queue_closed_without_the_end_marker_is_an_abort() {
        let (r, _, released) = run_writer(&[data(3 * UPLOAD_CHUNK)], false);
        assert!(matches!(r, Err(None)));
        assert!(released.load(Ordering::SeqCst), "the ticket's cleanup ran");
    }
}
