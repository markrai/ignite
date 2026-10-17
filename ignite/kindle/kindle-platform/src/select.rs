//! Stylus device selection: preferred OS symlink first, then capability score.
//!
//! On Kindle Scribe 1st Gen (firmware 5.18.6) the stable path is `/dev/input/stylus` → the
//! `WacomDigitizer` node. `stylus-custom` is a separate virtual/scaled device and must not win
//! merely because it advertises panel-pixel ranges.

use crate::capabilities::{InputCapability, InputDeviceInfo};
use crate::error::{PlatformError, PlatformResult};

/// OS-provided stable symlink on Scribe (and some other Kindles). Never a hardcoded `eventN`.
pub const PREFERRED_STYLUS_PATH: &str = "/dev/input/stylus";

/// True when the device name is the confirmed physical Wacom digitizer.
pub fn is_wacom_digitizer_name(name: &str) -> bool {
    let lower = name.to_lowercase().replace([' ', '_', '-'], "");
    lower.contains("wacomdigitizer") || (lower.contains("wacom") && lower.contains("digitizer"))
}

/// True when the device is the virtual/scaled `stylus-custom` node (not canonical ink input).
pub fn is_stylus_custom_name(name: &str) -> bool {
    name.to_lowercase().contains("stylus-custom")
}

/// Whether a probed preferred-path device is acceptable as the primary pen source.
pub fn preferred_path_is_usable(info: &InputDeviceInfo) -> bool {
    if !info.looks_like_stylus() {
        return false;
    }
    // Never treat stylus-custom as the preferred primary, even if the symlink pointed oddly.
    if is_stylus_custom_name(&info.name) {
        return false;
    }
    // Require real pen evidence: tool and/or pressure (Wacom has both).
    let has_tool =
        info.has(InputCapability::BtnToolPen) || info.has(InputCapability::BtnToolRubber);
    let has_pressure = info.has(InputCapability::AbsPressure);
    let has_xy = info.has(InputCapability::AbsX) && info.has(InputCapability::AbsY);
    has_xy && (has_tool || has_pressure || is_wacom_digitizer_name(&info.name))
}

/// Pick the best stylus among discovered devices, optionally favouring a pre-probed preferred path.
///
/// `preferred` is typically the result of opening [`PREFERRED_STYLUS_PATH`]. When usable it wins
/// outright. Otherwise capability scoring decides; `stylus-custom` is demoted relative to
/// `WacomDigitizer`.
pub fn select_stylus_device(
    devices: &[InputDeviceInfo],
    preferred: Option<&InputDeviceInfo>,
) -> PlatformResult<InputDeviceInfo> {
    if let Some(p) = preferred {
        if preferred_path_is_usable(p) {
            return Ok(p.clone());
        }
    }

    let mut ranked: Vec<&InputDeviceInfo> =
        devices.iter().filter(|d| d.stylus_score() > 0).collect();
    ranked.sort_by(|a, b| {
        b.stylus_score()
            .cmp(&a.stylus_score())
            .then_with(|| {
                // Tie-break: Wacom digitizer before stylus-custom before others.
                rank_name_preference(a).cmp(&rank_name_preference(b))
            })
            .then_with(|| a.path.cmp(&b.path))
    });

    ranked
        .into_iter()
        .next()
        .cloned()
        .ok_or_else(|| PlatformError::NotFound("no stylus device".into()))
}

fn rank_name_preference(d: &InputDeviceInfo) -> u8 {
    if is_wacom_digitizer_name(&d.name) {
        0
    } else if is_stylus_custom_name(&d.name) {
        2
    } else {
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capabilities::{CapabilitySource, DeviceKind, InputCapability};

    fn dev(path: &str, name: &str, caps: Vec<InputCapability>) -> InputDeviceInfo {
        InputDeviceInfo::new(
            path,
            name,
            DeviceKind::Stylus,
            caps,
            CapabilitySource::Synthetic,
        )
    }

    fn wacom_caps() -> Vec<InputCapability> {
        vec![
            InputCapability::AbsX,
            InputCapability::AbsY,
            InputCapability::AbsPressure,
            InputCapability::AbsDistance,
            InputCapability::AbsTiltX,
            InputCapability::AbsTiltY,
            InputCapability::BtnTouch,
            InputCapability::BtnToolPen,
            InputCapability::BtnToolRubber,
            InputCapability::BtnStylus,
        ]
    }

    #[test]
    fn preferred_stylus_path_wins_when_valid() {
        let preferred = dev(PREFERRED_STYLUS_PATH, "WacomDigitizer", wacom_caps());
        let custom = dev(
            "/dev/input/event5",
            "stylus-custom",
            vec![
                InputCapability::AbsX,
                InputCapability::AbsY,
                InputCapability::BtnToolPen,
            ],
        );
        let chosen = select_stylus_device(std::slice::from_ref(&custom), Some(&preferred)).unwrap();
        assert_eq!(chosen.path, PREFERRED_STYLUS_PATH);
        assert_eq!(chosen.name, "WacomDigitizer");
    }

    #[test]
    fn preferred_stylus_custom_is_rejected_then_fallback() {
        let preferred = dev(
            PREFERRED_STYLUS_PATH,
            "stylus-custom",
            vec![
                InputCapability::AbsX,
                InputCapability::AbsY,
                InputCapability::BtnToolPen,
            ],
        );
        let wacom = dev("/dev/input/event3", "WacomDigitizer", wacom_caps());
        let chosen =
            select_stylus_device(&[wacom.clone(), preferred.clone()], Some(&preferred)).unwrap();
        assert_eq!(chosen.name, "WacomDigitizer");
        assert_eq!(chosen.path, "/dev/input/event3");
    }

    #[test]
    fn fallback_when_preferred_absent() {
        let wacom = dev("/dev/input/event3", "WacomDigitizer", wacom_caps());
        let touch = InputDeviceInfo::new(
            "/dev/input/event4",
            "pt_mt",
            DeviceKind::Touch,
            vec![
                InputCapability::AbsX,
                InputCapability::AbsY,
                InputCapability::BtnTouch,
            ],
            CapabilitySource::Synthetic,
        );
        let chosen = select_stylus_device(&[touch, wacom.clone()], None).unwrap();
        assert_eq!(chosen.name, "WacomDigitizer");
    }

    #[test]
    fn wacom_wins_over_stylus_custom_on_score() {
        let wacom = dev("/dev/input/event3", "WacomDigitizer", wacom_caps());
        let custom = dev(
            "/dev/input/event5",
            "stylus-custom",
            vec![
                InputCapability::AbsX,
                InputCapability::AbsY,
                InputCapability::BtnToolPen,
                InputCapability::BtnTouch,
            ],
        );
        let custom_score = custom.stylus_score();
        let chosen = select_stylus_device(&[custom, wacom.clone()], None).unwrap();
        assert_eq!(chosen.name, "WacomDigitizer");
        assert!(wacom.stylus_score() > custom_score);
    }

    #[test]
    fn preferred_path_constant_is_not_an_event_number() {
        assert!(!PREFERRED_STYLUS_PATH.contains("event"));
    }
}
