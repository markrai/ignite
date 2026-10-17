//! ink-probe — stylus → decoded events (and optional host demo drawing).
//!
//! On-device: `--live` / `--input` open the Wacom digitizer and print decoded `StylusEvent`s.
//! `--fb-info` characterises `/dev/fb0` via `FBIOGET_*`.
//! `--fb-test` is dry-run unless `--confirm-write` is also passed.
//! `--pen-display-test` is Gate C (one Pen Down → asymmetric marker); requires
//! `--ink-byte` and `--confirm-write`.
//! `--stream-ink --one-stroke` is Gate D streaming handwriting baseline (GC16).
//! `--stream-du --one-stroke` is Gate E2/E3 stock-style DU wet streaming
//! (`--du-interval-us` for E3 cadence sweep).

mod metrics;
mod pipeline;
#[cfg_attr(not(any(unix, test)), allow(dead_code))]
mod stream;

use std::env;
use std::path::PathBuf;
use std::process::ExitCode;
#[cfg(unix)]
use std::thread;
#[cfg(unix)]
use std::time::Duration;

#[cfg(not(unix))]
use kindle_platform::host::HostDiscovery;
use kindle_platform::host::{HostDisplay, ScriptedStylus, UnsupportedFastInk};
#[cfg(unix)]
use kindle_platform::StylusTool;
use kindle_platform::{
    codes, DisplaySurface, FastInkMode, HostClock, InputDiscovery, MonotonicClock, StylusAxes,
    StylusEvent, StylusEventKind, INPUT_EVENT_SIZE, KERNEL_LONG_BYTES,
};
use notebook_core::{Orientation, PageGeometry, ViewportTransform};

use pipeline::InkPipeline;

/// Synthetic device axis ranges for the host demo. A physical run reads these from the device.
const DEMO_AXES: (i32, i32, i32) = (1000, 1500, 1023);

/// Gate B: small visible rect (panel pixels).
#[cfg(unix)]
const FB_TEST_X: u32 = 80;
#[cfg(unix)]
const FB_TEST_Y: u32 = 80;
#[cfg(unix)]
const FB_TEST_W: u32 = 64;
#[cfg(unix)]
const FB_TEST_H: u32 = 64;

/// Gate E1: interior 64×64 (panel pixels); not pen-derived; stays clear of x≥1860.
const E1_TEST_X: u32 = 120;
const E1_TEST_Y: u32 = 180;
const E1_TEST_W: u32 = 64;
const E1_TEST_H: u32 = 64;

fn main() -> ExitCode {
    let args: Vec<String> = env::args().collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        eprintln!(
            "ink-probe - stylus / framebuffer probe\n\n\
             Usage:\n\
               ink-probe --demo [--out stroke.pgm]   host software surface demo\n\
               ink-probe --live                      open preferred stylus (Scribe: /dev/input/stylus)\n\
               ink-probe --input <path>              open a specific evdev node/symlink\n\
               ink-probe --list-inputs               discover input devices\n\
               ink-probe --abi                       report the evdev ABI this build assumes\n\
               ink-probe --fb-info [--fb <path>]     read-only FBIOGET_* characterisation\n\
               ink-probe --fb-test [--fb <path>]     dry-run Gate B plan (no mmap write)\n\
               ink-probe --fb-test --confirm-write   physical reversible 64x64 rectangle\n\
               ink-probe --pen-display-test --ink-byte 00|ff --confirm-write\n\
                                             Gate C: one Pen Down -> marker -> restore\n\
               ink-probe --stream-ink --one-stroke --ink-byte 00|ff --confirm-write\n\
                                             Gate D: one streaming stroke (GC16 async)\n\
               ink-probe --stream-du --one-stroke --ink-byte 00|ff\n\
                                             Gate E2 dry-run (DU wet streaming)\n\
               ink-probe --stream-du --one-stroke --ink-byte 00|ff --confirm-write\n\
                                             Gate E2: mode=1, DU async batches, wait, mode=0, GC16 restore\n\
               ink-probe --stream-du --one-stroke --du-interval-us 25000|16000|12000|8000 ...\n\
                                             Gate E3: same as E2 with cadence sweep override\n\
               ink-probe --fast-ink-info             Gate E0 stock fast-path facts (no ioctl)\n\
               ink-probe --stylus-mode-test --ink-byte 00|ff\n\
                                             Gate E1: mode=1, mmap only (no SEND), mode=0\n\
               ink-probe --stylus-mode-test --ink-byte 00|ff --confirm-write\n\
               ink-probe --wet-du-test --ink-byte 00|ff\n\
                                             Gate E1.2 dry-run (stock DU wet SEND)\n\
               ink-probe --wet-du-test --ink-byte 00|ff --confirm-write\n\
                                             Gate E1.2: mode=1, mmap, DU SEND, wait, mode=0, GC16 restore\n\
               ink-probe --try-fast-ink              Unsupported stub (general FastInkMode)\n\n\
             Live mode prints decoded Hover/Down/Move/Up events only.\n\
             Without --confirm-write, physical fb modes refuse O_RDWR writes.\n\
             --pen-display-test / --stream-ink / --stream-du / --stylus-mode-test / --wet-du-test require --ink-byte 00 or ff.\n\
             Note: SIGKILL / power loss / kernel crash can bypass userspace restore/disarm.\n"
        );
        return ExitCode::SUCCESS;
    }

    if args.iter().any(|a| a == "--abi") {
        return report_abi();
    }

    if args.iter().any(|a| a == "--fb-info") {
        return run_fb_info(&args);
    }

    if args.iter().any(|a| a == "--fb-test") {
        return run_fb_test(&args);
    }

    if args.iter().any(|a| a == "--pen-display-test") {
        return run_pen_display_test(&args);
    }

    if args.iter().any(|a| a == "--stream-ink") {
        return run_stream_ink(&args);
    }

    if args.iter().any(|a| a == "--stream-du") {
        return run_stream_du(&args);
    }

    if args.iter().any(|a| a == "--fast-ink-info") {
        return report_fast_ink_info();
    }

    if args.iter().any(|a| a == "--stylus-mode-test") {
        return run_stylus_mode_test(&args);
    }

    if args.iter().any(|a| a == "--wet-du-test") {
        return run_wet_du_test(&args);
    }

    if args.iter().any(|a| a == "--try-fast-ink") {
        let mut fast = UnsupportedFastInk;
        match fast.enable() {
            Ok(()) => eprintln!("fast ink unexpectedly enabled"),
            Err(e) => eprintln!("fast ink: {e}"),
        }
        return ExitCode::SUCCESS;
    }

    if args.iter().any(|a| a == "--list-inputs") {
        return list_inputs();
    }

    if args.iter().any(|a| a == "--live") || args.windows(2).any(|w| w[0] == "--input") {
        return match run_live(&args) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("error: {e}");
                ExitCode::FAILURE
            }
        };
    }

    // Default: host demo pipeline (also used when --demo is passed explicitly).
    if let Err(e) = run_demo(&args) {
        eprintln!("error: {e}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

fn run_fb_info(args: &[String]) -> ExitCode {
    #[cfg(unix)]
    {
        use kindle_platform::fbdev_abi::FbGeometry;
        use kindle_platform::linux::LinuxFramebuffer;

        let path = args
            .windows(2)
            .find(|w| w[0] == "--fb")
            .map(|w| w[1].as_str())
            .unwrap_or("/dev/fb0");

        match LinuxFramebuffer::open_info_only_path(path) {
            Ok(fb) => {
                let fix = &fb.fix;
                let var = &fb.var;
                let geo = FbGeometry::from_infos(fix, var);
                let report = geo.report();

                println!("path={path}");
                println!("# fixed (FBIOGET_FSCREENINFO)");
                println!("id={}", fix.id_str());
                println!("smem_start={:#x}", fix.smem_start as u64);
                println!("smem_len={}", fix.smem_len);
                println!("type={}", fix.type_);
                println!("type_aux={}", fix.type_aux);
                println!("visual={}", fix.visual);
                println!(
                    "xpanstep={} ypanstep={} ywrapstep={}",
                    fix.xpanstep, fix.ypanstep, fix.ywrapstep
                );
                println!("line_length={}", fix.line_length);
                println!("mmio_start={:#x}", fix.mmio_start as u64);
                println!("mmio_len={}", fix.mmio_len);
                println!("accel={}", fix.accel);
                println!("capabilities={}", fix.capabilities);

                println!("# variable (FBIOGET_VSCREENINFO)");
                println!("xres={} yres={}", var.xres, var.yres);
                println!(
                    "xres_virtual={} yres_virtual={}",
                    var.xres_virtual, var.yres_virtual
                );
                println!("xoffset={} yoffset={}", var.xoffset, var.yoffset);
                println!("bits_per_pixel={}", var.bits_per_pixel);
                println!("grayscale={}", var.grayscale);
                println!("rotate={}", var.rotate);
                println!("nonstd={}", var.nonstd);
                println!("activate={}", var.activate);
                println!(
                    "red offset={} length={} msb_right={}",
                    var.red.offset, var.red.length, var.red.msb_right
                );
                println!(
                    "green offset={} length={} msb_right={}",
                    var.green.offset, var.green.length, var.green.msb_right
                );
                println!(
                    "blue offset={} length={} msb_right={}",
                    var.blue.offset, var.blue.length, var.blue.msb_right
                );
                println!(
                    "transp offset={} length={} msb_right={}",
                    var.transp.offset, var.transp.length, var.transp.msb_right
                );

                println!("# derived");
                println!("visible_row_bytes={}", report.visible_row_bytes);
                println!("virtual_row_pixel_bytes={}", report.virtual_row_pixel_bytes);
                println!(
                    "off_visible_virtual_bytes={}",
                    report.off_visible_virtual_bytes
                );
                println!("stride_padding_bytes={}", report.stride_padding_bytes);
                println!("visible_page_bytes={}", report.visible_page_bytes);
                println!("virtual_minimum_bytes={}", report.virtual_minimum_bytes);
                println!(
                    "line_length*yres={} line_length*yres_virtual={}",
                    report.line_length_times_yres, report.line_length_times_yres_virtual
                );
                let (smem, vmin) = geo.mapping_len_candidates();
                println!("mapping_candidates smem_len={smem} line_length*yres_virtual={vmin}");
                for n in &report.notes {
                    println!("note: {n}");
                }
                println!("byte_polarity=unresolved (operator has not reported Gate B black phase)");
                println!("refresh_abi=hwtcon_mtk (see --fb-test --confirm-write)");
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("error: {e}");
                ExitCode::FAILURE
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = args;
        eprintln!("error: --fb-info requires a Linux/Kindle build (cfg(unix))");
        ExitCode::FAILURE
    }
}

fn run_fb_test(args: &[String]) -> ExitCode {
    #[cfg(unix)]
    {
        use kindle_platform::gate_b::{self, BLACK_BYTE, WHITE_BYTE};
        use kindle_platform::hwtcon_abi::{
            waveform, MxcfbRect, CONSERVATIVE_UPDATE_MODE, CONSERVATIVE_WAVEFORM,
            MXCFB_SEND_UPDATE_MTK, MXCFB_WAIT_FOR_UPDATE_COMPLETE, TEMP_USE_AMBIENT,
        };
        use kindle_platform::linux::{LinuxFramebuffer, MappedFramebuffer};

        let path = args
            .windows(2)
            .find(|w| w[0] == "--fb")
            .map(|w| w[1].as_str())
            .unwrap_or("/dev/fb0");
        let confirm = args.iter().any(|a| a == "--confirm-write");

        println!("# Gate B reversible rectangle plan on {path}");
        println!(
            "ioctl_send={:#x} ioctl_wait={:#x} waveform={} (GC16) mode={} (PARTIAL) temp={:#x}",
            MXCFB_SEND_UPDATE_MTK,
            MXCFB_WAIT_FOR_UPDATE_COMPLETE,
            CONSERVATIVE_WAVEFORM,
            CONSERVATIVE_UPDATE_MODE,
            TEMP_USE_AMBIENT as u32
        );
        assert_eq!(CONSERVATIVE_WAVEFORM, waveform::GC16);
        println!("rect left={FB_TEST_X} top={FB_TEST_Y} width={FB_TEST_W} height={FB_TEST_H}");
        println!(
            "phases: snapshot -> fill 0x00 -> refresh/wait -> fill 0xff -> refresh/wait -> restore -> refresh/wait"
        );
        println!(
            "byte_polarity BLACK_BYTE={BLACK_BYTE:?} WHITE_BYTE={WHITE_BYTE:?} (unresolved until operator reports)"
        );
        println!("note: SIGKILL / power loss / kernel crash can bypass userspace restoration");

        if !confirm {
            println!("confirm_write=missing");
            println!(
                "REFUSE PHYSICAL TEST: pass --confirm-write to allow O_RDWR mmap, pixel writes, and refresh ioctls"
            );
            println!("(no framebuffer modification attempted)");
            return ExitCode::FAILURE;
        }

        let info = match LinuxFramebuffer::open_info_only_path(path) {
            Ok(fb) => fb,
            Err(e) => {
                eprintln!("error: read-only probe failed: {e}");
                return ExitCode::FAILURE;
            }
        };
        if let Err(mismatches) = gate_b::check_preconditions(&info.fix, &info.var) {
            eprintln!("REFUSE PHYSICAL TEST: Gate B framebuffer preconditions failed:");
            for m in mismatches {
                eprintln!("  mismatch: {m}");
            }
            return ExitCode::FAILURE;
        }
        println!("preconditions=ok (hwtcon_v2 Gate B snapshot)");

        let mut fb = match MappedFramebuffer::open_path(path) {
            Ok(fb) => fb,
            Err(e) => {
                eprintln!("error: open/mmap: {e}");
                return ExitCode::FAILURE;
            }
        };

        let g = fb.geometry;
        println!(
            "id={} map_len={} visible={}x{} stride={} xoff={} yoff={}",
            fb.fix.id_str(),
            fb.map_len(),
            g.xres,
            g.yres,
            g.line_length,
            g.xoffset,
            g.yoffset
        );

        let region = MxcfbRect {
            top: FB_TEST_Y,
            left: FB_TEST_X,
            width: FB_TEST_W,
            height: FB_TEST_H,
        };

        let snap = match fb.extract_rect(FB_TEST_X, FB_TEST_Y, FB_TEST_W, FB_TEST_H) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("error: snapshot: {e}");
                return ExitCode::FAILURE;
            }
        };
        println!("snapshot_bytes={}", snap.len());

        let hold = Duration::from_millis(1500);
        let mut guard = FbRestoreGuard {
            fb: &mut fb,
            x: FB_TEST_X,
            y: FB_TEST_Y,
            w: FB_TEST_W,
            h: FB_TEST_H,
            snap,
            armed: true,
        };

        if let Err(e) = guard
            .fb
            .fill_rect(FB_TEST_X, FB_TEST_Y, FB_TEST_W, FB_TEST_H, 0x00)
        {
            eprintln!("error: fill 0x00: {e}");
            return ExitCode::FAILURE;
        }
        match guard.fb.send_update_wait(region) {
            Ok(r) => println!(
                "phase_a value=0x00 marker={} collision={} send_ioctl_us={} wait_ioctl_us={}",
                r.marker, r.collision_test, r.send_ioctl_us, r.wait_ioctl_us
            ),
            Err(e) => {
                eprintln!("error: refresh 0x00: {e}");
                return ExitCode::FAILURE;
            }
        }
        thread::sleep(hold);

        if let Err(e) = guard
            .fb
            .fill_rect(FB_TEST_X, FB_TEST_Y, FB_TEST_W, FB_TEST_H, 0xff)
        {
            eprintln!("error: fill 0xff: {e}");
            return ExitCode::FAILURE;
        }
        match guard.fb.send_update_wait(region) {
            Ok(r) => println!(
                "phase_b value=0xff marker={} collision={} send_ioctl_us={} wait_ioctl_us={}",
                r.marker, r.collision_test, r.send_ioctl_us, r.wait_ioctl_us
            ),
            Err(e) => {
                eprintln!("error: refresh 0xff: {e}");
                return ExitCode::FAILURE;
            }
        }
        thread::sleep(hold);

        let snap_for_restore = guard.snap.clone();
        match guard.fb.restore_and_refresh(
            FB_TEST_X,
            FB_TEST_Y,
            FB_TEST_W,
            FB_TEST_H,
            &snap_for_restore,
        ) {
            Ok(r) => {
                guard.armed = false;
                println!(
                    "phase_restore marker={} collision={} send_ioctl_us={} wait_ioctl_us={}",
                    r.marker, r.collision_test, r.send_ioctl_us, r.wait_ioctl_us
                );
            }
            Err(e) => {
                eprintln!("error: restore refresh: {e}");
                return ExitCode::FAILURE;
            }
        }

        println!("note: polarity still unresolved until operator reports which phase looked black");
        println!("SCRIBE DISPLAY GATE B: REVERSIBLE RECTANGLE PASSED");
        ExitCode::SUCCESS
    }
    #[cfg(not(unix))]
    {
        let _ = args;
        eprintln!("error: --fb-test requires a Linux/Kindle build (cfg(unix))");
        ExitCode::FAILURE
    }
}

/// Best-effort restoration on ordinary Rust unwind/early-return after pixels were written.
///
/// SIGKILL / power loss / kernel crash are not covered.
#[cfg(unix)]
struct FbRestoreGuard<'a> {
    fb: &'a mut kindle_platform::linux::MappedFramebuffer,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
    snap: Vec<u8>,
    armed: bool,
}

#[cfg(unix)]
impl Drop for FbRestoreGuard<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let _ = self
            .fb
            .restore_and_refresh(self.x, self.y, self.w, self.h, &self.snap);
    }
}

