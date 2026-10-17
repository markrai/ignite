//! Stylus-mode lifecycle (host-testable; no ioctl here).
//!
//! Stock WetInkHandler on FW 5.18.6 uses `MXCFB_SET_STYLUS_MODE` with modes **0** / **1**
//! only, and issues **DU/PARTIAL** `MXCFB_SEND_UPDATE_MTK` while armed (Gate E1.1).
//! Ordinary **GC16** SEND remains forbidden while armed (Gate E1.2).

use crate::error::{PlatformError, PlatformResult};
use crate::hwtcon_abi::stylus_mode;

/// Ordinary HWTCON path vs stylus-mode-armed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum StylusModePhase {
    #[default]
    Ordinary,
    StylusModeArmed,
}

/// Class of `MXCFB_SEND_UPDATE_MTK` payload for lifecycle checks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UpdateClass {
    /// Gate B/C/D ordinary refresh: GC16 / PARTIAL / ambient (not stock wet ink).
    Ordinary,
    /// Stock WetInkHandler wet path: DU / PARTIAL / ambient (Gate E1.1 / E1.2).
    WetInkDu,
}

/// Validates diagnostic stylus-mode arguments (modes 0/1 only).
pub fn validate_e1_stylus_mode(mode: u32) -> PlatformResult<()> {
    match mode {
        stylus_mode::DISABLED | stylus_mode::WITH_NO_TPS => Ok(()),
        stylus_mode::WITH_BLACK_TPS | stylus_mode::WITH_WHITE_TPS => {
            Err(PlatformError::Unsupported(
                "rejects stylus modes 2/3 (WITH_*_TPS); WetInkHandler used 0/1 only",
            ))
        }
        _ => Err(PlatformError::Unsupported(
            "rejects non-stock stylus mode (allowed: 0 DISABLED, 1 WITH_NO_TPS)",
        )),
    }
}

/// Tiny controller: `Ordinary → Armed → Ordinary` with update-class gating.
#[derive(Clone, Debug, Default)]
pub struct StylusModeLifecycle {
    phase: StylusModePhase,
}

impl StylusModeLifecycle {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn phase(&self) -> StylusModePhase {
        self.phase
    }

    pub fn is_armed(&self) -> bool {
        self.phase == StylusModePhase::StylusModeArmed
    }

    /// Record successful arm (mode=1 ioctl already succeeded).
    pub fn mark_armed(&mut self) -> PlatformResult<()> {
        match self.phase {
            StylusModePhase::Ordinary => {
                self.phase = StylusModePhase::StylusModeArmed;
                Ok(())
            }
            StylusModePhase::StylusModeArmed => {
                Err(PlatformError::Io("stylus mode already armed".into()))
            }
        }
    }

    /// Record successful disarm (mode=0 ioctl already succeeded).
    pub fn mark_disarmed(&mut self) -> PlatformResult<()> {
        match self.phase {
            StylusModePhase::StylusModeArmed => {
                self.phase = StylusModePhase::Ordinary;
                Ok(())
            }
            StylusModePhase::Ordinary => Err(PlatformError::Io(
                "stylus mode already ordinary (not armed)".into(),
            )),
        }
    }

    /// Whether [`UpdateClass`] may be submitted in the current phase.
    ///
    /// * [`UpdateClass::Ordinary`] (GC16) — only when **not** armed.
    /// * [`UpdateClass::WetInkDu`] — only when **armed** (stock wet path).
    pub fn can_send(&self, class: UpdateClass) -> bool {
        match (self.phase, class) {
            (StylusModePhase::Ordinary, UpdateClass::Ordinary) => true,
            (StylusModePhase::Ordinary, UpdateClass::WetInkDu) => false,
            (StylusModePhase::StylusModeArmed, UpdateClass::Ordinary) => false,
            (StylusModePhase::StylusModeArmed, UpdateClass::WetInkDu) => true,
        }
    }

    pub fn require_send(&self, class: UpdateClass) -> PlatformResult<()> {
        if self.can_send(class) {
            Ok(())
        } else {
            Err(PlatformError::Unsupported(match (self.phase, class) {
                (StylusModePhase::StylusModeArmed, UpdateClass::Ordinary) => {
                    "ordinary GC16 SEND_UPDATE forbidden while StylusModeArmed"
                }
                (StylusModePhase::Ordinary, UpdateClass::WetInkDu) => {
                    "WetInk DU SEND_UPDATE only allowed while StylusModeArmed"
                }
                _ => "SEND_UPDATE rejected by stylus-mode lifecycle",
            }))
        }
    }

    /// Ordinary GC16 SEND — allowed only when not armed.
    pub fn allow_ordinary_send_update(&self) -> bool {
        self.can_send(UpdateClass::Ordinary)
    }

    pub fn require_ordinary_send_update(&self) -> PlatformResult<()> {
        self.require_send(UpdateClass::Ordinary)
    }

    /// Stock wet DU SEND — allowed only when armed.
    pub fn allow_wetink_du_send_update(&self) -> bool {
        self.can_send(UpdateClass::WetInkDu)
    }

    pub fn require_wetink_du_send_update(&self) -> PlatformResult<()> {
        self.require_send(UpdateClass::WetInkDu)
    }
}

/// Simple checksum for snapshot logging (not cryptographic).
pub fn rect_checksum(pixels: &[u8]) -> u32 {
    let mut sum = 0u32;
    for (i, &b) in pixels.iter().enumerate() {
        sum = sum.wrapping_add((b as u32).wrapping_mul((i as u32).wrapping_add(1)));
    }
    sum
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_stock_modes_only() {
        assert!(validate_e1_stylus_mode(0).is_ok());
        assert!(validate_e1_stylus_mode(1).is_ok());
        assert!(matches!(
            validate_e1_stylus_mode(2),
            Err(PlatformError::Unsupported(_))
        ));
        assert!(matches!(
            validate_e1_stylus_mode(3),
            Err(PlatformError::Unsupported(_))
        ));
        assert!(matches!(
            validate_e1_stylus_mode(99),
            Err(PlatformError::Unsupported(_))
        ));
    }

    #[test]
    fn lifecycle_gates_update_classes() {
        let mut lc = StylusModeLifecycle::new();
        assert!(lc.can_send(UpdateClass::Ordinary));
        assert!(!lc.can_send(UpdateClass::WetInkDu));
        assert!(lc.require_send(UpdateClass::Ordinary).is_ok());
        assert!(lc.require_send(UpdateClass::WetInkDu).is_err());

        lc.mark_armed().unwrap();
        assert!(!lc.can_send(UpdateClass::Ordinary));
        assert!(lc.can_send(UpdateClass::WetInkDu));
        assert!(lc.require_ordinary_send_update().is_err());
        assert!(lc.require_wetink_du_send_update().is_ok());

        lc.mark_disarmed().unwrap();
        assert!(lc.can_send(UpdateClass::Ordinary));
        assert!(!lc.can_send(UpdateClass::WetInkDu));
        assert!(lc.require_ordinary_send_update().is_ok());
        assert!(lc.require_wetink_du_send_update().is_err());
    }

    #[test]
    fn cannot_double_arm_or_disarm_from_ordinary() {
        let mut lc = StylusModeLifecycle::new();
        assert!(lc.mark_disarmed().is_err());
        lc.mark_armed().unwrap();
        assert!(lc.mark_armed().is_err());
    }

    #[test]
    fn checksum_stable() {
        let a = [0u8; 64];
        let b = [0xffu8; 64];
        assert_ne!(rect_checksum(&a), rect_checksum(&b));
        assert_eq!(rect_checksum(&a), rect_checksum(&a));
    }
}
