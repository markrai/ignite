//! `scribe-app` — Gate F1 durable page + live multi-stroke notebook runtime.
//!
//! Diagnostic hardware laboratory remains `ink-probe`. This binary owns the page session.

#[cfg(not(unix))]
use scribe_app::ink;
#[cfg(unix)]
use scribe_app::runtime;

use std::process::ExitCode;

use scribe_app::cadence::parse_du_interval_us;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match scribe_app::storage_capabilities::command_requested(&args) {
        Ok(true) => return emit_storage_capabilities(),
        Ok(false) => {}
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::FAILURE;
        }
    }
    match scribe_app::storage_inspect_library::parse_command(&args) {
        Ok(scribe_app::storage_inspect_library::InspectionCommand::Inspect(path)) => {
            return inspect_storage_library(&path);
        }
        Ok(scribe_app::storage_inspect_library::InspectionCommand::NotRequested) => {}
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::FAILURE;
        }
    }
    match scribe_app::storage_inspect_external::parse_command(&args) {
        Ok(scribe_app::storage_inspect_external::InspectionCommand::Inspect(config)) => {
            return inspect_storage_external(&config);
        }
        Ok(scribe_app::storage_inspect_external::InspectionCommand::NotRequested) => {}
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::FAILURE;
        }
    }
    match scribe_app::storage_release_provenance::parse_command(&args) {
        Ok(scribe_app::storage_release_provenance::InspectionCommand::Inspect(config)) => {
            return inspect_release_provenance(&config);
        }
        Ok(scribe_app::storage_release_provenance::InspectionCommand::NotRequested) => {}
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::FAILURE;
        }
    }
    match scribe_app::storage_writer_exclusion::parse_command(&args) {
        Ok(scribe_app::storage_writer_exclusion::InspectionCommand::Inspect(config)) => {
            return inspect_writer_exclusion(&config);
        }
        Ok(scribe_app::storage_writer_exclusion::InspectionCommand::NotRequested) => {}
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::FAILURE;
        }
    }

    scribe_app::launch_clock::mark("process_entry");
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print_help();
        return ExitCode::SUCCESS;
    }
    if args.iter().any(|a| a == "--diagnose-fb") {
        return diagnose_fb(&args);
    }

    let (du_interval_us, du_override) = match parse_du_interval_us(&args) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };

    #[cfg(unix)]
    {
        use std::path::Path;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        use scribe_app::launch_health::Reporter;
        use scribe_app::singleton::{process_exit_code, run_with_singleton, DEFAULT_LOCK_PATH};

        // Claim the lifecycle-only descriptor before initialization can launch subprocesses.
        // A singleton loser or startup failure drops it without reporting readiness.
        let mut launch_health = Reporter::from_environment();
        // Kernel flock before any framebuffer / input / library / sync work.
        // A second invocation leaves the live instance alone (never SIGTERM, never paint).
        match run_with_singleton(Path::new(DEFAULT_LOCK_PATH), || {
            let stop = Arc::new(AtomicBool::new(false));
            let signalled = Arc::new(AtomicBool::new(false));
            let (stop_c, signalled_c) = (Arc::clone(&stop), Arc::clone(&signalled));
            if let Err(e) = ctrlc::set_handler(move || {
                eprintln!("signal=caught requesting shutdown");
                signalled_c.store(true, Ordering::SeqCst);
                stop_c.store(true, Ordering::SeqCst);
            }) {
                eprintln!("warning: ctrlc handler: {e}");
            }
            loop {
                let cfg = match runtime::default_config(du_interval_us, du_override, &args) {
                    Ok(c) => c,
                    Err(e) => {
                        eprintln!("{e}");
                        return 1;
                    }
                };
                let code = runtime::run_page_session(cfg, Arc::clone(&stop), &mut launch_health);
                // S2B: bounded stop of background sync after the pen session is released (never
                // delays the input/panel shutdown; the durable outbox survives for the next
                // launch). D1: stop the server link first (closes discovery + the firewall
                // aperture).
                scribe_app::server_link::shutdown_global();
                scribe_app::display_events::stop_powerd_watcher();
                scribe_app::sync_outbound::shutdown_global(std::time::Duration::from_millis(1500));
                // 1H-D2: a library restore restarts the session cleanly in this process (same
                // singleton lock); startup recovery then completes it. A signal, or the host
                // closing its relay (it is stopping or gone), always wins.
                if runtime::take_restart_request()
                    && !signalled.load(Ordering::SeqCst)
                    && !scribe_app::display_events::host_relay_closed()
                {
                    println!("session_restart=library_restore");
                    stop.store(false, Ordering::SeqCst);
                    continue;
                }
                break code;
            }
        }) {
            Ok(run) => ExitCode::from(process_exit_code(run)),
            Err(e) => {
                eprintln!("{e}");
                ExitCode::FAILURE
            }
        }
    }

    #[cfg(not(unix))]
    {
        // Still validate CLI ink parsing on host.
        if let Err(e) = ink::parse_ink_request(&args) {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
        let _ = (du_interval_us, du_override);
        eprintln!("scribe-app physical runtime requires Linux/Kindle (unix).");
        eprintln!("Host tests: cargo test -p scribe-app");
        ExitCode::FAILURE
    }
}

