//! The D1 control plane over HTTP (`/control/v1/*`) and product-mode `/v1` authorization.
//!
//! Authentication:
//! * `Authorization: Bearer <credential>` — Kindle / native clients (per-device credential).
//! * `Cookie: scribe_session=<credential>` — browsers (HttpOnly, SameSite=Strict, Path=/; no
//!   `Secure` because D1 is plain HTTP on a trusted LAN — documented, not Internet-safe).
//!
//! CSRF: every control-plane mutation, and every cookie-authenticated `/v1` mutation, must carry
//! `X-Scribe-Request: 1` (a custom header a cross-origin page cannot send without a CORS
//! preflight, which this server never grants) and, when the browser sends `Origin`, its host
//! must equal `Host`. Bearer-authenticated requests carry no ambient credential, so they need
//! no CSRF marker for `/v1`.
//!
//! Logging: events carry ids and counts only — never credentials, cookies, setup codes,
//! request secrets or Authorization headers.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};

use control_protocol::{
    ClientType, PairingCreate, PairingCreated, PairingPoll, PairingState, PairingStatus, Role,
    ServerId, ServerStatus, SessionInfo, StatusResponse, CONTROL_VERSION, CSRF_HEADER,
    PAIRING_TTL_S, SESSION_COOKIE,
};
use scribe_sync_server::{
    ControlPlane, ControlReply, DownloadSlot, RequestInfo, Store, UploadSlot, UploadTicket,
    UploadedBody,
};
use serde::Serialize;

use crate::control::{ControlError, ControlStore, Principal};
use crate::export::ExportJobs;
use crate::restore::RestoreJobs;

pub type EventSink = Arc<dyn Fn(&str) + Send + Sync>;

/// Default sink: one line per event on stderr (journald on the Linux deployment).
pub fn stderr_sink() -> EventSink {
    Arc::new(|line| eprintln!("{line}"))
}

/// Session cookie lifetime (the browser keeps it across restarts; revocation is server-side).
pub const SESSION_MAX_AGE_S: u64 = 400 * 24 * 3600;
pub const MAX_PENDING: u64 = 16;

/// Sliding-window counters (per key) for unauthenticated endpoints.
pub struct RateLimiter {
    windows: Mutex<HashMap<String, Vec<u64>>>,
}

impl RateLimiter {
    pub fn new() -> Self {
        Self {
            windows: Mutex::new(HashMap::new()),
        }
    }

    /// Record one attempt under `key`; false if more than `max` in the last `window_ms`.
    pub fn allow(&self, key: &str, max: usize, window_ms: u64, now: u64) -> bool {
        let mut w = self.windows.lock().unwrap();
        if w.len() > 4096 {
            w.retain(|_, v| v.last().is_some_and(|t| now.saturating_sub(*t) < window_ms));
        }
        let v = w.entry(key.to_string()).or_default();
        v.retain(|t| now.saturating_sub(*t) < window_ms);
        if v.len() >= max {
            return false;
        }
        v.push(now);
        true
    }
}

impl Default for RateLimiter {
    fn default() -> Self {
        Self::new()
    }
}

pub struct Plane {
    pub store: ControlStore,
    pub server_id: ServerId,
    pub server_name: String,
    pub started_ms: u64,
    pub limiter: RateLimiter,
    pub events: EventSink,
    /// D1.1 network boundary (None = accept everything; tests of other behavior only).
    pub lan: Option<std::sync::Arc<crate::netpolicy::LanPolicy>>,
    /// Slice 1F: prepared backup-restore jobs (`/control/v1/admin/restore/*`).
    pub restore: Arc<RestoreJobs>,
    /// Slice 1G-D1: prepared backup exports (`/control/v1/admin/backup/*`).
    pub exports: Arc<ExportJobs>,
}

pub(crate) fn json<T: Serialize>(status: u16, v: &T) -> ControlReply {
    ControlReply {
        status,
        content_type: "application/json",
        body: serde_json::to_vec(v).expect("serializable"),
        headers: Vec::new(),
    }
}

