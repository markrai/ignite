//! Slice 1H-D2: Restore from Server through the real UI and pairing coordinator.
//!
//! A disposable Kindle installation syncs with product server A (saved profile, real
//! `OutboundSync` behind the link sink). Server B holds A's library restored from a portable
//! backup. Every step is a pen tap resolved by `UiState::on_tap` against the drawn Sync screen,
//! mapped to the server-link command the runtime sends; "restart" is what the runtime does at
//! startup: adoption recovery, `OutboundSync::open`, a new `ServerLink`.

use std::net::IpAddr;
use std::sync::Mutex;

use control_protocol::advertisement_txt;

use super::*;
use crate::app_ui::{UiCommand, UiState};
use crate::discovery::{DiscoveryEvent, ServerTable};
use crate::home::AppView;
use crate::server_link::{
    load_profile, save_profile, DiscoverySource, LibraryChoice, LinkCmd, LinkPhase, LinkSink,
    LinkView, RestoreContext, RestorePhase, ServerLink, RESTORE_FAULTS,
};
use crate::sync_ui::{SyncHit, SyncLayout};
use crate::ui_gfx::{panel_geo, UiPoint};

#[derive(Clone, Default)]
struct Net(Arc<Mutex<ServerTable>>);

impl Net {
    fn advertise(&self, p: &Product) {
        let port = p.running.as_ref().unwrap().handle.addr().port();
        self.0.lock().unwrap().apply(DiscoveryEvent::Resolved {
            instance: format!("srv-{}", p.id()),
            txt: advertisement_txt(p.id(), "Adoption Test"),
            addrs: vec!["127.0.0.1".parse::<IpAddr>().unwrap()],
            port,
        });
    }
}

impl DiscoverySource for Net {
    fn start(&mut self) -> bool {
        true
    }
    fn table(&mut self) -> ServerTable {
        self.0.lock().unwrap().clone()
    }
    fn stop(&mut self) {}
}

/// The runtime's sink: the installation's one `OutboundSync`.
#[derive(Clone, Default)]
struct Engine(Arc<Mutex<Option<OutboundSync>>>);

impl LinkSink for Engine {
    fn set_link(&mut self, link: Option<SyncConfigV1>) {
        if let Some(s) = self.0.lock().unwrap().as_ref() {
            s.set_link(link);
        }
    }
    fn auth_revoked(&mut self) -> bool {
        self.0
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|s| s.auth_revoked())
    }
    fn stop_sync(&mut self) {
        if let Some(s) = self.0.lock().unwrap().take() {
            s.shutdown(Duration::from_secs(5));
        }
    }
}

impl Product {
    /// Approve every pending pairing request as the administrator.
    fn approve_pending(&self) -> usize {
        let (_, b) = self.admin_call("GET", "/control/v1/admin/pairing", None);
        let ids: Vec<String> = json_of(&b)["requests"]
            .as_array()
            .map(|a| {
                a.iter()
                    .map(|r| r["request_id"].as_str().unwrap().to_string())
                    .collect()
            })
            .unwrap_or_default();
        for id in &ids {
            let (s, _) = self.admin_call(
                "POST",
                &format!("/control/v1/admin/pairing/{id}/approve"),
                None,
            );
            assert_eq!(s, 200);
        }
        ids.len()
    }
    fn stop(&mut self) {
        if let Some(r) = self.running.take() {
            r.handle.shutdown();
        }
    }
    fn profile(&self) -> ServerProfileV1 {
        ServerProfileV1 {
            v: 1,
            server_id: self.id(),
            server_name: "Adoption Test".into(),
            credential: self.credential.clone(),
            last_endpoint: self.url(),
            paired_ms: 1,
        }
    }
}

/// The Kindle app: installation + sync engine + server link, booted like the runtime.
struct Device {
    k: Kindle,
    engine: Engine,
    link: Option<ServerLink>,
    net: Net,
}

impl Device {
    /// An installation whose saved pairing is `paired`.
    fn new(label: &str, paired: &Product, net: &Net) -> Self {
        let k = Kindle::new(label);
        save_profile(&k.device_dir, &paired.profile()).unwrap();
        let mut d = Self {
            k,
            engine: Engine::default(),
            link: None,
            net: net.clone(),
        };
        d.boot();
        d
    }

