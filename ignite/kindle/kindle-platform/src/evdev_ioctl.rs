//! evdev ioctl numbers, `input_absinfo` layout, and bitmap addressing.
//!
//! Pure ABI arithmetic, deliberately not gated on `cfg(unix)` so that the numbers and the
//! bitmap addressing are covered by host tests on any development machine. The `unsafe` calls
//! that use them live in [`crate::linux`].
//!
//! Only the asm-generic `_IOC` encoding is used, which arm, aarch64 and x86 all share. No
//! Kindle-specific ioctl is invented here.

const IOC_NRBITS: u32 = 8;
const IOC_TYPEBITS: u32 = 8;
const IOC_SIZEBITS: u32 = 14;

const IOC_NRSHIFT: u32 = 0;
const IOC_TYPESHIFT: u32 = IOC_NRSHIFT + IOC_NRBITS;
const IOC_SIZESHIFT: u32 = IOC_TYPESHIFT + IOC_TYPEBITS;
const IOC_DIRSHIFT: u32 = IOC_SIZESHIFT + IOC_SIZEBITS;

const IOC_WRITE: u32 = 1;
const IOC_READ: u32 = 2;

const EVDEV_IOC_MAGIC: u32 = b'E' as u32;

const fn ioc(dir: u32, ty: u32, nr: u32, size: u32) -> u32 {
    (dir << IOC_DIRSHIFT) | (ty << IOC_TYPESHIFT) | (nr << IOC_NRSHIFT) | (size << IOC_SIZESHIFT)
}

/// `EVIOCGKEY(len)` — current key/button state bitmap.
pub const fn eviocgkey(len: u32) -> u32 {
    ioc(IOC_READ, EVDEV_IOC_MAGIC, 0x18, len)
}

/// `EVIOCGNAME(len)`
pub const fn eviocgname(len: u32) -> u32 {
    ioc(IOC_READ, EVDEV_IOC_MAGIC, 0x06, len)
}

/// `EVIOCGBIT(ev, len)`
pub const fn eviocgbit(ev: u32, len: u32) -> u32 {
    ioc(IOC_READ, EVDEV_IOC_MAGIC, 0x20 + ev, len)
}

/// `EVIOCGABS(abs)`
pub const fn eviocgabs(abs: u32) -> u32 {
    ioc(IOC_READ, EVDEV_IOC_MAGIC, 0x40 + abs, ABSINFO_SIZE as u32)
}

/// `EVIOCSCLOCKID`
pub const fn eviocsclockid() -> u32 {
    ioc(IOC_WRITE, EVDEV_IOC_MAGIC, 0xa0, 4)
}

/// `EVIOCGRAB` — exclusive grab / release.
///
/// UAPI declares `_IOW('E', 0x90, int)`, but the kernel treats the **ioctl third argument
/// itself** as a boolean pointer value: non-null → grab, null → ungrab. Do **not** pass
/// `&int`; pass `1`/`0` cast to a pointer-sized value (see [`evdev_grab_ioctl_arg`]).
pub const fn eviocgrab() -> u32 {
    ioc(IOC_WRITE, EVDEV_IOC_MAGIC, 0x90, 4)
}

/// Third argument for `ioctl(fd, EVIOCGRAB, …)`.
///
/// Returns a pointer-sized integer that is **nonzero** for grab and **zero** for release.
/// This is the value passed as the variadic ioctl argument — never the address of a local.
pub const fn evdev_grab_ioctl_arg(grab: bool) -> usize {
    if grab {
        1
    } else {
        0
    }
}

/// `struct input_absinfo`: six `__s32` fields, 24 bytes on every ABI we target.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InputAbsinfo {
    pub value: i32,
    pub minimum: i32,
    pub maximum: i32,
    pub fuzz: i32,
    pub flat: i32,
    pub resolution: i32,
}

pub const ABSINFO_SIZE: usize = core::mem::size_of::<InputAbsinfo>();
const _: () = assert!(ABSINFO_SIZE == 24);
const _: () = assert!(core::mem::align_of::<InputAbsinfo>() == 4);