pub(crate) fn err(status: u16, code: &str, message: &str) -> ControlReply {
    json(
        status,
        &serde_json::json!({ "error": code, "message": message }),
    )
}

fn session_cookie(value: &str) -> String {
    format!(
        "{SESSION_COOKIE}={value}; Path=/; HttpOnly; SameSite=Strict; Max-Age={SESSION_MAX_AGE_S}"
    )
}

fn clear_cookie() -> String {
    format!("{SESSION_COOKIE}=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0")
}

pub(crate) fn cookie_value(cookie: Option<&str>) -> Option<&str> {
    cookie?
        .split(';')
        .map(str::trim)
        .find_map(|kv| kv.strip_prefix(SESSION_COOKIE)?.strip_prefix('='))
        .filter(|v| !v.is_empty())
}

fn bearer(auth: Option<&str>) -> Option<&str> {
    auth?.strip_prefix("Bearer ").map(str::trim)
}

/// Any `Authorization` header at all (restore routes accept the session cookie only).
pub(crate) fn has_authorization(req: &RequestInfo) -> bool {
    req.authorization.is_some()
}

/// `Origin` (if present) must name the same host as `Host`, and the CSRF marker is required.
pub fn csrf_ok(req: &RequestInfo) -> bool {
    if req.scribe_request.as_deref() != Some("1") {
        return false;
    }
    match (req.origin.as_deref(), req.host.as_deref()) {
        (None, _) => true,
        (Some(origin), Some(host)) => {
            let o = origin
                .strip_prefix("http://")
                .or_else(|| origin.strip_prefix("https://"))
                .unwrap_or("");
            o.eq_ignore_ascii_case(host)
        }
        (Some(_), None) => false,
    }
}

fn clamp(s: &str, max: usize) -> String {
    control_protocol::clamp_label(&s.chars().take(max).collect::<String>())
}

fn peer_ip(req: &RequestInfo) -> String {
    req.peer.map_or_else(
        || "unknown".into(),
        |p| match p.ip() {
            IpAddr::V4(v) => v.to_string(),
            IpAddr::V6(v) => v.to_string(),
        },
    )
}

impl Plane {
    pub(crate) fn event(&self, what: &str, fields: &[(&str, &str)]) {
        let mut line = format!("scribe-server event={what}");
        for (k, v) in fields {
            line.push_str(&format!(" {k}={v}"));
        }
        (self.events)(&line);
    }

    /// Who is making this request (bearer first, then the session cookie).
    pub fn principal(&self, req: &RequestInfo) -> Option<(Principal, bool)> {
        if let Some(b) = bearer(req.authorization.as_deref()) {
            return self
                .store
                .authenticate(b)
                .ok()
                .flatten()
                .map(|p| (p, false));
        }
        let c = cookie_value(req.cookie.as_deref())?;
        self.store.authenticate(c).ok().flatten().map(|p| (p, true))
    }

    fn status(&self, req: &RequestInfo) -> ControlReply {
        let session = self.principal(req).map(|(p, _)| SessionInfo {
            device_id: p.device_id,
            role: p.role,
            display_name: p.display_name,
        });
        json(
            200,
            &StatusResponse {
                v: CONTROL_VERSION,
                mode: "product".into(),
                server_id: self.server_id,
                server_name: self.server_name.clone(),
                version: env!("CARGO_PKG_VERSION").into(),
                claimed: self.store.is_claimed().unwrap_or(false),
                session,
            },
        )
    }

