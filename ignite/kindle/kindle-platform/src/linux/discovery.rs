//! Discover input devices by name and capabilities. Never hardcode `eventN`.
//!
//! Event numbering is not stable across boots, firmware updates or probe order, so known Scribe
//! node numbers from public logs are useless as paths. Known *names* are useful, but only as
//! scoring evidence inside [`crate::capabilities::InputDeviceInfo::stylus_score`].
//!
//! Capabilities come from `EVIOCGBIT` on the node when it can be opened (byte-addressed, so no
//! kernel word-size assumption). The sysfs text bitmaps are a fallback, parsed by
//! [`crate::sysfs_bitmap`], which refuses to guess when the two plausible word widths disagree.

use std::fs;
use std::path::Path;

use crate::capabilities::{CapabilitySource, DeviceKind, InputCapability, InputDeviceInfo};
use crate::error::{PlatformError, PlatformResult};
use crate::evdev_abi::{abs, btn, ev};
use crate::evdev_ioctl::{bitmap_bit, bitmap_len_bytes, ABS_MAX, KEY_MAX};
use crate::input::InputDiscovery;
use crate::linux::fd::{capability_bitmap, device_name, OwnedFd};
use crate::sysfs_bitmap::{bit_evidence, BitEvidence};

const ABS_CAPS: &[(u16, InputCapability)] = &[
    (abs::X, InputCapability::AbsX),
    (abs::Y, InputCapability::AbsY),
    (abs::PRESSURE, InputCapability::AbsPressure),
    (abs::DISTANCE, InputCapability::AbsDistance),
    (abs::TILT_X, InputCapability::AbsTiltX),
    (abs::TILT_Y, InputCapability::AbsTiltY),
];

const KEY_CAPS: &[(u16, InputCapability)] = &[
    (btn::TOUCH, InputCapability::BtnTouch),
    (btn::TOOL_PEN, InputCapability::BtnToolPen),
    (btn::TOOL_RUBBER, InputCapability::BtnToolRubber),
    (btn::STYLUS, InputCapability::BtnStylus),
    (btn::STYLUS2, InputCapability::BtnStylus2),
];

/// Scans `/sys/class/input` for `event*` nodes, then queries each node's capabilities.
#[derive(Debug, Clone)]
pub struct LinuxInputDiscovery {
    sys_class_input: String,
    dev_input: String,
    /// When false, only sysfs text is consulted (used by fixture-tree tests).
    allow_ioctl: bool,
}

impl Default for LinuxInputDiscovery {
    fn default() -> Self {
        Self::new()
    }
}

impl LinuxInputDiscovery {
    pub fn new() -> Self {
        Self {
            sys_class_input: "/sys/class/input".into(),
            dev_input: "/dev/input".into(),
            allow_ioctl: true,
        }
    }

    /// Injectable roots for tests with fixtures. Node ioctls are disabled.
    pub fn with_roots(sys_class_input: impl Into<String>, dev_input: impl Into<String>) -> Self {
        Self {
            sys_class_input: sys_class_input.into(),
            dev_input: dev_input.into(),
            allow_ioctl: false,
        }
    }
}

impl InputDiscovery for LinuxInputDiscovery {
    fn list_devices(&self) -> PlatformResult<Vec<InputDeviceInfo>> {
        let root = Path::new(&self.sys_class_input);
        if !root.exists() {
            return Err(PlatformError::NotFound(format!(
                "{} missing (not a Linux input sysfs tree)",
                self.sys_class_input
            )));
        }
        let mut out = Vec::new();
        let entries = fs::read_dir(root).map_err(|e| PlatformError::Io(e.to_string()))?;
        for entry in entries.flatten() {
            let file_name = entry.file_name();
            let file_name = file_name.to_string_lossy();
            if !file_name.starts_with("event") {
                continue;
            }
            let dev_path = format!("{}/{}", self.dev_input, file_name);
            let sys_path = entry.path();

            let ioctl_probe = if self.allow_ioctl {
                probe_node(&dev_path)
            } else {
                None
            };

            let sysfs_name = read_trimmed(sys_path.join("device/name"))
                .or_else(|| read_trimmed(sys_path.join("name")));
            let (name, caps, source) = match ioctl_probe {
                Some((n, caps)) => (
                    if n.is_empty() {
                        sysfs_name.unwrap_or_else(|| file_name.to_string())
                    } else {
                        n
                    },
                    caps,
                    CapabilitySource::Ioctl,
                ),
                None => {
                    let (caps, source) = read_sysfs_capabilities(&sys_path);
                    (
                        sysfs_name.unwrap_or_else(|| file_name.to_string()),
                        caps,
                        source,
                    )
                }
            };

            let kind = classify_kind(&name, &caps);
            out.push(InputDeviceInfo::new(dev_path, name, kind, caps, source));
        }
        out.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(out)
    }

