//! Evdev stylus reader (Unix). Frame decoding lives in [`crate::evdev_state`].
//!
//! # `SYN_DROPPED` recovery lifecycle
//!
//! ```text
//! SYN_DROPPED
//!   → FeedResult::StreamGap  → SourcePoll::StreamGap (cancel stroke; stay Desynchronized)
//!   → discard buffered KEY/ABS/… while Desynchronized
//! SYN_REPORT (while Desynchronized)
//!   → FeedResult::RecoveryBoundary
//!   → EVIOCGKEY + EVIOCGABS (or fail-closed)
//!   → recover_*; resume normal processing
//! ```
//!
//! Recovery is **not** performed at `SYN_DROPPED`. A second application-level `StreamGap` is
//! never emitted merely because the recovery boundary was reached.

use crate::axes::{AxisRange, StylusAxes};
use crate::error::{PlatformError, PlatformResult};
use crate::evdev_abi::{abs, decode_native, INPUT_EVENT_SIZE};
use crate::evdev_state::{EvdevStateMachine, FeedResult, StreamIntegrity};
use crate::input::{SourceMode, SourcePoll, StylusSource};
use crate::linux::fd::{
    abs_info, query_device_snapshot, set_clock_monotonic, set_evdev_grab, OwnedFd,
};
use crate::time::ClockDomain;

/// Number of records buffered per `read(2)`.
const READ_BATCH: usize = 32;

/// Reads `/dev/input/eventN` and turns frames into stylus events.
///
/// Opened **blocking** by default: a live probe must park in `read(2)` while the user is not
/// writing, not spin and not mistake an idle digitizer for a closed one. A non-blocking variant
/// exists for callers that multiplex, and it reports [`SourcePoll::WouldBlock`] explicitly.
#[derive(Debug)]
pub struct LinuxEvdevStylus {
    path: String,
    fd: OwnedFd,
    state: EvdevStateMachine,
    mode: SourceMode,
    buf: Vec<u8>,
    /// Bytes of a partially read record carried over to the next `read`.
    buffered: usize,
    cursor: usize,
    eof: bool,
    kernel_clock: ClockDomain,
    /// True when this reader holds `EVIOCGRAB` and must release it on drop.
    grabbed: bool,
}

impl LinuxEvdevStylus {
    /// Open blocking. Axis ranges are read from the device, never assumed.
    pub fn open(path: impl Into<String>) -> PlatformResult<Self> {
        Self::open_with_mode(path, SourceMode::Blocking)
    }

    /// Open with `O_NONBLOCK`. `poll_event` then returns [`SourcePoll::WouldBlock`] when idle.
    pub fn open_nonblocking(path: impl Into<String>) -> PlatformResult<Self> {
        Self::open_with_mode(path, SourceMode::NonBlocking)
    }

