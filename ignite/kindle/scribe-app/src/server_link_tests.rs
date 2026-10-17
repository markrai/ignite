//! D1 server-link tests against a REAL in-process Scribe Server (control plane + sync API),
//! with scripted discovery (no multicast needed) and a recording sync sink.

use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use control_protocol::advertisement_txt;
use scribe_server::config::{Config, DiscoverySection, ServerSection};
use scribe_server::Running;

use super::*;
use crate::discovery::{DiscoveryEvent, ServerTable};

fn tmp(label: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "scribe-link-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

struct Server {
    state: PathBuf,
    running: Option<Running>,
    admin: String,
}

fn config(state: &std::path::Path) -> Config {
    Config {
        server: ServerSection {
            listen: "127.0.0.1:0".parse().unwrap(),
            state_dir: state.to_path_buf(),
            web_root: None,
            name: "Test Server B".into(),
        },
        discovery: DiscoverySection { advertise: false },
    }
}

impl Server {
    fn start(state: &std::path::Path) -> Self {
        let r = scribe_server::serve(
            &config(state),
            scribe_server::system_clock(),
            Arc::new(|_: &str| {}),
        )
        .unwrap();
        Self {
            state: state.to_path_buf(),
            running: Some(r),
            admin: String::new(),
        }
    }
    fn fresh(label: &str) -> Self {
        let state = tmp(label);
        let mut s = Self::start(&state);
        let (_, _, store) =
            scribe_server::prepare(&config(&state), scribe_server::system_clock()).unwrap();
        let code = store.issue_setup_code().unwrap().unwrap();
        let resp = ureq::post(&format!("{}/control/v1/claim", s.url()))
            .set("X-Scribe-Request", "1")
            .send_string(&format!("{{\"setup_code\":\"{code}\"}}"))
            .unwrap();
        let sc = resp.header("set-cookie").unwrap().to_string();
        s.admin = sc.split(';').next().unwrap().to_string();
        s
    }
    fn url(&self) -> String {
        self.running.as_ref().unwrap().handle.base_url()
    }
    fn port(&self) -> u16 {
        self.running.as_ref().unwrap().handle.addr().port()
    }
    fn id(&self) -> ServerId {
        self.running.as_ref().unwrap().plane.server_id
    }
    fn admin_post(&self, path: &str) -> u16 {
        match ureq::post(&format!("{}{path}", self.url()))
            .set("Cookie", &self.admin)
            .set("X-Scribe-Request", "1")
            .call()
        {
            Ok(r) => r.status(),
            Err(ureq::Error::Status(c, _)) => c,
            Err(e) => panic!("{e}"),
        }
    }
    fn pending_ids(&self) -> Vec<(String, String)> {
        let v = ureq::get(&format!("{}/control/v1/admin/pairing", self.url()))
            .set("Cookie", &self.admin)
            .call()
            .unwrap()
            .into_string()
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&v).unwrap();
        v["requests"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| {
                (
                    r["request_id"].as_str().unwrap().into(),
                    r["comparison_code"].as_str().unwrap().into(),
                )
            })
            .collect()
    }
    fn stop(&mut self) {
        if let Some(r) = self.running.take() {
            r.handle.shutdown();
        }
    }
    fn restart(&mut self) {
        self.stop();
        // Serve the SAME state dir again (never via a temporary `Server`, whose Drop would
        // delete the state directory — Linux allows deleting open files).
        self.running = Some(
            scribe_server::serve(
                &config(&self.state),
                scribe_server::system_clock(),
                Arc::new(|_: &str| {}),
            )
            .unwrap(),
        );
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop();
        let _ = std::fs::remove_dir_all(&self.state);
    }
}

#[derive(Clone, Default)]
struct Scripted(Arc<Mutex<ServerTable>>);

impl Scripted {
    fn advertise(&self, id: ServerId, name: &str, port: u16) {
        self.0.lock().unwrap().apply(DiscoveryEvent::Resolved {
            instance: format!("{name}-{id}"),
            txt: advertisement_txt(id, name),
            addrs: vec!["127.0.0.1".parse::<IpAddr>().unwrap()],
            port,
        });
    }
}

impl DiscoverySource for Scripted {
    fn start(&mut self) -> bool {
        true
    }
    fn table(&mut self) -> ServerTable {
        self.0.lock().unwrap().clone()
    }
    fn stop(&mut self) {}
}

/// `(endpoint, credential)` or withdrawn.
type Link = Option<(String, String)>;

