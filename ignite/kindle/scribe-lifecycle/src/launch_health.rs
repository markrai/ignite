//! Observation-only launch-health protocol and state machine.
//!
//! The lifecycle host gives each child a fresh connected Unix datagram socket and a UUID.
//! `scribe-app` may send exactly one bounded readiness datagram on that inherited descriptor.
//! Nothing in this module changes installation state or performs a recovery action.

#![cfg_attr(not(unix), allow(dead_code))]

pub const ATTEMPT_ENV: &str = "IGNITE_LAUNCH_HEALTH_ATTEMPT";
pub const FD_ENV: &str = "IGNITE_LAUNCH_HEALTH_FD";
pub const READY_PREFIX: &str = "ignite-launch-health-v1 ready ";
pub const ATTEMPT_HEX_LEN: usize = 32;
pub const MAX_DATAGRAM_BYTES: usize = 128;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LaunchState {
    AwaitingSpawn,
    Started,
    Ready,
    SpawnFailed,
    PrematureExit,
    NormalExit,
    TimedOut,
    IntentionalStop,
    Interrupted,
}

impl LaunchState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AwaitingSpawn => "awaiting_spawn",
            Self::Started => "started",
            Self::Ready => "ready",
            Self::SpawnFailed => "spawn_failed",
            Self::PrematureExit => "premature_exit",
            Self::NormalExit => "normal_exit_before_ready",
            Self::TimedOut => "timeout",
            Self::IntentionalStop => "intentional_stop",
            Self::Interrupted => "interrupted",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AckDisposition {
    MatchingReady,
    IncorrectIdentifier { received: String },
    Malformed,
    IgnoredAfter(LaunchState),
}

#[derive(Debug)]
pub struct LaunchObservation {
    attempt: String,
    state: LaunchState,
}

impl LaunchObservation {
    pub fn new(attempt: String) -> Self {
        debug_assert!(valid_attempt_id(&attempt));
        Self {
            attempt,
            state: LaunchState::AwaitingSpawn,
        }
    }

    pub fn attempt(&self) -> &str {
        &self.attempt
    }

    pub fn state(&self) -> LaunchState {
        self.state
    }

    pub fn child_spawned(&mut self) -> bool {
        self.transition_from(LaunchState::AwaitingSpawn, LaunchState::Started)
    }

    pub fn spawn_failed(&mut self) -> bool {
        self.transition_from(LaunchState::AwaitingSpawn, LaunchState::SpawnFailed)
    }

    pub fn acknowledge(&mut self, datagram: &[u8]) -> AckDisposition {
        if self.state != LaunchState::Started {
            return AckDisposition::IgnoredAfter(self.state);
        }
        match parse_ready_datagram(datagram) {
            Some(received) if received == self.attempt => {
                self.state = LaunchState::Ready;
                AckDisposition::MatchingReady
            }
            Some(received) => AckDisposition::IncorrectIdentifier {
                received: received.to_owned(),
            },
            None => AckDisposition::Malformed,
        }
    }

    pub fn child_exited(&mut self, successful: bool) -> bool {
        let target = if successful {
            LaunchState::NormalExit
        } else {
            LaunchState::PrematureExit
        };
        self.transition_from(LaunchState::Started, target)
    }

    pub fn timeout(&mut self) -> bool {
        self.transition_from(LaunchState::Started, LaunchState::TimedOut)
    }

    pub fn intentional_stop(&mut self) -> bool {
        self.transition_from(LaunchState::Started, LaunchState::IntentionalStop)
    }

    pub fn interrupted(&mut self) -> bool {
        match self.state {
            LaunchState::AwaitingSpawn | LaunchState::Started => {
                self.state = LaunchState::Interrupted;
                true
            }
            _ => false,
        }
    }

    fn transition_from(&mut self, from: LaunchState, to: LaunchState) -> bool {
        if self.state == from {
            self.state = to;
            true
        } else {
            false
        }
    }
}

pub fn valid_attempt_id(attempt: &str) -> bool {
    attempt.len() == ATTEMPT_HEX_LEN
        && attempt
            .as_bytes()
            .iter()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b))
}

