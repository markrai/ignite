//! Test support for asserting that a released `flock` becomes available again.
//!
//! A `flock` belongs to the open file description, not to the descriptor. When a test drops a
//! lock while another test thread is between `fork` and `exec` of a subprocess, that child holds
//! a copy of every descriptor, `O_CLOEXEC` ones included, until its `exec` closes them. The
//! released lock therefore stays held by the shared description for that window. Under the
//! parallel harness, release is eventual rather than instantaneous, so a re-acquisition after
//! release waits up to a real deadline and reports who still holds the lock if the deadline passes.

use std::fs;
use std::path::Path;
use std::time::Duration;

/// Long enough to cover a sibling's fork-to-exec window on a loaded machine. A lock that is
/// genuinely never released still fails.
pub const RELEASE_VISIBILITY_DEADLINE: Duration = Duration::from_secs(2);

/// Every process whose descriptor table refers to `path`, with descriptor flags (`02000000` is
/// `O_CLOEXEC`) and executable image, for diagnosing a lock that outlives its release.
pub fn descriptor_holders(path: &Path) -> Vec<String> {
    let mut holders = Vec::new();
    let Ok(processes) = fs::read_dir("/proc") else {
        return holders;
    };
    let current = std::process::id().to_string();
    for process in processes.flatten() {
        let pid = process.file_name().to_string_lossy().into_owned();
        // Skip non-process entries and the `self`/`thread-self` aliases of this process.
        if !pid.bytes().all(|byte| byte.is_ascii_digit()) {
            continue;
        }
        let Ok(descriptors) = fs::read_dir(process.path().join("fd")) else {
            continue;
        };
        for descriptor in descriptors.flatten() {
            if fs::read_link(descriptor.path()).ok().as_deref() != Some(path) {
                continue;
            }
            let fd = descriptor.file_name().to_string_lossy().into_owned();
            let flags = fs::read_to_string(process.path().join("fdinfo").join(&fd))
                .ok()
                .and_then(|info| {
                    info.lines()
                        .find_map(|line| line.strip_prefix("flags:").map(|v| v.trim().to_owned()))
                })
                .unwrap_or_else(|| "?".to_owned());
            let exe = fs::read_link(process.path().join("exe"))
                .map(|exe| exe.display().to_string())
                .unwrap_or_else(|_| "?".to_owned());
            let role = if pid == current {
                " (this test process)"
            } else {
                ""
            };
            holders.push(format!("pid={pid}{role} fd={fd} flags={flags} exe={exe}"));
        }
    }
    holders
}
