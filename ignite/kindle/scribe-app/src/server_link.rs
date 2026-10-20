//! D1 paired-server link for Scribe for Kindle (product mode, `--device-dir`).
//!
//! Device configuration — never notebook content: `<device-dir>/server-profile.json`
//! (`{v, server_id, server_name, credential, last_endpoint, paired_ms}`, 0600) and
//! `<device-dir>/device.json` (`{v, client_id}`), both outside every library. One profile
//! serves every library; no notebook carries the credential.
//!
//! A background thread (never the pen/render path) owns:
//! * discovery while the Sync screen is open (with the temporary firewall aperture),
//! * pairing (request → comparison code → poll → one-time credential → profile),
//! * resolution by **ServerId**: the last endpoint is tried first; if it is unreachable or
//!   answers with a different ServerId, a short bounded rediscovery finds the same ServerId at
//!   its new address (DHCP change) and the profile's `last_endpoint` is updated — never a
//!   re-pair,
//! * health: `/control/v1/status` with the credential; `session == null` on the right server
//!   means the credential was revoked (→ "Pair again"), transport failure means Offline,
//! * feeding the existing sync uploader (`OutboundSync::set_link`),
//! * 1H-D2 **Restore from Server** (`LinkCmd::BeginRestore` …): choose a server, pair with it
//!   *provisionally* (the credential stays in memory: never saved, never given to sync), list its
//!   libraries (`GET /v1/libraries`), confirm, then a job thread stops sync, verifies and adopts
//!   the library (`sync_adopt`), publishing the provisional pairing as the device's server
//!   profile in the same commit. Until the commit the original profile, library and sync are
//!   untouched; afterwards the runtime restarts and startup recovery owns completion.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use control_protocol::{
    ClientType, PairingCreate, PairingCreated, PairingPoll, PairingState, PairingStatus, ServerId,
    StatusResponse, CONTROL_BASE, CSRF_HEADER,
};
use serde::{Deserialize, Serialize};

use crate::discovery::{Aperture, DiscoveredServer, MdnsBrowser, ServerTable};
use crate::sync_adopt::{AdoptError, AdoptErrorKind, AdoptionOutcome};
use crate::sync_materialize::DeviceGeometry;
use crate::sync_outbound::SyncConfigV1;
use sync_protocol::LibraryId;

pub const PROFILE_FILE: &str = "server-profile.json";
pub const DEVICE_FILE: &str = "device.json";
const TICK: Duration = Duration::from_millis(250);
const HEALTH_EVERY: Duration = Duration::from_secs(20);
const POLL_EVERY: Duration = Duration::from_secs(2);
const REDISCOVER_FOR: Duration = Duration::from_secs(6);
const REDISCOVER_BACKOFF: Duration = Duration::from_secs(60);

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

fn log(msg: impl AsRef<str>) {
    println!("server_link: {}", msg.as_ref());
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerProfileV1 {
    pub v: u32,
    pub server_id: ServerId,
    pub server_name: String,
    /// Per-device credential (secret): never logged, never in notebook/protocol content.
    pub credential: String,
    pub last_endpoint: String,
    pub paired_ms: u64,
}

impl std::fmt::Debug for ServerProfileV1 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerProfileV1")
            .field("server_id", &self.server_id)
            .field("server_name", &self.server_name)
            .field("credential", &"<redacted>")
            .field("last_endpoint", &self.last_endpoint)
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DeviceFileV1 {
    v: u32,
    client_id: String,
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<(), String> {
    use std::io::Write;
    let tmp = path.with_extension("tmp");
    {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts
            .open(&tmp)
            .map_err(|e| format!("create {}: {e}", tmp.display()))?;
        f.write_all(bytes)
            .and_then(|()| f.sync_all())
            .map_err(|e| format!("write {}: {e}", tmp.display()))?;
    }
    std::fs::rename(&tmp, path).map_err(|e| format!("rename {}: {e}", path.display()))
}

pub fn load_profile(dir: &Path) -> Result<Option<ServerProfileV1>, String> {
    match std::fs::read(dir.join(PROFILE_FILE)) {
        Ok(b) => serde_json::from_slice(&b)
            .map(Some)
            .map_err(|e| format!("server profile: {e}")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("server profile: {e}")),
    }
}

pub fn save_profile(dir: &Path, p: &ServerProfileV1) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    let mut b = serde_json::to_vec_pretty(p).map_err(|e| e.to_string())?;
    b.push(b'\n');
    write_private(&dir.join(PROFILE_FILE), &b)
}

/// Stable, random, non-secret client id (descriptive only on the server).
pub fn client_id(dir: &Path) -> String {
    if let Ok(b) = std::fs::read(dir.join(DEVICE_FILE)) {
        if let Ok(d) = serde_json::from_slice::<DeviceFileV1>(&b) {
            return d.client_id;
        }
    }
    let id = uuid::Uuid::now_v7().to_string();
    let _ = std::fs::create_dir_all(dir);
    let _ = write_private(
        &dir.join(DEVICE_FILE),
        &serde_json::to_vec(&DeviceFileV1 {
            v: 1,
            client_id: id.clone(),
        })
        .unwrap_or_default(),
    );
    id
}

// ---------------------------------------------------------------------------- HTTP client

#[derive(Debug, PartialEq, Eq)]
pub enum CallError {
    /// Could not reach the endpoint (offline / moved).
    Transport,
    Status(u16, String),
    Bad(String),
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(3))
        .timeout_read(Duration::from_secs(6))
        .timeout_write(Duration::from_secs(6))
        // Bounded whole exchange, no stale pooled connections (see sync_outbound::agent).
        .timeout(Duration::from_secs(15))
        .max_idle_connections(0)
        .max_idle_connections_per_host(0)
        .build()
}

