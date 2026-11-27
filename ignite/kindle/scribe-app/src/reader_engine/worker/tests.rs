use std::time::{Duration, Instant};

use super::*;
use crate::reader_engine::location::Restore;
use crate::reader_engine::tests::{rectangle_pdf, stored_zip, TempDocumentDir};

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
        let files = TempDocumentDir::new("worker");
        let path = files.write("book.pdf", &rectangle_pdf());
        let root = files.0.join("positions");
        Self {
            _files: files,
            path,
            root,
        }
    }

    fn open(&self) -> Command {
        Command::Open {
            path: self.path.clone(),
            store_root: self.root.clone(),
            layout: LAYOUT,
        }
    }
}

fn response(worker: &mut ReaderWorker) -> Response {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(reply) = worker.try_response().unwrap() {
            return reply;
        }
        assert!(Instant::now() < deadline, "reader worker did not reply");
        thread::sleep(Duration::from_millis(1));
    }
}

fn call(worker: &mut ReaderWorker, command: Command) -> Response {
    worker.try_submit(command).unwrap();
    response(worker)
}

fn render(worker: &mut ReaderWorker) -> PagePixels {
    match call(
        worker,
        Command::Render {
            width: 260,
            height: 360,
        },
    )
    .unwrap()
    {
        Reply::Rendered(page) => page,
        other => panic!("expected rendered page, got {other:?}"),
    }
}

#[test]
fn worker_round_trip_saves_only_acknowledged_pixels() {
    let fixture = Fixture::new();
    let mut worker = ReaderWorker::start().unwrap();
    assert!(worker.try_response().unwrap().is_none());
    assert!(matches!(
        call(&mut worker, fixture.open()).unwrap(),
        Reply::Opened {
            status: OpenStatus::New,
            page_count: 1,
            selected_page: Some(0),
        }
    ));
    assert!(matches!(
        call(&mut worker, fixture.open()),
        Err(CommandError::AlreadyOpen)
    ));
    assert!(matches!(
        call(&mut worker, Command::Save).unwrap(),
        Reply::Saved(SaveStatus::NoDisplayedPage)
    ));
    let page = render(&mut worker);
    assert_eq!(page.page, 0);
    assert_eq!(page.pixels.len(), (page.width * page.height) as usize);
    assert!(!page.pixels.is_empty());
    assert!(matches!(
        call(&mut worker, Command::Save).unwrap(),
        Reply::Saved(SaveStatus::NoDisplayedPage)
    ));
    assert!(!fixture.root.exists());
    assert!(matches!(
        call(&mut worker, Command::Acknowledge(page.token)).unwrap(),
        Reply::Acknowledged
    ));
    assert!(matches!(
        call(&mut worker, Command::Save).unwrap(),
        Reply::Saved(SaveStatus::Saved(1))
    ));
    assert!(matches!(
        call(&mut worker, Command::Close).unwrap(),
        Reply::Closed
    ));
    assert!(matches!(
        call(&mut worker, fixture.open()).unwrap(),
        Reply::Opened {
            status: OpenStatus::Saved(Restore::Exact { page: 0, .. }),
            ..
        }
    ));
    let next = render(&mut worker);
    assert!(matches!(
        call(&mut worker, Command::Acknowledge(page.token)),
        Err(CommandError::StaleSession)
    ));
    assert!(matches!(
        call(&mut worker, Command::Acknowledge(next.token)).unwrap(),
        Reply::Acknowledged
    ));
    assert!(matches!(
        call(&mut worker, Command::Save).unwrap(),
        Reply::Saved(SaveStatus::Unchanged)
    ));
    worker.shutdown().unwrap();
    assert_eq!(page.pixels.len(), (page.width * page.height) as usize);
}

#[test]
fn admission_is_single_flight_even_if_the_worker_already_finished() {
    let fixture = Fixture::new();
    let mut worker = ReaderWorker::start().unwrap();
    worker.try_submit(fixture.open()).unwrap();
    assert_eq!(worker.try_submit(Command::Close), Err(SubmitError::Busy));
    assert!(matches!(
        response(&mut worker).unwrap(),
        Reply::Opened { .. }
    ));
    assert!(matches!(
        call(&mut worker, Command::Close).unwrap(),
        Reply::Closed
    ));
    assert!(matches!(
        call(&mut worker, Command::Close),
        Err(CommandError::NoDocument)
    ));
    worker.shutdown().unwrap();
}

