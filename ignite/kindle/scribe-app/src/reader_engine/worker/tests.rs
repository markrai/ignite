use std::time::{Duration, Instant};

use super::*;
use crate::reader_engine::location::Restore;
use crate::reader_engine::tests::{rectangle_pdf, TempDocumentDir};

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