#[cfg(test)]
pub fn ready_datagram(attempt: &str) -> Option<String> {
    valid_attempt_id(attempt).then(|| format!("{READY_PREFIX}{attempt}\n"))
}

fn parse_ready_datagram(datagram: &[u8]) -> Option<&str> {
    if datagram.len() > MAX_DATAGRAM_BYTES {
        return None;
    }
    let text = std::str::from_utf8(datagram).ok()?;
    let attempt = text.strip_prefix(READY_PREFIX)?.strip_suffix('\n')?;
    valid_attempt_id(attempt).then_some(attempt)
}

#[cfg(unix)]
mod unix {
    use super::{
        AckDisposition, LaunchObservation, LaunchState, ATTEMPT_ENV, FD_ENV, MAX_DATAGRAM_BYTES,
    };
    use std::io;
    use std::os::fd::{AsRawFd, RawFd};
    use std::os::unix::net::UnixDatagram;
    use std::os::unix::process::CommandExt;
    use std::process::Command;
    use std::time::{Duration, Instant};
    use uuid::Uuid;

    const MAX_DATAGRAMS_PER_POLL: usize = 8;

    #[derive(Debug)]
    pub struct PrepareError {
        pub attempt: String,
        pub error: io::Error,
    }

    pub struct PreparedLaunchHealth {
        observation: LaunchObservation,
        receiver: UnixDatagram,
        child_sender: UnixDatagram,
    }

    pub struct LaunchHealthObserver {
        observation: LaunchObservation,
        receiver: Option<UnixDatagram>,
        deadline: Instant,
    }

    #[derive(Debug)]
    pub enum ObserverEvent {
        Ready,
        IncorrectIdentifier { received: String },
        MalformedAcknowledgment,
        TimedOut,
        Interrupted { error: String },
    }

    pub fn clear_handshake_environment(command: &mut Command) {
        command.env_remove(ATTEMPT_ENV).env_remove(FD_ENV);
    }

    impl PreparedLaunchHealth {
        pub fn new() -> Result<Self, PrepareError> {
            let attempt = Uuid::new_v4().simple().to_string();
            let (receiver, child_sender) = UnixDatagram::pair().map_err(|error| PrepareError {
                attempt: attempt.clone(),
                error,
            })?;
            receiver
                .set_nonblocking(true)
                .map_err(|error| PrepareError {
                    attempt: attempt.clone(),
                    error,
                })?;
            child_sender
                .set_nonblocking(true)
                .map_err(|error| PrepareError {
                    attempt: attempt.clone(),
                    error,
                })?;
            Ok(Self {
                observation: LaunchObservation::new(attempt),
                receiver,
                child_sender,
            })
        }

        /// Configure only this child to inherit the sender. The descriptor remains CLOEXEC in
        /// the multi-threaded parent and is made inheritable after fork, immediately before exec.
        pub fn configure_command(&self, command: &mut Command) {
            configure_command_descriptor(
                command,
                self.observation.attempt(),
                self.child_sender.as_raw_fd(),
            );
        }

        pub fn spawned(mut self, now: Instant, timeout: Duration) -> LaunchHealthObserver {
            let _ = self.observation.child_spawned();
            drop(self.child_sender);
            LaunchHealthObserver {
                observation: self.observation,
                receiver: Some(self.receiver),
                deadline: now + timeout,
            }
        }

        pub fn spawn_failed(mut self) -> LaunchObservation {
            let _ = self.observation.spawn_failed();
            self.observation
        }
    }

    impl LaunchHealthObserver {
        pub fn attempt(&self) -> &str {
            self.observation.attempt()
        }

        pub fn state(&self) -> LaunchState {
            self.observation.state()
        }

