//! Dedicated stylus input pump: drains [`StylusSource`] on a background thread.
//!
//! Keeps the kernel/evdev queue moving while the consumer is blocked on WAIT / fsync / GC16.
//! Forwards semantic [`SourcePoll`] items in order — no coalescing, no silent drops.

use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::error::{PlatformError, PlatformResult};
use crate::input::{SourceMode, SourcePoll, StylusSource};

/// [`StylusSource::tool_in_proximity`] encoded for cross-thread sharing.
const PROX_UNKNOWN: u8 = 0;
const PROX_OUT: u8 = 1;
const PROX_IN: u8 = 2;

fn encode_proximity(p: Option<bool>) -> u8 {
    match p {
        None => PROX_UNKNOWN,
        Some(false) => PROX_OUT,
        Some(true) => PROX_IN,
    }
}

enum PumpMsg {
    Item(SourcePoll),
    Err(String),
}

/// Background drain of a [`StylusSource`] into an in-process queue.
pub struct InputPump {
    rx: Receiver<PumpMsg>,
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
    backlog: Arc<AtomicUsize>,
    max_backlog: Arc<AtomicUsize>,
    /// Latest tool proximity sampled by the pump thread after each poll (observation only).
    proximity: Arc<AtomicU8>,
}

impl InputPump {
    /// Move `src` onto a dedicated thread. Grab (if any) must already be held on `src`.
    pub fn start<S>(src: S) -> Self
    where
        S: StylusSource + Send + 'static,
    {
        let (tx, rx) = mpsc::channel::<PumpMsg>();
        let stop = Arc::new(AtomicBool::new(false));
        let backlog = Arc::new(AtomicUsize::new(0));
        let max_backlog = Arc::new(AtomicUsize::new(0));
        let stop_t = Arc::clone(&stop);
        let backlog_t = Arc::clone(&backlog);
        let max_t = Arc::clone(&max_backlog);
        let proximity = Arc::new(AtomicU8::new(PROX_UNKNOWN));
        let prox_t = Arc::clone(&proximity);

        let join = thread::spawn(move || {
            let mut src = src;
            loop {
                if stop_t.load(Ordering::SeqCst) {
                    break;
                }
                let polled = src.poll_event();
                prox_t.store(encode_proximity(src.tool_in_proximity()), Ordering::SeqCst);
                match polled {
                    Ok(SourcePoll::WouldBlock) => {
                        thread::sleep(Duration::from_millis(1));
                    }
                    Ok(item) => {
                        let is_end = matches!(item, SourcePoll::EndOfStream);
                        let n = backlog_t.fetch_add(1, Ordering::SeqCst) + 1;
                        max_t.fetch_max(n, Ordering::SeqCst);
                        if tx.send(PumpMsg::Item(item)).is_err() {
                            break;
                        }
                        if is_end {
                            break;
                        }
                    }
                    Err(e) => {
                        let _ = tx.send(PumpMsg::Err(e.to_string()));
                        break;
                    }
                }
            }
            // Drop src here → LinuxEvdevStylus Drop releases EVIOCGRAB.
        });

        Self {
            rx,
            stop,
            join: Some(join),
            backlog,
            max_backlog,
            proximity,
        }
    }

    /// Peak queued semantic items observed since start.
    pub fn max_backlog(&self) -> usize {
        self.max_backlog.load(Ordering::SeqCst)
    }

    /// Current queued depth.
    pub fn backlog(&self) -> usize {
        self.backlog.load(Ordering::SeqCst)
    }