    /// Runtime startup order: adoption recovery, sync engine, server link.
    fn boot(&mut self) -> AdoptionRecovery {
        let r = recover_interrupted_adoption(&self.k.lib, &self.k.sync, Some(&self.k.device_dir))
            .unwrap();
        *self.engine.0.lock().unwrap() = Some(self.k.open());
        self.link = Some(
            ServerLink::start(
                &self.k.device_dir,
                self.engine.clone(),
                self.net.clone(),
                "Ignite",
            )
            .unwrap(),
        );
        self.send(LinkCmd::EnterSyncScreen);
        r
    }

    /// The runtime's restart: everything stops, then boots again.
    fn restart(&mut self) -> AdoptionRecovery {
        if let Some(l) = self.link.take() {
            l.shutdown();
        }
        self.engine.stop_sync();
        self.boot()
    }

    fn send(&self, cmd: LinkCmd) {
        self.link.as_ref().unwrap().send(cmd);
    }
    fn view(&self) -> LinkView {
        self.link.as_ref().unwrap().view()
    }
    fn wait_view(&self, what: &str, f: impl Fn(&LinkView) -> bool) -> LinkView {
        let t0 = Instant::now();
        loop {
            let v = self.view();
            if f(&v) {
                return v;
            }
            assert!(
                t0.elapsed() < Duration::from_secs(60),
                "timed out waiting for {what}: {:?} notice={:?}",
                v.restore,
                v.notice
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    fn phase(&self) -> Option<RestorePhase> {
        self.view().restore
    }
    fn with_sync<R>(&self, f: impl FnOnce(&OutboundSync) -> R) -> R {
        f(self
            .engine
            .0
            .lock()
            .unwrap()
            .as_ref()
            .expect("sync running"))
    }
    fn linked_to(&self, p: &Product) {
        let url = p.url();
        let t0 = Instant::now();
        while self.with_sync(|s| s.link_endpoint()) != Some(url.clone()) {
            assert!(t0.elapsed() < Duration::from_secs(30), "never linked");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    fn ctx(&self) -> RestoreContext {
        RestoreContext {
            library_root: self.k.lib.clone(),
            sync_dir: self.k.sync.clone(),
            device: device(),
        }
    }
    /// A pen tap on whatever draws `hit` on the current Sync screen, resolved by the UI state
    /// and sent exactly as the runtime does.
    fn tap(&self, hit: SyncHit) {
        let g = panel_geo();
        let v = self.view();
        let layout = SyncLayout::new(&g, &v);
        let (x, y) = (0..g.yres)
            .step_by(10)
            .flat_map(|y| (0..g.xres).step_by(10).map(move |x| (x, y)))
            .find(|&(x, y)| layout.hit_view(&v, x, y) == Some(hit))
            .unwrap_or_else(|| panic!("{hit:?} is not on screen in {:?}", v.restore));
        let mut ui = UiState::launch_home(0, 1);
        ui.view = AppView::Sync;
        ui.sync_view = Some(v);
        match ui.on_tap(&g, UiPoint::new(x, y), &[], "", None) {
            UiCommand::SyncRestoreBegin => self.send(LinkCmd::BeginRestore(self.ctx())),
            cmd => self.send(cmd.restore_link_cmd().expect("a restore step")),
        }
    }
    fn profile(&self) -> Option<ServerProfileV1> {
        load_profile(&self.k.device_dir).unwrap()
    }
    fn write(&self, y: f32, captured: bool) -> Stroke {
        let (nb, page) = (nb_of(&self.k, 0), page_of(&self.k, 0, 0));
        match captured {
            true => self.with_sync(|s| {
                let st = write_stroke(Some(s), &self.k.lib, &nb, &page, y);
                s.wait_journal_durable(&AtomicBool::new(false)).unwrap();
                st
            }),
            false => write_stroke(None, &self.k.lib, &nb, &page, y),
        }
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        if let Some(l) = self.link.take() {
            l.shutdown();
        }
        self.engine.stop_sync();
    }
}

/// A (the device's server, its library synced with content), B (A's backup restored).
fn servers(label: &str) -> (Product, Product, Net) {
    let a = Product::start(&format!("{label}-a"));
    let b = Product::start(&format!("{label}-b"));
    let net = Net::default();
    net.advertise(&a);
    net.advertise(&b);
    (a, b, net)
}

/// The device's library on A with remote content, then A's backup restored onto B.
fn synced_and_restored(a: &Product, b: &Product, dev: &Device) -> LibraryId {
    dev.linked_to(a);
    let lib_a = dev.with_sync(|s| {
        wait("genesis upload", || s.state().genesis_uploaded);
        s.state().library_id
    });
    let mut remote = author_on(lib_a);
    let envs: Vec<SyncEnvelopeV1> = content_ops(&a.snapshot(lib_a))
        .into_iter()
        .map(|o| remote.env(o))
        .collect();
    a.submit(lib_a, &envs);
    dev.with_sync(|s| converge(s, a));
    b.restore(&a.export()).1
}

/// Tap through to the Confirm screen for B's `lib` (pairing provisionally with B).
fn to_confirm(dev: &Device, b: &Product, lib: LibraryId) -> LibraryChoice {
    dev.tap(SyncHit::Restore);
    dev.wait_view("server choice", |v| {
        v.restore == Some(RestorePhase::ChooseServer) && v.servers.len() >= 2
    });
    dev.tap(SyncHit::RestoreServer(b.id()));
    dev.wait_view("provisional pairing", |v| {
        matches!(v.restore, Some(RestorePhase::Pairing { .. }))
    });
    assert_eq!(b.approve_pending(), 1);
    dev.wait_view("libraries", |v| {
        matches!(v.restore, Some(RestorePhase::Libraries { .. }))
    });
    dev.tap(SyncHit::RestoreLibrary(lib));
    let v = dev.wait_view("confirmation", |v| {
        matches!(v.restore, Some(RestorePhase::Confirm { .. }))
    });
    match v.restore {
        Some(RestorePhase::Confirm { library, .. }) => library,
        _ => unreachable!(),
    }
}

#[test]
fn provisional_pairing_lists_libraries_and_cancel_leaves_a_in_use() {
    let (a, b, net) = servers("flow-cancel");
    let dev = Device::new("flow-cancel", &a, &net);
    let lib_b = synced_and_restored(&a, &b, &dev);
    let profile_bytes = fs::read(dev.k.device_dir.join(crate::server_link::PROFILE_FILE)).unwrap();
    let library_before = tree(&dev.k.lib);

    dev.tap(SyncHit::Restore);
    dev.wait_view("server choice", |v| {
        v.restore == Some(RestorePhase::ChooseServer)
    });
    // Ordinary pairing commands do nothing during the flow.
    dev.send(LinkCmd::Pair(b.id()));
    dev.wait_view("servers", |v| v.servers.len() == 2);
    dev.tap(SyncHit::RestoreServer(b.id()));
    let v = dev.wait_view("provisional pairing", |v| {
        matches!(v.restore, Some(RestorePhase::Pairing { .. }))
    });
    assert!(
        matches!(v.phase, LinkPhase::Connected { .. }),
        "{:?}",
        v.phase
    );
    assert_eq!(v.paired_server, Some(a.id()));
    dev.send(LinkCmd::CancelPairing); // the ordinary cancel cannot drop the provisional one
    assert_eq!(b.approve_pending(), 1);

    // B's libraries from the lightweight list; the active pairing is still A's, byte for byte.
    let v = dev.wait_view("libraries", |v| {
        matches!(v.restore, Some(RestorePhase::Libraries { .. }))
    });
    let Some(RestorePhase::Libraries { libraries, .. }) = v.restore else {
        unreachable!()
    };
    let listed = json_of(&b.sync_call("GET", "/v1/libraries", None).1);
    let row = &listed["libraries"][0];
    assert_eq!(
        libraries,
        vec![LibraryChoice {
            library_id: lib_b,
            label: "Library 1".into(),
            notebooks: row["notebook_count"].as_u64().unwrap() as usize,
            pages: row["page_count"].as_u64().unwrap() as usize,
            on_this_kindle: false,
        }]
    );
    assert_eq!((libraries[0].notebooks, libraries[0].pages), (2, 3));
    assert_eq!(
        fs::read(dev.k.device_dir.join(crate::server_link::PROFILE_FILE)).unwrap(),
        profile_bytes
    );
    assert_eq!(v.paired_server, Some(a.id()));
    dev.linked_to(&a);

    dev.tap(SyncHit::RestoreLibrary(lib_b));
    dev.wait_view("confirmation", |v| {
        matches!(v.restore, Some(RestorePhase::Confirm { .. }))
    });
    dev.tap(SyncHit::RestoreCancel);
    let v = dev.wait_view("flow closed", |v| v.restore.is_none());
    assert!(!v.restart);

    // A remains fully usable: same pairing, same library, sync uploads to A.
    assert_eq!(
        fs::read(dev.k.device_dir.join(crate::server_link::PROFILE_FILE)).unwrap(),
        profile_bytes
    );
    assert_eq!(tree(&dev.k.lib), library_before);
    let lib_a = dev.with_sync(|s| s.state().library_id);
    let before = a.head(lib_a);
    dev.write(6000.0, true);
    dev.with_sync(|s| converge(s, &a));
    assert_eq!(a.head(lib_a), before + 1);
    assert!(!adoption_pending(&dev.k.sync));
}

#[test]
fn confirmed_restore_moves_the_kindle_to_b_and_survives_restarts() {
    let (mut a, b, net) = servers("flow-adopt");
    let mut dev = Device::new("flow-adopt", &a, &net);
    let lib_b = synced_and_restored(&a, &b, &dev);
    let b_libraries = b.library_ids();
    let b_genesis = b.snapshot(lib_b);

    // Local-only work: queued (A is gone) and uncaptured.
    let lib_a = dev.with_sync(|s| s.state().library_id);
    a.stop();
    let queued = dev.write(7000.0, true);
    let uncaptured = dev.write(7500.0, false);
    let old = dev.with_sync(|s| s.state());
    assert_eq!(dev.with_sync(|s| s.pending()), 1);

    let choice = to_confirm(&dev, &b, lib_b);
    assert!(!choice.on_this_kindle);
    dev.tap(SyncHit::RestoreConfirm);
    // Repeated confirmation starts nothing more.
    dev.send(LinkCmd::RestoreConfirm);
    dev.send(LinkCmd::RestoreConfirm);
    let v = dev.wait_view("restored", |v| v.restore == Some(RestorePhase::Done));
    assert!(v.restart, "the runtime restarts after a restore");
    assert_eq!(v.paired_server, Some(b.id()));
    dev.send(LinkCmd::RestoreConfirm);

    // Library, sync epoch and pairing moved to B together.
    let p = dev.profile().unwrap();
    assert_eq!((p.server_id, p.last_endpoint.clone()), (b.id(), b.url()));
    assert_ne!(p.credential, a.credential);
    let st = dev.k.state();
    assert_eq!((st.library_id, st.server_id), (lib_b, Some(b.id())));
    assert_ne!(st.replica_id, old.replica_id);
    assert_eq!(dev.k.snapshot(), b_genesis);
    let pts = all_points(&dev.k.snapshot());
    assert!(!pts.contains(&local_points(&queued)) && !pts.contains(&local_points(&uncaptured)));
    // The old installation is kept, retired, exactly once.
    let (ret_lib, ret_sync) = retired_dirs(&dev.k);
    assert_eq!(read_identity(&ret_lib).unwrap().unwrap().library_id, lib_a);
    let retired: SyncStateV1 = read_json(&ret_sync.join(STATE_FILE)).unwrap();
    assert_eq!(
        (retired.library_id, retired.replica_id),
        (lib_a, old.replica_id)
    );
    assert_eq!(
        crate::sync_outbound::parse_journal(&fs::read(ret_sync.join(OUTBOX_FILE)).unwrap())
            .unwrap()
            .committed
            .len(),
        1
    );

    // Restart: the new pairing links to B; first ordinary upload is client_seq 1.
    assert_eq!(dev.restart(), AdoptionRecovery::None);
    assert_eq!(dev.view().paired_server, Some(b.id()));
    dev.linked_to(&b);
    dev.with_sync(|s| {
        assert_eq!((s.fault(), s.pending(), s.next_client_seq()), (None, 0, 1));
        converge(s, &b);
    });
    let first = dev.write(4000.0, true);
    dev.with_sync(|s| converge(s, &b));
    let tail = b.tail(lib_b, 0);
    assert_eq!(tail.len(), 1);
    assert_eq!(
        (tail[0].0, tail[0].1.client_seq, tail[0].1.replica_id),
        (1, 1, st.replica_id)
    );
    assert!(all_points(&b.snapshot(lib_b)).contains(&local_points(&first)));
    assert_eq!(b.library_ids(), b_libraries, "no duplicate genesis on B");

    // Another restart: the pairing persists and sequencing continues.
    dev.restart();
    dev.linked_to(&b);
    dev.write(4500.0, true);
    dev.with_sync(|s| converge(s, &b));
    let tail = b.tail(lib_b, 1);
    assert_eq!((tail[0].0, tail[0].1.client_seq), (2, 2));
    assert_eq!(dev.k.snapshot(), b.snapshot(lib_b));

    // Restoring again: B's library is reported as already on this Kindle (existing pairing,
    // no new pairing request) and cannot be confirmed.
    dev.tap(SyncHit::Restore);
    dev.wait_view("server choice", |v| {
        v.restore == Some(RestorePhase::ChooseServer) && v.servers.len() == 2
    });
    dev.tap(SyncHit::RestoreServer(b.id()));
    let v = dev.wait_view("libraries", |v| {
        matches!(v.restore, Some(RestorePhase::Libraries { .. }))
    });
    assert_eq!(b.approve_pending(), 0);
    let Some(RestorePhase::Libraries { libraries, .. }) = v.restore else {
        unreachable!()
    };
    let here = libraries.iter().find(|l| l.library_id == lib_b).unwrap();
    assert!(here.on_this_kindle);
    dev.tap(SyncHit::RestoreLibrary(lib_b));
    dev.wait_view("confirmation", |v| {
        matches!(&v.restore, Some(RestorePhase::Confirm { library, .. }) if library.on_this_kindle)
    });
    dev.send(LinkCmd::RestoreConfirm);
    std::thread::sleep(Duration::from_millis(300));
    assert!(matches!(dev.phase(), Some(RestorePhase::Confirm { .. })));
    assert!(
        dev.with_sync(|s| s.link_endpoint()).is_some(),
        "sync never stopped"
    );
    dev.tap(SyncHit::RestoreCancel);
    dev.wait_view("closed", |v| v.restore.is_none());
}

#[test]
fn interruptions_around_the_commit_are_resolved_at_startup() {
    let (a, b, net) = servers("flow-crash");
    let source = Device::new("flow-crash-source", &a, &net);
    let lib_b = synced_and_restored(&a, &b, &source);
    drop(source);
    for point in ["after_verify", "after_plan", "after_publish_sync"] {
        let mut dev = Device::new("flow-crash", &a, &net);
        dev.linked_to(&a);
        let old = dev.k.state();
        let a_profile = dev.profile().unwrap();
        RESTORE_FAULTS
            .lock()
            .unwrap()
            .push((dev.k.sync.clone(), point));
        to_confirm(&dev, &b, lib_b);
        dev.tap(SyncHit::RestoreConfirm);
        let v = dev.wait_view("job finished", |v| {
            matches!(
                v.restore,
                Some(RestorePhase::Failed { .. }) | Some(RestorePhase::Done)
            ) || v.restart
        });
        RESTORE_FAULTS
            .lock()
            .unwrap()
            .retain(|(d, _)| *d != dev.k.sync);
        if point == "after_verify" {
            // Before the commit: nothing changed, the pairing is still A's.
            let Some(RestorePhase::Failed { message }) = v.restore else {
                panic!("{point}: {:?}", v.restore)
            };
            assert!(message.ends_with("Nothing was changed."), "{message}");
            assert!(!adoption_pending(&dev.k.sync));
            assert_eq!(dev.profile().unwrap(), a_profile);
            assert_eq!(dev.k.state(), old);
            dev.tap(SyncHit::RestoreOk);
            dev.wait_view("restart", |v| v.restart);
            assert_eq!(dev.restart(), AdoptionRecovery::DiscardedPreparation);
            assert_eq!(dev.profile().unwrap(), a_profile);
            assert_eq!(dev.k.state().library_id, old.library_id);
            dev.linked_to(&a);
            continue;
        }
        // After the commit: completion belongs to startup recovery; no Cancel is offered.
        assert!(v.restart, "{point}");
        assert!(
            matches!(
                &v.restore,
                Some(RestorePhase::Working {
                    cancellable: false,
                    ..
                })
            ),
            "{point}: {:?}",
            v.restore
        );
        assert!(adoption_pending(&dev.k.sync), "{point}");
        assert_eq!(
            dev.profile().unwrap(),
            a_profile,
            "{point}: profile published early"
        );
        if point == "after_publish_sync" {
            // Library and epoch are B's, the profile still A's: the epoch refuses A's link and
            // ordinary sync refuses to start until recovery publishes the pairing.
            let st = dev.k.state();
            assert_eq!((st.library_id, st.server_id), (lib_b, Some(b.id())));
            assert!(OutboundSync::open(&dev.k.lib, &dev.k.sync)
                .err()
                .unwrap()
                .contains("adoption"));
        }
        let rec = dev.restart();
        assert!(
            matches!(rec, AdoptionRecovery::Completed { library_id, .. } if library_id == lib_b),
            "{point}: {rec:?}"
        );
        let p = dev.profile().unwrap();
        assert_eq!((p.server_id, p.last_endpoint), (b.id(), b.url()), "{point}");
        assert_eq!(dev.k.state().server_id, Some(b.id()));
        assert_eq!(dev.k.snapshot().library_id, lib_b);
        dev.linked_to(&b);
        dev.with_sync(|s| assert_eq!(s.fault(), None));
    }
}

#[test]
fn an_unreachable_or_invalid_target_leaves_a_unchanged() {
    let (a, mut b, net) = servers("flow-unreachable");
    let mut dev = Device::new("flow-unreachable", &a, &net);
    let lib_b = synced_and_restored(&a, &b, &dev);
    let a_profile = dev.profile().unwrap();

    // An address that is not a server is refused where it is typed.
    dev.tap(SyncHit::Restore);
    dev.wait_view("server choice", |v| {
        v.restore == Some(RestorePhase::ChooseServer)
    });
    dev.send(LinkCmd::ManualUrl("127.0.0.1:9".into()));
    dev.wait_view("notice", |v| v.notice.is_some());
    dev.tap(SyncHit::RestoreCancel);
    dev.wait_view("closed", |v| v.restore.is_none());

    to_confirm(&dev, &b, lib_b);
    let library_before = tree(&dev.k.lib);
    let state_before = dev.k.state();
    b.stop();
    dev.tap(SyncHit::RestoreConfirm);
    let v = dev.wait_view("failure", |v| {
        matches!(v.restore, Some(RestorePhase::Failed { .. }))
    });
    assert_eq!(
        v.restore,
        Some(RestorePhase::Failed {
            message: "Could not reach the server. Nothing was changed.".into()
        })
    );
    assert_eq!(tree(&dev.k.lib), library_before);
    assert_eq!(dev.k.state(), state_before);
    assert_eq!(dev.profile().unwrap(), a_profile);
    assert!(!dev.k.layout().workspace.exists() && !dev.k.layout().staged_library.exists());
    dev.tap(SyncHit::RestoreOk);
    dev.wait_view("restart", |v| v.restart);
    assert_eq!(dev.restart(), AdoptionRecovery::None);
    dev.linked_to(&a);
    dev.write(3000.0, true);
    dev.with_sync(|s| converge(s, &a));
}
