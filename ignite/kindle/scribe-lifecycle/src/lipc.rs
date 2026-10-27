//! Kindle LIPC runtime loader (`/usr/lib/liblipc.so.1`).

#![allow(non_camel_case_types, dead_code)]

use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_void};
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

pub type LIPCcode = c_int;
pub const LIPC_OK: LIPCcode = 0;

pub type LipcHandle = *mut c_void;

pub type LipcPropCallback = Option<
    unsafe extern "C" fn(
        lipc: LipcHandle,
        property: *const c_char,
        value: *mut c_void,
        data: *mut c_void,
    ) -> LIPCcode,
>;

/// `LipcEventCallback(lipc, event_name, event, data)` from openlipc.h.
pub type LipcEventCallback = Option<
    unsafe extern "C" fn(
        lipc: LipcHandle,
        name: *const c_char,
        event: *mut c_void,
        data: *mut c_void,
    ) -> LIPCcode,
>;

type FnLipcOpenEx = unsafe extern "C" fn(*const c_char, *mut LIPCcode) -> LipcHandle;
type FnLipcClose = unsafe extern "C" fn(LipcHandle);
type FnLipcRegisterStringProperty = unsafe extern "C" fn(
    LipcHandle,
    *const c_char,
    LipcPropCallback,
    LipcPropCallback,
    *mut c_void,
) -> LIPCcode;
type FnLipcSetStringProperty =
    unsafe extern "C" fn(LipcHandle, *const c_char, *const c_char, *const c_char) -> LIPCcode;
type FnLipcGetErrorString = unsafe extern "C" fn(LIPCcode) -> *const c_char;
type FnLipcSubscribeExt = unsafe extern "C" fn(
    LipcHandle,
    *const c_char,
    *const c_char,
    LipcEventCallback,
    *mut c_void,
) -> LIPCcode;

pub struct LipcLib {
    _lib: libloading::Library,
    open_ex: FnLipcOpenEx,
    close: FnLipcClose,
    register_string: FnLipcRegisterStringProperty,
    set_string: FnLipcSetStringProperty,
    get_error: FnLipcGetErrorString,
    subscribe_ext: FnLipcSubscribeExt,
}

impl LipcLib {
    pub fn load(path: &Path) -> Result<Self, String> {
        unsafe {
            let lib = libloading::Library::new(path)
                .map_err(|e| format!("dlopen {}: {e}", path.display()))?;
            let open_ex = *lib
                .get::<FnLipcOpenEx>(b"LipcOpenEx\0")
                .map_err(|e| format!("LipcOpenEx: {e}"))?;
            let close = *lib
                .get::<FnLipcClose>(b"LipcClose\0")
                .map_err(|e| format!("LipcClose: {e}"))?;
            let register_string = *lib
                .get::<FnLipcRegisterStringProperty>(b"LipcRegisterStringProperty\0")
                .map_err(|e| format!("LipcRegisterStringProperty: {e}"))?;
            let set_string = *lib
                .get::<FnLipcSetStringProperty>(b"LipcSetStringProperty\0")
                .map_err(|e| format!("LipcSetStringProperty: {e}"))?;
            let get_error = *lib
                .get::<FnLipcGetErrorString>(b"LipcGetErrorString\0")
                .map_err(|e| format!("LipcGetErrorString: {e}"))?;
            let subscribe_ext = *lib
                .get::<FnLipcSubscribeExt>(b"LipcSubscribeExt\0")
                .map_err(|e| format!("LipcSubscribeExt: {e}"))?;
            Ok(Self {
                _lib: lib,
                open_ex,
                close,
                register_string,
                set_string,
                get_error,
                subscribe_ext,
            })
        }
    }

    pub fn open_ex(&self, service: &str) -> Result<LipcHandle, String> {
        let c = CString::new(service).map_err(|e| e.to_string())?;
        let mut code: LIPCcode = LIPC_OK;
        let h = unsafe { (self.open_ex)(c.as_ptr(), &mut code) };
        if h.is_null() || code != LIPC_OK {
            return Err(format!(
                "LipcOpenEx({service}) failed code={code} ({})",
                self.error_string(code)
            ));
        }
        Ok(h)
    }