    fn claim(&self, req: &RequestInfo, body: &[u8]) -> ControlReply {
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Claim {
            setup_code: String,
            #[serde(default)]
            display_name: Option<String>,
        }
        let now = self.store.now();
        let ip = peer_ip(req);
        if !self.limiter.allow(&format!("claim:{ip}"), 5, 60_000, now)
            || !self.limiter.allow("claim:*", 30, 60_000, now)
        {
            self.event("claim_rate_limited", &[("peer", &ip)]);
            return err(
                429,
                "rate_limited",
                "Too many attempts. Wait a minute and try again.",
            );
        }
        let Ok(c) = serde_json::from_slice::<Claim>(body) else {
            return err(400, "bad_request", "expected {setup_code}");
        };
        let name = clamp(c.display_name.as_deref().unwrap_or("Admin browser"), 48);
        match self.store.claim(&c.setup_code, &name) {
            Ok(issued) => {
                self.event("server_claimed", &[("device_id", &issued.device_id)]);
                let mut r = json(
                    200,
                    &SessionInfo {
                        device_id: issued.device_id.clone(),
                        role: Role::Admin,
                        display_name: name,
                    },
                );
                r.headers
                    .push(("Set-Cookie", session_cookie(&issued.credential.encode())));
                r
            }
            Err(ControlError::Conflict(_)) => {
                err(409, "already_claimed", "This server is already set up.")
            }
            Err(ControlError::Invalid(_)) => {
                self.event("claim_refused", &[("peer", &ip)]);
                err(403, "wrong_setup_code", "That setup code is not valid.")
            }
            Err(e) => self.storage(e),
        }
    }

    fn create_pairing(&self, req: &RequestInfo, body: &[u8]) -> ControlReply {
        let now = self.store.now();
        let ip = peer_ip(req);
        if !self.limiter.allow(&format!("pair:{ip}"), 6, 60_000, now)
            || !self.limiter.allow("pair:*", 60, 60_000, now)
        {
            self.event("pairing_rate_limited", &[("peer", &ip)]);
            return err(
                429,
                "rate_limited",
                "Too many requests. Wait a minute and try again.",
            );
        }
        let Ok(p) = serde_json::from_slice::<PairingCreate>(body) else {
            return err(400, "bad_request", "expected {client_type, display_name}");
        };
        let name = clamp(&p.display_name, 48);
        if name.is_empty() {
            return err(400, "bad_request", "display_name required");
        }
        let client_id = p.client_id.as_deref().map(|c| clamp(c, 64));
        match self.store.create_pairing(
            p.client_type,
            client_id.as_deref(),
            &name,
            Some(&ip),
            MAX_PENDING,
        ) {
            Ok((request_id, secret, code)) => {
                self.event(
                    "pairing_requested",
                    &[
                        ("request_id", &request_id),
                        ("client_type", ctype(p.client_type)),
                        ("peer", &ip),
                    ],
                );
                json(
                    201,
                    &PairingCreated {
                        request_id,
                        request_secret: secret,
                        comparison_code: code,
                        expires_in_s: PAIRING_TTL_S,
                    },
                )
            }
            Err(ControlError::Conflict("unclaimed")) => {
                err(409, "unclaimed", "This server has not been set up yet.")
            }
            Err(ControlError::Conflict(_)) => err(
                429,
                "too_many_pending",
                "Too many pending requests. Try again shortly.",
            ),
            Err(e) => self.storage(e),
        }
    }

    fn poll_pairing(&self, id: &str, body: &[u8]) -> ControlReply {
        let Ok(p) = serde_json::from_slice::<PairingPoll>(body) else {
            return err(400, "bad_request", "expected {request_secret}");
        };
        match self.store.poll_pairing(id, &p.request_secret) {
            Ok((state, ct, issued)) => {
                let mut status = PairingStatus {
                    state,
                    device_id: None,
                    credential: None,
                    server_id: self.server_id,
                };
                let mut cookie = None;
                if let Some(issued) = issued {
                    self.event(
                        "device_paired",
                        &[
                            ("device_id", &issued.device_id),
                            ("client_type", ctype(ct)),
                            ("request_id", id),
                        ],
                    );
                    status.device_id = Some(issued.device_id.clone());
                    match ct {
                        // Delivered once, in the response body, to the requester only.
                        ClientType::Kindle => status.credential = Some(issued.credential.encode()),
                        // Browsers never see the credential: it becomes an HttpOnly cookie.
                        ClientType::Browser => {
                            cookie = Some(session_cookie(&issued.credential.encode()))
                        }
                    }
                }
                let mut r = json(200, &status);
                if let Some(c) = cookie {
                    r.headers.push(("Set-Cookie", c));
                }
                if state == PairingState::Expired {
                    r.status = 410;
                }
                r
            }
            Err(ControlError::NotFound) => err(404, "not_found", "No such pairing request."),
            Err(e) => self.storage(e),
        }
    }

