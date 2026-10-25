//! Kindle appmgrd lifecycle host / probe.
//!
//! Protocol (proven from KOReaderIntegration `src/koreader/main.c` and matching
//! SH Integration launcher strings on FW 5.18.6):
//!
//! 1. `LipcOpenEx(<lipcId>)`
//! 2. Register write-only string props: `load`, `go`, `pause`, `unload`
//! 3. `LipcSetStringProperty(com.lab126.appmgrd, runresult, "0:<lipcId>")`
//! 4. On each lifecycle write: parse `"<id>:..."`, reply
//!    `LipcSetStringProperty(com.lab126.appmgrd, "<prop>result", "<id>:0:")`
//! 5. On `unload`: acknowledge, then exit normally
//!
//! Probe mode never opens framebuffer, HWTCON, stylus, or notebook data.

mod launch_health;
mod protocol;

#[cfg(unix)]
mod candidate_health;

#[cfg(unix)]
mod lipc;

#[cfg(unix)]
mod unix_main {
    use crate::candidate_health::{
        EvidenceObserver, EvidenceReport, InitialEvidence, LaunchAttempt, DEFAULT_UPDATE_DIR,
    };
    use crate::launch_health::{
        clear_handshake_environment, LaunchState, ObserverEvent, PreparedLaunchHealth,
    };
    use crate::lipc::{ChildSupervisor, LIPCcode, LifecycleState, LipcHandle, LipcLib, LIPC_OK};
    use crate::protocol::{
        ack_value, relay_line_for_powerd_event, request_id_from_payload, result_property,
        runresult_value, unrequested_exit_reason, CHILD_DISPLAY_EVENTS_FLAG,
        EVENT_EXITING_SCREENSAVER, EVENT_GOING_TO_SCREENSAVER, KINDLE_HOME_APP_URI, POWERD,
    };
    use std::ffi::CStr;
    use std::io::Write;
    use std::os::raw::{c_char, c_void};
    use std::path::PathBuf;
    use std::process::{Command, Stdio};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Mutex, OnceLock};
    use std::time::{Duration, Instant};

    const APPMGRD: &str = "com.lab126.appmgrd";
    const DEFAULT_LIPC_SO: &str = "/usr/lib/liblipc.so.1";
    const DEFAULT_PROBE_SERVICE: &str = "com.markrai.ignite.lifecycleprobe";
    const DEFAULT_SCRIBE_SERVICE: &str = "com.markrai.ignite";
    const DEFAULT_LOG_PROBE: &str = "/mnt/us/scribe-app/logs/lifecycle-probe.log";
    const DEFAULT_LOG_HOST: &str = "/mnt/us/scribe-app/logs/lifecycle-host.log";
    const DEFAULT_CHILD: &str = "/mnt/us/scribe-app/bin/scribe-app";
    const DEFAULT_CHILD_LOG: &str = "/mnt/us/scribe-app/logs/scribe-app.child.log";
    const CHILD_SHUTDOWN_BUDGET: Duration = Duration::from_secs(45);
    const LAUNCH_HEALTH_TIMEOUT: Duration = Duration::from_secs(120);
    const READY_RUNNING_CHECKPOINT: Duration = Duration::from_secs(60);

    static STATE: OnceLock<LifecycleState> = OnceLock::new();
    static LIB: OnceLock<LipcLib> = OnceLock::new();
    static CHILD_LOG: OnceLock<String> = OnceLock::new();

    #[derive(Debug, Clone)]
    struct Args {
        service: String,
        log_path: String,
        lipc_so: PathBuf,
        child_bin: Option<String>,
        child_args: Vec<String>,
        child_log: String,
        probe: bool,
    }

    fn usage() -> ! {
        eprintln!(
            "scribe-lifecycle — Kindle appmgrd lifecycle host / probe\n\n\
             Probe (no FB/stylus/data):\n\
               scribe-lifecycle --probe\n\n\
             Production host:\n\
               scribe-lifecycle --host [--service com.markrai.ignite] \\\n\
                 [--log HOST_LOG] [--child-log CHILD_LOG] \\\n\
                 --child /mnt/us/scribe-app/bin/scribe-app -- \\\n\
                 --confirm-write --data-dir /mnt/us/scribe-data\n"
        );
        std::process::exit(2);
    }

    fn parse_args() -> Args {
        let mut rest: Vec<String> = std::env::args().skip(1).collect();
        if rest.iter().any(|a| a == "-h" || a == "--help") {
            usage();
        }

        let mut probe = false;
        let mut host = false;
        let mut service: Option<String> = None;
        let mut log_path: Option<String> = None;
        let mut lipc_so = PathBuf::from(DEFAULT_LIPC_SO);
        let mut child_bin: Option<String> = None;
        let mut child_log: Option<String> = None;
        let mut child_args: Vec<String> = Vec::new();
        let mut i = 0;

        while i < rest.len() {
            match rest[i].as_str() {
                "--probe" => {
                    probe = true;
                    i += 1;
                }
                "--host" => {
                    host = true;
                    i += 1;
                }
                "--service" => {
                    service = Some(rest.get(i + 1).cloned().unwrap_or_else(|| usage()));
                    i += 2;
                }
                "--log" => {
                    log_path = Some(rest.get(i + 1).cloned().unwrap_or_else(|| usage()));
                    i += 2;
                }
                "--lipc-so" => {
                    lipc_so = PathBuf::from(rest.get(i + 1).cloned().unwrap_or_else(|| usage()));
                    i += 2;
                }
                "--child-log" => {
                    child_log = Some(rest.get(i + 1).cloned().unwrap_or_else(|| usage()));
                    i += 2;
                }
                "--child" => {
                    child_bin = Some(rest.get(i + 1).cloned().unwrap_or_else(|| usage()));
                    i += 2;
                }
                "--" => {
                    child_args.extend(rest.split_off(i + 1));
                    break;
                }
                other => {
                    eprintln!("unknown argument: {other}");
                    usage();
                }
            }
        }

        if !probe && !host {
            eprintln!("error: specify --probe or --host");
            usage();
        }
        if probe && host {
            eprintln!("error: --probe and --host are mutually exclusive");
            usage();
        }

        if probe {
            Args {
                service: service.unwrap_or_else(|| DEFAULT_PROBE_SERVICE.to_string()),
                log_path: log_path.unwrap_or_else(|| DEFAULT_LOG_PROBE.to_string()),
                lipc_so,
                child_bin: None,
                child_args: Vec::new(),
                child_log: DEFAULT_CHILD_LOG.to_string(),
                probe: true,
            }
        } else {
            Args {
                service: service.unwrap_or_else(|| DEFAULT_SCRIBE_SERVICE.to_string()),
                log_path: log_path.unwrap_or_else(|| DEFAULT_LOG_HOST.to_string()),
                lipc_so,
                child_bin: Some(child_bin.unwrap_or_else(|| DEFAULT_CHILD.to_string())),
                child_args,
                child_log: child_log.unwrap_or_else(|| DEFAULT_CHILD_LOG.to_string()),
                probe: false,
            }
        }
    }

    fn state() -> &'static LifecycleState {
        STATE.get().expect("lifecycle state")
    }

    fn lib() -> &'static LipcLib {
        LIB.get().expect("lipc lib")
    }

    fn cstr<'a>(p: *const c_char) -> &'a str {
        if p.is_null() {
            return "";
        }
        unsafe { CStr::from_ptr(p).to_str().unwrap_or("") }
    }

    fn payload_from_value(value: *mut c_void) -> String {
        if value.is_null() {
            return String::new();
        }
        unsafe { CStr::from_ptr(value as *const c_char) }
            .to_string_lossy()
            .into_owned()
    }

    fn acknowledge(lipc: LipcHandle, property: &str, payload: &str) -> LIPCcode {
        let id = request_id_from_payload(payload);
        let ack = ack_value(id);
        let target = result_property(property);
        state().log(&format!(
            "lifecycle_ack prop={property} payload={payload:?} result_prop={target} value={ack}"
        ));
        if let Err(e) = lib().set_string(lipc, APPMGRD, &target, &ack) {
            state().log(&format!("lifecycle_ack_failed err={e}"));
        }
        LIPC_OK
    }

    fn report_evidence(report: EvidenceReport) {
        match report.result {
            Ok(()) => state().log(&format!(
                "launch_health_evidence attempt={} outcome={} status=published qualification=unqualified",
                report.launch_uuid, report.outcome
            )),
            Err(error) => state().log(&format!(
                "launch_health_evidence attempt={} outcome={} status=unavailable detail={error:?}",
                report.launch_uuid, report.outcome
            )),
        }
    }

    fn start_evidence_worker(
        child_bin: &str,
        attempt: LaunchAttempt,
        started_at: Instant,
        initial: InitialEvidence,
    ) -> Option<EvidenceObserver> {
        EvidenceObserver::start(
            std::path::PathBuf::from(DEFAULT_UPDATE_DIR),
            std::path::PathBuf::from(child_bin),
            attempt,
            started_at,
            initial,
            report_evidence,
        )
        .ok()
    }

    fn log_health_event(
        attempt: &str,
        pid: u32,
        event: ObserverEvent,
        evidence: &mut Option<EvidenceObserver>,
        now: Instant,
    ) {
        match event {
            ObserverEvent::Ready => {
                state().log(&format!(
                    "launch_health attempt={attempt} pid={pid} state=ready"
                ));
                if let Some(evidence) = evidence.as_mut() {
                    evidence.record_ready(now);
                }
            }
            ObserverEvent::IncorrectIdentifier { received } => state().log(&format!(
                "launch_health attempt={attempt} pid={pid} state=started ack=rejected reason=incorrect_identifier received={received}"
            )),
            ObserverEvent::MalformedAcknowledgment => state().log(&format!(
                "launch_health attempt={attempt} pid={pid} state=started ack=rejected reason=malformed"
            )),
            ObserverEvent::TimedOut => {
                state().log(&format!(
                    "launch_health attempt={attempt} pid={pid} state=timeout compatibility=missing_ack_or_legacy_child"
                ));
                if let Some(evidence) = evidence.as_mut() {
                    evidence.record_timeout(now);
                }
            }
            ObserverEvent::Interrupted { error } => {
                state().log(&format!(
                    "launch_health attempt={attempt} pid={pid} state=interrupted reason=ipc_read_error detail={error:?}"
                ));
                if let Some(evidence) = evidence.as_mut() {
                    evidence.record_interrupted(now);
                }
            }
        }
    }

    fn log_health_child_exit(attempt: &str, pid: u32, before: LaunchState, after: LaunchState) {
        if after == LaunchState::PrematureExit {
            state().log(&format!(
                "launch_health attempt={attempt} pid={pid} state=premature_exit"
            ));
        } else if after == LaunchState::NormalExit {
            state().log(&format!(
                "launch_health attempt={attempt} pid={pid} state=normal_exit_before_ready classification=non_failure"
            ));
        } else {
            state().log(&format!(
                "launch_health attempt={attempt} pid={pid} state={} event=child_exit observation=complete",
                before.as_str()
            ));
        }
    }

    fn signal_term(pid: u32) -> std::io::Result<()> {
        let rc = unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
        if rc == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }

    fn stop_child_sigterm(reason: &str) -> bool {
        let mut guard = match state().child.lock() {
            Ok(g) => g,
            Err(_) => return false,
        };
        let Some(sup) = guard.as_mut() else {
            state().log(&format!("child_stop skipped reason={reason} no_supervisor"));
            return true;
        };
        let Some(pid) = sup.child.as_ref().map(std::process::Child::id) else {
            state().log(&format!("child_stop skipped reason={reason} no_child"));
            return true;
        };
        if let Some(health) = sup.health.as_mut() {
            let before = health.state();
            let attempt = health.attempt().to_owned();
            let after = health.intentional_stop();
            state().log(&format!(
                "launch_health attempt={attempt} pid={pid} state=intentional_stop reason={reason} readiness_before={}",
                before.as_str()
            ));
            debug_assert!(matches!(
                after,
                LaunchState::IntentionalStop
                    | LaunchState::Ready
                    | LaunchState::TimedOut
                    | LaunchState::Interrupted
            ));
        }
        // Close the relay pipe first; the child's reader thread sees EOF.
        sup.events = None;
        state().log(&format!(
            "child_stop reason={reason} pid={pid} signal=SIGTERM budget_s={}",
            CHILD_SHUTDOWN_BUDGET.as_secs()
        ));
        if let Err(e) = signal_term(pid) {
            state().log(&format!("child_sigterm_failed pid={pid} err={e}"));
        }
        let child = sup.child.as_mut().expect("child checked above");
        let deadline = Instant::now() + CHILD_SHUTDOWN_BUDGET;
        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    let successful = status.success();
                    let outcome = if successful {
                        "intentional_stop"
                    } else {
                        "intentional_stop_failed"
                    };
                    state().log(&format!(
                        "child_exited pid={pid} status={status} reason={reason} requested_stop_outcome={outcome}"
                    ));
                    if let Some(evidence) = sup.evidence.as_mut() {
                        evidence.record_intentional_stop(successful, Instant::now());
                    }
                    sup.child = None;
                    sup.health = None;
                    sup.evidence = None;
                    return true;
                }
                Ok(None) => {
                    if Instant::now() >= deadline {
                        state().log(&format!(
                            "child_shutdown_timeout pid={pid} budget_s={} FAIL",
                            CHILD_SHUTDOWN_BUDGET.as_secs()
                        ));
                        if let Some(evidence) = sup.evidence.as_mut() {
                            evidence.record_interrupted(Instant::now());
                        }
                        return false;
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(e) => {
                    state().log(&format!("child_wait_failed pid={pid} err={e}"));
                    if let Some(evidence) = sup.evidence.as_mut() {
                        evidence.record_interrupted(Instant::now());
                    }
                    return false;
                }
            }
        }
    }

    fn start_child_if_configured(payload: &str) {
        let mut guard = match state().child.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        let Some(sup) = guard.as_mut() else {
            state().log(&format!("go_probe_only payload={payload:?}"));
            return;
        };
        if let Some(existing) = sup.child.as_mut() {
            let existing_pid = existing.id();
            match existing.try_wait() {
                Ok(Some(status)) => {
                    if let Some(health) = sup.health.as_mut() {
                        let before = health.state();
                        let attempt = health.attempt().to_owned();
                        let after = health.child_exited(status.success());
                        log_health_child_exit(&attempt, existing_pid, before, after);
                    }
                    if let Some(evidence) = sup.evidence.as_mut() {
                        evidence.record_exit(status.success(), Instant::now());
                    }
                    state().log(&format!(
                        "previous_child_exited status={status}; clearing before relaunch"
                    ));
                    sup.child = None;
                    sup.events = None;
                    sup.health = None;
                    sup.evidence = None;
                }
                Ok(None) => {
                    state().log(&format!(
                        "go_skipped_already_running pid={} payload={payload:?}",
                        existing.id()
                    ));
                    return;
                }
                Err(e) => {
                    if let Some(health) = sup.health.as_mut() {
                        let attempt = health.attempt().to_owned();
                        let after = health.interrupted();
                        state().log(&format!(
                            "launch_health attempt={attempt} pid={existing_pid} state={} reason=child_poll_failed",
                            after.as_str()
                        ));
                    }
                    if let Some(evidence) = sup.evidence.as_mut() {
                        evidence.record_interrupted(Instant::now());
                    }
                    state().log(&format!("go_child_poll_failed err={e}"));
                    return;
                }
            }
        }
        if sup.child.is_none() {
            if let Some(health) = sup.health.as_mut() {
                let attempt = health.attempt().to_owned();
                let after = health.interrupted();
                state().log(&format!(
                    "launch_health attempt={attempt} state={} reason=orphaned_observation",
                    after.as_str()
                ));
            }
            sup.health = None;
            if let Some(evidence) = sup.evidence.as_mut() {
                evidence.record_interrupted(Instant::now());
            }
            sup.evidence = None;
            sup.events = None;
        }
        state().log(&format!(
            "child_start bin={} args={:?} payload={payload:?}",
            sup.child_bin, sup.child_args
        ));
        // appmgrd often attaches the host to /dev/null; keep a dedicated child log.
        let child_log_path = CHILD_LOG.get().map_or(DEFAULT_CHILD_LOG, String::as_str);
        if let Some(parent) = std::path::Path::new(child_log_path).parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let child_log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(child_log_path);
        let (stdout, stderr) = match child_log {
            Ok(f) => {
                let f2 = f.try_clone().ok();
                (
                    f2.map(Stdio::from).unwrap_or_else(Stdio::null),
                    Stdio::from(f),
                )
            }
            Err(_) => (Stdio::null(), Stdio::null()),
        };
        let (mut pending_health, launch_attempt) = match PreparedLaunchHealth::new() {
            Ok(health) => {
                let attempt = health.attempt().to_owned();
                (Some(health), attempt)
            }
            Err(e) => {
                state().log(&format!(
                    "launch_health attempt={} state=interrupted reason=ipc_setup_failed detail={:?}",
                    e.attempt, e.error
                ));
                (None, e.attempt)
            }
        };
        let mut command = Command::new(&sup.child_bin);
        clear_handshake_environment(&mut command);
        command
            .args(&sup.child_args)
            .arg(CHILD_DISPLAY_EVENTS_FLAG)
            .stdin(Stdio::piped())
            .stdout(stdout)
            .stderr(stderr);
        if let Some(health) = pending_health.as_ref() {
            health.configure_command(&mut command);
        }
        // Capture ordering without filesystem access. The worker uses this pre-spawn token to keep
        // a delayed older worker from displacing a newer cross-process owner.
        let evidence_attempt = LaunchAttempt::capture(&launch_attempt).ok();
        match command.spawn() {
            Ok(mut child) => {
                let pid = child.id();
                let started_at = Instant::now();
                state().log(&format!(
                    "child_started pid={} child_log={child_log_path}",
                    pid
                ));
                let observer = pending_health
                    .take()
                    .map(|health| health.spawned(started_at, LAUNCH_HEALTH_TIMEOUT));
                if let Some(health) = observer.as_ref() {
                    state().log(&format!(
                        "launch_health attempt={} pid={pid} state=started timeout_s={} protocol=unix_datagram_v1",
                        health.attempt(),
                        LAUNCH_HEALTH_TIMEOUT.as_secs()
                    ));
                }
                // All evidence filesystem work occurs on a detached worker. This handoff performs
                // only allocation and thread creation on the launch path.
                let evidence = evidence_attempt.and_then(|attempt| {
                    start_evidence_worker(
                        &sup.child_bin,
                        attempt,
                        started_at,
                        if observer.is_some() {
                            InitialEvidence::Spawned
                        } else {
                            InitialEvidence::Unavailable
                        },
                    )
                });
                sup.events = child.stdin.take();
                sup.child = Some(child);
                sup.health = observer;
                sup.evidence = evidence;
                std::thread::spawn(move || watch_child(pid));
            }
            Err(e) => {
                if let Some(health) = pending_health.take() {
                    let observation = health.spawn_failed();
                    state().log(&format!(
                        "launch_health attempt={} state={} reason=child_spawn_failed",
                        observation.attempt(),
                        observation.state().as_str()
                    ));
                }
                if let Some(attempt) = evidence_attempt {
                    let _ = start_evidence_worker(
                        &sup.child_bin,
                        attempt,
                        Instant::now(),
                        InitialEvidence::SpawnFailed,
                    );
                }
                state().log(&format!("child_start_failed err={e}"));
            }
        }
    }

    /// Watch one child. Deliberate stops (pause/unload) reap it inside `stop_child_sigterm`
    /// while holding the supervisor lock, so this only ever sees an UNREQUESTED exit.
    ///
    /// Every unrequested exit hands the panel back to Kindle Home (see
    /// [`unrequested_exit_reason`]): a singleton loser included, since this host would otherwise
    /// stay in front with nothing to draw.
    fn watch_child(pid: u32) {
        loop {
            std::thread::sleep(Duration::from_millis(200));
            let status = {
                let Ok(mut guard) = state().child.lock() else {
                    return;
                };
                let Some(sup) = guard.as_mut() else {
                    return;
                };
                if sup.child.as_ref().map(std::process::Child::id) != Some(pid) {
                    return; // stopped deliberately (or replaced)
                }
                let now = Instant::now();
                if let Some(health) = sup.health.as_mut() {
                    let attempt = health.attempt().to_owned();
                    let events = health.poll(now);
                    for event in events {
                        log_health_event(&attempt, pid, event, &mut sup.evidence, now);
                    }
                }
                let wait = sup.child.as_mut().expect("child checked above").try_wait();
                match wait {
                    Ok(None) => {
                        if let Some(evidence) = sup.evidence.as_mut() {
                            evidence.maybe_record_running(now, READY_RUNNING_CHECKPOINT);
                        }
                        None
                    }
                    Ok(Some(status)) => {
                        if let Some(health) = sup.health.as_mut() {
                            let before = health.state();
                            let attempt = health.attempt().to_owned();
                            let after = health.child_exited(status.success());
                            log_health_child_exit(&attempt, pid, before, after);
                        }
                        if let Some(evidence) = sup.evidence.as_mut() {
                            evidence.record_exit(status.success(), now);
                        }
                        sup.child = None;
                        sup.events = None;
                        sup.health = None;
                        sup.evidence = None;
                        Some(status)
                    }
                    Err(e) => {
                        if let Some(health) = sup.health.as_mut() {
                            let attempt = health.attempt().to_owned();
                            let after = health.interrupted();
                            state().log(&format!(
                                "launch_health attempt={attempt} pid={pid} state={} reason=child_wait_failed",
                                after.as_str()
                            ));
                        }
                        if let Some(evidence) = sup.evidence.as_mut() {
                            evidence.record_interrupted(now);
                        }
                        state().log(&format!("child_watch_failed pid={pid} err={e}"));
                        return;
                    }
                }
            };
            let Some(status) = status else {
                continue;
            };
            state().log(&format!(
                "child_exited_unexpectedly pid={pid} status={status} reason={} action=handoff_kindle_home",
                unrequested_exit_reason(status.code())
            ));
            handoff_to_kindle_home();
            return;
        }
    }

    fn handoff_to_kindle_home() {
        match Command::new("lipc-set-prop")
            .args([APPMGRD, "start", KINDLE_HOME_APP_URI])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
        {
            Ok(st) => state().log(&format!(
                "handoff_requested target={KINDLE_HOME_APP_URI} status={st}"
            )),
            Err(e) => state().log(&format!("handoff_failed err={e}")),
        }
    }

    unsafe extern "C" fn on_load(
        lipc: LipcHandle,
        property: *const c_char,
        value: *mut c_void,
        _data: *mut c_void,
    ) -> LIPCcode {
        let prop = cstr(property);
        let payload = payload_from_value(value);
        state().load_seen.store(true, Ordering::SeqCst);
        state().log(&format!("lifecycle_event prop={prop} payload={payload:?}"));
        acknowledge(lipc, prop, &payload)
    }

    unsafe extern "C" fn on_go(
        lipc: LipcHandle,
        property: *const c_char,
        value: *mut c_void,
        _data: *mut c_void,
    ) -> LIPCcode {
        let prop = cstr(property);
        let payload = payload_from_value(value);
        state().go_seen.store(true, Ordering::SeqCst);
        state().log(&format!("lifecycle_event prop={prop} payload={payload:?}"));
        start_child_if_configured(&payload);
        acknowledge(lipc, prop, &payload)
    }

    unsafe extern "C" fn on_pause(
        lipc: LipcHandle,
        property: *const c_char,
        value: *mut c_void,
        _data: *mut c_void,
    ) -> LIPCcode {
        let prop = cstr(property);
        let payload = payload_from_value(value);
        state().pause_seen.store(true, Ordering::SeqCst);
        state().log(&format!("lifecycle_event prop={prop} payload={payload:?}"));
        let _ = stop_child_sigterm("pause");
        acknowledge(lipc, prop, &payload)
    }

    unsafe extern "C" fn on_unload(
        lipc: LipcHandle,
        property: *const c_char,
        value: *mut c_void,
        _data: *mut c_void,
    ) -> LIPCcode {
        let prop = cstr(property);
        let payload = payload_from_value(value);
        state().unload_seen.store(true, Ordering::SeqCst);
        state().log(&format!("lifecycle_event prop={prop} payload={payload:?}"));
        let ok = stop_child_sigterm("unload");
        let code = acknowledge(lipc, prop, &payload);
        if !ok {
            state().log("unload_child_shutdown_budget_exceeded FAIL");
        }
        state().done.store(true, Ordering::SeqCst);
        state().log("lifecycle_done unload_ack_sent exiting");
        code
    }

    /// F11.1: relay a powerd screensaver event to the child. Never touches `/dev/fb0`; the
    /// child remains the only Scribe renderer.
    fn relay_display_event(event: &str) {
        let Some(line) = relay_line_for_powerd_event(event) else {
            return;
        };
        let mut guard = match state().child.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        let Some(sup) = guard.as_mut() else {
            return;
        };
        let Some(pipe) = sup.events.as_mut() else {
            state().log(&format!(
                "display_relay_skipped event={event} no_child_pipe"
            ));
            return;
        };
        match pipe
            .write_all(
                format!(
                    "{line}
"
                )
                .as_bytes(),
            )
            .and_then(|()| pipe.flush())
        {
            Ok(()) => state().log(&format!("display_relay event={event} line={line}")),
            Err(e) => {
                state().log(&format!("display_relay_failed event={event} err={e}"));
                sup.events = None;
            }
        }
    }

    unsafe extern "C" fn on_powerd_event(
        _lipc: LipcHandle,
        name: *const c_char,
        _event: *mut c_void,
        _data: *mut c_void,
    ) -> LIPCcode {
        let event = cstr(name).to_string();
        state().log(&format!("powerd_event name={event}"));
        relay_display_event(&event);
        LIPC_OK
    }

    fn ensure_log_parent(path: &str) {
        if let Some(parent) = std::path::Path::new(path).parent() {
            let _ = std::fs::create_dir_all(parent);
        }
    }

    pub fn run() {
        let args = parse_args();
        ensure_log_parent(&args.log_path);
        let _ = CHILD_LOG.set(args.child_log.clone());

        let child = args.child_bin.as_ref().map(|bin| ChildSupervisor {
            child_bin: bin.clone(),
            child_args: args.child_args.clone(),
            child: None,
            events: None,
            health: None,
            evidence: None,
        });

        let st = LifecycleState {
            service_name: args.service.clone(),
            log_path: args.log_path.clone(),
            done: AtomicBool::new(false),
            unload_seen: AtomicBool::new(false),
            pause_seen: AtomicBool::new(false),
            go_seen: AtomicBool::new(false),
            load_seen: AtomicBool::new(false),
            child: Mutex::new(child),
        };
        let _ = STATE.set(st);

        state().log(&format!(
            "lifecycle_start service={} probe={} lipc_so={} pid={}",
            args.service,
            args.probe,
            args.lipc_so.display(),
            std::process::id()
        ));

        let loaded = match LipcLib::load(&args.lipc_so) {
            Ok(l) => l,
            Err(e) => {
                state().log(&format!("lipc_load_failed err={e}"));
                eprintln!("lipc_load_failed: {e}");
                std::process::exit(1);
            }
        };
        let _ = LIB.set(loaded);

        let handle = match lib().open_ex(&args.service) {
            Ok(h) => h,
            Err(e) => {
                state().log(&format!("lipc_open_failed err={e}"));
                eprintln!("lipc_open_failed: {e}");
                std::process::exit(1);
            }
        };

        for (name, cb) in [
            ("load", Some(on_load as _)),
            ("go", Some(on_go as _)),
            ("pause", Some(on_pause as _)),
            ("unload", Some(on_unload as _)),
        ] {
            if let Err(e) = lib().register_string_setter(handle, name, cb, std::ptr::null_mut()) {
                state().log(&format!("register_failed prop={name} err={e}"));
                lib().close(handle);
                std::process::exit(1);
            }
            state().log(&format!("registered_prop prop={name}"));
        }

        if !args.probe {
            // Failure is not fatal: scribe-app's first-contact fail-safe still prevents any
            // invisible UI target from activating after a wake.
            for event in [EVENT_GOING_TO_SCREENSAVER, EVENT_EXITING_SCREENSAVER] {
                match lib().subscribe(handle, POWERD, event, Some(on_powerd_event as _)) {
                    Ok(()) => state().log(&format!("subscribed source={POWERD} event={event}")),
                    Err(e) => state().log(&format!("subscribe_failed event={event} err={e}")),
                }
            }
        }

        let run = runresult_value(&args.service);
        if let Err(e) = lib().set_string(handle, APPMGRD, "runresult", &run) {
            state().log(&format!("runresult_failed err={e}"));
            lib().close(handle);
            std::process::exit(1);
        }
        state().log(&format!("runresult_set value={run}"));

        while !state().done.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(100));
        }

        state().log(&format!(
            "lifecycle_exit load={} go={} pause={} unload={}",
            state().load_seen.load(Ordering::SeqCst),
            state().go_seen.load(Ordering::SeqCst),
            state().pause_seen.load(Ordering::SeqCst),
            state().unload_seen.load(Ordering::SeqCst)
        ));
        lib().close(handle);
        std::process::exit(0);
    }
}

#[cfg(unix)]
fn main() {
    unix_main::run();
}

#[cfg(not(unix))]
fn main() {
    eprintln!("scribe-lifecycle is Kindle/Linux-only");
    std::process::exit(1);
}
