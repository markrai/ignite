use std::fs;
use std::path::Path;
use std::time::Instant;

use super::*;
use crate::reader_engine::app::ReaderPaths;
use crate::reader_engine::coordinator::SavePolicy;
use crate::reader_engine::session::PageLayout;
use crate::reader_engine::tests::{stored_zip, TempDocumentDir};

struct Fixture {
    _files: TempDocumentDir,
    root: std::path::PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let files = TempDocumentDir::new("reader-display");
        let root = files.0.join("reader");
        fs::create_dir_all(root.join("books")).unwrap();
        let archive = stored_zip(&[
            ("001.pgm", b"P5\n2 2\n255\n\x00\xff\x80\x40"),
            ("002.pgm", b"P5\n2 2\n255\n\xff\xff\xff\xff"),
            ("003.pgm", b"P5\n2 2\n255\n\x00\x00\x00\x00"),
        ]);
        fs::write(root.join("books/book.cbz"), archive).unwrap();
        Self {
            _files: files,
            root,
        }
    }

    fn screen(&self) -> ReaderScreen {
        let paths = ReaderPaths::new(&self.root, &[]).unwrap();
        let mut mode = ReaderMode::start(paths, SavePolicy::default()).unwrap();
        let layout = PageLayout {
            width: 260.0,
            height: 360.0,
            em: 18.0,
        };
        mode.enter(Path::new("book.cbz"), layout).unwrap();
        ReaderScreen::new(mode, Viewport::new(&geometry()).unwrap())
    }
}

fn geometry() -> FbGeometry {
    FbGeometry {
        xres: 80,
        yres: 96,
        xres_virtual: 84,
        yres_virtual: 98,
        xoffset: 2,
        yoffset: 1,
        bits_per_pixel: 8,
        line_length: 88,
        smem_len: 88 * 98,
    }
}

fn spin(mut poll: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !poll() {
        assert!(Instant::now() < deadline, "reader display did not progress");
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn close(screen: &mut ReaderScreen) -> Vec<Notice> {
    let mut notices = Vec::new();
    spin(|| {
        let notice = screen
            .tick(Duration::ZERO, |_, _| panic!("paint after close"))
            .unwrap();
        if let Some(notice) = notice {
            notices.push(notice);
        }
        screen.is_closed()
    });
    notices
}

fn first_page(screen: &mut ReaderScreen) {
    spin(|| {
        let mut shown = false;
        let notice = screen
            .tick(Duration::ZERO, |pixels, total| {
                assert_eq!(pixels.page, 0);
                assert_eq!(total, 3);
                shown = true;
                Ok(())
            })
            .unwrap();
        assert!(notice.is_none());
        shown
    });
}

#[test]
fn viewport_hit_targets_are_bounded() {
    let view = Viewport::new(&geometry()).unwrap();
    assert_eq!(view.render_box(), (80, 84));
    assert_eq!(view.action(0, 0), Some(Action::Previous));
    assert_eq!(view.action(26, 0), Some(Action::Home));
    assert_eq!(view.action(52, 0), Some(Action::Next));
    assert_eq!(view.action(80, 0), None);
    assert_eq!(view.action(20, 12), None);
}

#[test]
fn composition_centers_gray_pixels_and_preserves_stride_padding() {
    let fixture = Fixture::new();
    let mut screen = fixture.screen();
    let geo = geometry();
    let mut fb = vec![0x5a; geo.smem_len as usize];
    spin(|| {
        let mut done = false;
        screen
            .tick(Duration::ZERO, |pixels, total| {
                compose(&mut fb, &geo, pixels, total).unwrap();
                let view = Viewport::new(&geo).unwrap();
                let (width, height) = view.render_box();
                let x = geo.xoffset + (width - pixels.width) / 2;
                let y = geo.yoffset + view.bar + (height - pixels.height) / 2;
                for (row, bytes) in pixels.pixels.chunks_exact(pixels.width as usize).enumerate() {
                    let offset = (y as usize + row) * geo.line_length as usize + x as usize;
                    assert_eq!(&fb[offset..offset + bytes.len()], bytes);
                }
                for row in 0..geo.yres_virtual as usize {
                    let start = row * geo.line_length as usize;
                    let data = &fb[start..start + geo.line_length as usize];
                    if row == 0 || row > geo.yres as usize {
                        assert!(data.iter().all(|byte| *byte == 0x5a));
                    } else {
                        assert_eq!(&data[..2], &[0x5a; 2]);
                        assert_eq!(&data[82..], &[0x5a; 6]);
                    }
                }
                let before = fb.clone();
                assert!(compose(&mut fb, &geo, pixels, 0).is_err());
                assert_eq!(fb, before);
                let mut invalid = geo;
                invalid.bits_per_pixel = 16;
                assert!(compose(&mut fb, &invalid, pixels, total).is_err());
                assert_eq!(fb, before);
                done = true;
                Ok(())
            })
            .unwrap();
        done
    });
    screen.action(Action::Home).unwrap();
    close(&mut screen);
    screen.shutdown().unwrap();
}

#[test]
fn successful_presentation_flushes_before_home_handoff() {
    let fixture = Fixture::new();
    let mut screen = fixture.screen();
    first_page(&mut screen);
    assert!(!fixture.root.join("positions").exists());
    screen.action(Action::Home).unwrap();
    let notices = close(&mut screen);
    assert!(matches!(
        notices.as_slice(),
        [Notice::SaveComplete(SaveStatus::Saved(1)), Notice::Closed]
    ));
    assert!(fixture.root.join("positions").is_dir());
    screen.shutdown().unwrap();
}

#[test]
fn refresh_failure_never_acknowledges_or_saves_the_page() {
    let fixture = Fixture::new();
    let mut screen = fixture.screen();
    spin(|| {
        matches!(
            screen
                .tick(Duration::ZERO, |_, _| Err("simulated refresh failure".into()))
                .unwrap(),
            Some(Notice::DisplayFailed(_))
        )
    });
    let notices = close(&mut screen);
    assert!(matches!(notices.as_slice(), [Notice::Closed]));
    assert!(!fixture.root.join("positions").exists());
    screen.shutdown().unwrap();
}

#[test]
fn navigation_coalesces_while_acknowledgment_is_pending() {
    let fixture = Fixture::new();
    let mut screen = fixture.screen();
    first_page(&mut screen);
    screen.action(Action::Next).unwrap();
    screen.action(Action::Next).unwrap();
    screen.action(Action::Next).unwrap();
    spin(|| {
        let mut shown = false;
        let notice = screen
            .tick(Duration::ZERO, |pixels, _| {
                assert_eq!(pixels.page, 2);
                shown = true;
                Ok(())
            })
            .unwrap();
        assert!(notice.is_none());
        shown
    });
    screen.action(Action::Home).unwrap();
    let notices = close(&mut screen);
    assert!(matches!(
        notices.as_slice(),
        [Notice::SaveComplete(SaveStatus::Saved(1)), Notice::Closed]
    ));
    screen.shutdown().unwrap();
}

#[test]
fn leaving_during_open_cannot_paint_over_home() {
    let fixture = Fixture::new();
    let mut screen = fixture.screen();
    screen.action(Action::Home).unwrap();
    assert!(matches!(close(&mut screen).as_slice(), [Notice::Closed]));
    assert!(!fixture.root.join("positions").exists());
    screen.shutdown().unwrap();
}
