//! Thin owned-file-descriptor helpers and the `unsafe` evdev ioctl calls.

use std::ffi::CString;
use std::io;

use crate::error::{PlatformError, PlatformResult};
use crate::evdev_ioctl::{
    bitmap_bit, bitmap_len_bytes, eviocgabs, eviocgbit, eviocgkey, eviocgname, eviocgrab,
    eviocsclockid, InputAbsinfo, KEY_MAX,
};

/// Owned file descriptor: closed exactly once, on drop.
#[derive(Debug)]
pub struct OwnedFd(i32);

impl OwnedFd {
    /// Open `path` read-only. `nonblocking` adds `O_NONBLOCK`.
    pub fn open_read(path: &str, nonblocking: bool) -> PlatformResult<Self> {
        Self::open_flags(path, libc::O_RDONLY, nonblocking)
    }

    /// Open `path` read/write (framebuffer mmap + refresh).
    pub fn open_read_write(path: &str, nonblocking: bool) -> PlatformResult<Self> {
        Self::open_flags(path, libc::O_RDWR, nonblocking)
    }

    fn open_flags(path: &str, access: i32, nonblocking: bool) -> PlatformResult<Self> {
        let c = CString::new(path).map_err(|e| PlatformError::Io(e.to_string()))?;
        let mut flags = access | libc::O_CLOEXEC;
        if nonblocking {
            flags |= libc::O_NONBLOCK;
        }
        let fd = unsafe { libc::open(c.as_ptr(), flags) };
        if fd < 0 {
            return Err(PlatformError::Io(format!(
                "open {path}: {}",
                io::Error::last_os_error()
            )));
        }
        Ok(Self(fd))
    }

    pub fn raw(&self) -> i32 {
        self.0
    }