#[test]
fn invalid_open_and_invalid_commands_leave_worker_usable() {
    let fixture = Fixture::new();
    let mut worker = ReaderWorker::start().unwrap();
    assert!(matches!(
        call(&mut worker, Command::Navigate(0)),
        Err(CommandError::NoDocument)
    ));
    assert!(matches!(
        call(
            &mut worker,
            Command::Open {
                path: fixture.path.with_file_name("missing.pdf"),
                store_root: fixture.root.clone(),
                layout: LAYOUT,
            },
        ),
        Err(CommandError::Session(_))
    ));
    assert!(matches!(
        call(&mut worker, fixture.open()).unwrap(),
        Reply::Opened { .. }
    ));
    assert!(matches!(
        call(&mut worker, Command::Navigate(1)),
        Err(CommandError::Session(SessionError::PageOutOfRange { .. }))
    ));
    let first = render(&mut worker);
    assert!(matches!(
        call(
            &mut worker,
            Command::Render {
                width: 0,
                height: 360,
            },
        ),
        Err(CommandError::Session(_))
    ));
    assert!(matches!(
        call(&mut worker, Command::Acknowledge(first.token)),
        Err(CommandError::Session(SessionError::StaleRender))
    ));
    let second = render(&mut worker);
    assert!(matches!(
        call(&mut worker, Command::Acknowledge(second.token)).unwrap(),
        Reply::Acknowledged
    ));
    worker.shutdown().unwrap();
    assert!(!fixture.root.exists());
}

#[test]
fn tokens_cannot_cross_workers() {
    let fixture = Fixture::new();
    let mut first = ReaderWorker::start().unwrap();
    let mut second = ReaderWorker::start().unwrap();
    call(&mut first, fixture.open()).unwrap();
    call(&mut second, fixture.open()).unwrap();
    let old = render(&mut first);
    let current = render(&mut second);
    assert!(matches!(
        call(&mut second, Command::Acknowledge(old.token)),
        Err(CommandError::StaleSession)
    ));
    assert!(matches!(
        call(&mut second, Command::Acknowledge(current.token)).unwrap(),
        Reply::Acknowledged
    ));
    first.shutdown().unwrap();
    second.shutdown().unwrap();
    assert!(!fixture.root.exists());
}

#[test]
fn close_and_shutdown_do_not_save_implicitly() {
    let fixture = Fixture::new();
    let mut worker = ReaderWorker::start().unwrap();
    call(&mut worker, fixture.open()).unwrap();
    let page = render(&mut worker);
    call(&mut worker, Command::Acknowledge(page.token)).unwrap();
    call(&mut worker, Command::Close).unwrap();
    assert!(!fixture.root.exists());
    call(&mut worker, fixture.open()).unwrap();
    worker
        .try_submit(Command::Render {
            width: 260,
            height: 360,
        })
        .unwrap();
    worker.shutdown().unwrap();
    assert!(!fixture.root.exists());

    let mut worker = ReaderWorker::start().unwrap();
    worker.try_submit(fixture.open()).unwrap();
    drop(worker);
    assert!(!fixture.root.exists());
}

fn page_finished(worker: &mut ReaderWorker, expected: PageRequestId) -> PagePixels {
    match response(worker).unwrap() {
        Reply::PageFinished {
            request,
            result: Ok(pixels),
        } => {
            assert_eq!(request, expected);
            pixels
        }
        other => panic!("expected scheduled page, got {other:?}"),
    }
}

#[test]
fn latest_request_replaces_pending_work_and_preserves_control_order() {
    let fixture = Fixture::new();
    let mut worker = ReaderWorker::start().unwrap();
    worker.try_submit(fixture.open()).unwrap();
    assert_eq!(worker.request_page(0, 260, 360), Err(SubmitError::Busy));
    assert_eq!(worker.cancel_page(), None);
    response(&mut worker).unwrap();

    let first = worker.request_page(0, 100, 100).unwrap();
    let obsolete = worker.request_page(9, 0, 0).unwrap();
    let latest = worker.request_page(0, 260, 360).unwrap();
    assert_ne!(first, obsolete);
    assert_ne!(obsolete, latest);
    assert_eq!(worker.try_submit(Command::Save), Err(SubmitError::Busy));
    let page = page_finished(&mut worker, latest);
    assert_eq!(page.page, 0);
    assert_eq!((page.width, page.height), (260, 130));
    assert!(matches!(
        call(&mut worker, Command::Save).unwrap(),
        Reply::Saved(SaveStatus::NoDisplayedPage)
    ));
    call(&mut worker, Command::Acknowledge(page.token)).unwrap();
    assert!(matches!(
        call(&mut worker, Command::Save).unwrap(),
        Reply::Saved(SaveStatus::Saved(1))
    ));
    worker.shutdown().unwrap();
}

#[test]
fn cancellation_discards_unread_result_and_revokes_old_render_authority() {
    let fixture = Fixture::new();
    let mut worker = ReaderWorker::start().unwrap();
    call(&mut worker, fixture.open()).unwrap();
    let previous = render(&mut worker);
    let request = worker.request_page(0, 260, 360).unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    let buffered = loop {
        match worker.replies.as_ref().unwrap().try_recv() {
            Ok(reply) => break reply,
            Err(TryRecvError::Empty) => {
                assert!(Instant::now() < deadline);
                thread::sleep(Duration::from_millis(1));
            }
            Err(TryRecvError::Disconnected) => panic!("worker stopped"),
        }
    };
    assert!(matches!(
        &buffered,
        Ok(Reply::PageFinished {
            request: actual,
            result: Ok(_),
        }) if *actual == request
    ));
    assert_eq!(worker.cancel_page(), Some(request));
    assert_eq!(worker.cancel_page(), None);
    assert!(matches!(
        worker.handle_response(buffered).unwrap(),
        Some(Ok(Reply::PageCancelled(id))) if id == request
    ));
    assert!(matches!(
        call(&mut worker, Command::Acknowledge(previous.token)),
        Err(CommandError::Session(SessionError::StaleRender))
    ));
    assert!(matches!(
        call(&mut worker, Command::Save).unwrap(),
        Reply::Saved(SaveStatus::NoDisplayedPage)
    ));
    worker.shutdown().unwrap();
    assert!(!fixture.root.exists());
}

