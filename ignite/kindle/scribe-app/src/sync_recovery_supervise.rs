//! Disposable-only supervised `dbclient -B` session (Slice 18E.10L).
//!
//! Owns one child process and its pipes for a single HTTP exchange. Not reachable
//! from ordinary startup or [`crate::sync_recovery_exec::submit_recovery`]. Spawn
//! requires [`DisposableStreamPermit`], which has no production constructor.
#![allow(dead_code)]

use std::io::{self, Read, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::sync_recovery_policy::LIVE_NON_LOOPBACK_POST_ENABLED;
use crate::sync_recovery_stream::{
    build_dropbear_stream_invocation, is_dropbearmulti_basename, recovery_http1_exchange,
    recovery_stream_child_clear_vars, RecoveryHttp1Request, RecoveryHttp1Response,
    RecoveryStreamPlan, STREAM_IO_TIMEOUT,
};
use crate::sync_recovery_tunnel::{DropbearInvocation, HostKeyTrustInput};

pub const STREAM_CHILD_TERM: Duration = Duration::from_secs(2);
pub const STREAM_STDERR_CAP: usize = 8 * 1024;
#[cfg(not(test))]
const SUPERVISE_DEADLINE: Duration = crate::sync_recovery_stream::STREAM_EXCHANGE_DEADLINE;
#[cfg(test)]
const SUPERVISE_DEADLINE: Duration = Duration::from_secs(3);

/// Compile-time isolation: only tests / the Kindle supervisor harness can construct this token.
pub struct DisposableStreamPermit {
    _priv: (),
}

#[cfg(any(test, feature = "kindle_supervisor_harness"))]
impl DisposableStreamPermit {
    pub(crate) fn for_synthetic_fixture() -> Self {
        Self { _priv: () }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamStage {
    PlanValidated,
    TrustValidated,
    InvocationBuilt,
    ChildStarted,
    HttpWritten,
    HttpComplete,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamTransportOutcome {
    PreWriteRefuse { reason: String },
    WriteStartedAmbiguous { reason: String },
    Authoritative { status: u16, body: Vec<u8> },
}

pub struct SupervisedRecoveryStream {
    child: Arc<Mutex<Option<Child>>>,
    stdin: Option<ChildStdin>,
    stdout: Option<ChildStdout>,
    stderr_join: Option<thread::JoinHandle<Vec<u8>>>,
    pid: u32,
    invocation: DropbearInvocation,
    started: bool,
}

#[cfg(test)]
static LIVE_WATCHDOGS: AtomicUsize = AtomicUsize::new(0);
#[cfg(test)]
static LIVE_STDERR_DRAINS: AtomicUsize = AtomicUsize::new(0);

struct ChildStdio {
    stdin: ChildStdin,
    stdout: ChildStdout,
}

impl Read for ChildStdio {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.stdout.read(buf)
    }
}

impl Write for ChildStdio {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.stdin.write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.stdin.flush()
    }
}

impl SupervisedRecoveryStream {
    pub fn open(
        _permit: DisposableStreamPermit,
        plan: &RecoveryStreamPlan<'_>,
        host_key: &HostKeyTrustInput<'_>,
    ) -> Result<Self, StreamTransportOutcome> {
        if LIVE_NON_LOOPBACK_POST_ENABLED {
            return Err(StreamTransportOutcome::PreWriteRefuse {
                reason: "REFUSE RECOVERY STREAM: non-loopback recovery POST is not enabled".into(),
            });
        }
        let inv = build_dropbear_stream_invocation(plan, host_key).map_err(|e| {
            StreamTransportOutcome::PreWriteRefuse {
                reason: e.join("; "),
            }
        })?;
        spawn_invocation(inv)
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }

    pub fn argv(&self) -> &[String] {
        &self.invocation.argv
    }

    pub fn program(&self) -> &str {
        &self.invocation.program
    }

    pub fn exchange(
        &mut self,
        plan: &RecoveryStreamPlan<'_>,
        req: &RecoveryHttp1Request<'_>,
    ) -> StreamTransportOutcome {
        if LIVE_NON_LOOPBACK_POST_ENABLED {
            self.terminate();
            return StreamTransportOutcome::PreWriteRefuse {
                reason: "REFUSE RECOVERY STREAM: non-loopback recovery POST is not enabled".into(),
            };
        }
        if req.host.contains("127.0.0.1")
            || req.host.eq_ignore_ascii_case("localhost")
            || req.host.contains("::1")
        {
            self.terminate();
            return StreamTransportOutcome::PreWriteRefuse {
                reason: "REFUSE RECOVERY STREAM: loopback Host is not a stream destination".into(),
            };
        }
        let Some(stdin) = self.stdin.take() else {
            self.terminate();
            return StreamTransportOutcome::PreWriteRefuse {
                reason: "REFUSE RECOVERY STREAM: stdin already consumed".into(),
            };
        };
        let Some(stdout) = self.stdout.take() else {
            self.terminate();
            return StreamTransportOutcome::PreWriteRefuse {
                reason: "REFUSE RECOVERY STREAM: stdout already consumed".into(),
            };
        };
        let mut stdio = ChildStdio { stdin, stdout };
        let done = Arc::new(AtomicBool::new(false));
        let wd_done = done.clone();
        let wd_child = self.child.clone();
        let started = Instant::now();
        #[cfg(test)]
        LIVE_WATCHDOGS.fetch_add(1, Ordering::SeqCst);
        let watchdog = thread::spawn(move || {
            while started.elapsed() < SUPERVISE_DEADLINE {
                if wd_done.load(Ordering::SeqCst) {
                    break;
                }
                thread::sleep(Duration::from_millis(20));
            }
            if !wd_done.load(Ordering::SeqCst) {
                if let Ok(mut g) = wd_child.lock() {
                    if let Some(child) = g.as_mut() {
                        let _ = child.kill();
                    }
                }
            }
            #[cfg(test)]
            LIVE_WATCHDOGS.fetch_sub(1, Ordering::SeqCst);
        });
        let result = recovery_http1_exchange(&mut stdio, plan, req);
        done.store(true, Ordering::SeqCst);
        drop(stdio);
        let _ = watchdog.join();
        let timed_out = started.elapsed() >= SUPERVISE_DEADLINE;
        match result {
            Ok(RecoveryHttp1Response { status, body }) => {
                self.reap_after_stdio();
                StreamTransportOutcome::Authoritative { status, body }
            }
            Err(reason) => {
                self.terminate();
                if timed_out {
                    StreamTransportOutcome::WriteStartedAmbiguous {
                        reason: "AMBIGUOUS RECOVERY STREAM: exchange deadline exceeded".into(),
                    }
                } else {
                    classify_reason(reason)
                }
            }
        }
    }

    pub fn cancel(&mut self) {
        self.terminate();
    }

    fn reap_after_stdio(&mut self) {
        self.wait_child(false);
        self.join_stderr();
        self.stdin.take();
        self.stdout.take();
    }

    fn terminate(&mut self) {
        self.stdin.take();
        self.stdout.take();
        self.wait_child(true);
        self.join_stderr();
    }

    fn wait_child(&mut self, kill_first: bool) {
        let child = match self.child.lock() {
            Ok(mut g) => g.take(),
            Err(p) => p.into_inner().take(),
        };
        let Some(mut child) = child else {
            return;
        };
        if kill_first {
            let _ = child.kill();
            let _ = child.wait();
            return;
        }
        let deadline = Instant::now() + STREAM_CHILD_TERM;
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if Instant::now() >= deadline => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break;
                }
                Ok(None) => thread::sleep(Duration::from_millis(20)),
                Err(_) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break;
                }
            }
        }
    }

    fn join_stderr(&mut self) {
        if let Some(h) = self.stderr_join.take() {
            let _ = h.join();
        }
    }
}