    fn open_with_mode(path: impl Into<String>, mode: SourceMode) -> PlatformResult<Self> {
        let path = path.into();
        let fd = OwnedFd::open_read(&path, mode == SourceMode::NonBlocking)?;
        let axes = query_axes(&fd)?;
        // Ask the kernel to timestamp events on CLOCK_MONOTONIC so they share a clock domain
        // with LinuxMonotonicClock. If this fails, evdev keeps its CLOCK_REALTIME default and we
        // record that instead of pretending the epochs are comparable.
        let kernel_clock = match set_clock_monotonic(&fd) {
            Ok(()) => ClockDomain::LinuxMonotonic,
            Err(_) => ClockDomain::LinuxRealtime,
        };
        Ok(Self {
            path,
            fd,
            state: EvdevStateMachine::new(axes).with_kernel_clock(kernel_clock),
            mode,
            buf: vec![0u8; INPUT_EVENT_SIZE * READ_BATCH],
            buffered: 0,
            cursor: 0,
            eof: false,
            kernel_clock,
            grabbed: false,
        })
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    pub fn axes(&self) -> &StylusAxes {
        self.state.axes()
    }

    /// Clock domain of the kernel timestamps on this node.
    pub fn kernel_clock(&self) -> ClockDomain {
        self.kernel_clock
    }

    /// Record size this build expects, derived from the target ABI (16 or 24 bytes).
    pub fn record_size(&self) -> usize {
        INPUT_EVENT_SIZE
    }

    /// Take an exclusive `EVIOCGRAB` so Amazon `WetInkHandler` (and other clients) stop
    /// receiving events for the duration of this reader.
    ///
    /// Normal [`Drop`] explicitly ungrabs. Closing the fd / process teardown also releases an
    /// evdev grab at the kernel; Rust `Drop` is not guaranteed on every fatal signal.
    pub fn grab_exclusive(&mut self) -> PlatformResult<()> {
        if self.grabbed {
            return Ok(());
        }
        set_evdev_grab(&self.fd, true)?;
        self.grabbed = true;
        Ok(())
    }

    /// Query current device state and leave `Desynchronized`. Called only at RecoveryBoundary.
    fn recover_at_boundary(&mut self) {
        match query_device_snapshot(&self.fd, self.state.axes()) {
            Ok(snap) => self.state.recover_from_snapshot(snap),
            Err(_) => self.state.recover_fail_closed_after_gap(),
        }
    }
}

impl Drop for LinuxEvdevStylus {
    fn drop(&mut self) {
        // Explicit ungrab on normal destruction. Fd close / process exit also drops the grab
        // in the kernel; this path is not guaranteed on every fatal signal.
        if self.grabbed {
            let _ = set_evdev_grab(&self.fd, false);
            self.grabbed = false;
        }
    }
}

fn query_axes(fd: &OwnedFd) -> PlatformResult<StylusAxes> {
    let to_range = |i: crate::evdev_ioctl::InputAbsinfo| AxisRange {
        min: i.minimum,
        max: i.maximum,
        fuzz: i.fuzz,
        flat: i.flat,
        resolution: i.resolution,
    };
    let x = to_range(abs_info(fd, abs::X)?);
    let y = to_range(abs_info(fd, abs::Y)?);
    if x.span() <= 0 || y.span() <= 0 {
        return Err(PlatformError::InvalidInput(format!(
            "device reports a degenerate XY range (x {}..{}, y {}..{})",
            x.min, x.max, y.min, y.max
        )));
    }
    let pressure = abs_info(fd, abs::PRESSURE)
        .ok()
        .map(to_range)
        .filter(|r| r.span() > 0);
    let distance = abs_info(fd, abs::DISTANCE)
        .ok()
        .map(to_range)
        .filter(|r| r.span() > 0);
    let tilt_x = abs_info(fd, abs::TILT_X).ok().map(to_range);
    let tilt_y = abs_info(fd, abs::TILT_Y).ok().map(to_range);
    Ok(StylusAxes {
        x,
        y,
        pressure,
        distance,
        tilt_x,
        tilt_y,
    })
}

impl StylusSource for LinuxEvdevStylus {
    fn poll_event(&mut self) -> PlatformResult<SourcePoll> {
        loop {
            // Drain whole records already in the buffer first.
            while self.buffered - self.cursor >= INPUT_EVENT_SIZE {
                let end = self.cursor + INPUT_EVENT_SIZE;
                let raw = decode_native(&self.buf[self.cursor..end]).ok_or_else(|| {
                    PlatformError::Io(format!(
                        "failed to decode a {INPUT_EVENT_SIZE}-byte input_event record"
                    ))
                })?;
                self.cursor = end;
                match self.state.feed(raw) {
                    FeedResult::Event(ev) => return Ok(SourcePoll::Event(ev)),
                    FeedResult::StreamGap { had_active_contact } => {
                        // Stay Desynchronized. Application cancels the stroke; recover later.
                        return Ok(SourcePoll::StreamGap { had_active_contact });
                    }
                    FeedResult::RecoveryBoundary => {
                        self.recover_at_boundary();
                        // Continue draining — do not emit a second StreamGap.
                    }
                    FeedResult::None => {}
                }
            }

            if self.eof {
                return Ok(SourcePoll::EndOfStream);
            }

            // Carry over any partial record, then read more.
            let leftover = self.buffered - self.cursor;
            if leftover > 0 {
                self.buf.copy_within(self.cursor..self.buffered, 0);
            }
            self.buffered = leftover;
            self.cursor = 0;

            let free = self.buf.len() - self.buffered;
            let mut chunk = vec![0u8; free];
            match self.fd.read(&mut chunk)? {
                None => return Ok(SourcePoll::WouldBlock),
                Some(0) => {
                    self.eof = true;
                    return Ok(SourcePoll::EndOfStream);
                }
                Some(n) => {
                    if n % INPUT_EVENT_SIZE != 0 && n < INPUT_EVENT_SIZE {
                        // A read shorter than one record means our assumed ABI record size is
                        // wrong for this kernel. Say so instead of decoding garbage.
                        return Err(PlatformError::Io(format!(
                            "evdev read returned {n} bytes, shorter than one \
                             {INPUT_EVENT_SIZE}-byte input_event record; the kernel input ABI \
                             does not match this build's target word size"
                        )));
                    }
                    self.buf[self.buffered..self.buffered + n].copy_from_slice(&chunk[..n]);
                    self.buffered += n;
                }
            }
        }
    }

    fn mode(&self) -> SourceMode {
        self.mode
    }

    fn tool_in_proximity(&self) -> Option<bool> {
        match self.state.integrity() {
            StreamIntegrity::Synchronized => Some(self.state.in_proximity()),
            StreamIntegrity::Desynchronized => None,
        }
    }
}