#[test]
fn cancelling_then_replacing_keeps_only_the_newest_request() {
    let fixture = Fixture::new();
    let mut worker = ReaderWorker::start().unwrap();
    call(&mut worker, fixture.open()).unwrap();
    let first = worker.request_page(0, 100, 100).unwrap();
    let second = worker.request_page(0, 200, 200).unwrap();
    assert_eq!(worker.cancel_page(), Some(second));
    let third = worker.request_page(0, 260, 360).unwrap();
    assert_ne!(first, third);
    assert_ne!(second, third);
    let page = page_finished(&mut worker, third);
    assert_eq!(page.width, 260);
    worker.shutdown().unwrap();
}

#[test]
fn latest_failure_is_correlated_and_does_not_advance_saved_position() {
    let fixture = Fixture::new();
    let mut worker = ReaderWorker::start().unwrap();
    call(&mut worker, fixture.open()).unwrap();
    let first = worker.request_page(0, 260, 360).unwrap();
    let shown = page_finished(&mut worker, first);
    call(&mut worker, Command::Acknowledge(shown.token)).unwrap();
    let bad = worker.request_page(1, 260, 360).unwrap();
    assert!(matches!(
        response(&mut worker).unwrap(),
        Reply::PageFinished {
            request,
            result: Err(CommandError::Session(SessionError::PageOutOfRange { .. })),
        } if request == bad
    ));
    assert!(matches!(
        call(&mut worker, Command::Save).unwrap(),
        Reply::Saved(SaveStatus::Saved(1))
    ));
    call(&mut worker, Command::Close).unwrap();
    assert!(matches!(
        call(&mut worker, fixture.open()).unwrap(),
        Reply::Opened {
            status: OpenStatus::Saved(Restore::Exact { page: 0, .. }),
            ..
        }
    ));
    worker.shutdown().unwrap();
}

#[test]
fn pending_scheduled_work_can_be_cancelled_and_shutdown_without_saving() {
    let fixture = Fixture::new();
    let mut worker = ReaderWorker::start().unwrap();
    call(&mut worker, fixture.open()).unwrap();
    worker.request_page(0, 100, 100).unwrap();
    let latest = worker.request_page(0, 260, 360).unwrap();
    assert_eq!(worker.cancel_page(), Some(latest));
    assert!(matches!(
        response(&mut worker).unwrap(),
        Reply::PageCancelled(id) if id == latest
    ));
    worker.request_page(0, 260, 360).unwrap();
    worker.request_page(0, 100, 100).unwrap();
    worker.shutdown().unwrap();
    assert!(!fixture.root.exists());
}

#[test]
fn rapid_navigation_across_pages_saves_only_the_displayed_page() {
    let files = TempDocumentDir::new("worker-multipage");
    let archive = stored_zip(&[
        ("001.pgm", b"P5\n2 2\n255\n\x00\xff\x80\x40"),
        ("002.pgm", b"P5\n2 2\n255\n\xff\xff\xff\xff"),
        ("003.pgm", b"P5\n2 2\n255\n\x00\x00\x00\x00"),
    ]);
    let fixture = Fixture {
        path: files.write("book.cbz", &archive),
        root: files.0.join("positions"),
        _files: files,
    };
    let mut worker = ReaderWorker::start().unwrap();
    assert!(matches!(
        call(&mut worker, fixture.open()).unwrap(),
        Reply::Opened { page_count: 3, .. }
    ));
    let first = worker.request_page(0, 260, 360).unwrap();
    let shown = page_finished(&mut worker, first);
    call(&mut worker, Command::Acknowledge(shown.token)).unwrap();

    worker.request_page(1, 260, 360).unwrap();
    let latest = worker.request_page(2, 260, 360).unwrap();
    let last = page_finished(&mut worker, latest);
    assert_eq!(last.page, 2);
    assert!(matches!(
        call(&mut worker, Command::Save).unwrap(),
        Reply::Saved(SaveStatus::Saved(1))
    ));
    call(&mut worker, Command::Acknowledge(last.token)).unwrap();
    assert!(matches!(
        call(&mut worker, Command::Save).unwrap(),
        Reply::Saved(SaveStatus::Saved(2))
    ));
    call(&mut worker, Command::Close).unwrap();
    assert!(matches!(
        call(&mut worker, fixture.open()).unwrap(),
        Reply::Opened {
            status: OpenStatus::Saved(Restore::Exact { page: 2, .. }),
            ..
        }
    ));
    worker.shutdown().unwrap();
}