    fn storage(&self, e: ControlError) -> ControlReply {
        self.event("control_storage_error", &[]);
        let _ = e;
        err(500, "internal", "control storage error")
    }

    fn admin(&self, p: &Principal, req: &RequestInfo, rest: &[&str], body: &[u8]) -> ControlReply {
        let _ = body;
        let mutating = req.method != "GET" && req.method != "HEAD";
        if mutating && !csrf_ok(req) {
            return err(
                403,
                "csrf",
                "missing X-Scribe-Request or cross-origin request",
            );
        }
        match (rest, req.method.as_str()) {
            (["pairing"], "GET") => match self.store.pending() {
                Ok(v) => json(200, &serde_json::json!({ "requests": v })),
                Err(e) => self.storage(e),
            },
            (["pairing", id, action @ ("approve" | "deny")], "POST") => {
                match self.store.decide_pairing(id, *action == "approve") {
                    Ok((ct, _)) => {
                        self.event(
                            if *action == "approve" {
                                "pairing_approved"
                            } else {
                                "pairing_denied"
                            },
                            &[
                                ("request_id", id),
                                ("client_type", ctype(ct)),
                                ("by", &p.device_id),
                            ],
                        );
                        json(200, &serde_json::json!({ "ok": true }))
                    }
                    Err(ControlError::NotFound) => err(404, "not_found", "No such request."),
                    Err(ControlError::Conflict("expired")) => {
                        err(409, "expired", "That request expired.")
                    }
                    Err(ControlError::Conflict(_)) => {
                        err(409, "not_pending", "That request was already decided.")
                    }
                    Err(e) => self.storage(e),
                }
            }
            (["devices"], "GET") => match self.store.devices(&p.device_id) {
                Ok(v) => json(200, &serde_json::json!({ "devices": v })),
                Err(e) => self.storage(e),
            },
            (["devices", id, "revoke"], "POST") => {
                if *id == p.device_id {
                    return err(
                        409,
                        "cannot_revoke_self",
                        "You cannot revoke the browser you are using.",
                    );
                }
                match self.store.revoke(id) {
                    Ok(()) => {
                        self.event("device_revoked", &[("device_id", id), ("by", &p.device_id)]);
                        json(200, &serde_json::json!({ "ok": true }))
                    }
                    Err(ControlError::NotFound) => err(404, "not_found", "No such device."),
                    Err(ControlError::Conflict(_)) => {
                        err(409, "already_revoked", "Already revoked.")
                    }
                    Err(e) => self.storage(e),
                }
            }
            _ => err(404, "not_found", "unknown admin route"),
        }
    }

    fn server_status(&self, ledger: &Store) -> ControlReply {
        let libraries = ledger.list_libraries().map(|l| l.len() as u64).unwrap_or(0);
        let (devices, pending) = self.store.counts().unwrap_or((0, 0));
        json(
            200,
            &ServerStatus {
                server_id: self.server_id,
                server_name: self.server_name.clone(),
                version: env!("CARGO_PKG_VERSION").into(),
                started_ms: self.started_ms,
                library_count: libraries,
                device_count: devices,
                pending_requests: pending,
            },
        )
    }
}

fn ctype(t: ClientType) -> &'static str {
    match t {
        ClientType::Kindle => "kindle",
        ClientType::Browser => "browser",
    }
}

