//! Gate F11.1 display-event relay from the `scribe-lifecycle` host.
//!
//! The host owns the Kindle LIPC boundary and forwards two proven FW 5.18.6 powerd events as
//! lines on this process's stdin (`--display-events-stdin`):
//!
//! - `display-invalidate` — `goingToScreenSaver`: the screensaver is about to own `/dev/fb0`.
//! - `display-restore` — `exitingScreenSaver`: blanket has unmapped the screensaver and cleared
//!   the panel; Scribe must re-present its current UI.
//!
//! A reader thread only bumps atomic generation counters. No framebuffer, storage, or UI work
//! happens off the main loop.
//!
//! The pipe also proves the host is alive: the host closes it before every deliberate stop,
//! and it closes when the host dies. EOF therefore ends the hosted session cleanly (the
//! session's stop flag) — a child never outlives its host holding the singleton, the stylus
//! grab or the panel. One reader serves the whole process; each page session (including an
//! in-process restart) retargets it at its own counters and stop flag.

use std::io::BufRead;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

pub const RELAY_INVALIDATE: &str = "display-invalidate";
pub const RELAY_RESTORE: &str = "display-restore";

#[derive(Debug, Default)]
pub struct DisplayEventCounters {
    invalidations: AtomicU64,
    restores: AtomicU64,
}

impl DisplayEventCounters {
    /// Apply one relay line. Unknown lines are ignored (forward compatibility).
    pub fn apply_line(&self, line: &str) -> bool {
        match line.trim() {
            RELAY_INVALIDATE => {
                self.invalidations.fetch_add(1, Ordering::SeqCst);
                true
            }
            RELAY_RESTORE => {
                self.restores.fetch_add(1, Ordering::SeqCst);
                true
            }
            _ => false,
        }
    }

    fn snapshot(&self) -> (u64, u64) {
        (
            self.invalidations.load(Ordering::SeqCst),
            self.restores.load(Ordering::SeqCst),
        )
    }
}

/// What the main loop must do after draining new relay events.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DisplayEvents {
    pub invalidated: bool,
    pub restored: bool,
}

/// Main-loop side: remembers the generations already handled.
#[derive(Debug)]
pub struct DisplayEventReceiver {
    counters: Arc<DisplayEventCounters>,
    seen: (u64, u64),
}

impl DisplayEventReceiver {
    pub fn new(counters: Arc<DisplayEventCounters>) -> Self {
        let seen = counters.snapshot();
        Self { counters, seen }
    }

    pub fn take(&mut self) -> DisplayEvents {
        let now = self.counters.snapshot();
        let out = DisplayEvents {
            invalidated: now.0 != self.seen.0,
            restored: now.1 != self.seen.1,
        };
        self.seen = now;
        out
    }
}

/// Where relay lines go: the current page session's counters and stop flag.
type RelayTarget = (Arc<DisplayEventCounters>, Arc<AtomicBool>);

static RELAY_TARGET: Mutex<Option<RelayTarget>> = Mutex::new(None);
static RELAY_STARTED: AtomicBool = AtomicBool::new(false);
static HOST_GONE: AtomicBool = AtomicBool::new(false);

/// Feed relay lines from `input` to the current target until EOF (or a read error).
fn relay_until_eof(input: impl BufRead, target: &Mutex<Option<RelayTarget>>) {
    for line in input.lines() {
        let Ok(line) = line else {
            break;
        };
        if let Some((counters, _)) = target.lock().unwrap_or_else(|p| p.into_inner()).as_ref() {
            counters.apply_line(&line);
        }
    }
}

/// The host closed the relay pipe: end the current session and never restart in-process.
fn host_gone(target: &Mutex<Option<RelayTarget>>, gone: &AtomicBool) {
    gone.store(true, Ordering::SeqCst);
    if let Some((_, stop)) = target.lock().unwrap_or_else(|p| p.into_inner()).as_ref() {
        stop.store(true, Ordering::SeqCst);
    }
}

/// Route the stdin relay to this page session (`counters`, and `stop` on host EOF), starting
/// the one process-wide reader on first use.
pub fn spawn_stdin_reader(counters: Arc<DisplayEventCounters>, stop: Arc<AtomicBool>) {
    *RELAY_TARGET.lock().unwrap_or_else(|p| p.into_inner()) = Some((counters, stop));
    if HOST_GONE.load(Ordering::SeqCst) {
        host_gone(&RELAY_TARGET, &HOST_GONE);
        return;
    }
    if RELAY_STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    std::thread::spawn(|| {
        relay_until_eof(std::io::stdin().lock(), &RELAY_TARGET);
        println!("display_events=host relay closed; ending the hosted session");
        host_gone(&RELAY_TARGET, &HOST_GONE);
    });
}

/// The hosting `scribe-lifecycle` has gone (its relay pipe closed): no in-process restart.
pub fn host_relay_closed() -> bool {
    HOST_GONE.load(Ordering::SeqCst)
}

