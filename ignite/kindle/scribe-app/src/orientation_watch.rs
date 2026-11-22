//! Detect Kindle orientation events that can repaint the stock X11 layer over Ignite.
//!
//! Ignite is registered upright-only and draws directly into `/dev/fb0`. On Scribe FW 5.18.6,
//! the stock window manager can still repaint its white background window after an accelerometer
//! orientation interrupt without sending Ignite an app lifecycle or framebuffer-rotation event.
//! The dedicated `bma4xy_feature` evdev node reports those interrupts as `ABS_PRESSURE` values.
//! This observer never grabs the node; the runtime debounces events and reconstructs its current
//! view after the stock relayout settles.

use std::fs::{self, File};
use std::io::Read;
use std::path::Path;
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

use kindle_platform::codes::{abs, ev};
use kindle_platform::{decode_native, RawInputEvent, INPUT_EVENT_SIZE};

const DEVICE_NAME: &str = "bma4xy_feature";
const SYS_CLASS_INPUT: &str = "/sys/class/input";
const DEV_INPUT: &str = "/dev/input";

/// Delay after the newest sensor event before Ignite retakes the panel.
pub(crate) const SETTLE: Duration = Duration::from_millis(1200);

fn orientation_value(event: RawInputEvent) -> Option<i32> {
    (event.ev_type == ev::ABS && event.code == abs::PRESSURE).then_some(event.value)
}

fn device_path() -> Option<String> {
    device_path_in(Path::new(SYS_CLASS_INPUT), Path::new(DEV_INPUT))
}

/// Resolve the event node by its kernel-reported name; event numbering is never assumed.
fn device_path_in(sys_class_input: &Path, dev_input: &Path) -> Option<String> {
    let mut entries: Vec<_> = fs::read_dir(sys_class_input).ok()?.flatten().collect();
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let file_name = entry.file_name();
        let Some(event) = file_name.to_str() else {
            continue;
        };
        if !event.starts_with("event") {
            continue;
        }
        let Ok(name) = fs::read_to_string(entry.path().join("device/name")) else {
            continue;
        };
        if name.trim().eq_ignore_ascii_case(DEVICE_NAME) {
            let path = dev_input.join(event);
            if path.exists() {
                return Some(path.to_string_lossy().into_owned());
            }
        }
    }
    None
}

/// Non-grabbing observer for the Kindle orientation feature node.
pub(crate) struct Watcher {
    rx: Receiver<i32>,
}

impl Watcher {
    /// `None` on hosts or Kindle firmware without the confirmed feature node.
    pub(crate) fn start() -> Option<Self> {
        let path = device_path()?;
        let mut input = match File::open(&path) {
            Ok(input) => input,
            Err(error) => {
                println!("orientation_watch=unavailable path={path} reason={error}");
                return None;
            }
        };
        let (tx, rx) = mpsc::channel();
        let thread_path = path.clone();
        std::thread::spawn(move || loop {
            let mut record = [0u8; INPUT_EVENT_SIZE];
            if let Err(error) = input.read_exact(&mut record) {
                println!("orientation_watch=stopped path={thread_path} reason={error}");
                return;
            }
            let Some(event) = decode_native(&record) else {
                continue;
            };
            if let Some(value) = orientation_value(event) {
                if tx.send(value).is_err() {
                    return;
                }
            }
        });
        println!("orientation_watch=started path={path}");
        Some(Self { rx })
    }

    /// Drain queued transitions and return only the newest orientation value.
    pub(crate) fn take_latest(&self) -> Option<i32> {
        let mut latest = None;
        while let Ok(value) = self.rx.try_recv() {
            latest = Some(value);
        }
        latest
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kindle_platform::KernelTimestamp;

    fn record(ev_type: u16, code: u16, value: i32) -> RawInputEvent {
        RawInputEvent {
            time: KernelTimestamp::default(),
            ev_type,
            code,
            value,
        }
    }

    #[test]
    fn only_orientation_feature_records_trigger_repaint() {
        assert_eq!(
            orientation_value(record(ev::ABS, abs::PRESSURE, 2)),
            Some(2)
        );
        assert_eq!(orientation_value(record(ev::ABS, abs::X, 2)), None);
        assert_eq!(orientation_value(record(ev::SYN, 0, 0)), None);
    }
}