fn call<T: for<'de> Deserialize<'de>>(
    endpoint: &str,
    method: &str,
    path: &str,
    bearer: Option<&str>,
    body: Option<String>,
) -> Result<(u16, T), CallError> {
    let url = format!("{}{CONTROL_BASE}{path}", endpoint.trim_end_matches('/'));
    let mut req = agent().request(method, &url).set(CSRF_HEADER, "1");
    if let Some(b) = bearer {
        req = req.set("Authorization", &format!("Bearer {b}"));
    }
    let res = match body {
        Some(b) => req.set("Content-Type", "application/json").send_string(&b),
        None => req.call(),
    };
    let resp = match res {
        Ok(r) => r,
        Err(ureq::Error::Status(410, r)) => r,
        Err(ureq::Error::Status(code, r)) => {
            let text = r.into_string().unwrap_or_default();
            return Err(CallError::Status(code, text.chars().take(200).collect()));
        }
        Err(_) => return Err(CallError::Transport),
    };
    let status = resp.status();
    let text = resp.into_string().map_err(|_| CallError::Transport)?;
    serde_json::from_str(&text)
        .map(|v| (status, v))
        .map_err(|e| CallError::Bad(e.to_string()))
}

pub fn fetch_status(endpoint: &str, bearer: Option<&str>) -> Result<StatusResponse, CallError> {
    call::<StatusResponse>(endpoint, "GET", "/status", bearer, None).map(|(_, v)| v)
}

/// Normalize a manually typed server address to an `http://` base URL.
pub fn normalize_manual_url(s: &str) -> Option<String> {
    let t = s.trim().trim_end_matches('/');
    if t.is_empty() || t.contains(char::is_whitespace) {
        return None;
    }
    if t.starts_with("https://") {
        return None; // D1 is plain HTTP on a trusted LAN
    }
    let t = t.strip_prefix("http://").unwrap_or(t);
    let host = t.split('/').next()?;
    (!host.is_empty()).then(|| format!("http://{host}"))
}