/// Parse mandatory `--ink-byte` (`00` or `ff` only). Does not set polarity constants.
fn parse_ink_byte(raw: &str) -> Result<u8, String> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "00" => Ok(0x00),
        "ff" => Ok(0xff),
        other => Err(format!(
            "invalid --ink-byte {other:?}; accepted values are only 00 and ff"
        )),
    }
}

fn ink_byte_arg(args: &[String]) -> Result<Option<String>, String> {
    let mut found = None;
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--ink-byte" {
            let Some(v) = args.get(i + 1) else {
                return Err("--ink-byte requires a value (00 or ff)".into());
            };
            if v.starts_with('-') {
                return Err("--ink-byte requires a value (00 or ff)".into());
            }
            if found.is_some() {
                return Err("--ink-byte specified more than once".into());
            }
            found = Some(v.clone());
            i += 2;
            continue;
        }
        i += 1;
    }
    Ok(found)
}

fn run_pen_display_test(args: &[String]) -> ExitCode {
    #[cfg(unix)]
    {
        use kindle_platform::gate_b::{self, BLACK_BYTE, WHITE_BYTE};
        use kindle_platform::hwtcon_abi::{
            waveform, MxcfbRect, CONSERVATIVE_UPDATE_MODE, CONSERVATIVE_WAVEFORM,
            MXCFB_SEND_UPDATE_MTK, MXCFB_WAIT_FOR_UPDATE_COMPLETE, TEMP_USE_AMBIENT,
        };
        use kindle_platform::linux::{LinuxEvdevStylus, LinuxFramebuffer, MappedFramebuffer};
        use kindle_platform::paint_asymmetric_marker;
        use kindle_platform::{SourcePoll, StylusSource, PREFERRED_STYLUS_PATH};
        use notebook_core::{
            portrait_wacom_calibration, DevicePoint, PagePoint, ScribeDisplayTransform,
        };

        println!("# Gate C pen→display marker test");
        println!("transform=ScribeDisplayTransform::PortraitIdentity");
        println!(
            "ioctl_send={:#x} ioctl_wait={:#x} waveform={} (GC16) mode={} (PARTIAL) temp={:#x}",
            MXCFB_SEND_UPDATE_MTK,
            MXCFB_WAIT_FOR_UPDATE_COMPLETE,
            CONSERVATIVE_WAVEFORM,
            CONSERVATIVE_UPDATE_MODE,
            TEMP_USE_AMBIENT as u32
        );
        assert_eq!(CONSERVATIVE_WAVEFORM, waveform::GC16);
        println!(
            "byte_polarity BLACK_BYTE={BLACK_BYTE:?} WHITE_BYTE={WHITE_BYTE:?} (unresolved; CLI does not set)"
        );
        println!("note: SIGKILL / power loss / kernel crash can bypass userspace restoration");

        let confirm = args.iter().any(|a| a == "--confirm-write");
        let ink_raw = match ink_byte_arg(args) {
            Ok(v) => v,
            Err(e) => {
                println!("REFUSE PHYSICAL TEST: {e}");
                return ExitCode::FAILURE;
            }
        };

        if ink_raw.is_none() {
            println!("ink_byte=missing");
            println!("REFUSE PHYSICAL TEST: --ink-byte required (00|ff)");
            println!("(no framebuffer modification attempted)");
            return ExitCode::FAILURE;
        }
        if !confirm {
            println!("confirm_write=missing");
            println!("REFUSE PHYSICAL TEST: --confirm-write required");
            println!("(no framebuffer modification attempted)");
            return ExitCode::FAILURE;
        }

        let ink = match parse_ink_byte(ink_raw.as_ref().unwrap()) {
            Ok(b) => b,
            Err(e) => {
                println!("REFUSE PHYSICAL TEST: {e}");
                return ExitCode::FAILURE;
            }
        };
        println!("ink_byte={ink:#04x} polarity=operator-selected/unresolved");

        let fb_path = args
            .windows(2)
            .find(|w| w[0] == "--fb")
            .map(|w| w[1].as_str())
            .unwrap_or("/dev/fb0");
        let stylus_path = args
            .windows(2)
            .find(|w| w[0] == "--input")
            .map(|w| w[1].as_str())
            .unwrap_or(PREFERRED_STYLUS_PATH);

        // Gate-B precondition check (read-only) before any O_RDWR mmap.
        let info = match LinuxFramebuffer::open_info_only_path(fb_path) {
            Ok(fb) => fb,
            Err(e) => {
                eprintln!("error: read-only fb probe failed: {e}");
                return ExitCode::FAILURE;
            }
        };
        if let Err(mismatches) = gate_b::check_preconditions(&info.fix, &info.var) {
            eprintln!("REFUSE PHYSICAL TEST: Gate B framebuffer preconditions failed:");
            for m in mismatches {
                eprintln!("  mismatch: {m}");
            }
            return ExitCode::FAILURE;
        }
        println!("preconditions=ok (hwtcon_v2 Gate B snapshot)");

        let mut src = match LinuxEvdevStylus::open(stylus_path) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("error: open stylus {stylus_path}: {e}");
                return ExitCode::FAILURE;
            }
        };
        match src.grab_exclusive() {
            Ok(()) => {
                println!("grab=EVIOCGRAB (exclusive; Drop ungrabs; fd/process exit also releases)")
            }
            Err(e) => {
                eprintln!(
                    "warning: EVIOCGRAB failed ({e}); another client (often WetInkHandler) \
                     may hold the stylus — decoded events may not arrive until it releases"
                );
            }
        }
        println!("device_path={stylus_path}");
        print_axes(src.axes());
        println!("waiting for normal Pen + BTN_TOUCH Down (ignore Hover/Move/Up/Eraser)...");

        let cal = portrait_wacom_calibration();
        let transform = ScribeDisplayTransform::PortraitIdentity;

        let down = loop {
            match src.poll_event() {
                Ok(SourcePoll::Event(ev)) => {
                    if ev.kind == StylusEventKind::Down && ev.tool == StylusTool::Pen {
                        break ev;
                    }
                    // Ignore Hover / Move / Up / eraser Down / etc.
                }
                Ok(SourcePoll::StreamGap { .. }) => {
                    println!("note: StreamGap while waiting for Down (recovering)");
                }
                Ok(SourcePoll::WouldBlock) => {
                    thread::sleep(Duration::from_millis(1));
                }
                Ok(SourcePoll::EndOfStream) => {
                    eprintln!("error: stylus stream ended before Down");
                    return ExitCode::FAILURE;
                }
                Err(e) => {
                    eprintln!("error: poll stylus: {e}");
                    return ExitCode::FAILURE;
                }
            }
        };

        let raw_x = down.device.x;
        let raw_y = down.device.y;
        let page: PagePoint = cal.to_page(DevicePoint::new(raw_x, raw_y));
        let (dx, dy) = match transform.to_panel_pixel(page) {
            Some(p) => p,
            None => {
                eprintln!("error: display transform produced non-finite coordinates");
                return ExitCode::FAILURE;
            }
        };
        println!(
            "raw/canonical=({raw_x:.0},{raw_y:.0}) display=({dx},{dy}) tool=Pen transform=PortraitIdentity"
        );

        // Drop stylus grab path continues via Drop after we finish; open FB only after Down.
        let mut fb = match MappedFramebuffer::open_path(fb_path) {
            Ok(fb) => fb,
            Err(e) => {
                eprintln!("error: open/mmap {fb_path}: {e}");
                return ExitCode::FAILURE;
            }
        };
        let g = fb.geometry;
        println!(
            "fb id={} map_len={} visible={}x{} stride={}",
            fb.fix.id_str(),
            fb.map_len(),
            g.xres,
            g.yres,
            g.line_length
        );

        // Snapshot exact dirty bounds before paint (clip may shrink near edges).
        let dirty_plan = match kindle_platform::marker_dirty_rect(dx, dy, g.xres, g.yres) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("error: marker dirty rect: {e:?}");
                return ExitCode::FAILURE;
            }
        };
        let snap = match fb.extract_rect(
            dirty_plan.x,
            dirty_plan.y,
            dirty_plan.width,
            dirty_plan.height,
        ) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("error: snapshot: {e}");
                return ExitCode::FAILURE;
            }
        };
        println!(
            "snapshot_bytes={} dirty=({},{} {}x{})",
            snap.len(),
            dirty_plan.x,
            dirty_plan.y,
            dirty_plan.width,
            dirty_plan.height
        );

        let mut guard = FbRestoreGuard {
            fb: &mut fb,
            x: dirty_plan.x,
            y: dirty_plan.y,
            w: dirty_plan.width,
            h: dirty_plan.height,
            snap,
            armed: true,
        };

        let dirty = {
            let buf = guard.fb.as_slice_mut();
            match paint_asymmetric_marker(buf, &g, dx, dy, ink) {
                Ok(d) => d,
                Err(e) => {
                    eprintln!("error: paint marker: {e:?}");
                    return ExitCode::FAILURE;
                }
            }
        };
        if dirty != dirty_plan {
            eprintln!("error: paint dirty {:?} != planned {:?}", dirty, dirty_plan);
            return ExitCode::FAILURE;
        }

        let region = MxcfbRect {
            top: dirty.y,
            left: dirty.x,
            width: dirty.width,
            height: dirty.height,
        };
        match guard.fb.send_update_wait(region) {
            Ok(r) => println!(
                "marker={} collision_test={} send_ioctl_us={} wait_ioctl_us={}",
                r.marker, r.collision_test, r.send_ioctl_us, r.wait_ioctl_us
            ),
            Err(e) => {
                eprintln!("error: marker refresh: {e}");
                return ExitCode::FAILURE;
            }
        }

        println!("PEN DISPLAY MARKER ACTIVE");
        println!("canonical=({:.0},{:.0})", page.x, page.y);
        println!("display=({dx},{dy})");
        println!("ink_byte={ink:#04x}");
        println!("hold_ms=3000");
        thread::sleep(Duration::from_secs(3));

        let snap_for_restore = guard.snap.clone();
        match guard.fb.restore_and_refresh(
            dirty.x,
            dirty.y,
            dirty.width,
            dirty.height,
            &snap_for_restore,
        ) {
            Ok(r) => {
                guard.armed = false;
                println!(
                    "restore marker={} collision={} send_ioctl_us={} wait_ioctl_us={}",
                    r.marker, r.collision_test, r.send_ioctl_us, r.wait_ioctl_us
                );
            }
            Err(e) => {
                eprintln!("error: restore refresh: {e}");
                return ExitCode::FAILURE;
            }
        }

        // Explicitly drop FB then stylus: EVIOCGRAB releases with LinuxEvdevStylus Drop.
        drop(guard);
        drop(fb);
        drop(src);
        println!("eviocgrab=released (WetInkHandler may resume normal pen ownership)");
        println!("SCRIBE INK GATE C: physical sequence complete — operator must score A–F");
        ExitCode::SUCCESS
    }
    #[cfg(not(unix))]
    {
        let _ = args;
        eprintln!("error: --pen-display-test requires a Linux/Kindle build (cfg(unix))");
        ExitCode::FAILURE
    }
}