#[derive(Clone, Default)]
struct Sink {
    links: Arc<Mutex<Vec<Link>>>,
    revoked: Arc<Mutex<bool>>,
}

impl LinkSink for Sink {
    fn set_link(&mut self, link: Option<SyncConfigV1>) {
        self.links
            .lock()
            .unwrap()
            .push(link.map(|c| (c.server_base_url, c.token)));
    }
    fn auth_revoked(&mut self) -> bool {
        *self.revoked.lock().unwrap()
    }
}

impl Sink {
    fn last(&self) -> Option<(String, String)> {
        self.links.lock().unwrap().last().cloned().flatten()
    }
}

fn wait<F: FnMut() -> bool>(what: &str, mut f: F) {
    let t = Instant::now();
    while !f() {
        assert!(
            t.elapsed() < Duration::from_secs(15),
            "timeout waiting for {what}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn pair(srv: &Server, link: &ServerLink) {
    link.send(LinkCmd::EnterSyncScreen);
    wait("server listed", || {
        link.view().servers.iter().any(|s| s.server_id == srv.id())
    });
    link.send(LinkCmd::Pair(srv.id()));
    wait("pairing code", || {
        matches!(link.view().phase, LinkPhase::Pairing { .. })
    });
    let LinkPhase::Pairing { code, server_name } = link.view().phase else {
        unreachable!()
    };
    assert_eq!(server_name, "Test Server B");
    // The admin sees the SAME comparison code, then approves.
    let pending = srv.pending_ids();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].1, code);
    assert_eq!(
        srv.admin_post(&format!(
            "/control/v1/admin/pairing/{}/approve",
            pending[0].0
        )),
        200
    );
    wait("connected", || {
        matches!(link.view().phase, LinkPhase::Connected { .. })
    });
}

fn v1_status(url: &str, cred: &str) -> u16 {
    match ureq::get(&format!("{url}/v1/libraries"))
        .set("Authorization", &format!("Bearer {cred}"))
        .call()
    {
        Ok(r) => r.status(),
        Err(ureq::Error::Status(c, _)) => c,
        Err(e) => panic!("{e}"),
    }
}