        /// Drain a bounded number of nonblocking datagrams and advance the observation clock.
        pub fn poll(&mut self, now: Instant) -> Vec<ObserverEvent> {
            let mut events = Vec::new();
            for _ in 0..MAX_DATAGRAMS_PER_POLL {
                if self.observation.state() != LaunchState::Started {
                    break;
                }
                let Some(receiver) = self.receiver.as_ref() else {
                    break;
                };
                let mut buf = [0u8; MAX_DATAGRAM_BYTES + 1];
                match receiver.recv(&mut buf) {
                    Ok(size) => match self.observation.acknowledge(&buf[..size]) {
                        AckDisposition::MatchingReady => {
                            events.push(ObserverEvent::Ready);
                            self.receiver = None;
                            break;
                        }
                        AckDisposition::IncorrectIdentifier { received } => {
                            events.push(ObserverEvent::IncorrectIdentifier { received });
                        }
                        AckDisposition::Malformed => {
                            events.push(ObserverEvent::MalformedAcknowledgment);
                        }
                        AckDisposition::IgnoredAfter(_) => break,
                    },
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                    Err(error) => {
                        let _ = self.observation.interrupted();
                        events.push(ObserverEvent::Interrupted {
                            error: error.to_string(),
                        });
                        self.receiver = None;
                        break;
                    }
                }
            }
            if self.observation.state() == LaunchState::Started && now >= self.deadline {
                let _ = self.observation.timeout();
                self.receiver = None;
                events.push(ObserverEvent::TimedOut);
            }
            events
        }

        pub fn child_exited(&mut self, successful: bool) -> LaunchState {
            let _ = self.observation.child_exited(successful);
            self.receiver = None;
            self.observation.state()
        }

        pub fn intentional_stop(&mut self) -> LaunchState {
            let _ = self.observation.intentional_stop();
            self.receiver = None;
            self.observation.state()
        }

        pub fn interrupted(&mut self) -> LaunchState {
            let _ = self.observation.interrupted();
            self.receiver = None;
            self.observation.state()
        }
    }

