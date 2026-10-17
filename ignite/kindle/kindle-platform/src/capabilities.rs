//! Input device capability descriptors (not event node numbers).
//!
//! Device selection is evidence-based: Linux capability bits first, device names second. Known
//! Kindle Scribe device names (`WacomDigitizer`, `stylus-custom`, ...) are used only as scoring
//! evidence; no `/dev/input/eventN` path is ever assumed, because event numbering is not stable
//! across boots, firmware versions, or hotplug order.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeviceKind {
    Stylus,
    Touch,
    Button,
    Unknown,
}

/// Subset of Linux input capabilities we care about for discovery.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputCapability {
    AbsX,
    AbsY,
    AbsPressure,
    AbsDistance,
    AbsTiltX,
    AbsTiltY,
    BtnTouch,
    BtnToolPen,
    BtnToolRubber,
    BtnStylus,
    BtnStylus2,
}

/// How a device's capability list was obtained. Affects how much it can be trusted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CapabilitySource {
    /// `EVIOCGBIT` on the device node: a byte-addressed bitmap, ABI-independent.
    Ioctl,
    /// sysfs `capabilities/*` hex words, where both plausible kernel word widths agree.
    SysfsUnambiguous,
    /// sysfs hex words where the 32-bit and 64-bit readings disagree; bits in the disputed
    /// range were dropped rather than guessed.
    SysfsAmbiguous,
    /// Constructed by a test / host fixture.
    Synthetic,
    /// No capability information available.
    None,
}

/// Device names publicly reported / confirmed for Kindle Scribe input nodes.
///
/// Evidence only for scoring. Event node numbers are never assumed.
/// Confirmed on Scribe 1st Gen 5.18.6: `WacomDigitizer` is the physical pen; `stylus-custom`
/// is a separate virtual/scaled node (not the canonical ink source).
pub const KNOWN_SCRIBE_STYLUS_NAMES: &[&str] = &["wacomdigitizer", "stylus-custom", "stylus"];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InputDeviceInfo {
    /// Path such as `/dev/input/event12`, discovered at runtime, never assumed.
    pub path: String,
    pub name: String,
    pub kind: DeviceKind,
    pub capabilities: Vec<InputCapability>,
    pub capability_source: CapabilitySource,
}

impl InputDeviceInfo {
    pub fn new(
        path: impl Into<String>,
        name: impl Into<String>,
        kind: DeviceKind,
        capabilities: Vec<InputCapability>,
        capability_source: CapabilitySource,
    ) -> Self {
        Self {
            path: path.into(),
            name: name.into(),
            kind,
            capabilities,
            capability_source,
        }
    }

    pub fn has(&self, cap: InputCapability) -> bool {
        self.capabilities.contains(&cap)
    }

    /// How strongly this device looks like the pen digitizer. `0` means "not a candidate".
    ///
    /// Capability bits dominate; names only break ties and rescue devices whose capability
    /// bitmaps we could not read unambiguously.
    pub fn stylus_score(&self) -> u32 {
        let has_xy = self.has(InputCapability::AbsX) && self.has(InputCapability::AbsY);
        let has_tool = self.has(InputCapability::BtnToolPen)
            || self.has(InputCapability::BtnToolRubber)
            || self.has(InputCapability::BtnStylus);
        let has_touch = self.has(InputCapability::BtnTouch);
        let has_pressure = self.has(InputCapability::AbsPressure);

        let mut score = 0u32;
        if has_xy {
            score += 20;
        }
        if has_tool {
            score += 30;
        }
        if has_touch {
            score += 10;
        }
        if has_pressure {
            score += 15;
        }
        if self.has(InputCapability::AbsDistance) {
            score += 5;
        }
        if self.has(InputCapability::AbsTiltX) || self.has(InputCapability::AbsTiltY) {
            score += 5;
        }

        let lower = self.name.to_lowercase();
        if crate::select::is_wacom_digitizer_name(&self.name) {
            // Confirmed physical pen source on Scribe 1st Gen.
            score += 50;
        } else if crate::select::is_stylus_custom_name(&self.name) {
            // Virtual/scaled node (advertises panel pixels). Useful for later MTK work, not
            // for ordinary ink. Keep discoverable but demoted.
            score += 5;
        } else if KNOWN_SCRIBE_STYLUS_NAMES.iter().any(|n| lower.contains(n)) {
            score += 25;
        } else if lower.contains("wacom") || lower.contains("digitizer") || lower.contains("pen") {
            score += 15;
        }
        if self.kind == DeviceKind::Stylus {
            score += 10;
        }

        // A multitouch panel with no pen tool bits is not our device, however it is named.
        let multitouch_only = (lower.contains("touch") || lower.contains("mt"))
            && !has_tool
            && !has_pressure
            && self.kind != DeviceKind::Stylus;
        if multitouch_only {
            return 0;
        }

        // Positional axes alone are not enough: require a tool/pressure hint or a name hint.
        if score > 0 && !has_tool && !has_pressure && self.kind != DeviceKind::Stylus {
            let name_hint = lower.contains("wacom")
                || lower.contains("stylus")
                || lower.contains("pen")
                || lower.contains("digitizer");
            if !name_hint {
                return 0;
            }
        }
        score
    }

