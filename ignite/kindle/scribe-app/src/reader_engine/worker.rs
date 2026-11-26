use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError};
use std::thread::{self, JoinHandle};

use super::location::PositionStore;
use super::session::{OpenStatus, PageLayout, ReaderSession, SaveStatus, SessionError};

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
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WorkerStopped;

pub type Response = Result<Reply, CommandError>;

pub struct ReaderWorker {
    commands: Option<SyncSender<Command>>,
    replies: Option<Receiver<Response>>,
    thread: Option<JoinHandle<()>>,
    pending: bool,
}

impl ReaderWorker {
    pub fn start() -> std::io::Result<Self> {
        let (commands, incoming) = mpsc::sync_channel(1);
        let (outgoing, replies) = mpsc::sync_channel(1);
        let thread = thread::Builder::new()
            .name("ignite-reader".into())
            .spawn(move || run(incoming, outgoing))?;
        Ok(Self {
            commands: Some(commands),
            replies: Some(replies),
            thread: Some(thread),
            pending: false,
        })
    }

    pub fn try_submit(&mut self, command: Command) -> Result<(), SubmitError> {
        if self.pending {
            return Err(SubmitError::Busy);
        }
        let sender = self.commands.as_ref().ok_or(SubmitError::Stopped)?;
        match sender.try_send(command) {
            Ok(()) => {
                self.pending = true;
                Ok(())
            }
            Err(TrySendError::Full(_)) => Err(SubmitError::Busy),
            Err(TrySendError::Disconnected(_)) => Err(SubmitError::Stopped),
        }
    }

    pub fn try_response(&mut self) -> Result<Option<Response>, WorkerStopped> {
        let receiver = self.replies.as_ref().ok_or(WorkerStopped)?;
        match receiver.try_recv() {
            Ok(response) => {
                self.pending = false;
                Ok(Some(response))
            }
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => {
                self.pending = false;
                Err(WorkerStopped)
            }
        }
    }

    pub fn shutdown(mut self) -> thread::Result<()> {
        self.stop()
    }

    fn stop(&mut self) -> thread::Result<()> {
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

fn run(commands: Receiver<Command>, replies: SyncSender<Response>) {
    let mut session = None;
    while let Ok(command) = commands.recv() {
        if replies.send(execute(&mut session, command)).is_err() {
            break;
        }
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
                    Ok(Reply::Rendered(PagePixels {
                        page: rendered.page,
                        token: DisplayToken {
                            session: *id,
                            render: rendered.token,
                        },
                        width: rendered.image.width,
                        height: rendered.image.height,
                        pixels: rendered.image.pixels().into(),
                    }))
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