    fn clear_cloexec(fd: RawFd) -> io::Result<()> {
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        if flags < 0 {
            return Err(io::Error::last_os_error());
        }
        if unsafe { libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn configure_command_descriptor(command: &mut Command, attempt: &str, fd: RawFd) {
        clear_handshake_environment(command);
        command
            .env(ATTEMPT_ENV, attempt)
            .env(FD_ENV, fd.to_string());
        unsafe {
            command.pre_exec(move || {
                if clear_cloexec(fd).is_err() {
                    // Launch-health is observational. A descriptor setup failure must not abort
                    // exec, and closing here prevents an unusable capability from leaking.
                    libc::close(fd);
                }
                Ok(())
            });
        }
    }

    #[cfg(test)]
    mod integration_tests {
        use super::*;
        use crate::launch_health::ready_datagram;
        use std::io::{BufRead, BufReader, Read, Write};
        use std::os::fd::FromRawFd;
        use std::process::{Child, ExitStatus, Stdio};
        use std::sync::mpsc;
        use std::thread;

        const FIXTURE_ENV: &str = "IGNITE_LAUNCH_HEALTH_TEST_FIXTURE";
        const OVERRIDE_ENV: &str = "IGNITE_LAUNCH_HEALTH_TEST_ACK_OVERRIDE";
        const FIXTURE_MARKER: &str = "launch-health-fixture-ran";
        const FIRST: &str = "00112233445566778899aabbccddeeff";
        const CHILD_DEADLINE: Duration = Duration::from_secs(5);

        #[test]
        #[ignore = "subprocess fixture; invoked by launch-health integration tests"]
        fn launch_health_process_fixture() {
            let mode = std::env::var(FIXTURE_ENV).expect("fixture mode");
            match mode.as_str() {
                "ack" | "wrong" | "override" => {
                    let attempt = std::env::var(ATTEMPT_ENV).expect("attempt environment");
                    let fd = std::env::var(FD_ENV)
                        .expect("descriptor environment")
                        .parse::<RawFd>()
                        .expect("numeric descriptor");
                    let sent_attempt = match mode.as_str() {
                        "ack" => attempt,
                        "wrong" => "00000000000000000000000000000000".to_owned(),
                        "override" => std::env::var(OVERRIDE_ENV).expect("override attempt"),
                        _ => unreachable!(),
                    };
                    let sender = unsafe { UnixDatagram::from_raw_fd(fd) };
                    let message = ready_datagram(&sent_attempt).expect("valid fixture attempt");
                    assert_eq!(sender.send(message.as_bytes()).unwrap(), message.len());
                }
                "missing" | "noop" => {}
                "crash" => std::process::exit(17),
                "legacy" => {
                    println!("{FIXTURE_MARKER}");
                    std::io::stdout().flush().expect("flush fixture marker");
                    thread::park_timeout(CHILD_DEADLINE);
                    return;
                }
                other => panic!("unknown fixture mode: {other}"),
            }
            println!("{FIXTURE_MARKER}");
        }

        fn fixture_command(mode: &str) -> Command {
            let mut command = Command::new(std::env::current_exe().expect("current test binary"));
            command
                .arg("launch_health_process_fixture")
                .arg("--ignored")
                .arg("--nocapture")
                .arg("--test-threads=1")
                .env(FIXTURE_ENV, mode)
                .env_remove(OVERRIDE_ENV);
            command
        }

        fn spawn_observed(
            mode: &str,
            override_attempt: Option<&str>,
            timeout: Duration,
        ) -> (Child, LaunchHealthObserver) {
            let prepared = PreparedLaunchHealth::new().expect("prepare launch health");
            let mut command = fixture_command(mode);
            command.stdout(Stdio::piped()).stderr(Stdio::null());
            if let Some(override_attempt) = override_attempt {
                command.env(OVERRIDE_ENV, override_attempt);
            }
            prepared.configure_command(&mut command);
            let child = command.spawn().expect("spawn fixture child");
            let observer = prepared.spawned(Instant::now(), timeout);
            (child, observer)
        }

        fn wait_for_exit(child: &mut Child) -> ExitStatus {
            let deadline = Instant::now() + CHILD_DEADLINE;
            loop {
                if let Some(status) = child.try_wait().expect("query fixture status") {
                    return status;
                }
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("fixture child exceeded bounded deadline");
                }
                thread::sleep(Duration::from_millis(5));
            }
        }

        fn finish_observation(
            child: &mut Child,
            observer: &mut LaunchHealthObserver,
        ) -> (Vec<ObserverEvent>, LaunchState) {
            let status = wait_for_exit(child);
            let mut output = String::new();
            child
                .stdout
                .take()
                .expect("fixture stdout")
                .read_to_string(&mut output)
                .expect("read fixture stdout");
            if status.success() {
                assert!(output.contains(FIXTURE_MARKER));
            }
            let events = observer.poll(Instant::now());
            let state = observer.child_exited(status.success());
            (events, state)
        }

        #[test]
        fn inherited_datagram_acknowledgment_marks_actual_child_ready() {
            let (mut child, mut observer) = spawn_observed("ack", None, Duration::from_secs(2));
            let (events, state) = finish_observation(&mut child, &mut observer);
            assert!(events
                .iter()
                .any(|event| matches!(event, ObserverEvent::Ready)));
            assert_eq!(state, LaunchState::Ready);
        }

        #[test]
        fn incorrect_and_missing_acknowledgments_never_mark_children_ready() {
            let (mut wrong_child, mut wrong_observer) =
                spawn_observed("wrong", None, Duration::from_secs(2));
            let (wrong_events, wrong_state) =
                finish_observation(&mut wrong_child, &mut wrong_observer);
            assert!(wrong_events
                .iter()
                .any(|event| matches!(event, ObserverEvent::IncorrectIdentifier { .. })));
            assert!(!wrong_events
                .iter()
                .any(|event| matches!(event, ObserverEvent::Ready)));
            assert_eq!(wrong_state, LaunchState::NormalExit);

            let (mut missing_child, mut missing_observer) =
                spawn_observed("missing", None, Duration::from_secs(2));
            let (missing_events, missing_state) =
                finish_observation(&mut missing_child, &mut missing_observer);
            assert!(missing_events.is_empty());
            assert_eq!(missing_state, LaunchState::NormalExit);
        }

        #[test]
        fn descriptor_setup_failure_does_not_block_child_launch() {
            let mut command = fixture_command("noop");
            command.stdout(Stdio::piped()).stderr(Stdio::piped());
            configure_command_descriptor(&mut command, FIRST, -1);
            let output = command.output().expect("spawn despite setup failure");
            assert!(output.status.success());
            assert!(String::from_utf8_lossy(&output.stdout).contains(FIXTURE_MARKER));
        }

        #[test]
        fn child_exit_before_readiness_is_observed() {
            let (mut child, mut observer) = spawn_observed("crash", None, Duration::from_secs(2));
            let (events, state) = finish_observation(&mut child, &mut observer);
            assert!(events.is_empty());
            assert_eq!(state, LaunchState::PrematureExit);
        }

        #[test]
        fn legacy_child_times_out_without_becoming_a_failed_update() {
            let (mut child, mut observer) = spawn_observed("legacy", None, Duration::ZERO);
            wait_for_fixture_marker(&mut child);
            let events = observer.poll(Instant::now());
            assert!(events
                .iter()
                .any(|event| matches!(event, ObserverEvent::TimedOut)));
            assert_eq!(observer.state(), LaunchState::TimedOut);

            child.kill().expect("stop bounded legacy fixture");
            let _ = child.wait().expect("reap bounded legacy fixture");
            assert_eq!(observer.child_exited(false), LaunchState::TimedOut);
        }

        fn wait_for_fixture_marker(child: &mut Child) {
            let stdout = child.stdout.take().expect("fixture stdout");
            let (sender, receiver) = mpsc::channel();
            let reader = thread::spawn(move || {
                let seen = BufReader::new(stdout)
                    .lines()
                    .map_while(Result::ok)
                    .any(|line| line.contains(FIXTURE_MARKER));
                let _ = sender.send(seen);
            });
            let seen = receiver.recv_timeout(CHILD_DEADLINE).unwrap_or(false);
            if !seen {
                let _ = child.kill();
                let _ = child.wait();
            }
            reader.join().expect("fixture marker reader");
            assert!(seen, "fixture did not reach its controlled wait point");
        }

        #[test]
        fn repeated_launches_do_not_accept_a_stale_acknowledgment() {
            let (mut first_child, mut first_observer) =
                spawn_observed("ack", None, Duration::from_secs(2));
            let first_attempt = first_observer.attempt().to_owned();
            let (_, first_state) = finish_observation(&mut first_child, &mut first_observer);
            assert_eq!(first_state, LaunchState::Ready);

            let (mut second_child, mut second_observer) =
                spawn_observed("override", Some(&first_attempt), Duration::from_secs(2));
            assert_ne!(second_observer.attempt(), first_attempt);
            let (events, second_state) =
                finish_observation(&mut second_child, &mut second_observer);
            assert!(events.iter().any(|event| matches!(
                event,
                ObserverEvent::IncorrectIdentifier { received } if received == &first_attempt
            )));
            assert!(!events
                .iter()
                .any(|event| matches!(event, ObserverEvent::Ready)));
            assert_eq!(second_state, LaunchState::NormalExit);
        }
    }
}

#[cfg(unix)]
pub use unix::{
    clear_handshake_environment, LaunchHealthObserver, ObserverEvent, PreparedLaunchHealth,
};

#[cfg(test)]
mod tests {
    use super::*;

