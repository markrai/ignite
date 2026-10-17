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
}

/// Largest control-plane request body.
pub const MAX_CONTROL_BODY: usize = 64 * 1024;

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