    pub fn looks_like_stylus(&self) -> bool {
        self.stylus_score() > 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use InputCapability::*;

    fn dev(name: &str, kind: DeviceKind, caps: Vec<InputCapability>) -> InputDeviceInfo {
        InputDeviceInfo::new(
            "/dev/input/event0",
            name,
            kind,
            caps,
            CapabilitySource::Ioctl,
        )
    }

    #[test]
    fn pen_digitizer_scores_above_touchscreen() {
        let pen = dev(
            "WacomDigitizer",
            DeviceKind::Stylus,
            vec![AbsX, AbsY, AbsPressure, BtnTouch, BtnToolPen],
        );
        let touch = dev(
            "mtk-tpd multitouch",
            DeviceKind::Touch,
            vec![AbsX, AbsY, BtnTouch],
        );
        assert!(pen.stylus_score() > touch.stylus_score());
        assert!(pen.looks_like_stylus());
        assert!(!touch.looks_like_stylus());
    }

    #[test]
    fn stylus_custom_name_is_evidence_not_a_path() {
        let d = dev("stylus-custom", DeviceKind::Unknown, vec![AbsX, AbsY]);
        assert!(d.looks_like_stylus());
        // Scoring never encodes a node number.
        assert!(!d.path.is_empty());
    }

    #[test]
    fn wacom_digitizer_scores_above_stylus_custom() {
        let wacom = dev(
            "WacomDigitizer",
            DeviceKind::Stylus,
            vec![
                AbsX,
                AbsY,
                AbsPressure,
                AbsDistance,
                AbsTiltX,
                AbsTiltY,
                BtnTouch,
                BtnToolPen,
            ],
        );
        let custom = dev(
            "stylus-custom",
            DeviceKind::Stylus,
            vec![AbsX, AbsY, BtnToolPen, BtnTouch],
        );
        assert!(wacom.stylus_score() > custom.stylus_score());
    }

    #[test]
    fn capability_only_device_without_a_name_still_qualifies() {
        let d = dev("input-0", DeviceKind::Unknown, vec![AbsX, AbsY, BtnToolPen]);
        assert!(d.looks_like_stylus());
    }

    #[test]
    fn bare_xy_device_is_not_a_stylus_candidate() {
        let d = dev("accelerometer", DeviceKind::Unknown, vec![AbsX, AbsY]);
        assert_eq!(d.stylus_score(), 0);
    }

    #[test]
    fn eraser_tool_bit_counts_as_pen_evidence() {
        let d = dev(
            "unnamed",
            DeviceKind::Unknown,
            vec![AbsX, AbsY, BtnToolRubber],
        );
        assert!(d.looks_like_stylus());
    }

    #[test]
    fn no_capabilities_no_name_scores_zero() {
        let d = InputDeviceInfo::new(
            "/dev/input/event9",
            "event9",
            DeviceKind::Unknown,
            vec![],
            CapabilitySource::None,
        );
        assert_eq!(d.stylus_score(), 0);
    }
}
