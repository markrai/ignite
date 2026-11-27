use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use super::location::PositionStore;
use super::session::{
    OpenStatus, PageLayout, ReaderSession, RenderedPage, SaveStatus, SessionError,
};

static NEXT_SESSION: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DisplayToken {
    session: u64,
    render: u64,
}

#[derive(Debug)]
pub struct PagePixels {
    pub page: u32,
    pub token: DisplayToken,
    pub width: u32,
    pub height: u32,
    pub pixels: Box<[u8]>,
}

#[derive(Debug)]
pub enum Command {
    Open {
        path: PathBuf,
        store_root: PathBuf,
        layout: PageLayout,
    },
    Navigate(u32),
    Render {
        width: u32,
        height: u32,
    },
    Acknowledge(DisplayToken),
    Save,
    Close,
}

#[derive(Debug)]
pub enum Reply {
    Opened {
        status: OpenStatus,
        page_count: u32,
        selected_page: Option<u32>,
    },
    Navigated,
    Rendered(PagePixels),
    Acknowledged,
    Saved(SaveStatus),
    Closed,
    PageFinished {
        request: PageRequestId,
        result: Result<PagePixels, CommandError>,
    },
    PageCancelled(PageRequestId),
}

#[derive(Debug)]
pub enum CommandError {
    AlreadyOpen,
    NoDocument,
    SessionIdExhausted,
    StaleSession,
    Session(SessionError),
}

