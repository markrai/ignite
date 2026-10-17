//! ABI-correct representation of the Linux `struct input_event`.
//!
//! The kernel defines it as:
//!
//! ```c
//! struct input_event {
//!     struct timeval time;   /* __kernel_long_t tv_sec; __kernel_long_t tv_usec; */
//!     __u16 type;
//!     __u16 code;
//!     __s32 value;
//! };
//! ```
//!
//! The width of `__kernel_long_t` is **the kernel word size of the running binary's ABI**, so
//! the struct is 16 bytes for a 32-bit userspace binary and 24 bytes for a 64-bit one.
//!
//! On Kindle Scribe **1st Gen** firmware **5.18.6** the userspace ABI is confirmed
//! **32-bit ARMv7 hard-float**, so an on-device `ink-probe` binary sees **16-byte**
//! `input_event` records. That fact is device-specific — it must not be generalized to every
//! Kindle — and this module still derives layout from [`KERNEL_LONG_BYTES`] so host tests can
//! exercise both the 16-byte and 24-byte layouts on any machine. Nothing here may assume the
//! `16/18/20` field offsets of the 64-bit layout when compiling for 32-bit.
//!
//! The timeval fields are **not** dropped: the kernel event timestamp is preserved in
//! [`KernelTimestamp`], together with the [`crate::ClockDomain`] it was captured in (see
//! [`crate::linux::LinuxEvdevStylus`], which tries `EVIOCSCLOCKID` to move it to
//! `CLOCK_MONOTONIC`).

use crate::time::TimestampUs;

/// `__kernel_long_t` for the compilation target.
///
/// Derived from the pointer width because that is what the kernel ABI keys off for the
/// architectures we target (32-bit ARM, AArch64, x86_64). Note this is the *kernel* word size:
/// it is deliberately not `libc::time_t`, which a modern 64-bit-time_t libc (musl 1.2+) widens
/// without the kernel's evdev records following suit for a 32-bit process.
#[cfg(target_pointer_width = "64")]
pub type KernelLong = i64;
/// `__kernel_long_t` for the compilation target (32-bit and 16-bit targets).
#[cfg(not(target_pointer_width = "64"))]
pub type KernelLong = i32;

/// Size in bytes of `__kernel_long_t` on the compilation target.
pub const KERNEL_LONG_BYTES: usize = core::mem::size_of::<KernelLong>();

/// Linux `struct input_event`, laid out for the compilation target.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InputEvent {
    pub tv_sec: KernelLong,
    pub tv_usec: KernelLong,
    pub ev_type: u16,
    pub code: u16,
    pub value: i32,
}

/// Size of one `struct input_event` record for the compilation target.
pub const INPUT_EVENT_SIZE: usize = core::mem::size_of::<InputEvent>();

// Compile-time guards: the layout must follow the kernel word size, not a hardcoded constant.
const _: () = assert!(KERNEL_LONG_BYTES == 4 || KERNEL_LONG_BYTES == 8);
const _: () = assert!(INPUT_EVENT_SIZE == 2 * KERNEL_LONG_BYTES + 8);
const _: () = assert!(core::mem::align_of::<InputEvent>() == KERNEL_LONG_BYTES);
// The type/code/value triple always begins right after the two kernel longs.
const _: () = assert!(TYPE_OFFSET == 2 * KERNEL_LONG_BYTES);

/// Byte offset of `type` within the record, for the compilation target.
pub const TYPE_OFFSET: usize = 2 * KERNEL_LONG_BYTES;

/// Width-agnostic decoded form of one evdev record.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RawInputEvent {
    /// Kernel event timestamp, in the input node's current clock domain.
    pub time: KernelTimestamp,
    pub ev_type: u16,
    pub code: u16,
    pub value: i32,
}

/// Kernel event timestamp as delivered in `struct timeval`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct KernelTimestamp {
    pub tv_sec: i64,
    pub tv_usec: i64,
}

impl KernelTimestamp {
    /// Microseconds since the epoch of whichever clock the input node is configured for.
    ///
    /// Saturates rather than wrapping; a negative value (impossible for both `CLOCK_MONOTONIC`
    /// and `CLOCK_REALTIME` in practice) yields `0`.
    pub fn to_micros(self) -> TimestampUs {
        let us = self
            .tv_sec
            .saturating_mul(1_000_000)
            .saturating_add(self.tv_usec);
        if us < 0 {
            0
        } else {
            us as TimestampUs
        }
    }
}

/// Number of whole records in `len` bytes for a given kernel word width.
pub const fn record_size(long_bytes: usize) -> usize {
    2 * long_bytes + 8
}