    fn find_stylus(&self) -> PlatformResult<InputDeviceInfo> {
        let devices = self.list_devices()?;
        let preferred = self.probe_preferred_stylus();
        crate::select::select_stylus_device(&devices, preferred.as_ref())
    }
}

impl LinuxInputDiscovery {
    /// Probe the stable `/dev/input/stylus` symlink when present (real device only).
    fn probe_preferred_stylus(&self) -> Option<InputDeviceInfo> {
        if !self.allow_ioctl {
            // Fixture trees have no character devices; still honour a synthetic preferred entry
            // already present in list_devices via the default select path.
            return None;
        }
        let path = crate::select::PREFERRED_STYLUS_PATH;
        let (name, caps) = probe_node(path)?;
        let kind = classify_kind(&name, &caps);
        let info = InputDeviceInfo::new(path, name, kind, caps, CapabilitySource::Ioctl);
        if crate::select::preferred_path_is_usable(&info) {
            Some(info)
        } else {
            None
        }
    }
}

/// Open the node and ask the kernel directly. Returns `None` when the node cannot be opened
/// (missing, or insufficient permissions), in which case the caller falls back to sysfs.
fn probe_node(dev_path: &str) -> Option<(String, Vec<InputCapability>)> {
    let fd = OwnedFd::open_read(dev_path, true).ok()?;
    let name = device_name(&fd).unwrap_or_default();
    let mut caps = Vec::new();
    if let Ok(bits) = capability_bitmap(&fd, ev::ABS, bitmap_len_bytes(ABS_MAX)) {
        for &(code, cap) in ABS_CAPS {
            if bitmap_bit(&bits, code as usize) {
                caps.push(cap);
            }
        }
    }
    if let Ok(bits) = capability_bitmap(&fd, ev::KEY, bitmap_len_bytes(KEY_MAX)) {
        for &(code, cap) in KEY_CAPS {
            if bitmap_bit(&bits, code as usize) {
                caps.push(cap);
            }
        }
    }
    Some((name, caps))
}