impl From<SessionError> for CommandError {
    fn from(error: SessionError) -> Self {
        Self::Session(error)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubmitError {
    Busy,
    Stopped,
    RequestIdExhausted,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WorkerStopped;

pub type Response = Result<Reply, CommandError>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PageRequestId(u64);

#[derive(Clone, Copy)]
struct PageRequest {
    id: PageRequestId,
    page: u32,
    width: u32,
    height: u32,
}

enum Work {
    Control(Command),
    Page(PageRequest),
}

#[derive(Clone, Copy)]
enum Pending {
    Idle,
    Control,
    Page {
        running: PageRequestId,
        latest: PageRequest,
        cancelled: bool,
    },
}

pub struct ReaderWorker {
    commands: Option<SyncSender<Work>>,
    replies: Option<Receiver<Response>>,
    thread: Option<JoinHandle<()>>,
    pending: Pending,
    next_request: u64,
    wanted: Arc<AtomicU64>,
}

impl ReaderWorker {
    pub fn start() -> std::io::Result<Self> {
        let (commands, incoming) = mpsc::sync_channel(1);
        let (outgoing, replies) = mpsc::sync_channel(1);
        let wanted = Arc::new(AtomicU64::new(0));
        let worker_wanted = Arc::clone(&wanted);
        let thread = thread::Builder::new()
            .name("ignite-reader".into())
            .spawn(move || run(incoming, outgoing, &worker_wanted))?;
        Ok(Self {
            commands: Some(commands),
            replies: Some(replies),
            thread: Some(thread),
            pending: Pending::Idle,
            next_request: 0,
            wanted,
        })
    }

    fn send(&self, work: Work) -> Result<(), SubmitError> {
        let sender = self.commands.as_ref().ok_or(SubmitError::Stopped)?;
        match sender.try_send(work) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => Err(SubmitError::Busy),
            Err(TrySendError::Disconnected(_)) => Err(SubmitError::Stopped),
        }
    }

    pub fn try_submit(&mut self, command: Command) -> Result<(), SubmitError> {
        if !matches!(self.pending, Pending::Idle) {
            return Err(SubmitError::Busy);
        }
        self.send(Work::Control(command))?;
        self.pending = Pending::Control;
        Ok(())
    }

    pub fn request_page(
        &mut self,
        page: u32,
        width: u32,
        height: u32,
    ) -> Result<PageRequestId, SubmitError> {
        if matches!(self.pending, Pending::Control) {
            return Err(SubmitError::Busy);
        }
        let id = self
            .next_request
            .checked_add(1)
            .ok_or(SubmitError::RequestIdExhausted)?;
        let latest = PageRequest {
            id: PageRequestId(id),
            page,
            width,
            height,
        };
        self.next_request = id;
        self.wanted.store(id, Ordering::Release);
        let running = match self.pending {
            Pending::Page { running, .. } => running,
            Pending::Idle => {
                if let Err(error) = self.send(Work::Page(latest)) {
                    self.wanted.store(0, Ordering::Release);
                    return Err(error);
                }
                latest.id
            }
            Pending::Control => unreachable!(),
        };
        self.pending = Pending::Page {
            running,
            latest,
            cancelled: false,
        };
        Ok(latest.id)
    }

    pub fn cancel_page(&mut self) -> Option<PageRequestId> {
        let Pending::Page {
            running,
            latest,
            cancelled: false,
        } = self.pending
        else {
            return None;
        };
        self.wanted.store(0, Ordering::Release);
        self.pending = Pending::Page {
            running,
            latest,
            cancelled: true,
        };
        Some(latest.id)
    }

    pub fn try_response(&mut self) -> Result<Option<Response>, WorkerStopped> {
        let receiver = self.replies.as_ref().ok_or(WorkerStopped)?;
        let response = match receiver.try_recv() {
            Ok(response) => response,
            Err(TryRecvError::Empty) => return Ok(None),
            Err(TryRecvError::Disconnected) => {
                self.pending = Pending::Idle;
                self.wanted.store(0, Ordering::Release);
                return Err(WorkerStopped);
            }
        };
        self.handle_response(response)
    }

    fn handle_response(&mut self, response: Response) -> Result<Option<Response>, WorkerStopped> {
        let pending = std::mem::replace(&mut self.pending, Pending::Idle);
        match pending {
            Pending::Idle | Pending::Control => Ok(Some(response)),
            Pending::Page {
                running,
                latest,
                cancelled,
            } => {
                if cancelled {
                    self.wanted.store(0, Ordering::Release);
                    return Ok(Some(Ok(Reply::PageCancelled(latest.id))));
                }
                if running != latest.id {
                    drop(response);
                    if self.send(Work::Page(latest)).is_err() {
                        self.wanted.store(0, Ordering::Release);
                        return Err(WorkerStopped);
                    }
                    self.pending = Pending::Page {
                        running: latest.id,
                        latest,
                        cancelled: false,
                    };
                    return Ok(None);
                }
                self.wanted.store(0, Ordering::Release);
                Ok(Some(response))
            }
        }
    }

    pub fn shutdown(mut self) -> thread::Result<()> {
        self.stop()
    }

    fn stop(&mut self) -> thread::Result<()> {
        self.wanted.store(0, Ordering::Release);
        drop(self.commands.take());
        drop(self.replies.take());
        match self.thread.take() {
            Some(thread) => thread.join(),
            None => Ok(()),
        }
    }
}

impl Drop for ReaderWorker {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

fn run(commands: Receiver<Work>, replies: SyncSender<Response>, wanted: &AtomicU64) {
    let mut session = None;
    while let Ok(work) = commands.recv() {
        let response = match work {
            Work::Control(command) => execute(&mut session, command),
            Work::Page(request) => Ok(execute_page(&mut session, request, wanted)),
        };
        if replies.send(response).is_err() {
            break;
        }
    }
}

fn owned_pixels(id: u64, rendered: RenderedPage) -> PagePixels {
    PagePixels {
        page: rendered.page,
        token: DisplayToken {
            session: id,
            render: rendered.token,
        },
        width: rendered.image.width,
        height: rendered.image.height,
        pixels: rendered.image.pixels().into(),
    }
}

fn execute_page(
    active: &mut Option<(u64, ReaderSession)>,
    request: PageRequest,
    wanted: &AtomicU64,
) -> Reply {
    let current = || wanted.load(Ordering::Acquire) == request.id.0;
    let result = (|| -> Result<Option<PagePixels>, CommandError> {
        let (id, session) = active.as_mut().ok_or(CommandError::NoDocument)?;
        session.invalidate_render();
        if !current() {
            return Ok(None);
        }
        session.navigate_to(request.page)?;
        if !current() {
            return Ok(None);
        }
        let rendered = session.render_selected(request.width, request.height)?;
        if !current() {
            session.invalidate_render();
            return Ok(None);
        }
        Ok(Some(owned_pixels(*id, rendered)))
    })();
    if !current() {
        return Reply::PageCancelled(request.id);
    }
    match result {
        Ok(Some(pixels)) => Reply::PageFinished {
            request: request.id,
            result: Ok(pixels),
        },
        Ok(None) => Reply::PageCancelled(request.id),
        Err(error) => Reply::PageFinished {
            request: request.id,
            result: Err(error),
        },
    }
}

fn execute(active: &mut Option<(u64, ReaderSession)>, command: Command) -> Response {
    match command {
        Command::Open {
            path,
            store_root,
            layout,
        } => {
            if active.is_some() {
                return Err(CommandError::AlreadyOpen);
            }
            let session = ReaderSession::open(path, PositionStore::new(store_root), layout)?;
            let id = NEXT_SESSION
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
                .map_err(|_| CommandError::SessionIdExhausted)?;
            let reply = Reply::Opened {
                status: session.status().clone(),
                page_count: session.page_count(),
                selected_page: session.selected_page(),
            };
            *active = Some((id, session));
            Ok(reply)
        }
        Command::Close => {
            if active.take().is_none() {
                return Err(CommandError::NoDocument);
            }
            Ok(Reply::Closed)
        }
        command => {
            let (id, session) = active.as_mut().ok_or(CommandError::NoDocument)?;
            match command {
                Command::Navigate(page) => {
                    session.navigate_to(page)?;
                    Ok(Reply::Navigated)
                }
                Command::Render { width, height } => {
                    let rendered = session.render_selected(width, height)?;
                    Ok(Reply::Rendered(owned_pixels(*id, rendered)))
                }
                Command::Acknowledge(token) => {
                    if token.session != *id {
                        return Err(CommandError::StaleSession);
                    }
                    session.acknowledge_displayed(token.render)?;
                    Ok(Reply::Acknowledged)
                }
                Command::Save => Ok(Reply::Saved(session.save_displayed()?)),
                Command::Open { .. } | Command::Close => unreachable!(),
            }
        }
    }
}

#[cfg(test)]
mod tests;