    const FIRST: &str = "00112233445566778899aabbccddeeff";
    const SECOND: &str = "ffeeddccbbaa99887766554433221100";

    fn started(attempt: &str) -> LaunchObservation {
        let mut observation = LaunchObservation::new(attempt.to_owned());
        assert!(observation.child_spawned());
        observation
    }

    #[test]
    fn matching_acknowledgment_marks_ready() {
        let mut observation = started(FIRST);
        let message = ready_datagram(FIRST).unwrap();
        assert_eq!(
            observation.acknowledge(message.as_bytes()),
            AckDisposition::MatchingReady
        );
        assert_eq!(observation.state(), LaunchState::Ready);
    }

    #[test]
    fn missing_or_incorrect_identifier_never_marks_ready() {
        let mut observation = started(FIRST);
        assert_eq!(observation.state(), LaunchState::Started);
        assert_eq!(
            observation.acknowledge(ready_datagram(SECOND).unwrap().as_bytes()),
            AckDisposition::IncorrectIdentifier {
                received: SECOND.to_owned()
            }
        );
        assert_eq!(observation.state(), LaunchState::Started);
        assert_eq!(
            observation.acknowledge(b"ignite-launch-health-v1 ready missing\n"),
            AckDisposition::Malformed
        );
        assert_eq!(observation.state(), LaunchState::Started);
    }