#[test]
fn pairs_through_discovery_and_feeds_sync_with_its_own_credential() {
    let srv = Server::fresh("pair");
    let disc = Scripted::default();
    disc.advertise(srv.id(), "Test Server B", srv.port());
    let dir = tmp("dev-pair");
    let sink = Sink::default();
    let link = ServerLink::start(&dir, sink.clone(), disc.clone(), "Ignite").unwrap();
    assert_eq!(link.view().phase, LinkPhase::NotPaired);
    pair(&srv, &link);
    let (ep, cred) = sink.last().expect("sync link set");
    assert_eq!(ep, srv.url());
    assert_eq!(v1_status(&ep, &cred), 200);
    let p = load_profile(&dir).unwrap().unwrap();
    assert_eq!(p.server_id, srv.id());
    assert_eq!(p.credential, cred);
    assert!(
        !format!("{p:?}").contains(&cred),
        "profile Debug is redacted"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let m = std::fs::metadata(dir.join(PROFILE_FILE))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(m & 0o077, 0, "profile must be owner-only");
    }
    link.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn restart_of_client_and_server_keeps_pairing_and_follows_the_server_id_to_a_new_endpoint() {
    let mut srv = Server::fresh("restart");
    let disc = Scripted::default();
    disc.advertise(srv.id(), "Test Server B", srv.port());
    let dir = tmp("dev-restart");
    let sink = Sink::default();
    let link = ServerLink::start(&dir, sink.clone(), disc.clone(), "Ignite").unwrap();
    pair(&srv, &link);
    let (_, cred) = sink.last().unwrap();
    link.shutdown();

    // Server restarts on a NEW port (like a DHCP/endpoint change); same ServerId.
    let old_url = srv.url();
    srv.restart();
    assert_ne!(srv.url(), old_url);
    *disc.0.lock().unwrap() = ServerTable::default();
    disc.advertise(srv.id(), "Test Server B", srv.port());

    // Client restarts: loads the same profile, finds the ServerId at its new endpoint.
    let sink2 = Sink::default();
    let link = ServerLink::start(&dir, sink2.clone(), disc.clone(), "Ignite").unwrap();
    link.send(LinkCmd::CheckNow);
    wait("reconnected", || {
        matches!(link.view().phase, LinkPhase::Connected { .. })
    });
    let (ep, cred2) = sink2.last().unwrap();
    assert_eq!(ep, srv.url(), "new endpoint adopted automatically");
    assert_eq!(cred2, cred, "same credential: no re-pair");
    assert_eq!(
        load_profile(&dir).unwrap().unwrap().last_endpoint,
        srv.url()
    );
    assert_eq!(v1_status(&ep, &cred2), 200);
    link.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_different_server_at_the_old_address_is_never_trusted() {
    let srv_b = Server::fresh("srv_b");
    let other = Server::fresh("other");
    let dir = tmp("dev-imposter");
    // Profile says Test Server B was at `other`'s address (IP reused by another Scribe Server).
    let disc = Scripted::default();
    disc.advertise(srv_b.id(), "Test Server B", srv_b.port());
    let sink = Sink::default();
    let link = ServerLink::start(&dir, sink.clone(), disc.clone(), "Ignite").unwrap();
    pair(&srv_b, &link);
    link.shutdown();
    let mut p = load_profile(&dir).unwrap().unwrap();
    p.last_endpoint = other.url();
    save_profile(&dir, &p).unwrap();
    // Discovery can't see Test Server B now: the other server at the old address must not be used.
    *disc.0.lock().unwrap() = ServerTable::default();
    disc.advertise(other.id(), "Test Server B", other.port()); // same NAME, other id
    let sink2 = Sink::default();
    let link = ServerLink::start(&dir, sink2.clone(), disc.clone(), "Ignite").unwrap();
    link.send(LinkCmd::CheckNow);
    wait("offline", || {
        matches!(link.view().phase, LinkPhase::Offline { .. })
    });
    std::thread::sleep(Duration::from_millis(600));
    assert!(
        sink2.last().is_none(),
        "never linked to a server with another ServerId"
    );
    // Test Server B reappears (by ServerId) → connected again, profile endpoint corrected.
    disc.advertise(srv_b.id(), "Test Server B", srv_b.port());
    link.send(LinkCmd::CheckNow);
    wait("connected to srv_b", || {
        matches!(link.view().phase, LinkPhase::Connected { .. })
    });
    assert_eq!(sink2.last().unwrap().0, srv_b.url());
    link.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn revocation_shows_pair_again_and_denied_requests_do_not_pair() {
    let srv = Server::fresh("revoke");
    let disc = Scripted::default();
    disc.advertise(srv.id(), "Test Server B", srv.port());
    let dir = tmp("dev-revoke");
    let sink = Sink::default();
    let link = ServerLink::start(&dir, sink.clone(), disc.clone(), "Ignite").unwrap();
    pair(&srv, &link);
    let device_id = load_profile(&dir)
        .unwrap()
        .unwrap()
        .credential
        .split_once('.')
        .unwrap()
        .0
        .to_string();
    assert_eq!(
        srv.admin_post(&format!("/control/v1/admin/devices/{device_id}/revoke")),
        200
    );
    link.send(LinkCmd::CheckNow);
    wait("revoked", || {
        matches!(link.view().phase, LinkPhase::Revoked { .. })
    });
    assert!(sink.last().is_none(), "sync link withdrawn on revocation");
    // Pair again: a denied request leaves it unpaired-for-that-request with a notice.
    link.send(LinkCmd::Pair(srv.id()));
    wait("pairing code", || {
        matches!(link.view().phase, LinkPhase::Pairing { .. })
    });
    let id = srv.pending_ids()[0].0.clone();
    assert_eq!(
        srv.admin_post(&format!("/control/v1/admin/pairing/{id}/deny")),
        200
    );
    wait("declined notice", || {
        link.view().notice.as_deref() == Some("The request was declined.")
    });
    // Approve a fresh request → connected with a NEW credential.
    pair(&srv, &link);
    let (_, cred) = sink.last().unwrap();
    assert!(!cred.starts_with(&device_id));
    link.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn manual_url_fallback_lists_the_server() {
    let srv = Server::fresh("manual");
    let dir = tmp("dev-manual");
    let link = ServerLink::start(&dir, Sink::default(), Scripted::default(), "Ignite").unwrap();
    link.send(LinkCmd::EnterSyncScreen);
    link.send(LinkCmd::ManualUrl("not a url".into()));
    wait("notice", || link.view().notice.is_some());
    link.send(LinkCmd::ManualUrl(
        srv.url().trim_start_matches("http://").to_string(),
    ));
    wait("listed", || {
        link.view().servers.iter().any(|s| s.server_id == srv.id())
    });
    assert_eq!(normalize_manual_url("https://testhost"), None);
    assert_eq!(
        normalize_manual_url("test-server-b.local/"),
        Some("http://test-server-b.local".into())
    );
    link.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}