fn print_help() {
    println!(
        "\
scribe-app — Gate F11 V1 notebook UX: Home + library + pages (F0.1 fail-closed)

Usage:
  scribe-app --confirm-write [--ink-byte 00|ff] [--du-interval-us 8000]
             [--fb /dev/fb0] [--input /dev/input/stylus]

Options:
  --storage-capabilities
                        Emit the compiled canonical storage declaration and exit without
                        application initialization. Must be used alone.
  --inspect-storage-library --data-dir PATH
                        Inspect one existing native library read-only and emit a bounded
                        library-only report. Does not initialize or repair the application.
  --inspect-storage-external --data-dir LIBRARY --sync-dir SYNC
                             --device-dir DEVICE --state-dir STATE
                        Inspect existing external sync/device state read-only. All roots are
                        explicit; no application, recovery, network, or hardware initialization.
  --inspect-release-provenance --subject A|B --expected-tag vX.Y.Z
       --record PATH --record-signature PATH --release-manifest PATH
       --release-manifest-signature PATH --capabilities PATH
       --component ROLE=ABSOLUTE_PATH (one for every schema-v1 role)
                        Authenticate one caller-supplied release component set. This does not prove
                        canonical paths, installed-slot ownership, or an atomic state snapshot. It
                        is read-only/report-only and never establishes first-launch admission, data
                        compatibility, or promotion authorization. Production verification remains
                        unavailable while the compile-time public-key pin is UNPROVISIONED.
  --inspect-writer-exclusion --data-dir LIBRARY --sync-dir SYNC
                        Attempt a bounded, read-only acquisition of pre-existing coordination
                        locks and report coverage gaps. Never creates lock files, starts recovery,
                        claims an atomic snapshot, or authorizes candidate execution.
  --diagnose-fb         Read-only FBIOGET dump + Gate B check (no mmap write, no notebooks)
  --confirm-write       Required for physical write (fb mmap / stylus mode / SEND)
  --data-dir PATH       Library storage root (default /mnt/us/scribe-data)
  --ink-byte 00|ff      New page/notebook: pigment to persist (default 00 if omitted).
                        Existing: omitted → use persisted; explicit must match.
  --du-interval-us N    Developer cadence override: 25000|16000|12000|8000
                        Default 8000
  --trace-interstroke   Log Down/Up/StreamGap dispatch diagnostics (F4.1)
  --trace-pressure      Per completed stroke: pressure min/max/mean/p50/p95 (F6)
  --fb PATH             Framebuffer (default /dev/fb0)
  --input PATH          Stylus evdev (default /dev/input/stylus)
  --sync-dir PATH       S2B: enable outbound sync with state in PATH (outside --data-dir;
                        holds config.json with server URL + token). Off when omitted.
  --device-dir PATH     D1 product mode: paired Scribe Server profile (outside --data-dir).
                        With --sync-dir and no config.json: Home shows SYNC (discover,
                        pair, connect); the credential is provisioned by pairing.

Launch opens HOME (active notebook selected). Everything below is pen-operated:
  Home: exit icon (left of SCRIBE) leaves Scribe cleanly; tap a card to open,
        ⋯ for Rename/Delete (confirmed), + NEW (title keyboard),
        ‹ › to page through notebooks (6 per page).
  Notebooks open fullscreen (F11.3): the whole panel is paper. One finger tap shows the
  chrome; it hides after 3 s without chrome use, on a finger tap in the page, on a page
  swipe, or when the pen touches the page (that same pen contact writes).
  Notebook top band: HOME, title, UNDO, ⋯ (insert/move/delete page).
  Notebook bottom band: ‹ previous, page N of M, › next (creates from an authored last page).
  Pen contacts that start on visible chrome, on Home, or in a dialog never create ink.
  Finger (F11.2, /dev/input/touch observed read-only, never grabbed): taps operate the same
  controls; in a notebook a horizontal swipe in the writing area flips pages (left = next,
  right = previous). Finger never inks; multi-touch, palms, edge-origin swipes, and touches
  while the stylus is in proximity/active or the display is invalid are ignored. A contact
  that overlapped stylus proximity stays ignored until every finger lifts.
  --touch PATH          Touchscreen evdev (default /dev/input/touch)
  --no-touch            Pen-only

Navigation (F2/F4): hold BTN_STYLUS + Pen Down —
  outer 15% edges → prev / next (or create once from an authored final page);
  center 50% → undo last visible stroke lineage;
  intervening guard bands → no-op (consume contact; never ink or undo).
A pristine trailing blank page refuses further RightNav creates.
Eraser end: partial-stroke capsule erase (F7). Undo does not restore eraser deletions.
DEVELOPER-ONLY trigger files (diagnostics/tests; not the product UI):
Page delete (F8): touch `{{data-dir}}/.scribe-dev-delete-current-page` while idle.
  Refuses when only one page remains. Quarantines under `.page-trash/` (not navigable).
Home/library (F10), root-level idle-only triggers:
  `.scribe-dev-home`, `.scribe-dev-library-next`, `.scribe-dev-library-prev`,
  `.scribe-dev-library-open`, `.scribe-dev-create-notebook`,
  `.scribe-dev-delete-notebook`.
Home consumes all pen/eraser contacts without ink, page navigation, or Undo.

EVIOCGRAB is mandatory. Grab failure → REFUSE LIVE SESSION.
Exit: Ctrl+C / SIGTERM. SIGKILL / power loss may bypass userspace cleanup.
Disarm failure → nonzero exit (shutdown_safe=false).

Does not use Amazon NBK / SQLite / actions.log.
Does not open /dev/stylus.
ink-probe remains the diagnostic laboratory.
"
    );
}