/// Decode one record from `buf`, treating `__kernel_long_t` as `long_bytes` wide.
///
/// All offsets are computed from `long_bytes`; nothing is hardcoded to the 64-bit layout.
/// Returns `None` if the buffer is short or `long_bytes` is not 4 or 8.
pub fn decode_with_word_size(buf: &[u8], long_bytes: usize) -> Option<RawInputEvent> {
    if long_bytes != 4 && long_bytes != 8 {
        return None;
    }
    let size = record_size(long_bytes);
    if buf.len() < size {
        return None;
    }
    let tv_sec = read_signed_native(&buf[0..long_bytes]);
    let tv_usec = read_signed_native(&buf[long_bytes..2 * long_bytes]);
    let t = 2 * long_bytes;
    let ev_type = u16::from_ne_bytes([buf[t], buf[t + 1]]);
    let code = u16::from_ne_bytes([buf[t + 2], buf[t + 3]]);
    let value = i32::from_ne_bytes([buf[t + 4], buf[t + 5], buf[t + 6], buf[t + 7]]);
    Some(RawInputEvent {
        time: KernelTimestamp { tv_sec, tv_usec },
        ev_type,
        code,
        value,
    })
}

/// Decode one record laid out for the compilation target.
pub fn decode_native(buf: &[u8]) -> Option<RawInputEvent> {
    decode_with_word_size(buf, KERNEL_LONG_BYTES)
}

/// Encode a record for the given kernel word width (test/fixture helper).
pub fn encode_with_word_size(ev: RawInputEvent, long_bytes: usize) -> Option<Vec<u8>> {
    if long_bytes != 4 && long_bytes != 8 {
        return None;
    }
    let mut out = Vec::with_capacity(record_size(long_bytes));
    write_signed_native(&mut out, ev.time.tv_sec, long_bytes);
    write_signed_native(&mut out, ev.time.tv_usec, long_bytes);
    out.extend_from_slice(&ev.ev_type.to_ne_bytes());
    out.extend_from_slice(&ev.code.to_ne_bytes());
    out.extend_from_slice(&ev.value.to_ne_bytes());
    Some(out)
}

/// Read a `__kernel_long_t` in the target's native byte order.
fn read_signed_native(bytes: &[u8]) -> i64 {
    match bytes.len() {
        4 => i32::from_ne_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as i64,
        8 => i64::from_ne_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ]),
        _ => 0,
    }
}

fn write_signed_native(out: &mut Vec<u8>, value: i64, long_bytes: usize) {
    if long_bytes == 4 {
        out.extend_from_slice(&(value as i32).to_ne_bytes());
    } else {
        out.extend_from_slice(&value.to_ne_bytes());
    }
}

/// Linux `EV_*` event types we act on.
pub mod ev {
    pub const SYN: u16 = 0x00;
    pub const KEY: u16 = 0x01;
    pub const ABS: u16 = 0x03;
    pub const MSC: u16 = 0x04;
}

/// Linux `SYN_*` codes.
pub mod syn {
    pub const REPORT: u16 = 0;
    pub const DROPPED: u16 = 3;
}

/// Linux `ABS_*` codes relevant to a pen digitizer.
pub mod abs {
    pub const X: u16 = 0x00;
    pub const Y: u16 = 0x01;
    pub const PRESSURE: u16 = 0x18;
    pub const DISTANCE: u16 = 0x19;
    pub const TILT_X: u16 = 0x1a;
    pub const TILT_Y: u16 = 0x1b;
    /// Multitouch protocol B (`pt_mt` touchscreen on Scribe FW 5.18.6).
    pub const MT_SLOT: u16 = 0x2f;
    pub const MT_TOUCH_MAJOR: u16 = 0x30;
    pub const MT_POSITION_X: u16 = 0x35;
    pub const MT_POSITION_Y: u16 = 0x36;
    pub const MT_TOOL_TYPE: u16 = 0x37;
    pub const MT_TRACKING_ID: u16 = 0x39;
    pub const MT_PRESSURE: u16 = 0x3a;
}

/// `ABS_MT_TOOL_TYPE` values (`MT_TOOL_*`).
pub mod mt_tool {
    pub const FINGER: i32 = 0;
    /// Observed on FW 5.18.6 when a large contact is reclassified just before lift.
    pub const PALM: i32 = 2;
}

/// Linux `BTN_*` codes relevant to a pen digitizer (`input-event-codes.h`).
pub mod btn {
    /// Tool in proximity: a pen.
    pub const TOOL_PEN: u16 = 0x140;
    /// Tool in proximity: the eraser end.
    pub const TOOL_RUBBER: u16 = 0x141;
    /// Nib in contact with the surface. This, and only this, means "drawing".
    pub const TOUCH: u16 = 0x14a;
    /// First barrel / side button.
    pub const STYLUS: u16 = 0x14b;
    /// Second barrel / side button.
    pub const STYLUS2: u16 = 0x14c;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> RawInputEvent {
        RawInputEvent {
            time: KernelTimestamp {
                tv_sec: 1_700_000_123,
                tv_usec: 456_789,
            },
            ev_type: ev::ABS,
            code: abs::X,
            value: 12_345,
        }
    }