fn read_trimmed(path: impl AsRef<Path>) -> Option<String> {
    fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn classify_kind(name: &str, caps: &[InputCapability]) -> DeviceKind {
    if caps.contains(&InputCapability::BtnToolPen) || caps.contains(&InputCapability::BtnToolRubber)
    {
        return DeviceKind::Stylus;
    }
    let lower = name.to_lowercase();
    if lower.contains("wacom") || lower.contains("stylus") || lower.contains("digitizer") {
        return DeviceKind::Stylus;
    }
    if lower.contains("touch") || lower.contains("mt") {
        return DeviceKind::Touch;
    }
    if lower.contains("key") || lower.contains("button") || lower.contains("gpio") {
        return DeviceKind::Button;
    }
    DeviceKind::Unknown
}

/// sysfs fallback. Bits whose reading depends on the kernel word width are dropped, and the
/// device is marked [`CapabilitySource::SysfsAmbiguous`] so callers know the list is partial.
fn read_sysfs_capabilities(sys_path: &Path) -> (Vec<InputCapability>, CapabilitySource) {
    let mut caps = Vec::new();
    let mut any = false;
    let mut ambiguous = false;

    let abs_mask = read_trimmed(sys_path.join("device/capabilities/abs"))
        .or_else(|| read_trimmed(sys_path.join("capabilities/abs")));
    if let Some(mask) = abs_mask {
        any = true;
        for &(code, cap) in ABS_CAPS {
            match bit_evidence(&mask, code as usize) {
                BitEvidence::Present => caps.push(cap),
                BitEvidence::Ambiguous => ambiguous = true,
                BitEvidence::Absent => {}
            }
        }
    }

    let key_mask = read_trimmed(sys_path.join("device/capabilities/key"))
        .or_else(|| read_trimmed(sys_path.join("capabilities/key")));
    if let Some(mask) = key_mask {
        any = true;
        for &(code, cap) in KEY_CAPS {
            match bit_evidence(&mask, code as usize) {
                BitEvidence::Present => caps.push(cap),
                BitEvidence::Ambiguous => ambiguous = true,
                BitEvidence::Absent => {}
            }
        }
    }

    let source = match (any, ambiguous) {
        (false, _) => CapabilitySource::None,
        (true, true) => CapabilitySource::SysfsAmbiguous,
        (true, false) => CapabilitySource::SysfsUnambiguous,
    };
    (caps, source)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    struct Fixture {
        root: std::path::PathBuf,
    }

    impl Fixture {
        fn new(tag: &str) -> Self {
            let root = std::env::temp_dir()
                .join(format!("scribe_input_fixture_{}_{tag}", std::process::id()));
            let _ = fs::remove_dir_all(&root);
            fs::create_dir_all(root.join("dev/input")).unwrap();
            Self { root }
        }

        fn node(&self, event: &str, name: &str, abs_mask: &str, key_mask: &str) {
            let dir = self.root.join("sys/class/input").join(event);
            fs::create_dir_all(dir.join("device/capabilities")).unwrap();
            fs::write(dir.join("device/name"), format!("{name}\n")).unwrap();
            fs::write(dir.join("device/capabilities/abs"), format!("{abs_mask}\n")).unwrap();
            fs::write(dir.join("device/capabilities/key"), format!("{key_mask}\n")).unwrap();
        }

        fn discovery(&self) -> LinuxInputDiscovery {
            LinuxInputDiscovery::with_roots(
                self.root.join("sys/class/input").to_string_lossy(),
                self.root.join("dev/input").to_string_lossy(),
            )
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn discovers_stylus_by_name_and_axes_without_any_node_number() {
        let f = Fixture::new("basic");
        // abs: X | Y | PRESSURE
        f.node("event5", "WacomDigitizer", "1000003", "0");
        let devices = f.discovery().list_devices().unwrap();
        assert_eq!(devices.len(), 1);
        assert!(devices[0].looks_like_stylus());
        assert!(devices[0].has(InputCapability::AbsPressure));
        // The path comes from the scan, not from a constant in our code.
        assert!(devices[0].path.ends_with("event5"));
    }

    #[test]
    fn find_stylus_prefers_the_pen_over_the_touchscreen() {
        let f = Fixture::new("scoring");
        f.node("event1", "mtk-tpd", "3", "0");
        f.node("event9", "WacomDigitizer", "1000003", "0");
        let d = f.discovery();
        let picked = d.find_stylus().unwrap();
        assert_eq!(picked.name, "WacomDigitizer");
        // Selection is by score, not by the lower/higher event number.
        assert!(picked.path.ends_with("event9"));
    }

    #[test]
    fn find_stylus_prefers_wacom_over_stylus_custom() {
        let f = Fixture::new("wacom_vs_custom");
        f.node("event5", "stylus-custom", "3", "0");
        f.node("event3", "WacomDigitizer", "1000003", "0");
        let picked = f.discovery().find_stylus().unwrap();
        assert_eq!(picked.name, "WacomDigitizer");
        assert!(picked.path.ends_with("event3"));
    }

    #[test]
    fn stylus_custom_name_is_recognised_as_evidence() {
        let f = Fixture::new("custom");
        f.node("event12", "stylus-custom", "3", "0");
        let picked = f.discovery().find_stylus().unwrap();
        assert_eq!(picked.name, "stylus-custom");
    }

    #[test]
    fn ambiguous_sysfs_key_bitmap_is_flagged_not_guessed() {
        let f = Fixture::new("ambiguous");
        // BTN_TOOL_PEN set only under a 64-bit-word reading.
        f.node("event3", "unnamed-digitiser", "3", "1 0 0 0 0 0");
        let devices = f.discovery().list_devices().unwrap();
        assert_eq!(
            devices[0].capability_source,
            CapabilitySource::SysfsAmbiguous
        );
        assert!(!devices[0].has(InputCapability::BtnToolPen));
    }

    #[test]
    fn unambiguous_sysfs_bitmap_is_marked_as_such() {
        let f = Fixture::new("unambiguous");
        f.node("event4", "thing", "1000003", "0");
        let devices = f.discovery().list_devices().unwrap();
        assert_eq!(
            devices[0].capability_source,
            CapabilitySource::SysfsUnambiguous
        );
    }

    #[test]
    fn missing_sysfs_tree_is_an_error_not_an_empty_list() {
        let d = LinuxInputDiscovery::with_roots("/nonexistent/sys/class/input", "/nonexistent/dev");
        assert!(matches!(d.list_devices(), Err(PlatformError::NotFound(_))));
    }

    #[test]
    fn no_candidate_yields_not_found() {
        let f = Fixture::new("nocandidate");
        f.node("event0", "gpio-keys", "0", "0");
        assert!(f.discovery().find_stylus().is_err());
    }

    #[test]
    fn classify_kind_uses_capabilities_before_names() {
        assert_eq!(
            classify_kind("some-panel", &[InputCapability::BtnToolPen]),
            DeviceKind::Stylus
        );
        assert_eq!(classify_kind("mtk-tpd", &[]), DeviceKind::Touch);
        assert_eq!(classify_kind("gpio-keys", &[]), DeviceKind::Button);
    }
}