// ---------------------------------------------------------------------------- link state

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LinkPhase {
    NotPaired,
    Pairing { server_name: String, code: String },
    Connected { server_name: String },
    Offline { server_name: String },
    Revoked { server_name: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinkView {
    pub phase: LinkPhase,
    pub searching: bool,
    pub servers: Vec<DiscoveredServer>,
    pub paired_server: Option<ServerId>,
    /// One-line restrained notice (e.g. "Request expired. Try again.").
    pub notice: Option<String>,
    /// Restore from Server in progress (the Sync screen shows this flow instead).
    pub restore: Option<RestorePhase>,
    /// The app must restart now (sync was stopped for a restore; see module docs).
    pub restart: bool,
    /// Bumps on every visible change (the UI repaints only then).
    pub version: u64,
}

/// What a restore needs from the runtime: the installation it replaces.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RestoreContext {
    pub library_root: PathBuf,
    pub sync_dir: PathBuf,
    pub device: DeviceGeometry,
}

/// One library on the selected server. The server has no library names: libraries are labelled
/// by position ("Library 1", …) with their counts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LibraryChoice {
    pub library_id: LibraryId,
    pub label: String,
    pub notebooks: usize,
    pub pages: usize,
    /// This Kindle already holds this library (nothing to restore).
    pub on_this_kindle: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RestorePhase {
    ChooseServer,
    /// Provisional pairing (comparison code); the active profile is untouched.
    Pairing {
        server_name: String,
        code: String,
    },
    Loading {
        server_name: String,
    },
    Libraries {
        server_name: String,
        libraries: Vec<LibraryChoice>,
    },
    Confirm {
        server_name: String,
        library: LibraryChoice,
    },
    /// After confirmation; `cancellable` until the replacement commits.
    Working {
        status: String,
        cancellable: bool,
    },
    Failed {
        message: String,
    },
    /// Published; the app restarts.
    Done,
}

#[derive(Clone, Debug)]
pub enum LinkCmd {
    EnterSyncScreen,
    LeaveSyncScreen,
    Pair(ServerId),
    CancelPairing,
    ManualUrl(String),
    /// Re-check the paired server now (also implied by entering the Sync screen).
    CheckNow,
    /// 1H-D2 Restore from Server (see module docs).
    BeginRestore(RestoreContext),
    RestoreSelectServer(ServerId),
    RestoreSelectLibrary(LibraryId),
    RestoreConfirm,
    /// Leave the flow before confirmation, or cancel the preparation before it commits.
    RestoreCancel,
    /// Acknowledge a failure.
    RestoreDismiss,
}

/// Where sync connections go (the sync uploader, or a test double).
pub trait LinkSink: Send {
    fn set_link(&mut self, link: Option<SyncConfigV1>);
    fn auth_revoked(&mut self) -> bool;
    /// Stop the sync engine for good (a restore replaces its library; the app restarts after).
    /// Test doubles without an engine keep the default.
    fn stop_sync(&mut self) {}
}

/// The running app's sink: the global `OutboundSync`.
pub struct GlobalSync;

impl LinkSink for GlobalSync {
    fn set_link(&mut self, link: Option<SyncConfigV1>) {
        crate::sync_outbound::with_global(|s| s.set_link(link));
    }
    fn auth_revoked(&mut self) -> bool {
        crate::sync_outbound::with_global(|s| s.auth_revoked()).unwrap_or(false)
    }
    fn stop_sync(&mut self) {
        crate::sync_outbound::shutdown_global(Duration::from_millis(1500));
    }
}

/// Discovery source (real mDNS, or a scripted table in tests).
pub trait DiscoverySource: Send {
    fn start(&mut self) -> bool;
    fn table(&mut self) -> ServerTable;
    fn stop(&mut self);
}

pub struct MdnsSource {
    aperture: Arc<Aperture>,
    browser: Option<MdnsBrowser>,
}

impl MdnsSource {
    pub fn new(aperture: Arc<Aperture>) -> Self {
        Self {
            aperture,
            browser: None,
        }
    }
}

impl DiscoverySource for MdnsSource {
    fn start(&mut self) -> bool {
        if self.browser.is_some() {
            return true;
        }
        if !self.aperture.open() {
            log("discovery aperture unavailable (manual URL still works)");
        }
        match MdnsBrowser::start() {
            Ok(b) => {
                self.browser = Some(b);
                true
            }
            Err(e) => {
                log(format!("discovery unavailable: {e}"));
                self.aperture.close();
                false
            }
        }
    }
    fn table(&mut self) -> ServerTable {
        self.browser
            .as_ref()
            .map(MdnsBrowser::table)
            .unwrap_or_default()
    }
    fn stop(&mut self) {
        if let Some(b) = self.browser.take() {
            b.stop();
        }
        self.aperture.close();
    }
}

struct PairingSession {
    server: DiscoveredServer,
    request_id: String,
    secret: String,
    next_poll: Instant,
    /// For a restore: approval yields an in-memory credential, never a saved profile.
    provisional: bool,
}

/// The server a restore reads from, with a credential (provisional, or the active profile's).
struct RestoreTarget {
    server: DiscoveredServer,
    credential: String,
}

enum JobEvent {
    Status(&'static str),
    Committed,
    Finished(Box<Result<AdoptionOutcome, AdoptError>>),
}

struct RestoreJob {
    cancel: Arc<AtomicBool>,
    events: Receiver<JobEvent>,
    committed: bool,
    profile: ServerProfileV1,
}

struct RestoreFlow {
    ctx: RestoreContext,
    target: Option<RestoreTarget>,
    libraries: Vec<LibraryChoice>,
    job: Option<RestoreJob>,
    /// Sync was stopped for the job: the app must restart to resume it.
    sync_stopped: bool,
}

#[cfg(test)]
pub(crate) static RESTORE_FAULTS: Mutex<Vec<(PathBuf, &'static str)>> = Mutex::new(Vec::new());

struct Worker<S: LinkSink, D: DiscoverySource> {
    dir: PathBuf,
    sink: S,
    disc: D,
    view: Arc<Mutex<LinkView>>,
    profile: Option<ServerProfileV1>,
    screen: bool,
    manual: Vec<DiscoveredServer>,
    pairing: Option<PairingSession>,
    next_health: Instant,
    rediscover_until: Option<Instant>,
    last_rediscover: Option<Instant>,
    linked: Option<String>,
    display_name: String,
    restore: Option<RestoreFlow>,
}

impl<S: LinkSink, D: DiscoverySource> Worker<S, D> {
    fn publish(&self, f: impl FnOnce(&mut LinkView)) {
        let mut v = self.view.lock().unwrap();
        let before = v.clone();
        f(&mut v);
        if *v != before {
            v.version = before.version + 1;
        }
    }

    fn servers(&mut self) -> Vec<DiscoveredServer> {
        let mut all = self.disc.table().servers();
        for m in &self.manual {
            if !all.iter().any(|s| s.server_id == m.server_id) {
                all.push(m.clone());
            }
        }
        all
    }

    fn searching(&self) -> bool {
        self.screen || self.rediscover_until.is_some()
    }

    fn set_link(&mut self, endpoint: Option<String>) {
        if self.linked == endpoint {
            return;
        }
        let cfg = match (&endpoint, &self.profile) {
            (Some(ep), Some(p)) => Some(SyncConfigV1 {
                v: 1,
                server_base_url: ep.clone(),
                token: p.credential.clone(),
                debug_drop_first_ack: false,
                debug_crash_point: None,
                server_id: Some(p.server_id),
            }),
            _ => None,
        };
        self.sink.set_link(cfg);
        self.linked = endpoint;
    }

    fn command(&mut self, cmd: LinkCmd) {
        match cmd {
            LinkCmd::EnterSyncScreen => {
                self.screen = true;
                self.next_health = Instant::now();
                self.disc.start();
                self.publish(|v| v.notice = None);
            }
            LinkCmd::LeaveSyncScreen => {
                self.screen = false;
                if self.rediscover_until.is_none() {
                    self.disc.stop();
                }
            }
            LinkCmd::CheckNow => self.next_health = Instant::now(),
            LinkCmd::CancelPairing => {
                if self.pairing.as_ref().is_some_and(|p| p.provisional) {
                    return;
                }
                self.pairing = None;
                let phase = self.idle_phase();
                self.publish(|v| v.phase = phase);
            }
            LinkCmd::ManualUrl(raw) => match normalize_manual_url(&raw) {
                None => self.publish(|v| v.notice = Some("That address is not valid.".into())),
                Some(url) => match fetch_status(&url, None) {
                    Ok(st) => {
                        let s = DiscoveredServer {
                            server_id: st.server_id,
                            name: st.server_name.clone(),
                            endpoint: url,
                            instance: "manual".into(),
                        };
                        self.manual.retain(|m| m.server_id != s.server_id);
                        self.manual.push(s);
                        self.publish(|v| v.notice = None);
                    }
                    Err(_) => self.publish(|v| {
                        v.notice = Some("No Scribe Server answered at that address.".into())
                    }),
                },
            },
            LinkCmd::Pair(id) => {
                if self.restore.is_some() {
                    return;
                }
                let Some(server) = self.servers().into_iter().find(|s| s.server_id == id) else {
                    return;
                };
                self.request_pairing(server, false);
            }
            LinkCmd::BeginRestore(ctx) => self.begin_restore(ctx),
            LinkCmd::RestoreSelectServer(id) => self.restore_select_server(id),
            LinkCmd::RestoreSelectLibrary(id) => self.restore_select_library(id),
            LinkCmd::RestoreConfirm => self.restore_confirm(),
            LinkCmd::RestoreCancel => self.restore_cancel(),
            LinkCmd::RestoreDismiss => self.restore_dismiss(),
        }
    }

    fn request_pairing(&mut self, server: DiscoveredServer, provisional: bool) {
        let body = serde_json::to_string(&PairingCreate {
            client_type: ClientType::Kindle,
            client_id: Some(client_id(&self.dir)),
            display_name: self.display_name.clone(),
        })
        .unwrap_or_default();
        match call::<PairingCreated>(&server.endpoint, "POST", "/pairing", None, Some(body)) {
            Ok((_, c)) => {
                log(format!(
                    "pairing requested server_id={} request_id={}",
                    server.server_id, c.request_id
                ));
                let (name, code) = (server.name.clone(), c.comparison_code.clone());
                self.pairing = Some(PairingSession {
                    server,
                    request_id: c.request_id,
                    secret: c.request_secret,
                    next_poll: Instant::now() + POLL_EVERY,
                    provisional,
                });
                self.publish(|v| {
                    if provisional {
                        v.restore = Some(RestorePhase::Pairing {
                            server_name: name,
                            code,
                        });
                    } else {
                        v.phase = LinkPhase::Pairing {
                            server_name: name,
                            code,
                        };
                    }
                    v.notice = None;
                });
            }
            Err(CallError::Status(409, _)) => {
                self.publish(|v| {
                    v.notice =
                        Some("That server is not set up yet. Open it in a browser first.".into())
                });
            }
            Err(_) => {
                self.publish(|v| v.notice = Some("Could not reach that server. Try again.".into()))
            }
        }
    }

    fn idle_phase(&self) -> LinkPhase {
        match &self.profile {
            None => LinkPhase::NotPaired,
            Some(p) => match &self.linked {
                Some(_) => LinkPhase::Connected {
                    server_name: p.server_name.clone(),
                },
                None => LinkPhase::Offline {
                    server_name: p.server_name.clone(),
                },
            },
        }
    }

    fn poll_pairing(&mut self) {
        let Some(s) = &self.pairing else { return };
        if Instant::now() < s.next_poll {
            return;
        }
        let body = serde_json::to_string(&PairingPoll {
            request_secret: s.secret.clone(),
        })
        .unwrap_or_default();
        let path = format!("/pairing/{}/poll", s.request_id);
        let res = call::<PairingStatus>(&s.server.endpoint, "POST", &path, None, Some(body));
        let s = self.pairing.as_mut().unwrap();
        s.next_poll = Instant::now() + POLL_EVERY;
        match res {
            Ok((_, st))
                if st.state == PairingState::Approved && st.server_id == s.server.server_id =>
            {
                let Some(cred) = st.credential else {
                    self.pairing = None;
                    return;
                };
                if s.provisional {
                    let server = s.server.clone();
                    log(format!(
                        "provisional pairing approved server_id={} request_id={} (not saved)",
                        server.server_id, s.request_id
                    ));
                    self.pairing = None;
                    self.restore_target_ready(RestoreTarget {
                        server,
                        credential: cred,
                    });
                    return;
                }
                let p = ServerProfileV1 {
                    v: 1,
                    server_id: s.server.server_id,
                    server_name: s.server.name.clone(),
                    credential: cred,
                    last_endpoint: s.server.endpoint.clone(),
                    paired_ms: now_ms(),
                };
                let request_id = s.request_id.clone();
                self.pairing = None;
                match save_profile(&self.dir, &p) {
                    Ok(()) => {
                        log(format!(
                            "paired server_id={} request_id={request_id}",
                            p.server_id
                        ));
                        self.profile = Some(p);
                        self.linked = None;
                        self.next_health = Instant::now();
                        let name = self.profile.as_ref().unwrap().server_name.clone();
                        let id = self.profile.as_ref().unwrap().server_id;
                        self.publish(|v| {
                            v.phase = LinkPhase::Offline { server_name: name };
                            v.paired_server = Some(id);
                        });
                    }
                    Err(e) => {
                        log(format!("could not store pairing: {e}"));
                        self.publish(|v| {
                            v.notice = Some("Could not save the pairing on this Kindle.".into())
                        });
                    }
                }
            }
            Ok((_, st))
                if matches!(
                    st.state,
                    PairingState::Denied | PairingState::Expired | PairingState::Delivered
                ) =>
            {
                let msg = match st.state {
                    PairingState::Denied => "The request was declined.",
                    PairingState::Expired => "The request expired. Try again.",
                    _ => "That request can no longer be used. Try again.",
                };
                let provisional = s.provisional;
                self.pairing = None;
                let phase = self.idle_phase();
                self.publish(|v| {
                    if provisional {
                        v.restore = Some(RestorePhase::ChooseServer);
                    } else {
                        v.phase = phase;
                    }
                    v.notice = Some(msg.into());
                });
            }
            _ => {} // pending, or a transient failure: keep polling until it expires
        }
    }

    fn health(&mut self) {
        if self.restore.as_ref().is_some_and(|f| f.sync_stopped) {
            return;
        }
        let Some(p) = self.profile.clone() else {
            return;
        };
        if self.pairing.is_some() || Instant::now() < self.next_health {
            return;
        }
        self.next_health = Instant::now() + HEALTH_EVERY;
        if self.sink.auth_revoked() {
            self.mark_revoked(&p);
            return;
        }
        // Prefer the ServerId's currently advertised endpoint when discovery knows it.
        let mut candidates = Vec::new();
        if let Some(s) = self
            .servers()
            .into_iter()
            .find(|s| s.server_id == p.server_id)
        {
            candidates.push(s.endpoint);
        }
        if !candidates.contains(&p.last_endpoint) {
            candidates.push(p.last_endpoint.clone());
        }
        for ep in candidates {
            match fetch_status(&ep, Some(&p.credential)) {
                Ok(st) if st.server_id == p.server_id => {
                    if st.session.is_none() {
                        self.mark_revoked(&p);
                        return;
                    }
                    if ep != p.last_endpoint {
                        let mut np = p.clone();
                        np.last_endpoint = ep.clone();
                        if save_profile(&self.dir, &np).is_ok() {
                            log(format!(
                                "server_id={} endpoint changed (pairing kept)",
                                np.server_id
                            ));
                            self.profile = Some(np);
                        }
                    }
                    self.stop_rediscovery();
                    self.set_link(Some(ep));
                    let name = p.server_name.clone();
                    self.publish(|v| v.phase = LinkPhase::Connected { server_name: name });
                    return;
                }
                _ => continue, // unreachable, or a DIFFERENT server now owns that address
            }
        }
        self.set_link(None);
        let name = p.server_name.clone();
        self.publish(|v| v.phase = LinkPhase::Offline { server_name: name });
        self.start_rediscovery();
    }

    fn mark_revoked(&mut self, p: &ServerProfileV1) {
        self.set_link(None);
        let name = p.server_name.clone();
        self.publish(|v| v.phase = LinkPhase::Revoked { server_name: name });
    }

    fn start_rediscovery(&mut self) {
        if self.rediscover_until.is_some()
            || self
                .last_rediscover
                .is_some_and(|t| t.elapsed() < REDISCOVER_BACKOFF)
        {
            return;
        }
        self.last_rediscover = Some(Instant::now());
        self.rediscover_until = Some(Instant::now() + REDISCOVER_FOR);
        self.disc.start();
        // Re-check as soon as the paired ServerId shows up.
        self.next_health = Instant::now() + Duration::from_secs(2);
    }

    fn stop_rediscovery(&mut self) {
        if self.rediscover_until.take().is_some() && !self.screen {
            self.disc.stop();
        }
    }

    // ------------------------------------------------------------------ restore (1H-D2)

    fn restore_phase(&self) -> Option<RestorePhase> {
        self.view.lock().unwrap().restore.clone()
    }

    fn set_restore(&self, phase: Option<RestorePhase>, notice: Option<String>) {
        self.publish(|v| {
            v.restore = phase;
            v.notice = notice;
        });
    }

    fn begin_restore(&mut self, ctx: RestoreContext) {
        if self.restore.is_some() {
            return;
        }
        if self.pairing.as_ref().is_some_and(|p| !p.provisional) {
            self.pairing = None; // an ordinary pairing attempt yields to the restore flow
        }
        self.restore = Some(RestoreFlow {
            ctx,
            target: None,
            libraries: Vec::new(),
            job: None,
            sync_stopped: false,
        });
        self.screen = true;
        self.disc.start();
        let phase = self.idle_phase();
        self.publish(|v| {
            v.phase = phase;
            v.restore = Some(RestorePhase::ChooseServer);
            v.notice = None;
        });
        log("restore: choose server");
    }

    fn restore_select_server(&mut self, id: ServerId) {
        if self.restore_phase() != Some(RestorePhase::ChooseServer) || self.pairing.is_some() {
            return;
        }
        let Some(server) = self.servers().into_iter().find(|s| s.server_id == id) else {
            return;
        };
        let revoked = matches!(self.view.lock().unwrap().phase, LinkPhase::Revoked { .. });
        match self.profile.clone() {
            // Already paired with that server: its credential reads the library list.
            Some(p) if p.server_id == id && !revoked => self.restore_target_ready(RestoreTarget {
                server,
                credential: p.credential,
            }),
            _ => self.request_pairing(server, true),
        }
    }

    /// Credential in hand: list the server's libraries.
    fn restore_target_ready(&mut self, target: RestoreTarget) {
        let Some(sync_dir) = self.restore.as_ref().map(|f| f.ctx.sync_dir.clone()) else {
            return;
        };
        let name = target.server.name.clone();
        self.set_restore(
            Some(RestorePhase::Loading {
                server_name: name.clone(),
            }),
            None,
        );
        let listed =
            crate::sync_adopt::list_server_libraries(&target.server.endpoint, &target.credential);
        // The library this Kindle syncs now (any server): offered, but not restorable.
        let local = crate::sync_outbound::read_json::<crate::sync_outbound::SyncStateV1>(
            &sync_dir.join(crate::sync_outbound::STATE_FILE),
        )
        .ok()
        .map(|s| s.library_id);
        match listed {
            Ok(libs) if !libs.is_empty() => {
                let libraries: Vec<LibraryChoice> = libs
                    .iter()
                    .enumerate()
                    .map(|(i, l)| LibraryChoice {
                        library_id: l.library_id,
                        label: format!("Library {}", i + 1),
                        notebooks: l.notebook_count,
                        pages: l.page_count,
                        on_this_kindle: local == Some(l.library_id),
                    })
                    .collect();
                if let Some(f) = self.restore.as_mut() {
                    f.libraries = libraries.clone();
                    f.target = Some(target);
                }
                self.set_restore(
                    Some(RestorePhase::Libraries {
                        server_name: name,
                        libraries,
                    }),
                    None,
                );
            }
            Ok(_) => self.set_restore(
                Some(RestorePhase::ChooseServer),
                Some("That server has no libraries to restore.".into()),
            ),
            Err(e) => {
                log(format!("restore: library list failed: {e}"));
                self.set_restore(Some(RestorePhase::ChooseServer), Some(friendly(&e, false)));
            }
        }
    }

    fn restore_select_library(&mut self, id: LibraryId) {
        let Some(RestorePhase::Libraries { server_name, .. }) = self.restore_phase() else {
            return;
        };
        let Some(library) = self
            .restore
            .as_ref()
            .and_then(|f| f.libraries.iter().find(|l| l.library_id == id).cloned())
        else {
            return;
        };
        self.set_restore(
            Some(RestorePhase::Confirm {
                server_name,
                library,
            }),
            None,
        );
    }

    fn restore_confirm(&mut self) {
        let Some(RestorePhase::Confirm { library, .. }) = self.restore_phase() else {
            return; // repeated taps: only the Confirm state starts a job
        };
        if library.on_this_kindle {
            return;
        }
        let Some(flow) = self.restore.as_mut() else {
            return;
        };
        let (Some(target), None) = (flow.target.as_ref(), flow.job.as_ref()) else {
            return;
        };
        let profile = ServerProfileV1 {
            v: 1,
            server_id: target.server.server_id,
            server_name: target.server.name.clone(),
            credential: target.credential.clone(),
            last_endpoint: target.server.endpoint.clone(),
            paired_ms: now_ms(),
        };
        let cancel = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel();
        let ctx = flow.ctx.clone();
        let device_dir = self.dir.clone();
        let (job_profile, job_cancel) = (profile.clone(), Arc::clone(&cancel));
        let library_id = library.library_id;
        flow.job = Some(RestoreJob {
            cancel,
            events: rx,
            committed: false,
            profile,
        });
        flow.sync_stopped = true;
        // Sync stops before anything is downloaded: the old library never syncs again in
        // this run, and the new pairing is never fed to it.
        self.set_link(None);
        self.sink.stop_sync();
        self.set_restore(
            Some(RestorePhase::Working {
                status: "Downloading library...".into(),
                cancellable: true,
            }),
            None,
        );
        log(format!("restore: confirmed library={library_id}"));
        let spawned = std::thread::Builder::new()
            .name("restore-job".into())
            .spawn(move || {
                run_restore_job(
                    &ctx,
                    &device_dir,
                    &job_profile,
                    library_id,
                    &job_cancel,
                    &tx,
                )
            });
        if let Err(e) = spawned {
            log(format!("restore: could not start: {e}"));
            if let Some(f) = self.restore.as_mut() {
                f.job = None;
            }
            self.set_restore(
                Some(RestorePhase::Failed {
                    message: "The restore could not start. Nothing was changed.".into(),
                }),
                None,
            );
        }
    }

    fn restore_cancel(&mut self) {
        let Some(flow) = self.restore.as_ref() else {
            return;
        };
        if let Some(job) = &flow.job {
            if !job.committed {
                job.cancel.store(true, Ordering::SeqCst);
                self.set_restore(
                    Some(RestorePhase::Working {
                        status: "Cancelling...".into(),
                        cancellable: false,
                    }),
                    None,
                );
            }
            return; // after the commit, completion is not cancellable
        }
        if matches!(self.restore_phase(), Some(RestorePhase::Failed { .. })) {
            return self.restore_dismiss();
        }
        // Before confirmation: drop the flow and its provisional credential; nothing changed.
        if self.pairing.as_ref().is_some_and(|p| p.provisional) {
            self.pairing = None;
        }
        self.restore = None;
        self.next_health = Instant::now();
        let phase = self.idle_phase();
        self.publish(|v| {
            v.restore = None;
            v.phase = phase;
            v.notice = None;
        });
        log("restore: cancelled before confirmation");
    }

    fn restore_dismiss(&mut self) {
        if !matches!(self.restore_phase(), Some(RestorePhase::Failed { .. })) {
            return;
        }
        if self.restore.as_ref().is_some_and(|f| f.sync_stopped) {
            self.publish(|v| v.restart = true); // sync resumes only by restarting
            return;
        }
        self.restore = None;
        let phase = self.idle_phase();
        self.publish(|v| {
            v.restore = None;
            v.phase = phase;
        });
    }

    fn poll_restore_job(&mut self) {
        let Some(job) = self.restore.as_mut().and_then(|f| f.job.as_mut()) else {
            return;
        };
        let mut finished = None;
        let mut status = None;
        while let Ok(ev) = job.events.try_recv() {
            match ev {
                JobEvent::Status(s) => status = Some(s),
                JobEvent::Committed => {
                    job.committed = true;
                    status = Some("Finishing...");
                }
                JobEvent::Finished(r) => finished = Some(*r),
            }
        }
        let committed = job.committed;
        let profile = job.profile.clone();
        let cancelling = job.cancel.load(Ordering::SeqCst);
        if let Some(r) = finished {
            if let Some(f) = self.restore.as_mut() {
                f.job = None;
            }
            match r {
                Ok(AdoptionOutcome::Adopted(rep)) => {
                    log(format!(
                        "restore: adopted library={} server_id={} replica={}",
                        rep.library_id, rep.server_id, rep.replica_id
                    ));
                    let id = profile.server_id;
                    self.profile = Some(profile);
                    self.publish(|v| {
                        v.restore = Some(RestorePhase::Done);
                        v.paired_server = Some(id);
                        v.restart = true;
                    });
                }
                Ok(AdoptionOutcome::AlreadyAdopted { .. }) => self.set_restore(
                    Some(RestorePhase::Failed {
                        message: "This library is already on this Kindle.".into(),
                    }),
                    None,
                ),
                Err(e) if e.kind == AdoptErrorKind::Cancelled => {
                    log("restore: cancelled; restarting");
                    self.publish(|v| {
                        v.restore = Some(RestorePhase::Working {
                            status: "Cancelled. Restarting...".into(),
                            cancellable: false,
                        });
                        v.restart = true;
                    });
                }
                Err(e) if committed => {
                    // Committed: startup recovery completes it.
                    log(format!("restore: interrupted after commit: {e}"));
                    self.publish(|v| {
                        v.restore = Some(RestorePhase::Working {
                            status: "Finishing after restart...".into(),
                            cancellable: false,
                        });
                        v.restart = true;
                    });
                }
                Err(e) => {
                    log(format!("restore: failed: {e}"));
                    self.set_restore(
                        Some(RestorePhase::Failed {
                            message: friendly(&e, true),
                        }),
                        None,
                    );
                }
            }
            return;
        }
        if let Some(st) = status {
            self.publish(|v| {
                v.restore = Some(RestorePhase::Working {
                    status: st.into(),
                    cancellable: !committed && !cancelling,
                })
            });
        }
    }

    fn tick(&mut self) {
        if let Some(until) = self.rediscover_until {
            let found = self
                .profile
                .as_ref()
                .is_some_and(|p| self.disc.table().find(p.server_id).is_some());
            if found {
                self.next_health = Instant::now();
            }
            if Instant::now() >= until {
                self.stop_rediscovery();
            }
        }
        self.poll_pairing();
        self.poll_restore_job();
        self.health();
        let servers = self.servers();
        let searching = self.searching();
        self.publish(|v| {
            v.servers = servers;
            v.searching = searching;
        });
    }
}

pub struct ServerLink {
    tx: Sender<Option<LinkCmd>>,
    view: Arc<Mutex<LinkView>>,
    thread: Option<JoinHandle<()>>,
}

impl ServerLink {
    /// Start the manager for `device_dir`. `display_name` is how this device appears in the
    /// admin's approval list.
    pub fn start<S: LinkSink + 'static, D: DiscoverySource + 'static>(
        device_dir: &Path,
        sink: S,
        disc: D,
        display_name: &str,
    ) -> Result<Self, String> {
        let profile = load_profile(device_dir)?;
        let view = Arc::new(Mutex::new(LinkView {
            phase: match &profile {
                None => LinkPhase::NotPaired,
                Some(p) => LinkPhase::Offline {
                    server_name: p.server_name.clone(),
                },
            },
            searching: false,
            servers: Vec::new(),
            paired_server: profile.as_ref().map(|p| p.server_id),
            notice: None,
            restore: None,
            restart: false,
            version: 1,
        }));
        let (tx, rx): (Sender<Option<LinkCmd>>, Receiver<Option<LinkCmd>>) = mpsc::channel();
        let mut w = Worker {
            dir: device_dir.to_path_buf(),
            sink,
            disc,
            view: Arc::clone(&view),
            profile,
            screen: false,
            manual: Vec::new(),
            pairing: None,
            next_health: Instant::now(),
            rediscover_until: None,
            last_rediscover: None,
            linked: None,
            display_name: display_name.to_string(),
            restore: None,
        };
        if let Some(p) = &w.profile {
            log(format!(
                "profile server_id={} last_endpoint={}",
                p.server_id, p.last_endpoint
            ));
        }
        let thread = std::thread::Builder::new()
            .name("server-link".into())
            .spawn(move || {
                loop {
                    match rx.recv_timeout(TICK) {
                        Ok(Some(cmd)) => w.command(cmd),
                        Ok(None) | Err(RecvTimeoutError::Disconnected) => break,
                        Err(RecvTimeoutError::Timeout) => {}
                    }
                    w.tick();
                }
                w.disc.stop();
            })
            .map_err(|e| e.to_string())?;
        Ok(Self {
            tx,
            view,
            thread: Some(thread),
        })
    }

    pub fn view(&self) -> LinkView {
        self.view.lock().unwrap().clone()
    }

    pub fn send(&self, cmd: LinkCmd) {
        let _ = self.tx.send(Some(cmd));
    }

    /// Stop the thread; discovery (and the firewall aperture) is closed on the way out.
    pub fn shutdown(mut self) {
        let _ = self.tx.send(None);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// A short user-facing message for a refused restore (no IDs, hashes or internal terms).
fn friendly(e: &AdoptError, after_confirm: bool) -> String {
    let unchanged = if after_confirm {
        " Nothing was changed."
    } else {
        ""
    };
    match e.kind {
        AdoptErrorKind::Unreachable => format!("Could not reach the server.{unchanged}"),
        AdoptErrorKind::Unauthorized => {
            format!("The server did not accept this Kindle. Pair again.{unchanged}")
        }
        AdoptErrorKind::ServerMismatch
        | AdoptErrorKind::LibraryMismatch
        | AdoptErrorKind::GenesisMismatch
        | AdoptErrorKind::InvalidSnapshot => {
            format!("The server's copy could not be verified.{unchanged}")
        }
        AdoptErrorKind::Unsupported => {
            format!("This Kindle cannot open that library.{unchanged}")
        }
        AdoptErrorKind::InsufficientStorage => {
            format!("Not enough free space on this Kindle.{unchanged}")
        }
        AdoptErrorKind::Busy => "Another restore is in progress.".into(),
        AdoptErrorKind::Cancelled => "Restore cancelled.".into(),
        AdoptErrorKind::Local => format!("The restore could not be completed.{unchanged}"),
    }
}

/// The restore job (its own thread): download + verify, then adopt with the pairing.
fn run_restore_job(
    ctx: &RestoreContext,
    device_dir: &Path,
    profile: &ServerProfileV1,
    library_id: LibraryId,
    cancel: &AtomicBool,
    tx: &Sender<JobEvent>,
) {
    use crate::sync_adopt::{adopt_library, fetch_adoption_source, AdoptOptions, AdoptProfile};
    let target = crate::sync_adopt::AdoptionTarget {
        server_id: profile.server_id,
        library_id,
    };
    let res = fetch_adoption_source(&profile.last_endpoint, &profile.credential, target).and_then(
        |src| {
            let _ = tx.send(JobEvent::Status("Preparing library..."));
            let hook = |p: &str| -> Result<(), String> {
                match p {
                    "after_verify" => {
                        let _ = tx.send(JobEvent::Status("Saving library..."));
                    }
                    // The plan is durable when this point is reached: committed.
                    "after_plan" => {
                        let _ = tx.send(JobEvent::Committed);
                    }
                    _ => {}
                }
                #[cfg(test)]
                if RESTORE_FAULTS
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|(d, at)| *d == ctx.sync_dir && *at == p)
                {
                    return Err(format!("injected crash at {p}"));
                }
                Ok(())
            };
            adopt_library(
                &ctx.library_root,
                &ctx.sync_dir,
                &src,
                &AdoptOptions {
                    hook: &hook,
                    cancel: Some(cancel),
                    profile: Some(AdoptProfile {
                        device_dir,
                        profile,
                    }),
                    ..AdoptOptions::new(ctx.device)
                },
            )
        },
    );
    let _ = tx.send(JobEvent::Finished(Box::new(res)));
}

static GLOBAL: Mutex<Option<ServerLink>> = Mutex::new(None);

/// Install the process-wide link manager (product mode).
pub fn install_global(link: ServerLink) {
    *GLOBAL.lock().unwrap() = Some(link);
}

pub fn with_global<R>(f: impl FnOnce(&ServerLink) -> R) -> Option<R> {
    GLOBAL.lock().unwrap().as_ref().map(f)
}

/// Stop the manager (closes discovery and the firewall aperture). Idempotent.
pub fn shutdown_global() {
    let link = GLOBAL.lock().unwrap().take();
    if let Some(l) = link {
        l.shutdown();
    }
}

#[cfg(test)]
#[path = "server_link_tests.rs"]
mod tests;