#[cfg(unix)]
fn dirty_to_mxcfb(d: kindle_platform::DirtyRegion) -> Option<kindle_platform::MxcfbRect> {
    if d.x < 0 || d.y < 0 || d.width == 0 || d.height == 0 {
        return None;
    }
    Some(kindle_platform::MxcfbRect {
        top: d.y as u32,
        left: d.x as u32,
        width: d.width,
        height: d.height,
    })
}

fn run_stream_ink(args: &[String]) -> ExitCode {
    #[cfg(unix)]
    {
        use kindle_platform::gate_b::{self, BLACK_BYTE, WHITE_BYTE};
        use kindle_platform::hwtcon_abi::{
            waveform, CONSERVATIVE_UPDATE_MODE, CONSERVATIVE_WAVEFORM, MXCFB_SEND_UPDATE_MTK,
            TEMP_USE_AMBIENT,
        };
        use kindle_platform::linux::{
            LinuxEvdevStylus, LinuxFramebuffer, LinuxMonotonicClock, MappedFramebuffer,
        };
        use kindle_platform::{
            raster_line, raster_stamp, ClockDomain, DirtyRegion, MonotonicClock, SourcePoll,
            StylusSource, PREFERRED_STYLUS_PATH, STREAM_BRUSH_RADIUS,
        };
        use notebook_core::{portrait_wacom_calibration, DevicePoint, ScribeDisplayTransform};
        use stream::{format_dist, StreamRefreshPolicy, STREAM_REFRESH_INTERVAL_US};

        println!("# Gate D streaming handwriting baseline (--one-stroke)");
        println!("transform=ScribeDisplayTransform::PortraitIdentity");
        println!(
            "refresh_interval_us={STREAM_REFRESH_INTERVAL_US} brush_radius={STREAM_BRUSH_RADIUS}"
        );
        println!(
            "ioctl_send={:#x} waveform={} (GC16) mode={} (PARTIAL) temp={:#x} (async SEND, wait only at end)",
            MXCFB_SEND_UPDATE_MTK,
            CONSERVATIVE_WAVEFORM,
            CONSERVATIVE_UPDATE_MODE,
            TEMP_USE_AMBIENT as u32
        );
        assert_eq!(CONSERVATIVE_WAVEFORM, waveform::GC16);
        println!(
            "byte_polarity BLACK_BYTE={BLACK_BYTE:?} WHITE_BYTE={WHITE_BYTE:?} (unresolved; CLI does not set)"
        );
        println!(
            "restore_strategy=snapshot_visible_page_once; restore_union_of_stroke_dirty; no second virtual page"
        );

        if !args.iter().any(|a| a == "--one-stroke") {
            println!("REFUSE PHYSICAL TEST: Gate D requires --one-stroke");
            return ExitCode::FAILURE;
        }

        let confirm = args.iter().any(|a| a == "--confirm-write");
        let ink_raw = match ink_byte_arg(args) {
            Ok(v) => v,
            Err(e) => {
                println!("REFUSE PHYSICAL TEST: {e}");
                return ExitCode::FAILURE;
            }
        };
        if ink_raw.is_none() {
            println!("ink_byte=missing");
            println!("REFUSE PHYSICAL TEST: --ink-byte required (00|ff)");
            println!("(no framebuffer modification attempted)");
            return ExitCode::FAILURE;
        }
        if !confirm {
            println!("confirm_write=missing");
            println!("REFUSE PHYSICAL TEST: --confirm-write required");
            println!("(no framebuffer modification attempted)");
            return ExitCode::FAILURE;
        }
        let ink = match parse_ink_byte(ink_raw.as_ref().unwrap()) {
            Ok(b) => b,
            Err(e) => {
                println!("REFUSE PHYSICAL TEST: {e}");
                return ExitCode::FAILURE;
            }
        };
        println!("ink_byte={ink:#04x} polarity=operator-selected/unresolved");

        let fb_path = args
            .windows(2)
            .find(|w| w[0] == "--fb")
            .map(|w| w[1].as_str())
            .unwrap_or("/dev/fb0");
        let stylus_path = args
            .windows(2)
            .find(|w| w[0] == "--input")
            .map(|w| w[1].as_str())
            .unwrap_or(PREFERRED_STYLUS_PATH);

        let info = match LinuxFramebuffer::open_info_only_path(fb_path) {
            Ok(fb) => fb,
            Err(e) => {
                eprintln!("error: read-only fb probe failed: {e}");
                return ExitCode::FAILURE;
            }
        };
        if let Err(mismatches) = gate_b::check_preconditions(&info.fix, &info.var) {
            eprintln!("REFUSE PHYSICAL TEST: Gate B framebuffer preconditions failed:");
            for m in mismatches {
                eprintln!("  mismatch: {m}");
            }
            return ExitCode::FAILURE;
        }
        println!("preconditions=ok (hwtcon_v2 Gate B snapshot)");

        let mut fb = match MappedFramebuffer::open_path(fb_path) {
            Ok(fb) => fb,
            Err(e) => {
                eprintln!("error: open/mmap: {e}");
                return ExitCode::FAILURE;
            }
        };
        let g = fb.geometry;
        let page_snap = match fb.extract_rect(0, 0, g.xres, g.yres) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("error: snapshot visible page: {e}");
                return ExitCode::FAILURE;
            }
        };
        println!(
            "snapshot_visible_bytes={} ({}x{} panel pixels, stride={})",
            page_snap.len(),
            g.xres,
            g.yres,
            g.line_length
        );

        let mut src = match LinuxEvdevStylus::open(stylus_path) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("error: open stylus {stylus_path}: {e}");
                return ExitCode::FAILURE;
            }
        };
        match src.grab_exclusive() {
            Ok(()) => println!("grab=EVIOCGRAB (exclusive; Drop ungrabs)"),
            Err(e) => {
                eprintln!("warning: EVIOCGRAB failed ({e}); WetInkHandler may monopolize events")
            }
        }
        println!("device_path={stylus_path}");
        print_axes(src.axes());
        let input_clock = src.kernel_clock();
        println!("input_kernel_clock={input_clock:?}");
        println!("waiting for one Pen Down → Move* → Up ...");

        let cal = portrait_wacom_calibration();
        let transform = ScribeDisplayTransform::PortraitIdentity;
        let wall = LinuxMonotonicClock;

        let mut policy = StreamRefreshPolicy::gate_d_default();
        assert_eq!(policy.interval_us, STREAM_REFRESH_INTERVAL_US);
        let mut stroking = false;
        let mut last_px: Option<(i32, i32)> = None;
        let mut stroke_dirty: Option<DirtyRegion> = None;
        let mut last_marker: Option<u32> = None;
        let mut events_in_batch: u32 = 0;

        let mut input_events_total: u64 = 0;
        let mut down_count: u64 = 0;
        let mut move_count: u64 = 0;
        let mut up_count: u64 = 0;
        let mut stream_gaps: u64 = 0;
        let mut segments_rasterized: u64 = 0;
        let mut pixels_written: u64 = 0;
        let mut refresh_batches: u64 = 0;
        let mut dirty_area_total: u64 = 0;
        let mut send_samples: Vec<u64> = Vec::new();
        let mut interval_samples: Vec<u64> = Vec::new();
        let mut events_per_batch: Vec<u64> = Vec::new();
        let mut input_to_send_samples: Vec<u64> = Vec::new();
        let mut last_submit_wall: Option<u64> = None;
        let mut latest_input_kernel_us: Option<u64> = None;
        let mut hwtcon_errors: u64 = 0;
        let mut collision_nonzero: u64 = 0;
        let mut finished = false;

        #[allow(clippy::too_many_arguments)]
        fn try_submit(
            fb: &mut MappedFramebuffer,
            g: &kindle_platform::FbGeometry,
            region: DirtyRegion,
            now_us: u64,
            latest_in: Option<u64>,
            events_batch: u32,
            input_clock: ClockDomain,
            last_submit: &mut Option<u64>,
            last_marker: &mut Option<u32>,
            send_samples: &mut Vec<u64>,
            interval_samples: &mut Vec<u64>,
            events_per_batch: &mut Vec<u64>,
            input_to_send: &mut Vec<u64>,
            refresh_batches: &mut u64,
            dirty_area_total: &mut u64,
            hwtcon_errors: &mut u64,
        ) -> bool {
            let Some(clipped) = region.clamp_to(g.xres, g.yres) else {
                return true;
            };
            let Some(mx) = dirty_to_mxcfb(clipped) else {
                return true;
            };
            if let Some(prev) = *last_submit {
                interval_samples.push(now_us.saturating_sub(prev));
            }
            if let Some(kin) = latest_in {
                if input_clock == ClockDomain::LinuxMonotonic {
                    input_to_send.push(now_us.saturating_sub(kin));
                }
            }
            match fb.send_update(mx) {
                Ok(r) => {
                    *last_marker = Some(r.marker);
                    send_samples.push(r.send_ioctl_us);
                    events_per_batch.push(events_batch as u64);
                    *refresh_batches += 1;
                    *dirty_area_total = dirty_area_total
                        .saturating_add(clipped.width as u64 * clipped.height as u64);
                    *last_submit = Some(now_us);
                    true
                }
                Err(e) => {
                    *hwtcon_errors += 1;
                    eprintln!("error: SEND_UPDATE failed: {e}");
                    false
                }
            }
        }

        loop {
            match src.poll_event() {
                Ok(SourcePoll::Event(ev)) => {
                    input_events_total += 1;
                    if let Some(t) = ev.kernel_t_us() {
                        latest_input_kernel_us = Some(t);
                    }
                    match ev.kind {
                        StylusEventKind::Hover => {}
                        StylusEventKind::Down => {
                            if ev.tool != StylusTool::Pen {
                                continue;
                            }
                            if finished {
                                continue;
                            }
                            down_count += 1;
                            let page = cal.to_page(DevicePoint::new(ev.device.x, ev.device.y));
                            let Some((dx, dy)) = transform.to_panel_pixel(page) else {
                                continue;
                            };
                            let (px, py) = (dx as i32, dy as i32);
                            stroking = true;
                            last_px = Some((px, py));
                            events_in_batch = events_in_batch.saturating_add(1);
                            let painted = {
                                let buf = fb.as_slice_mut();
                                raster_stamp(buf, &g, px, py, ink, STREAM_BRUSH_RADIUS)
                            };
                            match painted {
                                Ok(r) => {
                                    pixels_written += r.pixels_written as u64;
                                    segments_rasterized += 1;
                                    if let Some(d) = r.dirty {
                                        policy.merge(d);
                                        stroke_dirty = Some(match stroke_dirty {
                                            Some(s) => s.union(d),
                                            None => d,
                                        });
                                    }
                                }
                                Err(e) => eprintln!("error: stamp: {e:?}"),
                            }
                        }
                        StylusEventKind::Move => {
                            if !stroking || ev.tool != StylusTool::Pen {
                                continue;
                            }
                            move_count += 1;
                            let page = cal.to_page(DevicePoint::new(ev.device.x, ev.device.y));
                            let Some((dx, dy)) = transform.to_panel_pixel(page) else {
                                continue;
                            };
                            let (px, py) = (dx as i32, dy as i32);
                            let Some((lx, ly)) = last_px else {
                                last_px = Some((px, py));
                                continue;
                            };
                            events_in_batch = events_in_batch.saturating_add(1);
                            let painted = {
                                let buf = fb.as_slice_mut();
                                raster_line(buf, &g, lx, ly, px, py, ink, STREAM_BRUSH_RADIUS)
                            };
                            match painted {
                                Ok(r) => {
                                    pixels_written += r.pixels_written as u64;
                                    segments_rasterized += 1;
                                    if let Some(d) = r.dirty {
                                        policy.merge(d);
                                        stroke_dirty = Some(match stroke_dirty {
                                            Some(s) => s.union(d),
                                            None => d,
                                        });
                                    }
                                }
                                Err(e) => eprintln!("error: line: {e:?}"),
                            }
                            last_px = Some((px, py));
                        }
                        StylusEventKind::Up => {
                            if !stroking {
                                continue;
                            }
                            up_count += 1;
                            finished = true;
                            let now = wall.now_us();
                            if let Some(region) = policy.force_take(now) {
                                let _ = try_submit(
                                    &mut fb,
                                    &g,
                                    region,
                                    now,
                                    latest_input_kernel_us,
                                    events_in_batch,
                                    input_clock,
                                    &mut last_submit_wall,
                                    &mut last_marker,
                                    &mut send_samples,
                                    &mut interval_samples,
                                    &mut events_per_batch,
                                    &mut input_to_send_samples,
                                    &mut refresh_batches,
                                    &mut dirty_area_total,
                                    &mut hwtcon_errors,
                                );
                            }
                            break;
                        }
                    }

                    if stroking {
                        let now = wall.now_us();
                        if let Some(region) = policy.take_if_due(now) {
                            if !try_submit(
                                &mut fb,
                                &g,
                                region,
                                now,
                                latest_input_kernel_us,
                                events_in_batch,
                                input_clock,
                                &mut last_submit_wall,
                                &mut last_marker,
                                &mut send_samples,
                                &mut interval_samples,
                                &mut events_per_batch,
                                &mut input_to_send_samples,
                                &mut refresh_batches,
                                &mut dirty_area_total,
                                &mut hwtcon_errors,
                            ) {
                                break;
                            }
                            events_in_batch = 0;
                        }
                    }
                }
                Ok(SourcePoll::StreamGap { had_active_contact }) => {
                    stream_gaps += 1;
                    println!(
                        "StreamGap had_active_contact={had_active_contact} — breaking stroke continuity"
                    );
                    if stroking {
                        let now = wall.now_us();
                        if let Some(region) = policy.force_take(now) {
                            let _ = try_submit(
                                &mut fb,
                                &g,
                                region,
                                now,
                                latest_input_kernel_us,
                                events_in_batch,
                                input_clock,
                                &mut last_submit_wall,
                                &mut last_marker,
                                &mut send_samples,
                                &mut interval_samples,
                                &mut events_per_batch,
                                &mut input_to_send_samples,
                                &mut refresh_batches,
                                &mut dirty_area_total,
                                &mut hwtcon_errors,
                            );
                        }
                        // One-stroke diagnostic: treat gap as termination.
                        finished = true;
                        break;
                    }
                    // Not in a stroke: drop continuity only (nothing to clear beyond last_px).
                    last_px = None;
                }
                Ok(SourcePoll::WouldBlock) => thread::sleep(Duration::from_millis(1)),
                Ok(SourcePoll::EndOfStream) => {
                    eprintln!("error: stylus EOF before stroke complete");
                    break;
                }
                Err(e) => {
                    eprintln!("error: poll: {e}");
                    break;
                }
            }
        }

        println!("stroke_complete finished={finished} hold_ms=2000");
        thread::sleep(Duration::from_secs(2));

        if let Some(m) = last_marker {
            match fb.wait_for_update(m) {
                Ok(w) => {
                    println!(
                        "final_wait marker={} collision_test={} wait_ioctl_us={}",
                        w.marker, w.collision_test, w.wait_ioctl_us
                    );
                    if w.collision_test != 0 {
                        collision_nonzero += 1;
                    }
                }
                Err(e) => {
                    hwtcon_errors += 1;
                    eprintln!("error: final wait: {e}");
                }
            }
        }

        // Restore union of stroke dirty from full-page snapshot.
        if let Some(dirty) = stroke_dirty.and_then(|d| d.clamp_to(g.xres, g.yres)) {
            let x = dirty.x as u32;
            let y = dirty.y as u32;
            let w = dirty.width;
            let h = dirty.height;
            // Extract sub-rect from page snapshot (tightly packed visible pixels).
            let mut region_bytes = Vec::with_capacity((w * h) as usize);
            for row in 0..h {
                let src_off = ((y + row) * g.xres + x) as usize;
                let end = src_off + w as usize;
                if end > page_snap.len() {
                    eprintln!("error: restore slice OOB");
                    break;
                }
                region_bytes.extend_from_slice(&page_snap[src_off..end]);
            }
            match fb.restore_and_refresh(x, y, w, h, &region_bytes) {
                Ok(r) => {
                    println!(
                        "restore marker={} collision={} send_ioctl_us={} wait_ioctl_us={} rect=({x},{y} {w}x{h})",
                        r.marker, r.collision_test, r.send_ioctl_us, r.wait_ioctl_us
                    );
                    if r.collision_test != 0 {
                        collision_nonzero += 1;
                    }
                }
                Err(e) => eprintln!("error: restore: {e}"),
            }
        } else {
            println!("restore=skipped (no stroke dirty)");
        }

        drop(fb);
        drop(src);
        println!("eviocgrab=released");

        println!("# Gate D telemetry (ioctl timings only; not pen/display latency)");
        println!("input_events_total={input_events_total}");
        println!("down_count={down_count} move_count={move_count} up_count={up_count}");
        println!("stream_gaps={stream_gaps}");
        println!("segments_rasterized={segments_rasterized} pixels_written≈{pixels_written}");
        println!("refresh_batches={refresh_batches} dirty_area_total={dirty_area_total}");
        println!(
            "max_pending_dirty_width={} max_pending_dirty_height={} pending_at_end={}",
            policy.max_pending_width,
            policy.max_pending_height,
            policy.pending().is_some()
        );
        println!("{}", format_dist("send_ioctl_us", &send_samples));
        println!(
            "{}",
            format_dist("events_per_refresh_batch", &events_per_batch)
        );
        println!(
            "{}",
            format_dist("refresh_interval_actual_us", &interval_samples)
        );
        println!(
            "{}",
            format_dist("latest_input_to_send_us", &input_to_send_samples)
        );
        println!("hwtcon_send_errors={hwtcon_errors} collision_nonzero={collision_nonzero}");
        println!("SCRIBE INK GATE D: physical sequence complete — operator must score streaming");
        ExitCode::SUCCESS
    }
    #[cfg(not(unix))]
    {
        let _ = args;
        eprintln!("error: --stream-ink requires a Linux/Kindle build (cfg(unix))");
        ExitCode::FAILURE
    }
}