    /// Signal stop and join the pump thread (releases grab via source Drop).
    pub fn stop_and_join(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

impl Drop for InputPump {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

impl StylusSource for InputPump {
    fn poll_event(&mut self) -> PlatformResult<SourcePoll> {
        match self.rx.try_recv() {
            Ok(PumpMsg::Item(item)) => {
                self.backlog.fetch_sub(1, Ordering::SeqCst);
                Ok(item)
            }
            Ok(PumpMsg::Err(e)) => Err(PlatformError::Io(e)),
            Err(TryRecvError::Empty) => Ok(SourcePoll::WouldBlock),
            Err(TryRecvError::Disconnected) => Ok(SourcePoll::EndOfStream),
        }
    }

    fn mode(&self) -> SourceMode {
        SourceMode::NonBlocking
    }

    /// Most recent proximity the pump thread observed. It may be slightly ahead of queued
    /// events; callers use it only to suppress (never to create) input actions.
    fn tool_in_proximity(&self) -> Option<bool> {
        match self.proximity.load(Ordering::SeqCst) {
            PROX_OUT => Some(false),
            PROX_IN => Some(true),
            _ => None,
        }
    }
}

/// Blocking poll helper for tests that wait for the next pumped item.
#[cfg(test)]
#[allow(dead_code)]
pub fn recv_timeout(pump: &mut InputPump, timeout: Duration) -> PlatformResult<Option<SourcePoll>> {
    use std::sync::mpsc::RecvTimeoutError;
    match pump.rx.recv_timeout(timeout) {
        Ok(PumpMsg::Item(item)) => {
            pump.backlog.fetch_sub(1, Ordering::SeqCst);
            Ok(Some(item))
        }
        Ok(PumpMsg::Err(e)) => Err(PlatformError::Io(e)),
        Err(RecvTimeoutError::Timeout) => Ok(None),
        Err(RecvTimeoutError::Disconnected) => Ok(Some(SourcePoll::EndOfStream)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::ScriptedStylus;
    use crate::input::{StylusEvent, StylusEventKind, StylusTool};
    use crate::time::ClockDomain;
    use notebook_core::DevicePoint;
    use std::time::Instant;

    fn ev(kind: StylusEventKind, x: f32, y: f32, t_us: u64) -> StylusEvent {
        StylusEvent {
            kind,
            device: DevicePoint::new(x, y),
            pressure: Some(0.5),
            distance: None,
            tilt_x: None,
            tilt_y: None,
            tool: StylusTool::Pen,
            buttons: Default::default(),
            in_proximity: true,
            kernel_time: Some(crate::evdev_abi::KernelTimestamp {
                tv_sec: 0,
                tv_usec: t_us as i64,
            }),
            kernel_clock: ClockDomain::Synthetic,
        }
    }

    fn two_stroke_script() -> Vec<StylusEvent> {
        vec![
            ev(StylusEventKind::Down, 100.0, 100.0, 1_000),
            ev(StylusEventKind::Move, 100.0, 200.0, 2_000),
            ev(StylusEventKind::Up, 100.0, 300.0, 3_000),
            // Immediate second stroke.
            ev(StylusEventKind::Down, 200.0, 100.0, 3_500),
            ev(StylusEventKind::Move, 250.0, 100.0, 4_000),
            ev(StylusEventKind::Move, 300.0, 120.0, 4_500),
            ev(StylusEventKind::Up, 350.0, 100.0, 5_000),
        ]
    }

    #[test]
    fn pump_delivers_second_stroke_while_consumer_blocked_after_first_up() {
        let mut pump = InputPump::start(ScriptedStylus::new(two_stroke_script()));
        // Consume stroke 1.
        let mut saw_up1 = false;
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline && !saw_up1 {
            match pump.poll_event().unwrap() {
                SourcePoll::Event(e) if e.kind == StylusEventKind::Up => {
                    saw_up1 = true;
                }
                SourcePoll::WouldBlock => thread::sleep(Duration::from_millis(1)),
                SourcePoll::EndOfStream => panic!("eos before up1"),
                _ => {}
            }
        }
        assert!(saw_up1);
        // Simulate Pen Up cleanup stall — do not poll consumer.
        thread::sleep(Duration::from_millis(80));
        assert!(
            pump.max_backlog() >= 1,
            "pump should have queued stroke-2 events during stall"
        );

        let mut down2 = false;
        let mut moves2 = 0u32;
        let mut up2 = false;
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline && !up2 {
            match pump.poll_event().unwrap() {
                SourcePoll::Event(e) => match e.kind {
                    StylusEventKind::Down => down2 = true,
                    StylusEventKind::Move => {
                        assert!(down2, "move before down2");
                        moves2 += 1;
                    }
                    StylusEventKind::Up => {
                        assert!(down2);
                        up2 = true;
                    }
                    _ => {}
                },
                SourcePoll::WouldBlock => thread::sleep(Duration::from_millis(1)),
                SourcePoll::EndOfStream => break,
                SourcePoll::StreamGap { .. } => panic!("unexpected gap"),
            }
        }
        assert!(down2 && up2, "second stroke must be delivered intact");
        assert!(moves2 >= 1);
        pump.stop_and_join();
    }

    #[test]
    fn pump_preserves_eraser_moves_and_up_while_consumer_blocked() {
        fn eraser_ev(kind: StylusEventKind, x: f32, y: f32, t_us: u64) -> StylusEvent {
            let mut e = ev(kind, x, y, t_us);
            e.tool = StylusTool::Eraser;
            e
        }
        let script = vec![
            eraser_ev(StylusEventKind::Down, 100.0, 500.0, 1_000),
            eraser_ev(StylusEventKind::Move, 400.0, 500.0, 2_000), // across A
            eraser_ev(StylusEventKind::Move, 700.0, 500.0, 3_000), // across B
            eraser_ev(StylusEventKind::Move, 1000.0, 500.0, 4_000), // across C
            eraser_ev(StylusEventKind::Up, 1100.0, 500.0, 5_000),
        ];
        let mut pump = InputPump::start(ScriptedStylus::new(script));
        // Take Down, then stall (simulate durable delete fsync).
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut saw_down = false;
        while Instant::now() < deadline && !saw_down {
            match pump.poll_event().unwrap() {
                SourcePoll::Event(e) if e.kind == StylusEventKind::Down => {
                    assert_eq!(e.tool, StylusTool::Eraser);
                    saw_down = true;
                }
                SourcePoll::WouldBlock => thread::sleep(Duration::from_millis(1)),
                SourcePoll::EndOfStream => panic!("eos before eraser down"),
                _ => {}
            }
        }
        assert!(saw_down);
        thread::sleep(Duration::from_millis(80));
        assert!(
            pump.max_backlog() >= 1,
            "pump must queue eraser Moves/Up during delete stall"
        );

        let mut moves = 0u32;
        let mut up = false;
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline && !up {
            match pump.poll_event().unwrap() {
                SourcePoll::Event(e) => {
                    assert_eq!(e.tool, StylusTool::Eraser);
                    match e.kind {
                        StylusEventKind::Move => moves += 1,
                        StylusEventKind::Up => up = true,
                        StylusEventKind::Down => panic!("extra down"),
                        _ => {}
                    }
                }
                SourcePoll::WouldBlock => thread::sleep(Duration::from_millis(1)),
                SourcePoll::EndOfStream => break,
                SourcePoll::StreamGap { .. } => panic!("unexpected gap"),
            }
        }
        assert_eq!(moves, 3, "all three eraser Moves must be preserved");
        assert!(up, "eraser Up must not be coalesced away");
        pump.stop_and_join();
    }

    #[test]
    fn pump_preserves_order_under_backlog_pressure() {
        let mut events = Vec::new();
        for i in 0..40u64 {
            let base = i * 10_000;
            events.push(ev(StylusEventKind::Down, i as f32, 0.0, base));
            events.push(ev(StylusEventKind::Move, i as f32, 1.0, base + 1));
            events.push(ev(StylusEventKind::Up, i as f32, 2.0, base + 2));
        }
        let mut pump = InputPump::start(ScriptedStylus::new(events));
        thread::sleep(Duration::from_millis(50));
        let max_bl = pump.max_backlog();
        assert!(
            max_bl > 10,
            "expected backlog while consumer idle, got {max_bl}"
        );

        let mut kinds = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            match pump.poll_event().unwrap() {
                SourcePoll::Event(e) => kinds.push(e.kind),
                SourcePoll::WouldBlock => {
                    if kinds.len() >= 120 {
                        break;
                    }
                    thread::sleep(Duration::from_millis(1));
                }
                SourcePoll::EndOfStream => break,
                SourcePoll::StreamGap { .. } => kinds.push(StylusEventKind::Hover), // unreachable marker
            }
        }
        assert_eq!(kinds.len(), 120);
        for chunk in kinds.chunks(3) {
            assert_eq!(
                chunk,
                &[
                    StylusEventKind::Down,
                    StylusEventKind::Move,
                    StylusEventKind::Up
                ]
            );
        }
        pump.stop_and_join();
    }

    /// Source whose proximity flips while it idles (the parser emits no event on exit).
    struct ProximitySource {
        polls: u32,
    }

    impl StylusSource for ProximitySource {
        fn poll_event(&mut self) -> PlatformResult<SourcePoll> {
            self.polls += 1;
            Ok(SourcePoll::WouldBlock)
        }

        fn tool_in_proximity(&self) -> Option<bool> {
            Some(self.polls < 20)
        }
    }

    #[test]
    fn pump_publishes_tool_proximity_including_the_silent_exit() {
        let pump = InputPump::start(ProximitySource { polls: 0 });
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut seen_in = false;
        let mut seen_out = false;
        while Instant::now() < deadline && !(seen_in && seen_out) {
            match pump.tool_in_proximity() {
                Some(true) => seen_in = true,
                Some(false) => seen_out = seen_in,
                None => {}
            }
            thread::sleep(Duration::from_millis(1));
        }
        assert!(seen_in && seen_out, "in={seen_in} out={seen_out}");
        pump.stop_and_join();
    }

    #[test]
    fn scripted_sources_report_unknown_proximity() {
        let pump = InputPump::start(ScriptedStylus::new(Vec::new()));
        thread::sleep(Duration::from_millis(20));
        assert_eq!(pump.tool_in_proximity(), None);
        pump.stop_and_join();
    }
}