/// Map one `lipc-wait-event` output line for powerd to a relay line (the event name may be
/// preceded by a timestamp or followed by arguments).
pub fn relay_for_powerd_output(line: &str) -> Option<&'static str> {
    line.split_whitespace().find_map(|t| match t {
        "goingToScreenSaver" => Some(RELAY_INVALIDATE),
        "exitingScreenSaver" => Some(RELAY_RESTORE),
        _ => None,
    })
}

static POWERD_WATCHER: std::sync::Mutex<Option<std::process::Child>> = std::sync::Mutex::new(None);
const LIPC_WAIT_EVENT: &str = "/usr/bin/lipc-wait-event";

/// Non-hosted runs (e.g. SSH-launched test sessions) have no `scribe-lifecycle` relay: watch the
/// SAME two powerd events with the device's `lipc-wait-event` and feed the same counters.
/// Returns false when unavailable (off-device). Stopped by [`stop_powerd_watcher`].
pub fn spawn_powerd_watcher(counters: Arc<DisplayEventCounters>) -> bool {
    if !std::path::Path::new(LIPC_WAIT_EVENT).exists() {
        return false;
    }
    let child = std::process::Command::new(LIPC_WAIT_EVENT)
        .args([
            "-m",
            "com.lab126.powerd",
            "goingToScreenSaver,exitingScreenSaver",
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn();
    let Ok(mut child) = child else {
        return false;
    };
    let Some(stdout) = child.stdout.take() else {
        let _ = child.kill();
        return false;
    };
    *POWERD_WATCHER.lock().unwrap() = Some(child);
    std::thread::spawn(move || {
        for line in std::io::BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if let Some(relay) = relay_for_powerd_output(&line) {
                counters.apply_line(relay);
            }
        }
    });
    true
}

/// Stop the non-hosted powerd watcher (idempotent).
pub fn stop_powerd_watcher() {
    if let Some(mut c) = POWERD_WATCHER.lock().unwrap().take() {
        let _ = c.kill();
        let _ = c.wait();
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn relay_follows_the_current_session_and_eof_stops_it() {
        let target: Mutex<Option<RelayTarget>> = Mutex::new(None);
        let gone = AtomicBool::new(false);
        let (first, stop1) = (
            Arc::new(DisplayEventCounters::default()),
            Arc::new(AtomicBool::new(false)),
        );
        *target.lock().unwrap() = Some((Arc::clone(&first), Arc::clone(&stop1)));
        relay_until_eof(
            std::io::Cursor::new(
                "display-invalidate
",
            ),
            &target,
        );
        assert_eq!(first.snapshot(), (1, 0));
        // An in-process restart retargets the same reader: lines reach only the new session.
        let (second, stop2) = (
            Arc::new(DisplayEventCounters::default()),
            Arc::new(AtomicBool::new(false)),
        );
        *target.lock().unwrap() = Some((Arc::clone(&second), Arc::clone(&stop2)));
        relay_until_eof(
            std::io::Cursor::new(
                "display-restore
unknown
",
            ),
            &target,
        );
        assert_eq!((first.snapshot(), second.snapshot()), ((1, 0), (0, 1)));
        // EOF = the host is gone: the current session stops (the previous one is untouched).
        assert!(!stop2.load(Ordering::SeqCst));
        host_gone(&target, &gone);
        assert!(stop2.load(Ordering::SeqCst) && gone.load(Ordering::SeqCst));
        assert!(!stop1.load(Ordering::SeqCst));
    }

    use super::*;

    #[test]
    fn powerd_output_lines_map_to_the_same_relay() {
        assert_eq!(
            relay_for_powerd_output("goingToScreenSaver"),
            Some(RELAY_INVALIDATE)
        );
        assert_eq!(
            relay_for_powerd_output("exitingScreenSaver 1"),
            Some(RELAY_RESTORE)
        );
        assert_eq!(
            relay_for_powerd_output("1790000000.123 goingToScreenSaver 2"),
            Some(RELAY_INVALIDATE)
        );
        assert_eq!(relay_for_powerd_output("outOfScreenSaver 1"), None);
        assert_eq!(relay_for_powerd_output(""), None);
    }

    #[test]
    fn relay_lines_bump_generations_and_repeated_cycles_are_not_lost() {
        let c = Arc::new(DisplayEventCounters::default());
        let mut rx = DisplayEventReceiver::new(Arc::clone(&c));
        assert_eq!(rx.take(), DisplayEvents::default());
        assert!(c.apply_line("display-invalidate\n"));
        assert_eq!(
            rx.take(),
            DisplayEvents {
                invalidated: true,
                restored: false
            }
        );
        assert_eq!(rx.take(), DisplayEvents::default());
        // Two full cycles between polls: both kinds still observed.
        for _ in 0..2 {
            c.apply_line(RELAY_INVALIDATE);
            c.apply_line(RELAY_RESTORE);
        }
        assert_eq!(
            rx.take(),
            DisplayEvents {
                invalidated: true,
                restored: true
            }
        );
        assert!(!c.apply_line("unrelated"));
        assert_eq!(rx.take(), DisplayEvents::default());
    }
}