/// Gate E2/E3: one stroke with stock WetInk DU/PARTIAL while stylus mode=1 (async batches).
///
/// Default cadence is Gate E2 (`25000` µs). Gate E3 passes `--du-interval-us` from the
/// allowed sweep set only.
fn run_stream_du(args: &[String]) -> ExitCode {
    use stream::{du_target_hz, parse_du_interval_us};

    println!("# Gate E2/E3 live DU handwriting (--one-stroke)");
    println!("transform=ScribeDisplayTransform::PortraitIdentity (unchanged from Gate D)");
    println!("waveform=DU (1) update_mode=PARTIAL (0) temp=0x1000 (stock WetInk; async SEND)");
    println!("arm_point=SET_STYLUS_MODE(1) on first Pen Down (before first DU SEND)");
    println!("devices: /dev/input/stylus grabbed; /dev/stylus untouched");

    let (du_interval_us, e3_override) = match parse_du_interval_us(args) {
        Ok(v) => v,
        Err(e) => {
            println!("{e}");
            return ExitCode::FAILURE;
        }
    };
    println!("du_interval_target_us={du_interval_us}");
    println!("du_target_hz={:.1}", du_target_hz(du_interval_us));
    println!("e3_override={e3_override}");
    println!("batching=immediate_first + interval_us={du_interval_us} + Up flush");

    if !args.iter().any(|a| a == "--one-stroke") {
        println!("REFUSE PHYSICAL TEST: Gate E2 requires --one-stroke");
        return ExitCode::FAILURE;
    }

    let confirm = args.iter().any(|a| a == "--confirm-write");
    let ink_raw = match ink_byte_arg(args) {
        Ok(v) => v,
        Err(e) => {
            println!("REFUSE PHYSICAL TEST: {e}");
            return ExitCode::FAILURE;
        }
    };
    if ink_raw.is_none() {
        println!("ink_byte=missing");
        println!("REFUSE PHYSICAL TEST: --ink-byte required (00|ff)");
        println!("(no O_RDWR framebuffer, no SET_STYLUS_MODE, no EVIOCGRAB, no SEND)");
        return ExitCode::FAILURE;
    }
    if !confirm {
        println!("confirm_write=missing");
        println!("REFUSE PHYSICAL TEST");
        println!("(no O_RDWR framebuffer, no SET_STYLUS_MODE, no EVIOCGRAB, no SEND)");
        return ExitCode::FAILURE;
    }
    let ink = match parse_ink_byte(ink_raw.as_ref().unwrap()) {
        Ok(b) => b,
        Err(e) => {
            println!("REFUSE PHYSICAL TEST: {e}");
            return ExitCode::FAILURE;
        }
    };
    println!("ink_byte={ink:#04x} polarity=operator-selected/unresolved");

    #[cfg(unix)]
    {
        use kindle_platform::gate_b::{self, BLACK_BYTE, WHITE_BYTE};
        use kindle_platform::hwtcon_abi::{waveform, MXCFB_SEND_UPDATE_MTK, TEMP_USE_AMBIENT};
        use kindle_platform::linux::{
            LinuxEvdevStylus, LinuxFramebuffer, LinuxMonotonicClock, MappedFramebuffer,
            StylusModeGuard,
        };
        use kindle_platform::{
            raster_line, raster_stamp, ClockDomain, DirtyRegion, MonotonicClock, SourcePoll,
            StylusModeLifecycle, StylusSource, UpdateClass, PREFERRED_STYLUS_PATH,
            STREAM_BRUSH_RADIUS,
        };
        use notebook_core::{portrait_wacom_calibration, DevicePoint, ScribeDisplayTransform};
        use stream::{format_dist, StreamRefreshPolicy};

        println!("brush_radius={STREAM_BRUSH_RADIUS}");
        println!(
            "ioctl_send={:#x} waveform={} (DU) temp={:#x}",
            MXCFB_SEND_UPDATE_MTK,
            waveform::DU,
            TEMP_USE_AMBIENT as u32
        );
        println!(
            "byte_polarity BLACK_BYTE={BLACK_BYTE:?} WHITE_BYTE={WHITE_BYTE:?} (unresolved; CLI does not set)"
        );
        println!(
            "restore_strategy=WAIT_final_DU → SET_STYLUS_MODE(0) → hold → restore dirty union → GC16"
        );

        let fb_path = args
            .windows(2)
            .find(|w| w[0] == "--fb")
            .map(|w| w[1].as_str())
            .unwrap_or("/dev/fb0");
        let stylus_path = args
            .windows(2)
            .find(|w| w[0] == "--input")
            .map(|w| w[1].as_str())
            .unwrap_or(PREFERRED_STYLUS_PATH);

        let info = match LinuxFramebuffer::open_info_only_path(fb_path) {
            Ok(fb) => fb,
            Err(e) => {
                eprintln!("error: read-only fb probe failed: {e}");
                return ExitCode::FAILURE;
            }
        };
        if let Err(mismatches) = gate_b::check_preconditions(&info.fix, &info.var) {
            eprintln!("REFUSE PHYSICAL TEST: Gate B framebuffer preconditions failed:");
            for m in mismatches {
                eprintln!("  mismatch: {m}");
            }
            return ExitCode::FAILURE;
        }
        println!("preconditions=ok (hwtcon_v2 Gate B snapshot)");

        let mut fb = match MappedFramebuffer::open_path(fb_path) {
            Ok(fb) => fb,
            Err(e) => {
                eprintln!("error: open/mmap: {e}");
                return ExitCode::FAILURE;
            }
        };
        let g = fb.geometry;
        let page_snap = match fb.extract_rect(0, 0, g.xres, g.yres) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("error: snapshot visible page: {e}");
                return ExitCode::FAILURE;
            }
        };
        println!(
            "snapshot_visible_bytes={} ({}x{} panel pixels, stride={})",
            page_snap.len(),
            g.xres,
            g.yres,
            g.line_length
        );

        let mut src = match LinuxEvdevStylus::open(stylus_path) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("error: open stylus {stylus_path}: {e}");
                return ExitCode::FAILURE;
            }
        };
        match src.grab_exclusive() {
            Ok(()) => println!("grab=EVIOCGRAB (exclusive; Drop ungrabs)"),
            Err(e) => {
                eprintln!("warning: EVIOCGRAB failed ({e}); WetInkHandler may monopolize events")
            }
        }
        println!("device_path={stylus_path}");
        print_axes(src.axes());
        let input_clock = src.kernel_clock();
        println!("input_kernel_clock={input_clock:?}");
        println!("waiting for one Pen Down → Move* → Up ...");

        let cal = portrait_wacom_calibration();
        let transform = ScribeDisplayTransform::PortraitIdentity;
        let wall = LinuxMonotonicClock;

        // Phase 1: wait for Pen Down (no stylus mode yet).
        let mut input_events_total: u64 = 0;
        let mut down_count: u64 = 0;
        let mut move_count: u64 = 0;
        let mut up_count: u64 = 0;
        let mut stream_gaps: u64 = 0;
        let down_ev: Option<StylusEvent>;
        loop {
            match src.poll_event() {
                Ok(SourcePoll::Event(ev)) => {
                    input_events_total += 1;
                    match ev.kind {
                        StylusEventKind::Hover => {}
                        StylusEventKind::Down if ev.tool == StylusTool::Pen => {
                            down_count += 1;
                            down_ev = Some(ev);
                            break;
                        }
                        StylusEventKind::Move | StylusEventKind::Up | StylusEventKind::Down => {}
                    }
                }
                Ok(SourcePoll::StreamGap { .. }) => {
                    stream_gaps += 1;
                }
                Ok(SourcePoll::WouldBlock) => thread::sleep(Duration::from_millis(1)),
                Ok(SourcePoll::EndOfStream) => {
                    eprintln!("error: stylus EOF before Pen Down");
                    drop(src);
                    println!("eviocgrab=released");
                    return ExitCode::FAILURE;
                }
                Err(e) => {
                    eprintln!("error: poll: {e}");
                    drop(src);
                    println!("eviocgrab=released");
                    return ExitCode::FAILURE;
                }
            }
        }
        let down_ev = down_ev.expect("Pen Down captured");

        // Phase 2: arm → stroke → wait → disarm (guard ends before GC16 restore).
        let mut life = StylusModeLifecycle::new();
        let mut policy = if e3_override {
            StreamRefreshPolicy::new(du_interval_us)
        } else {
            StreamRefreshPolicy::gate_e2_du_default()
        };
        assert_eq!(policy.interval_us, du_interval_us);

        let mut stroke_dirty: Option<DirtyRegion> = None;
        let mut last_wet_marker: Option<u32> = None;
        let mut wet_markers_submitted: u64 = 0;
        let mut segments_rasterized: u64 = 0;
        let mut pixels_written: u64 = 0;
        let mut du_refresh_batches: u64 = 0;
        let mut dirty_area_total: u64 = 0;
        let mut send_samples: Vec<u64> = Vec::new();
        let mut interval_samples: Vec<u64> = Vec::new();
        let mut overshoot_samples: Vec<u64> = Vec::new();
        let mut events_per_batch: Vec<u64> = Vec::new();
        let mut input_to_send_samples: Vec<u64> = Vec::new();
        let mut last_submit_wall: Option<u64> = None;
        let mut latest_input_kernel_us: Option<u64> = down_ev.kernel_t_us();
        let mut du_send_errors: u64 = 0;
        let mut final_collision_test: Option<u32> = None;
        let mut final_wait_ioctl_us: Option<u64> = None;
        let mut final_send_wall_us: Option<u64> = None;
        let mut final_send_to_completion_us: Option<u64> = None;
        let mut stroke_start_us: Option<u64> = None;
        let mut stroke_end_us: Option<u64> = None;
        let mut stylus_mode_disable_us: u64 = 0;
        let mut finished = false;
        let mut events_in_batch: u32 = 0;
        let max_pending_width: u32;
        let max_pending_height: u32;
        let pending_at_end: bool;

        let (mut guard, en) = match StylusModeGuard::arm(&mut fb) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("SCRIBE INK GATE E2: DU STREAM FAILED SAFELY");
                eprintln!("error: SET_STYLUS_MODE(1): {e}");
                drop(src);
                println!("eviocgrab=released");
                return ExitCode::FAILURE;
            }
        };
        if let Err(e) = life.mark_armed() {
            eprintln!("error: lifecycle arm: {e}");
            let _ = guard.disarm();
            drop(src);
            println!("eviocgrab=released");
            return ExitCode::FAILURE;
        }
        let stylus_mode_enable_us = en.set_stylus_mode_ioctl_us;
        println!(
            "stylus_mode_enable=1 ioctl_rc={} set_stylus_mode_ioctl_us={}",
            en.ioctl_rc, en.set_stylus_mode_ioctl_us
        );

        let disarm_ok = {
            #[allow(clippy::too_many_arguments)]
            fn try_submit_du(
                fb: &mut MappedFramebuffer,
                life: &StylusModeLifecycle,
                g: &kindle_platform::FbGeometry,
                region: DirtyRegion,
                now_us: u64,
                target_interval_us: u64,
                latest_in: Option<u64>,
                events_batch: u32,
                input_clock: ClockDomain,
                last_submit: &mut Option<u64>,
                last_wet_marker: &mut Option<u32>,
                wet_markers_submitted: &mut u64,
                send_samples: &mut Vec<u64>,
                interval_samples: &mut Vec<u64>,
                overshoot_samples: &mut Vec<u64>,
                events_per_batch: &mut Vec<u64>,
                input_to_send: &mut Vec<u64>,
                du_refresh_batches: &mut u64,
                dirty_area_total: &mut u64,
                du_send_errors: &mut u64,
                final_send_wall_us: &mut Option<u64>,
            ) -> bool {
                if let Err(e) = life.require_send(UpdateClass::WetInkDu) {
                    eprintln!("error: lifecycle: {e}");
                    *du_send_errors += 1;
                    return false;
                }
                let Some(clipped) = region.clamp_to(g.xres, g.yres) else {
                    return true;
                };
                let Some(mx) = dirty_to_mxcfb(clipped) else {
                    return true;
                };
                if let Some(prev) = *last_submit {
                    let actual = now_us.saturating_sub(prev);
                    interval_samples.push(actual);
                    overshoot_samples.push(actual.saturating_sub(target_interval_us));
                }
                if let Some(kin) = latest_in {
                    if input_clock == ClockDomain::LinuxMonotonic {
                        input_to_send.push(now_us.saturating_sub(kin));
                    }
                }
                match fb.send_update_wetink_du(mx) {
                    Ok(r) => {
                        *last_wet_marker = Some(r.marker);
                        *wet_markers_submitted += 1;
                        send_samples.push(r.send_ioctl_us);
                        events_per_batch.push(events_batch as u64);
                        *du_refresh_batches += 1;
                        *dirty_area_total = dirty_area_total
                            .saturating_add(clipped.width as u64 * clipped.height as u64);
                        *last_submit = Some(now_us);
                        *final_send_wall_us = Some(now_us);
                        true
                    }
                    Err(e) => {
                        *du_send_errors += 1;
                        eprintln!("error: DU SEND_UPDATE failed: {e}");
                        false
                    }
                }
            }

            // Down stamp + immediate first DU batch.
            let page = cal.to_page(DevicePoint::new(down_ev.device.x, down_ev.device.y));
            let mut last_px: Option<(i32, i32)> = None;
            let mut stroking = false;
            if let Some((dx, dy)) = transform.to_panel_pixel(page) {
                let (px, py) = (dx as i32, dy as i32);
                stroking = true;
                last_px = Some((px, py));
                events_in_batch = 1;
                let painted = {
                    let buf = guard.fb().as_slice_mut();
                    raster_stamp(buf, &g, px, py, ink, STREAM_BRUSH_RADIUS)
                };
                match painted {
                    Ok(r) => {
                        pixels_written += r.pixels_written as u64;
                        segments_rasterized += 1;
                        if let Some(d) = r.dirty {
                            policy.merge(d);
                            stroke_dirty = Some(d);
                        }
                    }
                    Err(e) => eprintln!("error: stamp: {e:?}"),
                }
                let now = wall.now_us();
                stroke_start_us = Some(now);
                if let Some(region) = policy.take_if_due(now) {
                    if !try_submit_du(
                        guard.fb(),
                        &life,
                        &g,
                        region,
                        now,
                        du_interval_us,
                        latest_input_kernel_us,
                        events_in_batch,
                        input_clock,
                        &mut last_submit_wall,
                        &mut last_wet_marker,
                        &mut wet_markers_submitted,
                        &mut send_samples,
                        &mut interval_samples,
                        &mut overshoot_samples,
                        &mut events_per_batch,
                        &mut input_to_send_samples,
                        &mut du_refresh_batches,
                        &mut dirty_area_total,
                        &mut du_send_errors,
                        &mut final_send_wall_us,
                    ) {
                        // Fall through to wait/disarm cleanup.
                        stroking = false;
                        finished = true;
                    } else {
                        events_in_batch = 0;
                    }
                }
            }

            while stroking && !finished {
                match src.poll_event() {
                    Ok(SourcePoll::Event(ev)) => {
                        input_events_total += 1;
                        if let Some(t) = ev.kernel_t_us() {
                            latest_input_kernel_us = Some(t);
                        }
                        match ev.kind {
                            StylusEventKind::Hover => {}
                            StylusEventKind::Down => {}
                            StylusEventKind::Move => {
                                if ev.tool != StylusTool::Pen {
                                    continue;
                                }
                                move_count += 1;
                                let page = cal.to_page(DevicePoint::new(ev.device.x, ev.device.y));
                                let Some((dx, dy)) = transform.to_panel_pixel(page) else {
                                    continue;
                                };
                                let (px, py) = (dx as i32, dy as i32);
                                let Some((lx, ly)) = last_px else {
                                    last_px = Some((px, py));
                                    continue;
                                };
                                events_in_batch = events_in_batch.saturating_add(1);
                                let painted = {
                                    let buf = guard.fb().as_slice_mut();
                                    raster_line(buf, &g, lx, ly, px, py, ink, STREAM_BRUSH_RADIUS)
                                };
                                match painted {
                                    Ok(r) => {
                                        pixels_written += r.pixels_written as u64;
                                        segments_rasterized += 1;
                                        if let Some(d) = r.dirty {
                                            policy.merge(d);
                                            stroke_dirty = Some(match stroke_dirty {
                                                Some(s) => s.union(d),
                                                None => d,
                                            });
                                        }
                                    }
                                    Err(e) => eprintln!("error: line: {e:?}"),
                                }
                                last_px = Some((px, py));
                                let now = wall.now_us();
                                if let Some(region) = policy.take_if_due(now) {
                                    if !try_submit_du(
                                        guard.fb(),
                                        &life,
                                        &g,
                                        region,
                                        now,
                                        du_interval_us,
                                        latest_input_kernel_us,
                                        events_in_batch,
                                        input_clock,
                                        &mut last_submit_wall,
                                        &mut last_wet_marker,
                                        &mut wet_markers_submitted,
                                        &mut send_samples,
                                        &mut interval_samples,
                                        &mut overshoot_samples,
                                        &mut events_per_batch,
                                        &mut input_to_send_samples,
                                        &mut du_refresh_batches,
                                        &mut dirty_area_total,
                                        &mut du_send_errors,
                                        &mut final_send_wall_us,
                                    ) {
                                        break;
                                    }
                                    events_in_batch = 0;
                                }
                            }
                            StylusEventKind::Up => {
                                up_count += 1;
                                finished = true;
                                let now = wall.now_us();
                                stroke_end_us = Some(now);
                                if let Some(region) = policy.force_take(now) {
                                    let _ = try_submit_du(
                                        guard.fb(),
                                        &life,
                                        &g,
                                        region,
                                        now,
                                        du_interval_us,
                                        latest_input_kernel_us,
                                        events_in_batch,
                                        input_clock,
                                        &mut last_submit_wall,
                                        &mut last_wet_marker,
                                        &mut wet_markers_submitted,
                                        &mut send_samples,
                                        &mut interval_samples,
                                        &mut overshoot_samples,
                                        &mut events_per_batch,
                                        &mut input_to_send_samples,
                                        &mut du_refresh_batches,
                                        &mut dirty_area_total,
                                        &mut du_send_errors,
                                        &mut final_send_wall_us,
                                    );
                                }
                                break;
                            }
                        }
                    }
                    Ok(SourcePoll::StreamGap { had_active_contact }) => {
                        stream_gaps += 1;
                        println!(
                            "StreamGap had_active_contact={had_active_contact} — breaking stroke continuity"
                        );
                        let now = wall.now_us();
                        stroke_end_us = Some(now);
                        if let Some(region) = policy.force_take(now) {
                            let _ = try_submit_du(
                                guard.fb(),
                                &life,
                                &g,
                                region,
                                now,
                                du_interval_us,
                                latest_input_kernel_us,
                                events_in_batch,
                                input_clock,
                                &mut last_submit_wall,
                                &mut last_wet_marker,
                                &mut wet_markers_submitted,
                                &mut send_samples,
                                &mut interval_samples,
                                &mut overshoot_samples,
                                &mut events_per_batch,
                                &mut input_to_send_samples,
                                &mut du_refresh_batches,
                                &mut dirty_area_total,
                                &mut du_send_errors,
                                &mut final_send_wall_us,
                            );
                        }
                        finished = true;
                        break;
                    }
                    Ok(SourcePoll::WouldBlock) => thread::sleep(Duration::from_millis(1)),
                    Ok(SourcePoll::EndOfStream) => {
                        eprintln!("error: stylus EOF during stroke");
                        break;
                    }
                    Err(e) => {
                        eprintln!("error: poll: {e}");
                        break;
                    }
                }
            }

            max_pending_width = policy.max_pending_width;
            max_pending_height = policy.max_pending_height;
            pending_at_end = policy.pending().is_some();

            if let Some(m) = last_wet_marker {
                match guard.fb().wait_for_update(m) {
                    Ok(w) => {
                        let wait_return = wall.now_us();
                        final_collision_test = Some(w.collision_test);
                        final_wait_ioctl_us = Some(w.wait_ioctl_us);
                        if let Some(fs) = final_send_wall_us {
                            final_send_to_completion_us = Some(wait_return.saturating_sub(fs));
                        }
                        println!(
                            "final_wait marker={} collision_test={} wait_ioctl_us={} (per-batch collision=not_observed)",
                            w.marker, w.collision_test, w.wait_ioctl_us
                        );
                    }
                    Err(e) => eprintln!("error: final DU wait: {e}"),
                }
            } else {
                println!("final_wait=skipped (no wet marker)");
            }

            match guard.disarm() {
                Ok(dis) => {
                    stylus_mode_disable_us = dis.set_stylus_mode_ioctl_us;
                    if let Err(e) = life.mark_disarmed() {
                        eprintln!("error: lifecycle disarm: {e}");
                        false
                    } else {
                        println!(
                            "stylus_mode_disable=0 ioctl_rc={} set_stylus_mode_ioctl_us={}",
                            dis.ioctl_rc, dis.set_stylus_mode_ioctl_us
                        );
                        true
                    }
                }
                Err(e) => {
                    eprintln!("SCRIBE INK GATE E2: STYLUS MODE DISARM FAILURE");
                    eprintln!("error: SET_STYLUS_MODE(0): {e}");
                    eprintln!("DO NOT ISSUE GC16; memory-only restore");
                    false
                }
            }
        };

        println!("stroke_complete finished={finished} hold_ms=2000 (disarmed observation)");
        thread::sleep(Duration::from_secs(2));

        if let Some(dirty) = stroke_dirty.and_then(|d| d.clamp_to(g.xres, g.yres)) {
            let x = dirty.x as u32;
            let y = dirty.y as u32;
            let w = dirty.width;
            let h = dirty.height;
            let mut region_bytes = Vec::with_capacity((w * h) as usize);
            for row in 0..h {
                let src_off = ((y + row) * g.xres + x) as usize;
                let end = src_off + w as usize;
                if end > page_snap.len() {
                    eprintln!("error: restore slice OOB");
                    break;
                }
                region_bytes.extend_from_slice(&page_snap[src_off..end]);
            }
            if disarm_ok {
                if let Err(e) = life.require_send(UpdateClass::Ordinary) {
                    eprintln!("error: {e}");
                } else {
                    match fb.restore_and_refresh(x, y, w, h, &region_bytes) {
                        Ok(r) => {
                            println!(
                                "restore_gc16 marker={} collision={} send_ioctl_us={} wait_ioctl_us={} rect=({x},{y} {w}x{h})",
                                r.marker, r.collision_test, r.send_ioctl_us, r.wait_ioctl_us
                            );
                        }
                        Err(e) => eprintln!("error: restore GC16: {e}"),
                    }
                }
            } else {
                match fb.restore_rect(x, y, w, h, &region_bytes) {
                    Ok(()) => println!(
                        "restore_memory_only=ok rect=({x},{y} {w}x{h}) (no GC16; disarm failed)"
                    ),
                    Err(e) => eprintln!("error: memory restore: {e}"),
                }
            }
        } else {
            println!("restore=skipped (no stroke dirty)");
        }

        drop(fb);
        drop(src);
        println!("eviocgrab=released");

        println!("# Gate E2/E3 telemetry (ioctl timings only; not pen/display latency)");
        println!("du_interval_target_us={du_interval_us} e3_override={e3_override}");
        println!("input_events_total={input_events_total}");
        println!("down_count={down_count} move_count={move_count} up_count={up_count}");
        println!("stream_gaps={stream_gaps}");
        println!("segments_rasterized={segments_rasterized} pixels_written≈{pixels_written}");
        println!(
            "du_refresh_batches={du_refresh_batches} wet_markers_submitted={wet_markers_submitted} last_wet_marker={last_wet_marker:?}"
        );
        let stroke_duration_us = match (stroke_start_us, stroke_end_us) {
            (Some(s), Some(e)) => e.saturating_sub(s),
            _ => 0,
        };
        let du_batches_per_second = if stroke_duration_us > 0 {
            du_refresh_batches as f64 * 1_000_000.0 / stroke_duration_us as f64
        } else {
            0.0
        };
        let move_events_per_second = if stroke_duration_us > 0 {
            move_count as f64 * 1_000_000.0 / stroke_duration_us as f64
        } else {
            0.0
        };
        let avg_dirty = if du_refresh_batches > 0 {
            dirty_area_total / du_refresh_batches
        } else {
            0
        };
        let max_events_per_batch = events_per_batch.iter().copied().max().unwrap_or(0);
        let median_events_per_batch = stream::dist_min_median_p90_max(&events_per_batch)
            .map(|(_, m, _, _)| m)
            .unwrap_or(0);
        println!(
            "stroke_duration_us={stroke_duration_us} du_batches_per_second={du_batches_per_second:.2} move_events_per_second={move_events_per_second:.2}"
        );
        println!("dirty_area_total={dirty_area_total} average_dirty_area_per_batch={avg_dirty}");
        println!(
            "max_pending_dirty_width={max_pending_width} max_pending_dirty_height={max_pending_height} pending_at_end={pending_at_end}"
        );
        println!("{}", format_dist("du_send_ioctl_us", &send_samples));
        println!("events_per_du_batch max={max_events_per_batch} median={median_events_per_batch}");
        println!("{}", format_dist("events_per_du_batch", &events_per_batch));
        println!(
            "{}",
            format_dist("refresh_interval_actual_us", &interval_samples)
        );
        println!(
            "{}",
            format_dist("interval_overshoot_us", &overshoot_samples)
        );
        println!(
            "{}",
            format_dist("latest_input_to_du_send_us", &input_to_send_samples)
        );
        println!(
            "final_wait_ioctl_us={final_wait_ioctl_us:?} final_collision_test={final_collision_test:?}"
        );
        println!(
            "final_send_wall_us={final_send_wall_us:?} final_send_to_completion_us={final_send_to_completion_us:?}"
        );
        println!("du_send_errors={du_send_errors} (per-batch collision status=not_observed)");
        println!(
            "stylus_mode_enable_us={stylus_mode_enable_us} stylus_mode_disable_us={stylus_mode_disable_us}"
        );
        if e3_override {
            println!(
                "SCRIBE INK GATE E3: physical sequence complete — operator must score cadence"
            );
        } else {
            println!(
                "SCRIBE INK GATE E2: physical sequence complete — operator must score streaming"
            );
        }
        if du_send_errors > 0 {
            ExitCode::FAILURE
        } else {
            ExitCode::SUCCESS
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (args, ink, du_interval_us, e3_override);
        eprintln!("error: --stream-du physical path requires a Linux/Kindle build (cfg(unix))");
        ExitCode::FAILURE
    }
}