/// Largest `KEY_*` / `BTN_*` code, from `input-event-codes.h`.
pub const KEY_MAX: usize = 0x2ff;
/// Largest `ABS_*` code.
pub const ABS_MAX: usize = 0x3f;

/// Buffer length in bytes needed to hold a bitmap covering codes `0..=max`.
pub const fn bitmap_len_bytes(max: usize) -> usize {
    max / 8 + 1
}

/// Read bit `bit` from an `EVIOCGBIT` byte buffer.
///
/// `EVIOCGBIT` copies the kernel bitmap out as bytes with compat handling already applied, so
/// byte `bit / 8` / bit `bit % 8` is correct regardless of the kernel's `unsigned long` width.
/// This is the mechanism to prefer over parsing sysfs hex words, which are `%lx`-formatted
/// kernel longs of an ambiguous width.
pub fn bitmap_bit(buf: &[u8], bit: usize) -> bool {
    let byte = bit / 8;
    byte < buf.len() && (buf[byte] & (1 << (bit % 8))) != 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ioctl_numbers_match_input_h() {
        // Cross-checked against linux/input.h with the asm-generic _IOC encoding.
        assert_eq!(eviocgname(256), 0x8100_4506);
        assert_eq!(eviocgkey(96), 0x8060_4518);
        assert_eq!(eviocgabs(0x18), 0x8018_4558);
        assert_eq!(eviocsclockid(), 0x4004_45a0);
        assert_eq!(eviocgrab(), 0x4004_4590);
        // EVIOCGBIT(EV_ABS = 3, len) -> _IOR('E', 0x20 + 3, len)
        assert_eq!(eviocgbit(3, 8), 0x8008_4523);
        // EVIOCGBIT(EV_KEY = 1, 96)
        assert_eq!(eviocgbit(1, 96), 0x8060_4521);
    }

    #[test]
    fn evdev_grab_ioctl_arg_is_value_not_address() {
        // Kernel checks `if (p)` on the third ioctl argument itself. A future regression that
        // passes `&0` would still be non-null and would grab on "release".
        assert_ne!(evdev_grab_ioctl_arg(true), 0);
        assert_eq!(evdev_grab_ioctl_arg(false), 0);
        // Release must be the null-pointer bit pattern (literal zero), not the address of a
        // stack integer.
        assert_eq!(
            evdev_grab_ioctl_arg(false) as *mut core::ffi::c_void as usize,
            0
        );
    }

    #[test]
    fn absinfo_is_24_bytes_on_every_abi() {
        assert_eq!(ABSINFO_SIZE, 24);
        assert_eq!(core::mem::align_of::<InputAbsinfo>(), 4);
    }

    #[test]
    fn bitmap_length_covers_the_highest_code() {
        assert_eq!(bitmap_len_bytes(KEY_MAX), 96);
        assert_eq!(bitmap_len_bytes(ABS_MAX), 8);
        assert!(bitmap_len_bytes(KEY_MAX) * 8 > KEY_MAX);
    }

    #[test]
    fn bitmap_bit_is_byte_addressed() {
        let mut buf = vec![0u8; bitmap_len_bytes(KEY_MAX)];
        // BTN_TOOL_PEN = 0x140 -> byte 0x28, bit 0.
        buf[0x140 / 8] |= 1 << (0x140 % 8);
        assert!(bitmap_bit(&buf, 0x140));
        assert!(!bitmap_bit(&buf, 0x141));
        // BTN_TOUCH = 0x14a -> byte 0x29, bit 2.
        buf[0x14a / 8] |= 1 << (0x14a % 8);
        assert!(bitmap_bit(&buf, 0x14a));
        assert!(!bitmap_bit(&buf, 0x14b));
    }

    #[test]
    fn bitmap_bit_past_the_buffer_is_absent_not_a_panic() {
        let buf = vec![0xffu8; 4];
        assert!(bitmap_bit(&buf, 31));
        assert!(!bitmap_bit(&buf, 32));
        assert!(!bitmap_bit(&buf, 100_000));
    }
}
