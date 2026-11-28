use super::*;
use crate::reader_engine::app::display::{self, Notice, ReaderScreen, Viewport};
use crate::reader_engine::app::{ReaderMode, ReaderPaths, DEFAULT_READER_ROOT};
use crate::reader_engine::coordinator::SavePolicy;
use crate::reader_engine::session::PageLayout;

pub(super) struct Launch {
    paths: ReaderPaths,
    book: PathBuf,
}

impl Launch {
    pub(super) fn parse(
        args: &[String],
        protected: &[&Path],
    ) -> Result<Option<Self>, String> {
        let mut book = None;
        let mut root = None;
        for (index, arg) in args.iter().enumerate() {
            let slot = match arg.as_str() {
                "--reader-display-book" => &mut book,
                "--reader-display-root" => &mut root,
                _ if arg.starts_with("--reader-display-") => {
                    return Err("unknown reader display option".into());
                }
                _ => continue,
            };
            let value = args
                .get(index + 1)
                .filter(|value| !value.is_empty() && !value.starts_with("--"))
                .ok_or("reader display option requires a value")?;
            if slot.replace(PathBuf::from(value)).is_some() {
                return Err("duplicate reader display option".into());
            }
        }
        let Some(book) = book else {
            return if root.is_some() {
                Err("reader display root requires a book".into())
            } else {
                Ok(None)
            };
        };
        if !args.iter().any(|arg| arg == "--confirm-write")
            || args.iter().any(|arg| arg == "--display-events-stdin")
        {
            return Err("reader display requires standalone --confirm-write".into());
        }
        let root = root.unwrap_or_else(|| PathBuf::from(DEFAULT_READER_ROOT));
        let paths = ReaderPaths::new(&root, protected)?;
        paths.book(&book)?;
        Ok(Some(Self { paths, book }))
    }
}

pub(super) struct Devices<'a> {
    pub ctrl: &'a mut KindleCtrl,
    pub input: &'a mut InputPump,
    pub touch: Option<&'a (TouchPump, TouchMapping)>,
    pub display: &'a mut DisplayEventReceiver,
    pub calibration: &'a DeviceCalibration,
    pub stop: &'a AtomicBool,
}

pub(super) fn run(launch: Launch, devices: Devices<'_>) -> Result<bool, String> {
    if devices.ctrl.is_armed() || !devices.ctrl.is_idle() {
        return Err("reader display requires idle disarmed ownership".into());
    }
    let geo = devices.ctrl.transport().geometry();
    let view = Viewport::new(&geo)?;
    let layout = PageLayout {
        width: 550.0,
        height: 720.0,
        em: 12.0,
    };
    let mut mode = ReaderMode::start(launch.paths, SavePolicy::default())
        .map_err(|error| error.to_string())?;
    mode.enter(&launch.book, layout)
        .map_err(|error| format!("reader enter: {error:?}"))?;
    let mut screen = ReaderScreen::new(mode, view);
    let start = Instant::now();
    let mut down = None;
    let mut visible = false;
    let mut invalidated = false;
    let mut failure = None;

    while !screen.is_closed() {
        let events = devices.display.take();
        invalidated |= events.invalidated || events.restored;
        if invalidated || devices.stop.load(Ordering::SeqCst) {
            screen
                .leave()
                .map_err(|error| format!("reader leave: {error:?}"))?;
            down = None;
        }

        if let Some((pump, _)) = devices.touch {
            for _ in 0..64 {
                if pump.try_next().is_none() {
                    break;
                }
            }
        }

        match devices.input.poll_event() {
            Ok(SourcePoll::Event(event)) => {
                let point = UiPoint::from_page(device_to_page(
                    devices.calibration,
                    event.device.x,
                    event.device.y,
                ));
                let action = point.and_then(|point| view.action(point.x, point.y));
                match event.kind {
                    StylusEventKind::Down if visible && event.tool == StylusTool::Pen => {
                        down = action;
                    }
                    StylusEventKind::Down => down = None,
                    StylusEventKind::Move if down != action => down = None,
                    StylusEventKind::Up => {
                        if let Some(action) = down.take().filter(|old| Some(*old) == action) {
                            if !invalidated && !devices.stop.load(Ordering::SeqCst) {
                                screen
                                    .action(action)
                                    .map_err(|error| format!("reader action: {error:?}"))?;
                            }
                        }
                    }
                    _ => {}
                }
            }
            Ok(SourcePoll::StreamGap { .. }) => down = None,
            Ok(SourcePoll::WouldBlock) => {}
            Ok(SourcePoll::EndOfStream) | Err(_) => {
                failure = Some("reader input stream ended".to_owned());
                screen
                    .leave()
                    .map_err(|error| format!("reader leave: {error:?}"))?;
            }
        }

        let notice = screen
            .tick(start.elapsed(), |pixels, total| {
                let before = devices.display.take();
                invalidated |= before.invalidated || before.restored;
                if invalidated || devices.stop.load(Ordering::SeqCst) {
                    return Err("display ownership lost before presentation".into());
                }
                let transport = devices.ctrl.transport_mut();
                display::compose(transport.fb.as_slice_mut(), &geo, pixels, total)?;
                let (marker, collision) = transport.refresh_full()?;
                let after = devices.display.take();
                invalidated |= after.invalidated || after.restored;
                if invalidated || devices.stop.load(Ordering::SeqCst) || collision != 0 {
                    return Err("reader refresh completion was not authoritative".into());
                }
                visible = true;
                println!("reader_presented page={} marker={marker}", pixels.page);
                Ok(())
            })
            .map_err(|error| format!("reader tick: {error:?}"))?;
        match notice {
            Some(Notice::OpenFailed(error)
            | Notice::RenderFailed(error)
            | Notice::DisplayFailed(error)) => failure = Some(error),
            Some(Notice::SaveFailed(error)) => {
                eprintln!("reader_save_retry detail={error}");
            }
            Some(Notice::SaveComplete(status)) => println!("reader_save status={status:?}"),
            Some(Notice::Closed) | None => {}
        }
        thread::sleep(Duration::from_millis(2));
    }
    screen.shutdown()?;
    if invalidated {
        return Err("reader display invalidated; restart required".into());
    }
    if let Some(error) = failure {
        return Err(error);
    }
    Ok(!devices.stop.load(Ordering::SeqCst))
}

pub(super) fn drain_after_reader(input: &mut InputPump) -> Result<(), String> {
    for _ in 0..1024 {
        match input.poll_event() {
            Ok(SourcePoll::Event(_) | SourcePoll::StreamGap { .. }) => {}
            Ok(SourcePoll::WouldBlock) => return Ok(()),
            Ok(SourcePoll::EndOfStream) | Err(_) => {
                return Err("reader handoff lost pen stream".into());
            }
        }
    }
    Err("reader handoff pen queue did not drain".into())
}

pub(super) fn drain_touch_after_reader(pump: &TouchPump) -> Result<(), String> {
    for _ in 0..1024 {
        if pump.try_next().is_none() {
            return Ok(());
        }
    }
    Err("reader handoff touch queue did not drain".into())
}