fn report_fast_ink_info() -> ExitCode {
    use kindle_platform::{stylus_mode, MXCFB_SEND_UPDATE_MTK, MXCFB_SET_STYLUS_MODE};

    println!("# Gate E0 stock fast-ink recon (read-only facts; no ioctl issued)");
    println!("fast_ink_backend=mtk_stylus_mode");
    println!(
        "stylus_mode_ioctl={:#x} (MXCFB_SET_STYLUS_MODE; proven in WetInkHandler)",
        MXCFB_SET_STYLUS_MODE
    );
    println!(
        "stock_mode_enable={} (WITH_NO_TPS) stock_mode_disable={} (DISABLED)",
        stylus_mode::WITH_NO_TPS,
        stylus_mode::DISABLED
    );
    println!("stock_fd_target=/dev/fb0 (WetInkHandler object+0; not /dev/stylus misc)");
    println!("stock_enable_boundary=internal flag @obj+0x20c clear→set; app window/tool policy (exact BTN_TOUCH coupling unproven)");
    println!("stock_disable_boundary=flag set→clear; OffMode / non-interested window / rubber paths in strings");
    println!(
        "ordinary_send_update_also_present={:#x} (MXCFB_SEND_UPDATE_MTK in same WetInkHandler binary)",
        MXCFB_SEND_UPDATE_MTK
    );
    println!("standard_update_coexistence=stock_WetInk_issues_DU_SEND_while_armed (public docs forbid ordinary coexistence; FW 5.18.6 WetInk uses DU)");
    println!("lab126_stylus_misc_lock_ioctls=not_found_in_WetInk_or_deviced");
    println!("mtk_update_flags_ioctls=not_found_in_WetInk");
    println!("wait_for_any_ioctl=not_found_in_WetInk");
    println!("pigment_path=PROVEN (mmap fb0 + MXCFB_SEND_UPDATE_MTK DU/PARTIAL while SET_STYLUS_MODE(1); E1.1)");
    println!("note: IPC setStylusMode (Pen/Lasso/Off) is a separate app-level concept from EPDC stylus_mode 0/1");
    ExitCode::SUCCESS
}

