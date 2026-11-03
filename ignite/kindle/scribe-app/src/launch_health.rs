//! Child side of the observation-only launch-health handshake.
//!
//! The lifecycle host passes a fresh connected Unix datagram descriptor and matching UUID in
//! two private environment variables. The app emits one strict, bounded readiness datagram;
//! the descriptor is never used for commands or persistent state.

pub const ATTEMPT_ENV: &str = "IGNITE_LAUNCH_HEALTH_ATTEMPT";
pub const FD_ENV: &str = "IGNITE_LAUNCH_HEALTH_FD";
pub const READY_PREFIX: &str = "ignite-launch-health-v1 ready ";
pub const ATTEMPT_HEX_LEN: usize = 32;

pub fn valid_attempt_id(attempt: &str) -> bool {
    attempt.len() == ATTEMPT_HEX_LEN
        && attempt
            .as_bytes()
            .iter()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b))
}

pub fn ready_datagram(attempt: &str) -> Option<String> {
    valid_attempt_id(attempt).then(|| format!("{READY_PREFIX}{attempt}\n"))
}

/// Acknowledgment is permitted only when the event loop is entered after one prior iteration.
#[derive(Default)]
pub struct InitialIterationGate {
    entered_once: bool,
}

impl InitialIterationGate {
    pub fn entering_iteration(&mut self) -> bool {
        std::mem::replace(&mut self.entered_once, true)
    }
}

#[cfg(unix)]
pub struct Reporter {
    attempt: Option<String>,
    sender: Option<std::os::unix::net::UnixDatagram>,
}

#[cfg(unix)]
impl Reporter {
    /// Consume the inherited launch-only environment and restore CLOEXEC immediately so any
    /// subprocess started during initialization cannot inherit the acknowledgment capability.
    pub fn from_environment() -> Self {
        use std::os::fd::FromRawFd;

        let attempt = std::env::var(ATTEMPT_ENV).ok();
        let fd = std::env::var(FD_ENV).ok();
        std::env::remove_var(ATTEMPT_ENV);
        std::env::remove_var(FD_ENV);

        let Some(fd) = fd else {
            return Self::disabled();
        };
        let Ok(fd) = fd.parse::<libc::c_int>() else {
            eprintln!("launch_health_ack status=disabled reason=invalid_fd");
            return Self::disabled();
        };
        if fd < 3 {
            eprintln!("launch_health_ack status=disabled reason=reserved_fd");
            return Self::disabled();
        }

        if let Err(error) = set_cloexec(fd) {
            unsafe {
                libc::close(fd);
            }
            eprintln!("launch_health_ack status=disabled reason=cloexec error={error:?}");
            return Self::disabled();
        }
        let sender = unsafe { std::os::unix::net::UnixDatagram::from_raw_fd(fd) };
        let Some(attempt) = attempt else {
            eprintln!("launch_health_ack status=disabled reason=missing_attempt");
            return Self::disabled();
        };
        if !valid_attempt_id(&attempt) {
            eprintln!("launch_health_ack status=disabled reason=invalid_attempt");
            return Self::disabled();
        }
        Self {
            attempt: Some(attempt),
            sender: Some(sender),
        }
    }

    fn disabled() -> Self {
        Self {
            attempt: None,
            sender: None,
        }
    }

    /// Best-effort observation only. Failure to report never changes application behavior.
    pub fn acknowledge_ready(&mut self) {
        let (Some(attempt), Some(sender)) = (self.attempt.take(), self.sender.take()) else {
            return;
        };
        let Some(message) = ready_datagram(&attempt) else {
            eprintln!("launch_health_ack status=rejected reason=invalid_attempt");
            return;
        };
        match sender.send(message.as_bytes()) {
            Ok(size) if size == message.len() => {
                println!("launch_health_ack attempt={attempt} status=sent")
            }
            Ok(size) => eprintln!(
                "launch_health_ack attempt={attempt} status=failed reason=short_send bytes={size}"
            ),
            Err(error) => eprintln!(
                "launch_health_ack attempt={attempt} status=failed reason=send error={error:?}"
            ),
        }
    }
}

#[cfg(unix)]
fn set_cloexec(fd: libc::c_int) -> std::io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 {
        return Err(std::io::Error::last_os_error());
    }
    if unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readiness_datagram_has_one_canonical_form() {
        let attempt = "00112233445566778899aabbccddeeff";
        assert_eq!(
            ready_datagram(attempt).as_deref(),
            Some("ignite-launch-health-v1 ready 00112233445566778899aabbccddeeff\n")
        );
        assert!(ready_datagram("short").is_none());
        assert!(ready_datagram("00112233445566778899AABBCCDDEEFF").is_none());
    }

    #[test]
    fn readiness_requires_one_completed_loop_iteration() {
        let mut gate = InitialIterationGate::default();
        assert!(!gate.entering_iteration());
        assert!(gate.entering_iteration());
        assert!(gate.entering_iteration());
    }
}
