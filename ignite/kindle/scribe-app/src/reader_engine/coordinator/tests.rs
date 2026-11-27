use std::fs;
use std::thread;
use std::time::Instant;

use super::*;
use crate::reader_engine::location::Restore;
use crate::reader_engine::tests::{stored_zip, TempDocumentDir};

const LAYOUT: PageLayout = PageLayout {
    width: 260.0,
    height: 360.0,
    em: 18.0,
};

struct Fixture {
    _files: TempDocumentDir,
    path: PathBuf,
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let files = TempDocumentDir::new("coordinator");
        let archive = stored_zip(&[
            ("001.pgm", b"P5\n2 2\n255\n\x00\xff\x80\x40"),
            ("002.pgm", b"P5\n2 2\n255\n\xff\xff\xff\xff"),
            ("003.pgm", b"P5\n2 2\n255\n\x00\x00\x00\x00"),
        ]);
        Self {
            path: files.write("book.cbz", &archive),
            root: files.0.join("positions"),
            _files: files,
        }
    }

    fn open(&self, reader: &mut ReaderCoordinator) {
        reader
            .open(self.path.clone(), self.root.clone(), LAYOUT)
            .unwrap();
    }
}

fn at(ms: u64) -> Duration {
    Duration::from_millis(ms)
}

fn next(reader: &mut ReaderCoordinator, ms: u64) -> Event {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(event) = reader.poll(at(ms)).unwrap() {
            return event;
        }
        assert!(Instant::now() < deadline, "coordinator did not reply");
        thread::sleep(Duration::from_millis(1));
    }
}

fn start(fixture: &Fixture) -> ReaderCoordinator {
    let mut reader = ReaderCoordinator::start(SavePolicy::default()).unwrap();
    fixture.open(&mut reader);
    assert!(matches!(
        next(&mut reader, 0),
        Event::Opened {
            status: OpenStatus::New,
            page_count: 3,
            selected_page: Some(0),
        }
    ));
    reader
}

fn page(reader: &mut ReaderCoordinator, index: u32, ms: u64) -> DisplayToken {
    let expected = reader.request_page(index, 260, 360).unwrap();
    match next(reader, ms) {
        Event::PageReady { request, pixels } => {
            assert_eq!(request, expected);
            assert_eq!(pixels.page, index);
            pixels.token
        }
        other => panic!("expected page, got {other:?}"),
    }
}

fn present(reader: &mut ReaderCoordinator, token: DisplayToken, ms: u64) {
    reader.presented(token).unwrap();
    assert!(matches!(next(reader, ms), Event::Presented));
}

fn finish(reader: ReaderCoordinator) {
    reader.shutdown().ok().unwrap().unwrap();
}

#[test]
fn debounce_coalesces_only_confirmed_presentations() {
    let fixture = Fixture::new();
    let mut reader = start(&fixture);
    let first = page(&mut reader, 0, 0);
    assert!(!reader.has_unsaved_position());
    assert!(reader.poll(at(100)).unwrap().is_none());
    assert!(!fixture.root.exists());

    present(&mut reader, first, 100);
    assert!(reader.has_unsaved_position());
    let second = page(&mut reader, 1, 500);
    present(&mut reader, second, 500);
    assert!(reader.poll(at(849)).unwrap().is_none());
    assert!(reader.poll(at(1249)).unwrap().is_none());
    assert!(!fixture.root.exists());
    assert!(matches!(
        next(&mut reader, 1250),
        Event::SaveComplete(SaveStatus::Saved(1))
    ));
    assert!(!reader.has_unsaved_position());

    reader.request_close().unwrap();
    assert!(matches!(next(&mut reader, 1250), Event::Closed));
    fixture.open(&mut reader);
    assert!(matches!(
        next(&mut reader, 1250),
        Event::Opened {
            status: OpenStatus::Saved(Restore::Exact { page: 1, .. }),
            ..
        }
    ));
    reader.request_close().unwrap();
    assert!(matches!(next(&mut reader, 1250), Event::Closed));
    finish(reader);
}

#[test]
fn unpresented_and_stale_tokens_do_not_gain_persistence_authority() {
    let fixture = Fixture::new();
    let mut reader = start(&fixture);
    let first = page(&mut reader, 0, 0);
    assert_eq!(
        reader.request_page(1, 260, 360),
        Err(CoordinatorError::Busy)
    );
    assert_eq!(reader.request_close(), Err(CoordinatorError::Busy));
    reader.discard(first).unwrap();
    assert_eq!(
        reader.presented(first),
        Err(CoordinatorError::StalePresentation)
    );
    let second = page(&mut reader, 1, 100);
    assert_eq!(
        reader.presented(first),
        Err(CoordinatorError::StalePresentation)
    );
    reader.discard(second).unwrap();
    reader.request_close().unwrap();
    assert!(matches!(next(&mut reader, 100), Event::Closed));
    assert!(!fixture.root.exists());
    finish(reader);
}