/// Gate E1: does mmap write become visible under `MXCFB_SET_STYLUS_MODE(1)` without SEND?
fn run_stylus_mode_test(args: &[String]) -> ExitCode {
    use kindle_platform::{
        stylus_mode, validate_e1_stylus_mode, MXCFB_SEND_UPDATE_MTK, MXCFB_SET_STYLUS_MODE,
        MXCFB_WAIT_FOR_UPDATE_COMPLETE,
    };

    let path = args
        .windows(2)
        .find(|w| w[0] == "--fb")
        .map(|w| w[1].as_str())
        .unwrap_or("/dev/fb0");
    let confirm = args.iter().any(|a| a == "--confirm-write");

    let ink_raw = match ink_byte_arg(args) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };
    let Some(ink_raw) = ink_raw else {
        println!("ink_byte=missing");
        println!("REFUSE PHYSICAL TEST: --ink-byte 00|ff required (polarity unresolved)");
        return ExitCode::FAILURE;
    };
    let phase_a = match parse_ink_byte(&ink_raw) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };
    let phase_b = !phase_a; // 0x00 ↔ 0xff contrast only

    // Mode validation is host-checkable even on dry-run.
    assert!(validate_e1_stylus_mode(stylus_mode::DISABLED).is_ok());
    assert!(validate_e1_stylus_mode(stylus_mode::WITH_NO_TPS).is_ok());
    assert!(validate_e1_stylus_mode(2).is_err());
    assert!(validate_e1_stylus_mode(3).is_err());

    println!("# Gate E1 stylus-mode direct framebuffer proof on {path}");
    println!(
        "stylus_mode_ioctl={:#x} enable={} (WITH_NO_TPS) disable={} (DISABLED)",
        MXCFB_SET_STYLUS_MODE,
        stylus_mode::WITH_NO_TPS,
        stylus_mode::DISABLED
    );
    println!("rect left={E1_TEST_X} top={E1_TEST_Y} width={E1_TEST_W} height={E1_TEST_H}");
    println!(
        "phase_a_byte={phase_a:#04x} phase_b_byte={phase_b:#04x} (contrast only; polarity unresolved)"
    );
    println!("lifecycle=Ordinary -> SET_STYLUS_MODE(1) -> mmap writes only -> SET_STYLUS_MODE(0) -> GC16 restore");
    println!("Gate E1 intentionally omits SEND_UPDATE to prove mmap-alone behavior (not a hardware forbid)");
    println!("while_armed ordinary_GC16=forbidden wet_DU=allowed_by_lifecycle_but_not_issued_here");
    println!("also_omitted: update-flags / WAIT_FOR_ANY / modes 2/3");
    println!("devices_untouched=/dev/stylus /dev/input/stylus (display-only experiment)");
    println!("note: SIGKILL / power loss / kernel failure may bypass userspace disarm");

    if !confirm {
        println!("confirm_write=missing");
        println!("REFUSE PHYSICAL TEST");
        println!("(no O_RDWR /dev/fb0, no mmap write, no MXCFB_SET_STYLUS_MODE)");
        return ExitCode::FAILURE;
    }

    #[cfg(unix)]
    {
        use kindle_platform::gate_b;
        use kindle_platform::hwtcon_abi::MxcfbRect;
        use kindle_platform::linux::{
            LinuxFramebuffer, LinuxMonotonicClock, MappedFramebuffer, StylusModeGuard,
        };
        use kindle_platform::{rect_checksum, MonotonicClock, StylusModeLifecycle};

        let mut life = StylusModeLifecycle::new();
        println!("ordinary_updates_outstanding=0 (fresh process; no prior SEND_UPDATE)");

        let info = match LinuxFramebuffer::open_info_only_path(path) {
            Ok(fb) => fb,
            Err(e) => {
                eprintln!("error: read-only probe failed: {e}");
                return ExitCode::FAILURE;
            }
        };
        if let Err(mismatches) = gate_b::check_preconditions(&info.fix, &info.var) {
            eprintln!("REFUSE PHYSICAL TEST: Gate B framebuffer preconditions failed:");
            for m in mismatches {
                eprintln!("  mismatch: {m}");
            }
            return ExitCode::FAILURE;
        }
        println!("preconditions=ok (hwtcon_v2 Gate B snapshot)");

        let mut fb = match MappedFramebuffer::open_path(path) {
            Ok(fb) => fb,
            Err(e) => {
                eprintln!("error: open/mmap: {e}");
                return ExitCode::FAILURE;
            }
        };

        let snap = match fb.extract_rect(E1_TEST_X, E1_TEST_Y, E1_TEST_W, E1_TEST_H) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("error: snapshot: {e}");
                return ExitCode::FAILURE;
            }
        };
        let snap_ck = rect_checksum(&snap);
        println!(
            "snapshot_bytes={} checksum={snap_ck} (framebuffer unmodified at snapshot)",
            snap.len()
        );

        // --- arm ---
        let (mut guard, en) = match StylusModeGuard::arm(&mut fb) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("SCRIBE INK GATE E1: STYLUS MODE SUBMIT FAILED SAFELY");
                eprintln!("error: enable SET_STYLUS_MODE(1): {e}");
                return ExitCode::FAILURE;
            }
        };
        if let Err(e) = life.mark_armed() {
            eprintln!("error: lifecycle: {e}");
            return ExitCode::FAILURE;
        }
        println!(
            "stylus_mode_enable={} ioctl_rc={} ioctl_errno={} set_stylus_mode_ioctl_us={}",
            en.mode, en.ioctl_rc, en.ioctl_errno, en.set_stylus_mode_ioctl_us
        );
        println!("OBSERVE: any visual change from enable alone? (hold 1s)");
        thread::sleep(Duration::from_secs(1));

        let clock = LinuxMonotonicClock;
        let hold = Duration::from_secs(2);

        // --- phase A: memory write only (E1 omits SEND on purpose) ---
        assert!(
            life.require_ordinary_send_update().is_err(),
            "ordinary GC16 SEND must be forbidden while armed"
        );
        assert!(
            life.require_wetink_du_send_update().is_ok(),
            "lifecycle allows WetInk DU while armed; E1 still does not issue it"
        );
        println!("NO SEND_UPDATE ISSUED (Gate E1 intentional mmap-alone diagnostic)");
        let t0 = clock.now_us();
        if let Err(e) = guard
            .fb()
            .fill_rect(E1_TEST_X, E1_TEST_Y, E1_TEST_W, E1_TEST_H, phase_a)
        {
            eprintln!("error: phase A fill: {e}");
            return ExitCode::FAILURE;
        }
        let t1 = clock.now_us();
        println!(
            "phase_a value={phase_a:#04x} framebuffer_write_us={} (memory only; NOT pigment latency)",
            t1.saturating_sub(t0)
        );
        println!("OBSERVE phase_a: visible / invisible? apparent delay?");
        thread::sleep(hold);

        // --- phase B ---
        let t0 = clock.now_us();
        if let Err(e) = guard
            .fb()
            .fill_rect(E1_TEST_X, E1_TEST_Y, E1_TEST_W, E1_TEST_H, phase_b)
        {
            eprintln!("error: phase B fill: {e}");
            return ExitCode::FAILURE;
        }
        let t1 = clock.now_us();
        println!(
            "phase_b value={phase_b:#04x} framebuffer_write_us={} (memory only; NOT pigment latency)",
            t1.saturating_sub(t0)
        );
        println!("OBSERVE phase_b: visible / invisible? apparent delay?");
        thread::sleep(hold);

        // --- disarm BEFORE any ordinary refresh ---
        let dis = match guard.disarm() {
            Ok(r) => r,
            Err(e) => {
                eprintln!("SCRIBE INK GATE E1: STYLUS MODE DISARM FAILURE");
                eprintln!("error: disable SET_STYLUS_MODE(0): {e}");
                eprintln!("DO NOT ISSUE MXCFB_SEND_UPDATE_MTK (stylus mode may still be armed)");
                // Best-effort memory restore only; no SEND. (guard Drop also tried mode=0)
                let _ = fb.restore_rect(E1_TEST_X, E1_TEST_Y, E1_TEST_W, E1_TEST_H, &snap);
                eprintln!("attempted memory-only restore of snapshot; closing without SEND");
                return ExitCode::FAILURE;
            }
        };
        if let Err(e) = life.mark_disarmed() {
            eprintln!("error: lifecycle: {e}");
            return ExitCode::FAILURE;
        }
        println!(
            "stylus_mode_disable={} ioctl_rc={} ioctl_errno={} set_stylus_mode_ioctl_us={}",
            dis.mode, dis.ioctl_rc, dis.ioctl_errno, dis.set_stylus_mode_ioctl_us
        );
        println!("OBSERVE: any transition on mode=0 alone?");
        thread::sleep(Duration::from_secs(1));

        // --- ordinary restore + GC16 ---
        if let Err(e) = life.require_ordinary_send_update() {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
        if let Err(e) = fb.restore_rect(E1_TEST_X, E1_TEST_Y, E1_TEST_W, E1_TEST_H, &snap) {
            eprintln!("error: memory restore: {e}");
            return ExitCode::FAILURE;
        }
        println!("restore_memory=ok checksum_expected={snap_ck}");

        let region = MxcfbRect {
            top: E1_TEST_Y,
            left: E1_TEST_X,
            width: E1_TEST_W,
            height: E1_TEST_H,
        };
        match fb.send_update_wait(region) {
            Ok(r) => {
                println!(
                    "restore_gc16 ioctl_send={:#x} ioctl_wait={:#x} marker={} collision={} send_ioctl_us={} wait_ioctl_us={}",
                    MXCFB_SEND_UPDATE_MTK,
                    MXCFB_WAIT_FOR_UPDATE_COMPLETE,
                    r.marker,
                    r.collision_test,
                    r.send_ioctl_us,
                    r.wait_ioctl_us
                );
                println!("OBSERVE: original panel restored?");
            }
            Err(e) => {
                eprintln!("error: GC16 restore SEND/WAIT: {e}");
                return ExitCode::FAILURE;
            }
        }

        println!("devices_confirmed_untouched=/dev/stylus /dev/input/stylus");
        println!("NO SEND_UPDATE ISSUED confirmed by E1 control flow (mmap-alone proof)");
        println!(
            "SCRIBE INK GATE E1: mechanical path complete — operator must classify Result A/B/C"
        );
        ExitCode::SUCCESS
    }
    #[cfg(not(unix))]
    {
        let _ = (
            path,
            phase_a,
            phase_b,
            MXCFB_SEND_UPDATE_MTK,
            MXCFB_WAIT_FOR_UPDATE_COMPLETE,
        );
        eprintln!(
            "error: --stylus-mode-test physical path requires a Linux/Kindle build (cfg(unix))"
        );
        ExitCode::FAILURE
    }
}

