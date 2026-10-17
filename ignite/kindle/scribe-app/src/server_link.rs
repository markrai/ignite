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
//! * feeding the existing sync uploader (`OutboundSync::set_link`).

use std::path::{Path, PathBuf};
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
use crate::sync_outbound::SyncConfigV1;

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
    /// Bumps on every visible change (the UI repaints only then).
    pub version: u64,
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
}

/// Where sync connections go (the sync uploader, or a test double).
pub trait LinkSink: Send {
    fn set_link(&mut self, link: Option<SyncConfigV1>);
    fn auth_revoked(&mut self) -> bool;
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
}

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
                let Some(server) = self.servers().into_iter().find(|s| s.server_id == id) else {
                    return;
                };
                let body = serde_json::to_string(&PairingCreate {
                    client_type: ClientType::Kindle,
                    client_id: Some(client_id(&self.dir)),
                    display_name: self.display_name.clone(),
                })
                .unwrap_or_default();
                match call::<PairingCreated>(&server.endpoint, "POST", "/pairing", None, Some(body))
                {
                    Ok((_, c)) => {
                        log(format!(
                            "pairing requested server_id={} request_id={}",
                            server.server_id, c.request_id
                        ));
                        let phase = LinkPhase::Pairing {
                            server_name: server.name.clone(),
                            code: c.comparison_code.clone(),
                        };
                        self.pairing = Some(PairingSession {
                            server,
                            request_id: c.request_id,
                            secret: c.request_secret,
                            next_poll: Instant::now() + POLL_EVERY,
                        });
                        self.publish(|v| {
                            v.phase = phase;
                            v.notice = None;
                        });
                    }
                    Err(CallError::Status(409, _)) => {
                        self.publish(|v| {
                            v.notice = Some(
                                "That server is not set up yet. Open it in a browser first.".into(),
                            )
                        });
                    }
                    Err(_) => self.publish(|v| {
                        v.notice = Some("Could not reach that server. Try again.".into())
                    }),
                }
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
                self.pairing = None;
                let phase = self.idle_phase();
                self.publish(|v| {
                    v.phase = phase;
                    v.notice = Some(msg.into());
                });
            }
            _ => {} // pending, or a transient failure: keep polling until it expires
        }
    }

    fn health(&mut self) {
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