#[test]
fn close_flushes_acknowledged_page_before_releasing_document() {
    let fixture = Fixture::new();
    let mut reader = start(&fixture);
    let token = page(&mut reader, 2, 0);
    reader.presented(token).unwrap();
    reader.request_close().unwrap();
    assert_eq!(
        reader.request_page(0, 260, 360),
        Err(CoordinatorError::Busy)
    );
    assert!(matches!(next(&mut reader, 0), Event::Presented));
    assert!(matches!(
        next(&mut reader, 0),
        Event::SaveComplete(SaveStatus::Saved(1))
    ));
    assert!(reader.close_pending());
    assert!(matches!(next(&mut reader, 0), Event::Closed));
    assert!(reader.is_closed());
    fixture.open(&mut reader);
    assert!(matches!(
        next(&mut reader, 0),
        Event::Opened {
            status: OpenStatus::Saved(Restore::Exact { page: 2, .. }),
            ..
        }
    ));
    reader.request_close().unwrap();
    assert!(matches!(next(&mut reader, 0), Event::Closed));
    finish(reader);
}

#[test]
fn failed_close_flush_retries_without_releasing_dirty_session() {
    let fixture = Fixture::new();
    let mut reader = start(&fixture);
    let token = page(&mut reader, 1, 0);
    present(&mut reader, token, 0);
    fs::write(&fixture.root, b"not a directory").unwrap();
    reader.request_close().unwrap();
    assert!(matches!(next(&mut reader, 0), Event::SaveFailed(_)));
    assert!(reader.has_unsaved_position());
    assert!(reader.close_pending());
    assert!(!reader.is_closed());
    assert_eq!(
        reader.open(fixture.path.clone(), fixture.root.clone(), LAYOUT),
        Err(CoordinatorError::Busy)
    );
    fs::remove_file(&fixture.root).unwrap();
    reader.request_close().unwrap();
    assert!(reader.poll(at(1999)).unwrap().is_none());
    assert!(!fixture.root.exists());
    assert!(matches!(
        next(&mut reader, 2000),
        Event::SaveComplete(SaveStatus::Saved(1))
    ));
    assert!(matches!(next(&mut reader, 2000), Event::Closed));
    finish(reader);
}

#[test]
fn retry_saves_newest_confirmed_location() {
    let fixture = Fixture::new();
    let mut reader = start(&fixture);
    let first = page(&mut reader, 0, 0);
    present(&mut reader, first, 0);
    fs::write(&fixture.root, b"not a directory").unwrap();
    assert!(matches!(next(&mut reader, 750), Event::SaveFailed(_)));
    let latest = page(&mut reader, 2, 1000);
    present(&mut reader, latest, 1000);
    fs::remove_file(&fixture.root).unwrap();
    assert!(reader.poll(at(2749)).unwrap().is_none());
    assert!(matches!(
        next(&mut reader, 2750),
        Event::SaveComplete(SaveStatus::Saved(1))
    ));
    reader.request_close().unwrap();
    assert!(matches!(next(&mut reader, 2750), Event::Closed));
    fixture.open(&mut reader);
    assert!(matches!(
        next(&mut reader, 2750),
        Event::Opened {
            status: OpenStatus::Saved(Restore::Exact { page: 2, .. }),
            ..
        }
    ));
    reader.request_close().unwrap();
    assert!(matches!(next(&mut reader, 2750), Event::Closed));
    finish(reader);
}

#[test]
fn close_cancels_pending_render_and_saves_prior_display() {
    let fixture = Fixture::new();
    let mut reader = start(&fixture);
    let token = page(&mut reader, 0, 0);
    present(&mut reader, token, 0);
    reader.request_page(1, 260, 360).unwrap();
    let latest = reader.request_page(2, 260, 360).unwrap();
    reader.request_close().unwrap();
    assert!(matches!(
        next(&mut reader, 0),
        Event::Cancelled(id) if id == latest
    ));
    assert!(matches!(
        next(&mut reader, 0),
        Event::SaveComplete(SaveStatus::Saved(1))
    ));
    assert!(matches!(next(&mut reader, 0), Event::Closed));
    fixture.open(&mut reader);
    assert!(matches!(
        next(&mut reader, 0),
        Event::Opened {
            status: OpenStatus::Saved(Restore::Exact { page: 0, .. }),
            ..
        }
    ));
    reader.request_close().unwrap();
    assert!(matches!(next(&mut reader, 0), Event::Closed));
    finish(reader);
}

#[test]
fn graceful_shutdown_refuses_open_session_and_clock_cannot_reverse() {
    let fixture = Fixture::new();
    let mut reader = start(&fixture);
    let token = page(&mut reader, 0, 100);
    present(&mut reader, token, 100);
    assert!(matches!(
        reader.poll(at(99)),
        Err(CoordinatorError::ClockReversed)
    ));
    let reader = match reader.shutdown() {
        Err(reader) => reader,
        Ok(_) => panic!("shutdown unexpectedly accepted an open session"),
    };
    assert!(reader.has_unsaved_position());
    reader.abort().unwrap();
    assert!(!fixture.root.exists());
}