impl Drop for SupervisedRecoveryStream {
    fn drop(&mut self) {
        self.terminate();
    }
}

fn spawn_invocation(
    inv: DropbearInvocation,
) -> Result<SupervisedRecoveryStream, StreamTransportOutcome> {
    let base = Path::new(&inv.program).file_name().and_then(|n| n.to_str());
    if !is_dropbearmulti_basename(base) {
        return Err(StreamTransportOutcome::PreWriteRefuse {
            reason: "REFUSE RECOVERY STREAM: executable must be dropbearmulti".into(),
        });
    }
    if inv
        .argv
        .iter()
        .any(|a| a == "-L" || a.contains("127.0.0.1"))
    {
        return Err(StreamTransportOutcome::PreWriteRefuse {
            reason: "REFUSE RECOVERY STREAM: local TCP listener forwarding is not permitted".into(),
        });
    }
    let mut cmd = Command::new(&inv.program);
    if inv.argv.len() > 1 {
        cmd.args(&inv.argv[1..]);
    }
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env_clear()
        .env("HOME", &inv.home)
        .current_dir(&inv.home);
    #[cfg(windows)]
    {
        if let Ok(v) = std::env::var("SystemRoot") {
            cmd.env("SystemRoot", v);
        }
        if let Ok(v) = std::env::var("WINDIR") {
            cmd.env("WINDIR", v);
        }
    }
    for var in recovery_stream_child_clear_vars() {
        cmd.env_remove(var);
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| StreamTransportOutcome::PreWriteRefuse {
            reason: format!("REFUSE RECOVERY STREAM: child startup failure: {e}"),
        })?;
    let pid = child.id();
    let stdin = child.stdin.take();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let stderr_join = stderr.map(|mut err| {
        thread::spawn(move || {
            #[cfg(test)]
            LIVE_STDERR_DRAINS.fetch_add(1, Ordering::SeqCst);
            let mut buf = Vec::new();
            let mut tmp = [0u8; 256];
            loop {
                match err.read(&mut tmp) {
                    Ok(0) => break,
                    Ok(n) => {
                        if buf.len() < STREAM_STDERR_CAP {
                            let take = n.min(STREAM_STDERR_CAP - buf.len());
                            buf.extend_from_slice(&tmp[..take]);
                        }
                    }
                    Err(_) => break,
                }
            }
            #[cfg(test)]
            LIVE_STDERR_DRAINS.fetch_sub(1, Ordering::SeqCst);
            buf
        })
    });
    let Some(stdin) = stdin else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(StreamTransportOutcome::PreWriteRefuse {
            reason: "REFUSE RECOVERY STREAM: child stdin missing".into(),
        });
    };
    let Some(stdout) = stdout else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(StreamTransportOutcome::PreWriteRefuse {
            reason: "REFUSE RECOVERY STREAM: child stdout missing".into(),
        });
    };
    let _ = STREAM_IO_TIMEOUT;
    Ok(SupervisedRecoveryStream {
        child: Arc::new(Mutex::new(Some(child))),
        stdin: Some(stdin),
        stdout: Some(stdout),
        stderr_join,
        pid,
        invocation: inv,
        started: true,
    })
}