impl ControlPlane for Plane {
    fn handle(&self, ledger: &Store, req: &RequestInfo, body: &[u8]) -> ControlReply {
        let path = req.path().to_string();
        let Some(rest) = path.strip_prefix("/control/v1/") else {
            return err(404, "not_found", "unknown control route");
        };
        let segs: Vec<&str> = rest.split('/').filter(|s| !s.is_empty()).collect();
        let method = req.method.as_str();
        let mutating = method != "GET" && method != "HEAD";
        // Every unauthenticated mutation is CSRF-checked too (claim/pairing from browsers).
        if mutating && !csrf_ok(req) {
            return err(
                403,
                "csrf",
                &format!("missing {CSRF_HEADER} or cross-origin request"),
            );
        }
        match (segs.as_slice(), method) {
            (["status"], "GET") => self.status(req),
            (["claim"], "POST") => self.claim(req, body),
            (["pairing"], "POST") => self.create_pairing(req, body),
            (["pairing", id, "poll"], "POST") => self.poll_pairing(id, body),
            (["session", "logout"], "POST") => {
                let mut r = json(200, &serde_json::json!({ "ok": true }));
                r.headers.push(("Set-Cookie", clear_cookie()));
                r
            }
            // Restore routes have their own, stricter caller rule (see `restore`).
            (["admin", "restore", rest @ ..], _) => self.restore_route(ledger, req, rest, body),
            // Backup exports share the restore caller rule (see `export`).
            (["admin", "backup", rest @ ..], _) => self.export_route(req, rest),
            (["admin", rest @ ..], _) => match self.principal(req) {
                Some((p, _)) if p.role == Role::Admin => {
                    if rest == ["server"] && method == "GET" {
                        self.server_status(ledger)
                    } else {
                        self.admin(&p, req, rest, body)
                    }
                }
                Some(_) => err(403, "forbidden", "administrator access required"),
                None => err(401, "unauthorized", "not signed in"),
            },
            _ => err(404, "not_found", "unknown control route"),
        }
    }

    fn accept(&self, local: std::net::SocketAddr, peer: std::net::SocketAddr) -> bool {
        let Some(lan) = &self.lan else { return true };
        if lan.allows(local, peer) {
            return true;
        }
        // Restrained: at most one line per peer per minute.
        let key = format!("refused:{}", peer.ip());
        if self.limiter.allow(&key, 1, 60_000, self.store.now()) {
            self.event(
                "connection_refused_non_lan",
                &[
                    ("local", &local.ip().to_string()),
                    ("peer", &peer.ip().to_string()),
                ],
            );
        }
        false
    }

    fn upload_route(&self, req: &RequestInfo) -> bool {
        Self::is_restore_upload(req)
    }

    fn begin_upload(&self, _ledger: &Store, req: &RequestInfo) -> Result<UploadSlot, ControlReply> {
        self.restore_begin_upload(req)
    }

    fn finish_upload(
        &self,
        ledger: &Store,
        req: &RequestInfo,
        ticket: UploadTicket,
        body: UploadedBody,
    ) -> ControlReply {
        self.restore_finish_upload(ledger, req, ticket, body)
    }

    fn download_route(&self, req: &RequestInfo) -> bool {
        Self::is_export_download(req)
    }

    fn begin_download(
        &self,
        _ledger: &Store,
        req: &RequestInfo,
    ) -> Result<DownloadSlot, ControlReply> {
        self.export_begin_download(req)
    }

    fn authorize_sync(&self, req: &RequestInfo) -> Result<(), (u16, &'static str, &'static str)> {
        match self.principal(req) {
            None => Err((
                401,
                "unauthorized",
                "missing, invalid or revoked credential",
            )),
            Some((_, via_cookie)) => {
                let mutating = req.method != "GET" && req.method != "HEAD";
                if via_cookie && mutating && !csrf_ok(req) {
                    return Err((
                        403,
                        "csrf",
                        "missing X-Scribe-Request or cross-origin request",
                    ));
                }
                Ok(())
            }
        }
    }
}