    #[test]
    fn stale_acknowledgment_cannot_satisfy_a_later_launch() {
        let mut first = started(FIRST);
        assert_eq!(
            first.acknowledge(ready_datagram(FIRST).unwrap().as_bytes()),
            AckDisposition::MatchingReady
        );
        let mut later = started(SECOND);
        assert_eq!(
            later.acknowledge(ready_datagram(FIRST).unwrap().as_bytes()),
            AckDisposition::IncorrectIdentifier {
                received: FIRST.to_owned()
            }
        );
        assert_eq!(later.state(), LaunchState::Started);
    }

    #[test]
    fn spawn_failure_and_premature_exit_are_distinct() {
        let mut failed = LaunchObservation::new(FIRST.to_owned());
        assert!(failed.spawn_failed());
        assert_eq!(failed.state(), LaunchState::SpawnFailed);

        let mut exited = started(SECOND);
        assert!(exited.child_exited(false));
        assert_eq!(exited.state(), LaunchState::PrematureExit);
    }

    #[test]
    fn normal_exit_before_readiness_is_not_a_failure() {
        let mut observation = started(FIRST);
        assert!(observation.child_exited(true));
        assert_eq!(observation.state(), LaunchState::NormalExit);
    }

    #[test]
    fn timeout_is_terminal_and_late_ack_is_ignored() {
        let mut observation = started(FIRST);
        assert!(observation.timeout());
        assert_eq!(observation.state(), LaunchState::TimedOut);
        assert_eq!(
            observation.acknowledge(ready_datagram(FIRST).unwrap().as_bytes()),
            AckDisposition::IgnoredAfter(LaunchState::TimedOut)
        );
        assert_eq!(observation.state(), LaunchState::TimedOut);
    }

    #[test]
    fn intentional_unload_before_readiness_is_not_a_failure() {
        let mut observation = started(FIRST);
        assert!(observation.intentional_stop());
        assert_eq!(observation.state(), LaunchState::IntentionalStop);
        assert!(!observation.child_exited(false));
        assert_eq!(observation.state(), LaunchState::IntentionalStop);
    }

    #[test]
    fn interruption_during_observation_is_distinct() {
        let mut observation = started(FIRST);
        assert!(observation.interrupted());
        assert_eq!(observation.state(), LaunchState::Interrupted);
    }

    #[test]
    fn repeated_launch_attempts_have_independent_state() {
        let mut first = started(FIRST);
        let second = started(SECOND);
        assert_eq!(
            first.acknowledge(ready_datagram(FIRST).unwrap().as_bytes()),
            AckDisposition::MatchingReady
        );
        assert_eq!(first.state(), LaunchState::Ready);
        assert_eq!(second.state(), LaunchState::Started);
        assert_ne!(first.attempt(), second.attempt());
    }

    #[test]
    fn legacy_child_without_protocol_support_times_out_without_recovery() {
        let mut observation = started(FIRST);
        assert!(observation.timeout());
        assert_eq!(observation.state(), LaunchState::TimedOut);
        // No state-machine transition mutates, retries, or rolls back software.
        assert!(!observation.child_exited(false));
        assert_eq!(observation.state(), LaunchState::TimedOut);
    }

    #[test]
    fn protocol_framing_is_strict_and_bounded() {
        let valid = ready_datagram(FIRST).unwrap();
        assert_eq!(parse_ready_datagram(valid.as_bytes()), Some(FIRST));
        for malformed in [
            format!("{READY_PREFIX}{FIRST}"),
            format!("{READY_PREFIX}{FIRST}\nextra"),
            format!("ignite-launch-health-v2 ready {FIRST}\n"),
            format!("{READY_PREFIX}{}\n", FIRST.to_uppercase()),
        ] {
            assert_eq!(parse_ready_datagram(malformed.as_bytes()), None);
        }
        assert_eq!(parse_ready_datagram(&[b'x'; MAX_DATAGRAM_BYTES + 1]), None);
    }
}