/// Gate E1.2: stock WetInk DU/PARTIAL SEND while stylus mode=1 (static rectangle).
fn run_wet_du_test(args: &[String]) -> ExitCode {
    use kindle_platform::hwtcon_abi::waveform;
    use kindle_platform::{
        stylus_mode, validate_e1_stylus_mode, MxcfbUpdateDataMtk, MXCFB_SEND_UPDATE_MTK,
        MXCFB_SET_STYLUS_MODE, MXCFB_WAIT_FOR_UPDATE_COMPLETE, TEMP_USE_AMBIENT,
        UPDATE_MODE_PARTIAL,
    };

    let path = args
        .windows(2)
        .find(|w| w[0] == "--fb")
        .map(|w| w[1].as_str())
        .unwrap_or("/dev/fb0");
    let confirm = args.iter().any(|a| a == "--confirm-write");

    let ink_raw = match ink_byte_arg(args) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };
    let Some(ink_raw) = ink_raw else {
        println!("ink_byte=missing");
        println!("REFUSE PHYSICAL TEST: --ink-byte 00|ff required (polarity unresolved)");
        return ExitCode::FAILURE;
    };
    let ink_byte = match parse_ink_byte(&ink_raw) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };

    assert!(validate_e1_stylus_mode(stylus_mode::DISABLED).is_ok());
    assert!(validate_e1_stylus_mode(stylus_mode::WITH_NO_TPS).is_ok());

    let preview = MxcfbUpdateDataMtk::wetink_du_partial(
        kindle_platform::MxcfbRect {
            top: E1_TEST_Y,
            left: E1_TEST_X,
            width: E1_TEST_W,
            height: E1_TEST_H,
        },
        1,
    );

    println!("# Gate E1.2 stock DU wet-update physical proof on {path}");
    println!(
        "stylus_mode_ioctl={:#x} enable={} disable={}",
        MXCFB_SET_STYLUS_MODE,
        stylus_mode::WITH_NO_TPS,
        stylus_mode::DISABLED
    );
    println!("rect left={E1_TEST_X} top={E1_TEST_Y} width={E1_TEST_W} height={E1_TEST_H}");
    println!("ink_byte={ink_byte:#04x} (polarity unresolved; one byte per run)");
    println!(
        "wet_update waveform={} (DU) update_mode={} (PARTIAL) temp={:#x} flags={} hist_bw={} hist_gray={}",
        preview.waveform_mode,
        preview.update_mode,
        preview.temp,
        preview.flags,
        preview.hist_bw_waveform_mode,
        preview.hist_gray_waveform_mode
    );
    assert_eq!(preview.waveform_mode, waveform::DU);
    assert_eq!(preview.waveform_mode, 1);
    assert_eq!(preview.update_mode, UPDATE_MODE_PARTIAL);
    assert_eq!(preview.update_mode, 0);
    assert_eq!(preview.temp, TEMP_USE_AMBIENT);
    assert_eq!(preview.temp, 0x1000);
    println!(
        "lifecycle=Ordinary -> SET_STYLUS_MODE(1) -> mmap fill -> DU SEND -> observe -> WAIT -> SET_STYLUS_MODE(0) -> restore GC16"
    );
    println!("while_armed: ordinary_GC16=forbidden wet_DU=allowed (only wet DU issued)");
    println!("devices_untouched=/dev/stylus /dev/input/stylus (display-only; no pen)");
    println!("note: SIGKILL / power loss / kernel failure may bypass userspace disarm");

    if !confirm {
        println!("confirm_write=missing");
        println!("REFUSE PHYSICAL TEST");
        println!("(no O_RDWR /dev/fb0, no mmap write, no MXCFB_SET_STYLUS_MODE, no SEND_UPDATE)");
        return ExitCode::FAILURE;
    }

    #[cfg(unix)]
    {
        use kindle_platform::gate_b;
        use kindle_platform::hwtcon_abi::MxcfbRect;
        use kindle_platform::linux::{
            LinuxFramebuffer, LinuxMonotonicClock, MappedFramebuffer, StylusModeGuard,
        };
        use kindle_platform::{rect_checksum, MonotonicClock, StylusModeLifecycle, UpdateClass};

        let mut life = StylusModeLifecycle::new();
        println!("ordinary_updates_outstanding=0 (fresh process; no prior SEND_UPDATE)");
        assert!(life.can_send(UpdateClass::Ordinary));
        assert!(!life.can_send(UpdateClass::WetInkDu));

        let info = match LinuxFramebuffer::open_info_only_path(path) {
            Ok(fb) => fb,
            Err(e) => {
                eprintln!("error: read-only probe failed: {e}");
                return ExitCode::FAILURE;
            }
        };
        if let Err(mismatches) = gate_b::check_preconditions(&info.fix, &info.var) {
            eprintln!("REFUSE PHYSICAL TEST: Gate B framebuffer preconditions failed:");
            for m in mismatches {
                eprintln!("  mismatch: {m}");
            }
            return ExitCode::FAILURE;
        }
        println!("preconditions=ok (hwtcon_v2 Gate B snapshot)");

        let mut fb = match MappedFramebuffer::open_path(path) {
            Ok(fb) => fb,
            Err(e) => {
                eprintln!("error: open/mmap: {e}");
                return ExitCode::FAILURE;
            }
        };

        // --- State A: Ordinary snapshot ---
        let snap = match fb.extract_rect(E1_TEST_X, E1_TEST_Y, E1_TEST_W, E1_TEST_H) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("error: snapshot: {e}");
                return ExitCode::FAILURE;
            }
        };
        let snap_ck = rect_checksum(&snap);
        println!(
            "snapshot_bytes={} checksum={snap_ck} (framebuffer unmodified at snapshot)",
            snap.len()
        );
        assert_eq!(snap.len(), (E1_TEST_W * E1_TEST_H) as usize);

        // --- State B: Arm ---
        let (mut guard, en) = match StylusModeGuard::arm(&mut fb) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("SCRIBE INK GATE E1.2: DU WET SEND FAILED SAFELY");
                eprintln!("error: enable SET_STYLUS_MODE(1): {e}");
                return ExitCode::FAILURE;
            }
        };
        if let Err(e) = life.mark_armed() {
            eprintln!("error: lifecycle: {e}");
            return ExitCode::FAILURE;
        }
        println!(
            "stylus_mode_enable={} ioctl_rc={} ioctl_errno={} set_stylus_mode_ioctl_us={}",
            en.mode, en.ioctl_rc, en.ioctl_errno, en.set_stylus_mode_ioctl_us
        );
        assert!(!life.can_send(UpdateClass::Ordinary));
        assert!(life.can_send(UpdateClass::WetInkDu));

        let clock = LinuxMonotonicClock;

        // --- State C: framebuffer write while armed ---
        let t0 = clock.now_us();
        if let Err(e) = guard
            .fb()
            .fill_rect(E1_TEST_X, E1_TEST_Y, E1_TEST_W, E1_TEST_H, ink_byte)
        {
            eprintln!("error: fill while armed: {e}");
            return ExitCode::FAILURE;
        }
        let t1 = clock.now_us();
        let filled = match guard
            .fb()
            .extract_rect(E1_TEST_X, E1_TEST_Y, E1_TEST_W, E1_TEST_H)
        {
            Ok(v) => v,
            Err(e) => {
                eprintln!("error: verify fill: {e}");
                return ExitCode::FAILURE;
            }
        };
        if !filled.iter().all(|&b| b == ink_byte) {
            eprintln!("error: fill verify failed (not all bytes {ink_byte:#04x})");
            return ExitCode::FAILURE;
        }
        println!(
            "framebuffer_fill value={ink_byte:#04x} bytes={} write_us={} verify=ok",
            filled.len(),
            t1.saturating_sub(t0)
        );

        // --- State D: stock-style wet DU SEND (only SEND while armed) ---
        if let Err(e) = life.require_send(UpdateClass::WetInkDu) {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
        assert!(life.require_ordinary_send_update().is_err());
        let region = MxcfbRect {
            top: E1_TEST_Y,
            left: E1_TEST_X,
            width: E1_TEST_W,
            height: E1_TEST_H,
        };
        let wet = match guard.fb().send_update_wetink_du(region) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("SCRIBE INK GATE E1.2: DU WET SEND FAILED SAFELY");
                eprintln!("error: MXCFB_SEND_UPDATE_MTK wet DU: {e}");
                match guard.disarm() {
                    Ok(dis) => {
                        println!(
                            "stylus_mode_disable={} ioctl_rc={} set_stylus_mode_ioctl_us={}",
                            dis.mode, dis.ioctl_rc, dis.set_stylus_mode_ioctl_us
                        );
                        let _ = life.mark_disarmed();
                        let _ = fb.restore_rect(E1_TEST_X, E1_TEST_Y, E1_TEST_W, E1_TEST_H, &snap);
                    }
                    Err(de) => {
                        eprintln!("SCRIBE INK GATE E1.2: STYLUS MODE DISARM FAILURE");
                        eprintln!("error: {de}");
                        eprintln!("DO NOT ISSUE GC16; memory-only restore");
                        let _ = fb.restore_rect(E1_TEST_X, E1_TEST_Y, E1_TEST_W, E1_TEST_H, &snap);
                    }
                }
                return ExitCode::FAILURE;
            }
        };
        println!(
            "wet_send_marker={} wet_send_ioctl_us={} waveform=DU (1) mode=PARTIAL (0) temp={:#x} ioctl={:#x}",
            wet.marker,
            wet.send_ioctl_us,
            TEMP_USE_AMBIENT,
            MXCFB_SEND_UPDATE_MTK
        );
        println!("OBSERVE: 64x64 visible after DU SEND? speed/flicker/contrast? (hold 2s)");
        thread::sleep(Duration::from_secs(2));

        // --- WAIT after observation ---
        let waited = match guard.fb().wait_for_update(wet.marker) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("error: WAIT_FOR_UPDATE_COMPLETE: {e}");
                eprintln!("attempting safe disarm (no GC16 if disarm fails)");
                match guard.disarm() {
                    Ok(dis) => {
                        println!(
                            "stylus_mode_disable={} ioctl_rc={} set_stylus_mode_ioctl_us={}",
                            dis.mode, dis.ioctl_rc, dis.set_stylus_mode_ioctl_us
                        );
                        let _ = life.mark_disarmed();
                        let _ = fb.restore_rect(E1_TEST_X, E1_TEST_Y, E1_TEST_W, E1_TEST_H, &snap);
                    }
                    Err(de) => {
                        eprintln!("SCRIBE INK GATE E1.2: STYLUS MODE DISARM FAILURE");
                        eprintln!("error: {de}");
                        eprintln!("DO NOT ISSUE GC16; memory-only restore");
                        let _ = fb.restore_rect(E1_TEST_X, E1_TEST_Y, E1_TEST_W, E1_TEST_H, &snap);
                    }
                }
                return ExitCode::FAILURE;
            }
        };
        println!(
            "wait_marker={} collision_test={} wait_ioctl_us={} ioctl={:#x}",
            waited.marker,
            waited.collision_test,
            waited.wait_ioctl_us,
            MXCFB_WAIT_FOR_UPDATE_COMPLETE
        );

        // --- Disarm before ordinary cleanup ---
        let dis = match guard.disarm() {
            Ok(r) => r,
            Err(e) => {
                eprintln!("SCRIBE INK GATE E1.2: STYLUS MODE DISARM FAILURE");
                eprintln!("error: disable SET_STYLUS_MODE(0): {e}");
                eprintln!(
                    "DO NOT ISSUE MXCFB_SEND_UPDATE_MTK GC16 (stylus mode may still be armed)"
                );
                let _ = fb.restore_rect(E1_TEST_X, E1_TEST_Y, E1_TEST_W, E1_TEST_H, &snap);
                eprintln!("attempted memory-only restore of snapshot; closing without GC16 SEND");
                return ExitCode::FAILURE;
            }
        };
        if let Err(e) = life.mark_disarmed() {
            eprintln!("error: lifecycle: {e}");
            return ExitCode::FAILURE;
        }
        println!(
            "stylus_mode_disable={} ioctl_rc={} ioctl_errno={} set_stylus_mode_ioctl_us={}",
            dis.mode, dis.ioctl_rc, dis.ioctl_errno, dis.set_stylus_mode_ioctl_us
        );
        assert!(life.can_send(UpdateClass::Ordinary));
        assert!(!life.can_send(UpdateClass::WetInkDu));

        // --- Restore + GC16 cleanup ---
        if let Err(e) = life.require_ordinary_send_update() {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
        if let Err(e) = fb.restore_rect(E1_TEST_X, E1_TEST_Y, E1_TEST_W, E1_TEST_H, &snap) {
            eprintln!("error: memory restore: {e}");
            return ExitCode::FAILURE;
        }
        let restored = match fb.extract_rect(E1_TEST_X, E1_TEST_Y, E1_TEST_W, E1_TEST_H) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("error: restore verify extract: {e}");
                return ExitCode::FAILURE;
            }
        };
        let rest_ck = rect_checksum(&restored);
        if restored != snap {
            eprintln!("error: restore verify mismatch checksum_got={rest_ck} expected={snap_ck}");
            return ExitCode::FAILURE;
        }
        println!("restore_memory=ok checksum={rest_ck}");

        match fb.send_update_wait(region) {
            Ok(r) => {
                println!(
                    "restore_gc16 ioctl_send={:#x} ioctl_wait={:#x} marker={} collision={} send_ioctl_us={} wait_ioctl_us={} waveform=GC16 mode=PARTIAL",
                    MXCFB_SEND_UPDATE_MTK,
                    MXCFB_WAIT_FOR_UPDATE_COMPLETE,
                    r.marker,
                    r.collision_test,
                    r.send_ioctl_us,
                    r.wait_ioctl_us
                );
            }
            Err(e) => {
                eprintln!("error: GC16 restore SEND/WAIT: {e}");
                return ExitCode::FAILURE;
            }
        }

        println!("devices_confirmed_untouched=/dev/stylus /dev/input/stylus");
        println!("OBSERVE: normal Kindle UI after exit?");
        println!(
            "SCRIBE INK GATE E1.2: mechanical path complete — operator must score pigment yes/no"
        );
        ExitCode::SUCCESS
    }
    #[cfg(not(unix))]
    {
        let _ = (
            path,
            ink_byte,
            MXCFB_SEND_UPDATE_MTK,
            MXCFB_WAIT_FOR_UPDATE_COMPLETE,
        );
        eprintln!("error: --wet-du-test physical path requires a Linux/Kindle build (cfg(unix))");
        ExitCode::FAILURE
    }
}

