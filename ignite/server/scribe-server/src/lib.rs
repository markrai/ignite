//! Scribe Server (D1 product): composes the existing sync server (ledger, reducer, snapshots,
//! blobs, ignite-web static serving — `scribe-sync-server`, reused, never forked) with a
//! product control plane (server identity, first-run claim, pairing, per-device credentials,
//! browser sessions, revocation) and DNS-SD advertisement.
//!
//! ```text
//! <state_dir>/server.json      ServerId (durable identity)
//! <state_dir>/control.sqlite   control plane (devices, pairing, claim state)
//! <state_dir>/ledger/          notebook ledger (scribe-sync-server Store: ledger.sqlite3, blobs/)
//! <state_dir>/ledger/restore-staging/job-*   private restore staging (removed at startup)
//! <state_dir>/ledger/export-staging/job-export-*   private backup exports (removed at startup)
//! ```

pub mod config;
pub mod control;
pub mod discovery;
pub mod export;
pub mod fsutil;
pub mod identity;
pub mod netpolicy;
pub mod plane;
pub mod restore;

use std::sync::Arc;

use scribe_sync_server::{ServerConfig, ServerHandle, Store, WebRoot};

use crate::config::Config;
use crate::control::{Clock, ControlStore};
use crate::plane::{EventSink, Plane, RateLimiter};

/// Everything a running product server needs (the binary and the integration tests).
pub struct Running {
    pub handle: ServerHandle,
    pub plane: Arc<Plane>,
    pub advertisement: Option<discovery::Advertisement>,
    /// Expires prepared restore jobs; stopped (joined) when dropped.
    pub restore_sweeper: restore::Sweeper,
    /// Expires backup exports; when dropped, waits for an export being prepared.
    pub export_sweeper: export::Sweeper,
}

/// Prepare state (identity + control DB); used by `init` and `serve`.
pub fn prepare(
    config: &Config,
    clock: Clock,
) -> Result<(identity::ServerIdentity, bool, ControlStore), String> {
    fsutil::ensure_private_dir(&config.server.state_dir)?;
    let (id, created) = identity::load_or_create(&config.server.state_dir)?;
    let store = ControlStore::open(&config.server.state_dir, clock)?;
    Ok((id, created, store))
}

pub fn display_name(config: &Config) -> String {
    if config.server.name.trim().is_empty() {
        control_protocol::default_display_name(&config::hostname())
    } else {
        control_protocol::clamp_label(&config.server.name)
    }
}

/// Start serving (sync plane + control plane + optional advertisement) with the host's LAN
/// policy (D1.1: only physical/private LAN interfaces + loopback diagnostics are served).
pub fn serve(config: &Config, clock: Clock, events: EventSink) -> Result<Running, String> {
    serve_with_policy(
        config,
        clock,
        events,
        Arc::new(netpolicy::LanPolicy::for_this_host()),
    )
}

/// [`serve`] with an injectable LAN policy (tests use a scripted interface inventory).
pub fn serve_with_policy(
    config: &Config,
    clock: Clock,
    events: EventSink,
    lan: Arc<netpolicy::LanPolicy>,
) -> Result<Running, String> {
    serve_with_restore_limits(
        config,
        clock,
        events,
        lan,
        restore::RestoreLimits::default(),
    )
}

/// [`serve_with_policy`] with explicit restore limits (tests use small sizes and timeouts).
pub fn serve_with_restore_limits(
    config: &Config,
    clock: Clock,
    events: EventSink,
    lan: Arc<netpolicy::LanPolicy>,
    restore_limits: restore::RestoreLimits,
) -> Result<Running, String> {
    serve_with_limits(
        config,
        clock,
        events,
        lan,
        restore_limits,
        export::ExportLimits::default(),
    )
}

/// [`serve_with_restore_limits`] with explicit export limits too.
pub fn serve_with_limits(
    config: &Config,
    clock: Clock,
    events: EventSink,
    lan: Arc<netpolicy::LanPolicy>,
    restore_limits: restore::RestoreLimits,
    export_limits: export::ExportLimits,
) -> Result<Running, String> {
    let (id, _, control) = prepare(config, clock.clone())?;
    let ledger_dir = config.ledger_dir();
    fsutil::ensure_private_dir(&ledger_dir)?;
    let ledger = Store::open(&ledger_dir).map_err(|e| format!("ledger: {e:?}"))?;
    // Abandoned restore staging (a crash, or prepared jobs of the previous process). Safe only
    // here: `Store::open` holds the ledger's exclusive process lock and nothing is served yet.
    let removed = backup_restore::remove_restore_staging(&ledger)
        .map_err(|e| format!("restore staging cleanup: {e}"))?;
    if removed > 0 {
        events(&format!(
            "scribe-server event=restore_staging_removed count={removed}"
        ));
    }
    let staging_root = ledger
        .restore_staging_root()
        .map_err(|e| format!("restore staging: {e:?}"))?;
    fsutil::ensure_private_dir(&staging_root)?;
    let restore_jobs = Arc::new(restore::RestoreJobs::new(
        staging_root,
        clock.clone(),
        restore_limits,
    ));
    // Backup exports of the previous process are never resumed or offered again.
    let export_root = ledger_dir.join(export::EXPORT_STAGING_DIR);
    let removed = export::remove_export_staging(&export_root)?;
    if removed > 0 {
        events(&format!(
            "scribe-server event=export_staging_removed count={removed}"
        ));
    }
    fsutil::ensure_private_dir(&export_root)?;
    let export_jobs = Arc::new(export::ExportJobs::new(
        export_root,
        clock.clone(),
        export_limits,
    ));
    let web_root = match &config.server.web_root {
        Some(p) => Some(WebRoot::new(p, &[&config.server.state_dir])?),
        None => None,
    };
    let name = display_name(config);
    let plane = Arc::new(Plane {
        store: control,
        server_id: id.server_id,
        server_name: name.clone(),
        started_ms: clock(),
        limiter: RateLimiter::new(),
        events,
        lan: Some(lan),
        restore: Arc::clone(&restore_jobs),
        exports: Arc::clone(&export_jobs),
    });
    let restore_sweeper = restore::Sweeper::start(Arc::clone(&restore_jobs), plane.events.clone());
    let export_sweeper = export::Sweeper::start(Arc::clone(&export_jobs), plane.events.clone());
    let handle = ServerHandle::start_with(
        config.server.listen,
        ledger,
        ServerConfig {
            auth: None,
            web_root,
            control: Some(plane.clone() as Arc<dyn scribe_sync_server::ControlPlane>),
        },
    )?;
    // Confirmed restores run with the transport's own ledger `Arc` (one `Store`, one process
    // lock); `restore_sweeper` joins a running restore before the ledger can be dropped.
    restore_jobs.attach_store(Arc::clone(&handle.store));
    export_jobs.attach_store(Arc::clone(&handle.store));
    let advertisement = if config.discovery.advertise {
        match discovery::Advertisement::start(
            id.server_id,
            &name,
            &config::hostname(),
            handle.addr().port(),
        ) {
            Ok(a) => Some(a),
            Err(e) => {
                // Discovery failure never stops the server (the friendly URL still works).
                (plane.events)(&format!(
                    "scribe-server event=discovery_unavailable reason={e:?}"
                ));
                None
            }
        }
    } else {
        None
    };
    Ok(Running {
        handle,
        plane,
        advertisement,
        restore_sweeper,
        export_sweeper,
    })
}

pub fn system_clock() -> Clock {
    Arc::new(fsutil::now_ms)
}
