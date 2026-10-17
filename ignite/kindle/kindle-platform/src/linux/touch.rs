//! Read-only observer of the Scribe touchscreen (`pt_mt`).
//!
//! This reader deliberately has **no grab method**: `/dev/input/touch` (event4) is never
//! `EVIOCGRAB`'d, so Xorg/Kindle keep receiving the identical stream for system gestures.

use crate::axes::AxisRange;
use crate::error::{PlatformError, PlatformResult};
use crate::evdev_abi::{abs, decode_native, INPUT_EVENT_SIZE};
use crate::linux::fd::{abs_info, OwnedFd};
use crate::touch_mt::{MtTouchDecoder, TouchFeed, TouchFrame};

const READ_BATCH: usize = 64;

/// Device-reported multitouch position ranges (never assumed).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TouchAxes {
    pub x: AxisRange,
    pub y: AxisRange,
    pub slot_max: i32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TouchPoll {
    Frame(TouchFrame),
    Gap,
    WouldBlock,
    EndOfStream,
}

#[derive(Debug)]
pub struct LinuxEvdevTouch {
    path: String,
    fd: OwnedFd,
    axes: TouchAxes,
    decoder: MtTouchDecoder,
    buf: Vec<u8>,
    buffered: usize,
    cursor: usize,
}

impl LinuxEvdevTouch {
    /// Open non-blocking, read-only, **without** `EVIOCGRAB`.
    pub fn open_observer(path: impl Into<String>) -> PlatformResult<Self> {
        let path = path.into();
        let fd = OwnedFd::open_read(&path, true)?;
        let x = abs_info(&fd, abs::MT_POSITION_X)?;
        let y = abs_info(&fd, abs::MT_POSITION_Y)?;
        let slot = abs_info(&fd, abs::MT_SLOT)?;
        if x.maximum <= x.minimum || y.maximum <= y.minimum {
            return Err(PlatformError::Io(format!(
                "touch {path}: degenerate MT position range x={}..{} y={}..{}",
                x.minimum, x.maximum, y.minimum, y.maximum
            )));
        }
        Ok(Self {
            path,
            fd,
            axes: TouchAxes {
                x: AxisRange {
                    min: x.minimum,
                    max: x.maximum,
                    fuzz: x.fuzz,
                    flat: x.flat,
                    resolution: x.resolution,
                },
                y: AxisRange {
                    min: y.minimum,
                    max: y.maximum,
                    fuzz: y.fuzz,
                    flat: y.flat,
                    resolution: y.resolution,
                },
                slot_max: slot.maximum,
            },
            decoder: MtTouchDecoder::new(),
            buf: vec![0u8; INPUT_EVENT_SIZE * READ_BATCH],
            buffered: 0,
            cursor: 0,
        })
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    pub fn axes(&self) -> TouchAxes {
        self.axes
    }

    pub fn poll(&mut self) -> PlatformResult<TouchPoll> {
        loop {
            while self.cursor + INPUT_EVENT_SIZE <= self.buffered {
                let rec = &self.buf[self.cursor..self.cursor + INPUT_EVENT_SIZE];
                self.cursor += INPUT_EVENT_SIZE;
                let Some(ev) = decode_native(rec) else {
                    continue;
                };
                match self.decoder.feed(ev) {
                    TouchFeed::None => {}
                    TouchFeed::Frame(f) => return Ok(TouchPoll::Frame(f)),
                    TouchFeed::Gap => return Ok(TouchPoll::Gap),
                }
            }
            // Keep any partial record, then refill.
            let rest = self.buffered - self.cursor;
            self.buf.copy_within(self.cursor..self.buffered, 0);
            self.buffered = rest;
            self.cursor = 0;
            match self.fd.read(&mut self.buf[self.buffered..])? {
                None => return Ok(TouchPoll::WouldBlock),
                Some(0) => return Ok(TouchPoll::EndOfStream),
                Some(n) => self.buffered += n,
            }
        }
    }
}

/// Background drain of the touch observer. Each item carries the monotonic instant it was
/// read (touch kernel timestamps are `CLOCK_REALTIME` and are not used for gesture timing).
///
/// The channel is **unbounded** and FIFO: the reader thread keeps producing while the
/// consumer is busy (e.g. selection-move preview recompose). Depth is exposed so lag
/// diagnosis can prove consumer backpressure without guessing.
pub struct TouchPump {
    rx: std::sync::mpsc::Receiver<(std::time::Instant, TouchPoll)>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    join: Option<std::thread::JoinHandle<()>>,
    backlog: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    max_backlog: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    produced_seq: std::sync::Arc<std::sync::atomic::AtomicU64>,
    consumed_seq: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl TouchPump {
    pub fn start(mut src: LinuxEvdevTouch) -> Self {
        use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
        use std::sync::Arc;
        let (tx, rx) = std::sync::mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let backlog = Arc::new(AtomicUsize::new(0));
        let max_backlog = Arc::new(AtomicUsize::new(0));
        let produced_seq = Arc::new(AtomicU64::new(0));
        let consumed_seq = Arc::new(AtomicU64::new(0));
        let stop_t = Arc::clone(&stop);
        let backlog_t = Arc::clone(&backlog);
        let max_t = Arc::clone(&max_backlog);
        let produced_t = Arc::clone(&produced_seq);
        let join = std::thread::spawn(move || {
            while !stop_t.load(Ordering::SeqCst) {
                match src.poll() {
                    Ok(TouchPoll::WouldBlock) => {
                        std::thread::sleep(std::time::Duration::from_millis(2));
                    }
                    Ok(item) => {
                        let end = item == TouchPoll::EndOfStream;
                        let n = backlog_t.fetch_add(1, Ordering::SeqCst) + 1;
                        max_t.fetch_max(n, Ordering::SeqCst);
                        produced_t.fetch_add(1, Ordering::SeqCst);
                        if tx.send((std::time::Instant::now(), item)).is_err() || end {
                            break;
                        }
                    }
                    Err(_) => {
                        let n = backlog_t.fetch_add(1, Ordering::SeqCst) + 1;
                        max_t.fetch_max(n, Ordering::SeqCst);
                        produced_t.fetch_add(1, Ordering::SeqCst);
                        let _ = tx.send((std::time::Instant::now(), TouchPoll::EndOfStream));
                        break;
                    }
                }
            }
        });
        Self {
            rx,
            stop,
            join: Some(join),
            backlog,
            max_backlog,
            produced_seq,
            consumed_seq,
        }
    }

    /// Current queued depth (producer ahead of consumer).
    pub fn backlog(&self) -> usize {
        self.backlog.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Peak queued depth since start.
    pub fn max_backlog(&self) -> usize {
        self.max_backlog.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Monotonic count of items enqueued by the reader thread.
    pub fn produced_seq(&self) -> u64 {
        self.produced_seq.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Monotonic count of items dequeued by [`Self::try_next`].
    pub fn consumed_seq(&self) -> u64 {
        self.consumed_seq.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Next queued item, if any (never blocks). Oldest-first.
    pub fn try_next(&self) -> Option<(std::time::Instant, TouchPoll)> {
        match self.rx.try_recv() {
            Ok(item) => {
                self.backlog
                    .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                self.consumed_seq
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Some(item)
            }
            Err(_) => None,
        }
    }
}

impl Drop for TouchPump {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}