fn classify_reason(reason: String) -> StreamTransportOutcome {
    if reason.contains("AMBIGUOUS") {
        StreamTransportOutcome::WriteStartedAmbiguous { reason }
    } else {
        StreamTransportOutcome::PreWriteRefuse { reason }
    }
}

#[allow(dead_code)]
pub fn stream_stage_order() -> &'static [StreamStage] {
    &[
        StreamStage::PlanValidated,
        StreamStage::TrustValidated,
        StreamStage::InvocationBuilt,
        StreamStage::ChildStarted,
        StreamStage::HttpWritten,
        StreamStage::HttpComplete,
    ]
}

#[cfg(test)]
pub(crate) fn spawn_started_flag(s: &SupervisedRecoveryStream) -> bool {
    s.started
}

#[cfg(test)]
pub(crate) fn live_watchdogs() -> usize {
    LIVE_WATCHDOGS.load(Ordering::SeqCst)
}

#[cfg(test)]
pub(crate) fn live_stderr_drains() -> usize {
    LIVE_STDERR_DRAINS.load(Ordering::SeqCst)
}

#[cfg(any(test, feature = "kindle_supervisor_harness"))]
pub(crate) fn stream_resources_released(s: &SupervisedRecoveryStream) -> bool {
    let child_gone = match s.child.lock() {
        Ok(g) => g.is_none(),
        Err(p) => p.into_inner().is_none(),
    };
    child_gone && s.stdin.is_none() && s.stdout.is_none() && s.stderr_join.is_none()
}