    pub fn close(&self, h: LipcHandle) {
        if !h.is_null() {
            unsafe { (self.close)(h) }
        }
    }

    pub fn register_string_setter(
        &self,
        h: LipcHandle,
        property: &str,
        setter: LipcPropCallback,
        data: *mut c_void,
    ) -> Result<(), String> {
        let c = CString::new(property).map_err(|e| e.to_string())?;
        let code = unsafe { (self.register_string)(h, c.as_ptr(), None, setter, data) };
        if code != LIPC_OK {
            return Err(format!(
                "LipcRegisterStringProperty({property}) code={code} ({})",
                self.error_string(code)
            ));
        }
        Ok(())
    }

    pub fn set_string(
        &self,
        h: LipcHandle,
        service: &str,
        property: &str,
        value: &str,
    ) -> Result<(), String> {
        let s = CString::new(service).map_err(|e| e.to_string())?;
        let p = CString::new(property).map_err(|e| e.to_string())?;
        let v = CString::new(value).map_err(|e| e.to_string())?;
        let code = unsafe { (self.set_string)(h, s.as_ptr(), p.as_ptr(), v.as_ptr()) };
        if code != LIPC_OK {
            return Err(format!(
                "LipcSetStringProperty({service},{property}) code={code} ({})",
                self.error_string(code)
            ));
        }
        Ok(())
    }

    /// Subscribe to one named event from `source` (callbacks run on the LIPC thread).
    pub fn subscribe(
        &self,
        h: LipcHandle,
        source: &str,
        event: &str,
        callback: LipcEventCallback,
    ) -> Result<(), String> {
        let s = CString::new(source).map_err(|e| e.to_string())?;
        let e = CString::new(event).map_err(|e| e.to_string())?;
        let code = unsafe {
            (self.subscribe_ext)(h, s.as_ptr(), e.as_ptr(), callback, std::ptr::null_mut())
        };
        if code != LIPC_OK {
            return Err(format!(
                "LipcSubscribeExt({source},{event}) code={code} ({})",
                self.error_string(code)
            ));
        }
        Ok(())
    }

    fn error_string(&self, code: LIPCcode) -> String {
        unsafe {
            let p = (self.get_error)(code);
            if p.is_null() {
                return format!("code={code}");
            }
            CStr::from_ptr(p).to_string_lossy().into_owned()
        }
    }
}

pub struct LifecycleState {
    pub service_name: String,
    pub log_path: String,
    pub done: AtomicBool,
    pub unload_seen: AtomicBool,
    pub pause_seen: AtomicBool,
    pub go_seen: AtomicBool,
    pub load_seen: AtomicBool,
    /// Present only when the host established all cross-process coordination primitives. If setup
    /// fails, ordinary launches remain available but every future decision boundary refuses.
    pub launch_coordination: Option<Arc<crate::launch_gate::HostLaunchCoordination>>,
    /// Lifetime shared gate held when full coordination setup failed. This prevents a stale or
    /// different host lease from authorizing a decision while this host can still launch.
    pub degraded_decision_blocker: Option<crate::launch_gate::LaunchAdmission>,
    pub launch_coordination_failed: bool,
    pub child: Mutex<Option<ChildSupervisor>>,
}

pub struct ChildSupervisor {
    pub child_bin: String,
    pub child_args: Vec<String>,
    pub child: Option<std::process::Child>,
    /// F11.1: display-event relay pipe (child stdin). `None` when the child is not running.
    pub events: Option<std::process::ChildStdin>,
    /// Observation-only readiness for the currently associated child process.
    pub health: Option<crate::launch_health::LaunchHealthObserver>,
    /// Nonblocking sender for best-effort, candidate-bound durable evidence.
    pub evidence: Option<crate::candidate_health::EvidenceObserver>,
    /// Kernel activity token retained by both host and child. An orphan child therefore remains
    /// visible to a future exclusive decision after lifecycle-host death.
    pub running_activity: Option<crate::launch_gate::RunningChildActivity>,
}

impl LifecycleState {
    pub fn log(&self, msg: &str) {
        use std::io::Write;
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let line = format!("ts={secs} {msg}\n");
        let _ = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log_path)
            .and_then(|mut f| f.write_all(line.as_bytes()));
        print!("{line}");
        let _ = std::io::stdout().flush();
    }
}