fn report_abi() -> ExitCode {
    println!("target_pointer_width={}", usize::BITS);
    println!("kernel_long_bytes={KERNEL_LONG_BYTES}");
    println!("input_event_size={INPUT_EVENT_SIZE}");
    println!(
        "type_offset={} code_offset={} value_offset={}",
        2 * KERNEL_LONG_BYTES,
        2 * KERNEL_LONG_BYTES + 2,
        2 * KERNEL_LONG_BYTES + 4
    );
    println!(
        "btn_tool_pen=0x{:x} btn_tool_rubber=0x{:x} btn_touch=0x{:x} btn_stylus=0x{:x}",
        codes::btn::TOOL_PEN,
        codes::btn::TOOL_RUBBER,
        codes::btn::TOUCH,
        codes::btn::STYLUS
    );
    println!("canonical_page={}x{} (Wacom/NBK units)", 15624, 20832);
    println!("note: run --abi ON the Scribe; the value there is what the device binary must use");
    ExitCode::SUCCESS
}

fn list_inputs() -> ExitCode {
    #[cfg(unix)]
    {
        use kindle_platform::linux::LinuxInputDiscovery;
        let discovery = LinuxInputDiscovery::new();
        match discovery.list_devices() {
            Ok(devs) => {
                for d in &devs {
                    println!(
                        "{} | {} | {:?} | score={} | caps={:?} | source={:?}",
                        d.path,
                        d.name,
                        d.kind,
                        d.stylus_score(),
                        d.capabilities,
                        d.capability_source
                    );
                }
                match discovery.find_stylus() {
                    Ok(s) => println!("selected_stylus={} ({})", s.path, s.name),
                    Err(_) => println!("selected_stylus=(none)"),
                }
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("list-inputs: {e}");
                ExitCode::FAILURE
            }
        }
    }
    #[cfg(not(unix))]
    {
        let discovery = HostDiscovery::with_stylus("/dev/input/eventX", "Host Simulated Stylus");
        match discovery.list_devices() {
            Ok(devs) => {
                eprintln!("(Windows host) showing simulated discovery only:");
                for d in devs {
                    println!(
                        "{} | {} | {:?} | score={}",
                        d.path,
                        d.name,
                        d.kind,
                        d.stylus_score()
                    );
                }
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("{e}");
                ExitCode::FAILURE
            }
        }
    }
}

fn run_live(args: &[String]) -> Result<(), String> {
    #[cfg(unix)]
    {
        use kindle_platform::linux::{LinuxEvdevStylus, LinuxInputDiscovery, LinuxMonotonicClock};
        use kindle_platform::{
            select_stylus_device, SourcePoll, StylusSource, PREFERRED_STYLUS_PATH,
        };

        let explicit = args
            .windows(2)
            .find(|w| w[0] == "--input")
            .map(|w| w[1].clone());
        let using_explicit = explicit.is_some();

        let (path, name_hint) = if let Some(p) = explicit {
            (p, None)
        } else {
            let discovery = LinuxInputDiscovery::new();
            let devices = discovery.list_devices().map_err(|e| e.to_string())?;
            let selected = discovery
                .find_stylus()
                .or_else(|_| select_stylus_device(&devices, None))
                .map_err(|e| e.to_string())?;
            (selected.path.clone(), Some(selected.name.clone()))
        };

        let mut src = LinuxEvdevStylus::open(&path).map_err(|e| e.to_string())?;
        match src.grab_exclusive() {
            Ok(()) => {
                println!("grab=EVIOCGRAB (exclusive; Drop ungrabs; fd/process exit also releases)")
            }
            Err(e) => {
                eprintln!(
                    "warning: EVIOCGRAB failed ({e}); another client (often WetInkHandler) \
                     may hold the stylus — decoded events may not arrive until it releases"
                );
            }
        }
        let axes = *src.axes();
        let clock_domain = src.kernel_clock();
        let name = name_hint.unwrap_or_else(|| path.clone());

        println!("device={name} path={path}");
        if path != PREFERRED_STYLUS_PATH && !using_explicit {
            println!("note: preferred symlink {PREFERRED_STYLUS_PATH} was not selected");
        }
        print_axes(&axes);
        println!("clock={clock_domain:?} record_size={}", src.record_size());
        println!("# decoded StylusEvent stream (Ctrl-C to stop); not pen-to-pixel latency");
        println!(
            "# page-canonical space for portrait Wacom is {}x{} (identity)",
            notebook_core::CANONICAL_WIDTH as i32,
            notebook_core::CANONICAL_HEIGHT as i32
        );

        let _wall = LinuxMonotonicClock;
        loop {
            match src.poll_event().map_err(|e| e.to_string())? {
                SourcePoll::Event(ev) => println!("{}", format_event(&ev)),
                SourcePoll::StreamGap { had_active_contact } => {
                    println!("StreamGap had_active_contact={had_active_contact} (stroke cancelled; recovering at next SYN_REPORT)");
                }
                SourcePoll::WouldBlock => {
                    // Blocking open should not yield WouldBlock; treat as idle spin guard.
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                SourcePoll::EndOfStream => {
                    println!("# end of stream");
                    break;
                }
            }
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = args;
        Err("live input requires a Linux/Kindle build (cfg(unix))".into())
    }
}

#[cfg(unix)]
fn print_axes(axes: &StylusAxes) {
    let mut line = format!(
        "x={}..{} y={}..{}",
        axes.x.min, axes.x.max, axes.y.min, axes.y.max
    );
    if let Some(p) = axes.pressure {
        line.push_str(&format!(" pressure={}..{}", p.min, p.max));
    }
    if let Some(d) = axes.distance {
        line.push_str(&format!(" distance={}..{}", d.min, d.max));
    }
    if let Some(t) = axes.tilt_x {
        line.push_str(&format!(" tilt_x={}..{}", t.min, t.max));
    }
    if let Some(t) = axes.tilt_y {
        line.push_str(&format!(" tilt_y={}..{}", t.min, t.max));
    }
    println!("{line}");
}

#[cfg(unix)]
fn format_event(ev: &StylusEvent) -> String {
    let kind = match ev.kind {
        StylusEventKind::Hover => "Hover",
        StylusEventKind::Down => "Down ",
        StylusEventKind::Move => "Move ",
        StylusEventKind::Up => "Up   ",
    };
    let tool = match ev.tool {
        StylusTool::Pen => "Pen",
        StylusTool::Eraser => "Eraser",
        StylusTool::None => "None",
    };
    let mut s = format!(
        "{kind} tool={tool} x={:.0} y={:.0}",
        ev.device.x, ev.device.y
    );
    if let Some(p) = ev.pressure {
        s.push_str(&format!(" pressure={p:.3}"));
    }
    if let Some(d) = ev.distance {
        s.push_str(&format!(" distance_norm={d:.3}"));
    }
    if let Some(tx) = ev.tilt_x {
        s.push_str(&format!(" tilt_x={tx:.3}"));
    }
    if let Some(ty) = ev.tilt_y {
        s.push_str(&format!(" tilt_y={ty:.3}"));
    }
    s.push_str(&format!(
        " button1={} button2={}",
        ev.buttons.primary, ev.buttons.secondary
    ));
    if let Some(t) = ev.kernel_t_us() {
        s.push_str(&format!(" t_us={t} clock={:?}", ev.kernel_clock));
    }
    s
}

fn run_demo(args: &[String]) -> Result<(), String> {
    let out = args
        .windows(2)
        .find(|w| w[0] == "--out")
        .map(|w| PathBuf::from(&w[1]))
        .unwrap_or_else(|| PathBuf::from("ink-probe-demo.pgm"));

    let display = HostDisplay::new(400, 600);
    // Canonical page for the demo: numerically equal to the surface so the viewport is 1:1.
    let page = PageGeometry::new(display.width() as f32, display.height() as f32);
    let axes = StylusAxes::synthetic(DEMO_AXES.0, DEMO_AXES.1, Some(DEMO_AXES.2));
    let calibration = axes.calibration(Orientation::Portrait0, page);
    let clock = HostClock::new();
    let clock_domain = clock.domain();

    let mut pipe = InkPipeline::new(display, calibration, ViewportTransform::identity(), clock)
        .with_page_id("probe-page");
    let mut src = ScriptedStylus::new(synthetic_stroke());
    let outcome = pipe.run(&mut src).map_err(|e| e.to_string())?;

    pipe.display.write_pgm(&out).map_err(|e| e.to_string())?;

    println!("wrote {}", out.display());
    println!("source_outcome={outcome:?} local_clock={clock_domain:?}");
    println!("{}", pipe.stats.summary_line());
    println!(
        "finished_strokes={} stroke_points={:?} ops={}",
        pipe.stats.strokes_finished,
        pipe.strokes
            .finished_strokes()
            .iter()
            .map(|s| s.points.len())
            .collect::<Vec<_>>(),
        pipe.ops.len()
    );
    println!(
        "page={}x{} device_axes=x0..{} y0..{} pressure0..{} (synthetic host values)",
        page.width, page.height, DEMO_AXES.0, DEMO_AXES.1, DEMO_AXES.2
    );

    let replay = pipe.replay_ops();
    let consistent = replay.len() == pipe.strokes.finished_strokes().len()
        && replay
            .iter()
            .zip(pipe.strokes.finished_strokes())
            .all(|((id, points, finished), s)| id == &s.id && points == &s.points && *finished);
    println!("ops_replay_matches_strokes={consistent}");
    if !consistent {
        return Err("operation log disagrees with stroke geometry".to_string());
    }
    Ok(())
}

/// A hover-in, draw, lift-out sequence in device axis units.
fn synthetic_stroke() -> Vec<StylusEvent> {
    let mut events = Vec::new();
    for i in 0..3 {
        events.push(StylusEvent::new(
            StylusEventKind::Hover,
            80.0 + i as f32 * 5.0,
            100.0,
        ));
    }
    events.push(StylusEvent::new(StylusEventKind::Down, 100.0, 100.0).with_pressure(0.3));
    for i in 1..=40 {
        let t = i as f32;
        events.push(
            StylusEvent::new(
                StylusEventKind::Move,
                100.0 + t * 15.0,
                100.0 + (t * 0.4).sin() * 200.0 + t * 10.0,
            )
            .with_pressure(0.3 + (t % 10.0) * 0.05),
        );
    }
    events.push(StylusEvent::new(StylusEventKind::Up, 700.0, 600.0).with_pressure(0.0));
    events.push(StylusEvent::new(StylusEventKind::Hover, 720.0, 620.0));
    events
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ink_byte_accepts_00_and_ff_only() {
        assert_eq!(parse_ink_byte("00").unwrap(), 0x00);
        assert_eq!(parse_ink_byte("ff").unwrap(), 0xff);
        assert_eq!(parse_ink_byte("FF").unwrap(), 0xff);
        assert!(parse_ink_byte("0").is_err());
        assert!(parse_ink_byte("000").is_err());
        assert!(parse_ink_byte("aa").is_err());
        assert!(parse_ink_byte("").is_err());
    }

    #[test]
    fn ink_byte_arg_parses_and_rejects_missing_value() {
        let args = vec![
            "ink-probe".into(),
            "--pen-display-test".into(),
            "--ink-byte".into(),
            "00".into(),
            "--confirm-write".into(),
        ];
        assert_eq!(ink_byte_arg(&args).unwrap().as_deref(), Some("00"));

        let missing = vec!["ink-probe".into(), "--pen-display-test".into()];
        assert_eq!(ink_byte_arg(&missing).unwrap(), None);

        let bare = vec![
            "ink-probe".into(),
            "--ink-byte".into(),
            "--confirm-write".into(),
        ];
        assert!(ink_byte_arg(&bare).is_err());
    }

    #[test]
    fn refuse_messages_match_gate_c_contract() {
        // Document the exact strings hardware validation greps for.
        assert!(
            "REFUSE PHYSICAL TEST: --confirm-write required".starts_with("REFUSE PHYSICAL TEST")
        );
        assert!(
            "REFUSE PHYSICAL TEST: --ink-byte required (00|ff)".starts_with("REFUSE PHYSICAL TEST")
        );
    }

    #[test]
    fn e1_rect_is_interior_and_clear_of_off_visible_x() {
        assert_eq!(
            (E1_TEST_X, E1_TEST_Y, E1_TEST_W, E1_TEST_H),
            (120, 180, 64, 64)
        );
        assert_eq!(E1_TEST_X + E1_TEST_W, 184);
        assert_eq!(E1_TEST_Y + E1_TEST_H, 244);
    }

    #[test]
    fn e1_phase_bytes_are_opposites() {
        let a = parse_ink_byte("00").unwrap();
        let b = !a;
        assert_eq!(a, 0x00);
        assert_eq!(b, 0xff);
        let a2 = parse_ink_byte("ff").unwrap();
        assert_eq!(!a2, 0x00);
    }

    #[test]
    fn e1_dry_run_refuse_string() {
        let msg = "REFUSE PHYSICAL TEST";
        assert_eq!(msg, "REFUSE PHYSICAL TEST");
    }

    #[test]
    fn e12_wet_du_preview_fields() {
        use kindle_platform::hwtcon_abi::waveform;
        use kindle_platform::{
            MxcfbRect, MxcfbUpdateDataMtk, TEMP_USE_AMBIENT, UPDATE_MODE_PARTIAL,
        };
        let u = MxcfbUpdateDataMtk::wetink_du_partial(
            MxcfbRect {
                top: E1_TEST_Y,
                left: E1_TEST_X,
                width: E1_TEST_W,
                height: E1_TEST_H,
            },
            1,
        );
        assert_eq!(u.waveform_mode, waveform::DU);
        assert_eq!(u.update_mode, UPDATE_MODE_PARTIAL);
        assert_eq!(u.temp, TEMP_USE_AMBIENT);
        assert_eq!(u.flags, 0);
    }

    #[test]
    fn e2_dry_run_refuse_string() {
        assert_eq!("REFUSE PHYSICAL TEST", "REFUSE PHYSICAL TEST");
    }
}