fn emit_storage_capabilities() -> ExitCode {
    use std::io::Write as _;

    let bytes = match scribe_app::storage_capabilities::canonical_bytes() {
        Ok(bytes) => bytes,
        Err(error) => {
            eprintln!("storage capabilities unavailable: {error}");
            return ExitCode::FAILURE;
        }
    };
    let mut stdout = std::io::stdout().lock();
    match stdout.write_all(&bytes).and_then(|()| stdout.flush()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("write storage capabilities: {error}");
            ExitCode::FAILURE
        }
    }
}

fn inspect_storage_library(path: &std::path::Path) -> ExitCode {
    use std::io::Write as _;

    let report = scribe_app::storage_inspect_library::inspect_library(path);
    let bytes = match report.canonical_bytes() {
        Ok(bytes) => bytes,
        Err(error) => {
            eprintln!("storage library inspection unavailable: {error}");
            return ExitCode::FAILURE;
        }
    };
    let mut stdout = std::io::stdout().lock();
    if let Err(error) = stdout.write_all(&bytes).and_then(|()| stdout.flush()) {
        eprintln!("write storage library inspection: {error}");
        return ExitCode::FAILURE;
    }
    ExitCode::from(report.exit_code())
}

fn inspect_storage_external(
    config: &scribe_app::storage_inspect_external::ExternalInspectionConfig,
) -> ExitCode {
    use std::io::Write as _;

    let report = scribe_app::storage_inspect_external::inspect_external(config);
    let bytes = match report.canonical_bytes() {
        Ok(bytes) => bytes,
        Err(error) => {
            eprintln!("storage external inspection unavailable: {error}");
            return ExitCode::FAILURE;
        }
    };
    let mut stdout = std::io::stdout().lock();
    if let Err(error) = stdout.write_all(&bytes).and_then(|()| stdout.flush()) {
        eprintln!("write storage external inspection: {error}");
        return ExitCode::FAILURE;
    }
    ExitCode::from(report.exit_code())
}

fn inspect_release_provenance(
    config: &scribe_app::storage_release_provenance::InspectionConfig,
) -> ExitCode {
    use std::io::Write as _;

    let report = scribe_app::storage_release_provenance::inspect_official(config);
    let bytes = report.canonical_bytes();
    let mut stdout = std::io::stdout().lock();
    if let Err(error) = stdout.write_all(&bytes).and_then(|()| stdout.flush()) {
        eprintln!("write installed release provenance inspection: {error}");
        return ExitCode::FAILURE;
    }
    ExitCode::from(report.exit_code())
}

fn inspect_writer_exclusion(
    config: &scribe_app::storage_writer_exclusion::WriterExclusionConfig,
) -> ExitCode {
    use std::io::Write as _;

    let report = scribe_app::storage_writer_exclusion::inspect_writer_exclusion(config);
    let bytes = report.canonical_bytes();
    let mut stdout = std::io::stdout().lock();
    if let Err(error) = stdout.write_all(&bytes).and_then(|()| stdout.flush()) {
        eprintln!("write data-writer exclusion inspection: {error}");
        return ExitCode::FAILURE;
    }
    ExitCode::from(report.exit_code())
}

fn diagnose_fb(args: &[String]) -> ExitCode {
    let fb_path = args
        .windows(2)
        .find(|w| w[0] == "--fb")
        .map(|w| w[1].as_str())
        .unwrap_or("/dev/fb0");
    println!("diagnose_fb path={fb_path}");
    #[cfg(unix)]
    {
        match kindle_platform::linux::LinuxFramebuffer::open_info_only_path(fb_path) {
            Ok(info) => {
                print!(
                    "{}",
                    kindle_platform::format_fb_diagnose(&info.fix, &info.var)
                );
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("fb_ioctl=unavailable error={e}");
                ExitCode::FAILURE
            }
        }
    }
    #[cfg(not(unix))]
    {
        eprintln!("fb_ioctl=unavailable (host build; needs Kindle Linux)");
        ExitCode::FAILURE
    }
}