    #[test]
    fn native_layout_matches_kernel_word_size() {
        assert_eq!(KERNEL_LONG_BYTES, core::mem::size_of::<KernelLong>());
        assert_eq!(INPUT_EVENT_SIZE, record_size(KERNEL_LONG_BYTES));
        assert_eq!(TYPE_OFFSET, 2 * KERNEL_LONG_BYTES);
        // Never the hardcoded 64-bit assumption unless the target really is 64-bit.
        if KERNEL_LONG_BYTES == 4 {
            assert_eq!(INPUT_EVENT_SIZE, 16);
            assert_eq!(TYPE_OFFSET, 8);
        } else {
            assert_eq!(INPUT_EVENT_SIZE, 24);
            assert_eq!(TYPE_OFFSET, 16);
        }
    }

    #[test]
    fn repr_c_struct_field_offsets_match_the_decoder() {
        // The offsets the decoder uses must equal the real offsets of the repr(C) struct.
        let base = InputEvent::default();
        let base_ptr = &base as *const InputEvent as usize;
        let type_off = (&base.ev_type as *const u16 as usize) - base_ptr;
        let code_off = (&base.code as *const u16 as usize) - base_ptr;
        let value_off = (&base.value as *const i32 as usize) - base_ptr;
        assert_eq!(type_off, TYPE_OFFSET);
        assert_eq!(code_off, TYPE_OFFSET + 2);
        assert_eq!(value_off, TYPE_OFFSET + 4);
    }

    #[test]
    fn decodes_32_bit_abi_record() {
        let bytes = encode_with_word_size(sample(), 4).unwrap();
        assert_eq!(bytes.len(), 16);
        assert_eq!(decode_with_word_size(&bytes, 4).unwrap(), sample());
    }

    #[test]
    fn decodes_64_bit_abi_record() {
        let bytes = encode_with_word_size(sample(), 8).unwrap();
        assert_eq!(bytes.len(), 24);
        assert_eq!(decode_with_word_size(&bytes, 8).unwrap(), sample());
    }

    #[test]
    fn wrong_word_size_does_not_silently_produce_a_plausible_event() {
        // A 32-bit record read as 64-bit must not decode at all (it is too short).
        let bytes32 = encode_with_word_size(sample(), 4).unwrap();
        assert!(decode_with_word_size(&bytes32, 8).is_none());
        // A 64-bit record read as 32-bit decodes, but to obviously different field values,
        // which is why the reader must use the target-correct size rather than guessing.
        let bytes64 = encode_with_word_size(sample(), 8).unwrap();
        let misread = decode_with_word_size(&bytes64, 4).unwrap();
        assert_ne!(misread.ev_type, sample().ev_type);
    }

    #[test]
    fn rejects_unsupported_word_sizes_and_short_buffers() {
        assert!(decode_with_word_size(&[0u8; 64], 2).is_none());
        assert!(encode_with_word_size(sample(), 16).is_none());
        assert!(decode_native(&[0u8; 3]).is_none());
    }

    #[test]
    fn native_decode_round_trips() {
        let bytes = encode_with_word_size(sample(), KERNEL_LONG_BYTES).unwrap();
        assert_eq!(bytes.len(), INPUT_EVENT_SIZE);
        assert_eq!(decode_native(&bytes).unwrap(), sample());
    }

    #[test]
    fn kernel_timestamp_to_micros() {
        assert_eq!(
            KernelTimestamp {
                tv_sec: 2,
                tv_usec: 500_000
            }
            .to_micros(),
            2_500_000
        );
        assert_eq!(
            KernelTimestamp {
                tv_sec: -1,
                tv_usec: 0
            }
            .to_micros(),
            0
        );
        assert_eq!(
            KernelTimestamp {
                tv_sec: i64::MAX,
                tv_usec: i64::MAX
            }
            .to_micros(),
            i64::MAX as u64
        );
    }

    #[test]
    fn event_code_constants_match_input_event_codes_h() {
        assert_eq!(btn::TOOL_PEN, 0x140);
        assert_eq!(btn::TOOL_RUBBER, 0x141);
        assert_eq!(btn::TOUCH, 0x14a);
        assert_eq!(btn::STYLUS, 0x14b);
        assert_eq!(btn::STYLUS2, 0x14c);
        assert_eq!(abs::PRESSURE, 0x18);
        assert_eq!(ev::KEY, 1);
        assert_eq!(ev::ABS, 3);
    }
}