    /// `read(2)` into `buf`. Returns `Ok(None)` for `EAGAIN`/`EWOULDBLOCK`, `Ok(Some(0))` for EOF.
    pub fn read(&self, buf: &mut [u8]) -> PlatformResult<Option<usize>> {
        loop {
            let n = unsafe { libc::read(self.0, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
            if n >= 0 {
                return Ok(Some(n as usize));
            }
            let err = io::Error::last_os_error();
            match err.kind() {
                io::ErrorKind::WouldBlock => return Ok(None),
                io::ErrorKind::Interrupted => continue,
                _ => return Err(PlatformError::Io(format!("read: {err}"))),
            }
        }
    }
}

impl Drop for OwnedFd {
    fn drop(&mut self) {
        unsafe {
            libc::close(self.0);
        }
    }
}

fn ioctl_err(what: &str) -> PlatformError {
    PlatformError::Io(format!("{what}: {}", io::Error::last_os_error()))
}

/// `EVIOCGNAME`: the device's own name string.
pub fn device_name(fd: &OwnedFd) -> PlatformResult<String> {
    let mut buf = vec![0u8; 256];
    let rc = unsafe {
        libc::ioctl(
            fd.raw(),
            eviocgname(buf.len() as u32) as _,
            buf.as_mut_ptr() as *mut libc::c_void,
        )
    };
    if rc < 0 {
        return Err(ioctl_err("EVIOCGNAME"));
    }
    let len = (rc as usize).min(buf.len());
    let s = String::from_utf8_lossy(&buf[..len]);
    Ok(s.trim_end_matches('\0').trim().to_string())
}

/// `EVIOCGBIT(ev_type, ...)`: the capability bitmap for one event type, as bytes.
pub fn capability_bitmap(fd: &OwnedFd, ev_type: u16, len_bytes: usize) -> PlatformResult<Vec<u8>> {
    let mut buf = vec![0u8; len_bytes];
    let rc = unsafe {
        libc::ioctl(
            fd.raw(),
            eviocgbit(ev_type as u32, len_bytes as u32) as _,
            buf.as_mut_ptr() as *mut libc::c_void,
        )
    };
    if rc < 0 {
        return Err(ioctl_err("EVIOCGBIT"));
    }
    buf.truncate((rc as usize).min(len_bytes));
    Ok(buf)
}

/// `EVIOCGABS(axis)`: the kernel-reported range of one ABS axis.
pub fn abs_info(fd: &OwnedFd, axis: u16) -> PlatformResult<InputAbsinfo> {
    let mut info = InputAbsinfo::default();
    let rc = unsafe {
        libc::ioctl(
            fd.raw(),
            eviocgabs(axis as u32) as _,
            &mut info as *mut InputAbsinfo as *mut libc::c_void,
        )
    };
    if rc < 0 {
        return Err(ioctl_err("EVIOCGABS"));
    }
    Ok(info)
}

/// `EVIOCSCLOCKID(CLOCK_MONOTONIC)`: move this node's event timestamps onto `CLOCK_MONOTONIC`.
///
/// Succeeding is what makes kernel event timestamps comparable with
/// [`crate::linux::LinuxMonotonicClock`]. Failing is not fatal; it only means dispatch latency
/// cannot be measured.
pub fn set_clock_monotonic(fd: &OwnedFd) -> PlatformResult<()> {
    let clk: i32 = libc::CLOCK_MONOTONIC;
    let rc = unsafe {
        libc::ioctl(
            fd.raw(),
            eviocsclockid() as _,
            &clk as *const i32 as *const libc::c_void,
        )
    };
    if rc < 0 {
        return Err(ioctl_err("EVIOCSCLOCKID"));
    }
    Ok(())
}

/// `EVIOCGRAB`: exclusive grab (`true`) or release (`false`).
///
/// The kernel treats the **third ioctl argument itself** as a boolean (`if (p)`): nonzero →
/// grab, zero → ungrab. Passing `&local_int` is wrong because `&0` is still non-null.
pub fn set_evdev_grab(fd: &OwnedFd, grab: bool) -> PlatformResult<()> {
    let arg = crate::evdev_ioctl::evdev_grab_ioctl_arg(grab);
    let rc = unsafe { libc::ioctl(fd.raw(), eviocgrab() as _, arg as *mut libc::c_void) };
    if rc < 0 {
        return Err(ioctl_err("EVIOCGRAB"));
    }
    Ok(())
}

/// `EVIOCGKEY`: current key/button state as a byte bitmap covering `0..=KEY_MAX`.
pub fn key_state_bitmap(fd: &OwnedFd) -> PlatformResult<Vec<u8>> {
    let len = bitmap_len_bytes(KEY_MAX);
    let mut buf = vec![0u8; len];
    let rc = unsafe {
        libc::ioctl(
            fd.raw(),
            eviocgkey(len as u32) as _,
            buf.as_mut_ptr() as *mut libc::c_void,
        )
    };
    if rc < 0 {
        return Err(ioctl_err("EVIOCGKEY"));
    }
    Ok(buf)
}

/// Build a [`crate::DeviceSnapshot`] from standard evdev ioctls (not from the event stream).
pub fn query_device_snapshot(
    fd: &OwnedFd,
    axes: &crate::axes::StylusAxes,
) -> PlatformResult<crate::DeviceSnapshot> {
    use crate::evdev_abi::{abs, btn};
    use crate::input::BarrelButtons;
    use crate::DeviceSnapshot;

    let keys = key_state_bitmap(fd)?;
    let tool_pen = bitmap_bit(&keys, btn::TOOL_PEN as usize);
    let tool_rubber = bitmap_bit(&keys, btn::TOOL_RUBBER as usize);
    let contact = bitmap_bit(&keys, btn::TOUCH as usize);
    let buttons = BarrelButtons {
        primary: bitmap_bit(&keys, btn::STYLUS as usize),
        secondary: bitmap_bit(&keys, btn::STYLUS2 as usize),
    };

    let x = abs_info(fd, abs::X).ok().map(|i| i.value);
    let y = abs_info(fd, abs::Y).ok().map(|i| i.value);
    let pressure = if axes.pressure.is_some() {
        abs_info(fd, abs::PRESSURE).ok().map(|i| i.value)
    } else {
        None
    };

    Ok(DeviceSnapshot {
        tool_pen,
        tool_rubber,
        contact,
        buttons,
        x,
        y,
        pressure,
    })
}
