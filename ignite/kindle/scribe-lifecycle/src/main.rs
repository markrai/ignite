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

mod protocol;

#[cfg(unix)]
mod lipc;

#[cfg(unix)]
mod unix_main {
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
        let Some(child) = sup.child.as_mut() else {
            state().log(&format!("child_stop skipped reason={reason} no_child"));
            return true;
        };
        let pid = child.id();
        // Close the relay pipe first; the child's reader thread sees EOF.
        sup.events = None;
        state().log(&format!(
            "child_stop reason={reason} pid={pid} signal=SIGTERM budget_s={}",
            CHILD_SHUTDOWN_BUDGET.as_secs()
        ));
        if let Err(e) = signal_term(pid) {
            state().log(&format!("child_sigterm_failed pid={pid} err={e}"));
        }
        let deadline = Instant::now() + CHILD_SHUTDOWN_BUDGET;
        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    state().log(&format!(
                        "child_exited pid={pid} status={status} reason={reason}"
                    ));
                    sup.child = None;
                    return true;
                }
                Ok(None) => {
                    if Instant::now() >= deadline {
                        state().log(&format!(
                            "child_shutdown_timeout pid={pid} budget_s={} FAIL",
                            CHILD_SHUTDOWN_BUDGET.as_secs()
                        ));
                        return false;
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(e) => {
                    state().log(&format!("child_wait_failed pid={pid} err={e}"));
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
            match existing.try_wait() {
                Ok(Some(status)) => {
                    state().log(&format!(
                        "previous_child_exited status={status}; clearing before relaunch"
                    ));
                    sup.child = None;
                }
                Ok(None) => {
                    state().log(&format!(
                        "go_skipped_already_running pid={} payload={payload:?}",
                        existing.id()
                    ));
                    return;
                }
                Err(e) => {
                    state().log(&format!("go_child_poll_failed err={e}"));
                    return;
                }
            }
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
        match Command::new(&sup.child_bin)
            .args(&sup.child_args)
            .arg(CHILD_DISPLAY_EVENTS_FLAG)
            .stdin(Stdio::piped())
            .stdout(stdout)
            .stderr(stderr)
            .spawn()
        {
            Ok(child) => {
                state().log(&format!(
                    "child_started pid={} child_log={child_log_path}",
                    child.id()
                ));
                let watch_pid = child.id();
                std::thread::spawn(move || watch_child(watch_pid));
                let mut child = child;
                sup.events = child.stdin.take();
                sup.child = Some(child);
            }
            Err(e) => {
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
                let Some(child) = sup.child.as_mut().filter(|c| c.id() == pid) else {
                    return; // stopped deliberately (or replaced)
                };
                match child.try_wait() {
                    Ok(None) => continue,
                    Ok(Some(status)) => {
                        sup.child = None;
                        sup.events = None;
                        status
                    }
                    Err(e) => {
                        state().log(&format!("child_watch_failed pid={pid} err={e}"));
                        return;
                    }
                }
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
