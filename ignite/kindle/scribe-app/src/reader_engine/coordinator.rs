use std::path::PathBuf;
use std::time::Duration;

use super::session::{OpenStatus, PageLayout, SaveStatus};
use super::worker::{Command, CommandError, Reply, SubmitError};
use super::worker::{DisplayToken, PagePixels, PageRequestId, ReaderWorker};

#[derive(Clone, Copy, Debug)]
pub struct SavePolicy {
    pub debounce: Duration,
    pub retry: Duration,
}

impl Default for SavePolicy {
    fn default() -> Self {
        Self {
            debounce: Duration::from_millis(750),
            retry: Duration::from_secs(2),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CoordinatorError {
    Busy,
    NoDocument,
    StalePresentation,
    WorkerStopped,
    RequestIdExhausted,
    Protocol,
    Faulted,
    ClockReversed,
}

#[derive(Debug)]
pub enum Event {
    Opened {
        status: OpenStatus,
        page_count: u32,
        selected_page: Option<u32>,
    },
    OpenFailed(CommandError),
    PageReady {
        request: PageRequestId,
        pixels: PagePixels,
    },
    RenderFailed {
        request: PageRequestId,
        error: CommandError,
    },
    Cancelled(PageRequestId),
    Presented,
    AcknowledgeFailed(CommandError),
    SaveComplete(SaveStatus),
    SaveFailed(CommandError),
    Closed,
    CloseFailed(CommandError),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Closed,
    Opening,
    Idle,
    Rendering(PageRequestId),
    Offered(DisplayToken),
    Acknowledging,
    Saving,
    Closing,
    Faulted,
}

pub struct ReaderCoordinator {
    worker: ReaderWorker,
    phase: Phase,
    policy: SavePolicy,
    now: Duration,
    save_due: Option<Duration>,
    retrying: bool,
    closing: bool,
}

impl ReaderCoordinator {
    pub fn start(policy: SavePolicy) -> std::io::Result<Self> {
        Ok(Self {
            worker: ReaderWorker::start()?,
            phase: Phase::Closed,
            policy,
            now: Duration::ZERO,
            save_due: None,
            retrying: false,
            closing: false,
        })
    }

    fn submit_error(&mut self, error: SubmitError) -> CoordinatorError {
        match error {
            SubmitError::Busy => CoordinatorError::Busy,
            SubmitError::RequestIdExhausted => CoordinatorError::RequestIdExhausted,
            SubmitError::Stopped => {
                self.phase = Phase::Faulted;
                CoordinatorError::WorkerStopped
            }
        }
    }

    fn submit(&mut self, command: Command, phase: Phase) -> Result<(), CoordinatorError> {
        if let Err(error) = self.worker.try_submit(command) {
            return Err(self.submit_error(error));
        }
        self.phase = phase;
        Ok(())
    }

    fn ensure_healthy(&self) -> Result<(), CoordinatorError> {
        if self.phase == Phase::Faulted {
            return Err(CoordinatorError::Faulted);
        }
        Ok(())
    }

    pub fn open(
        &mut self,
        path: PathBuf,
        store_root: PathBuf,
        layout: PageLayout,
    ) -> Result<(), CoordinatorError> {
        self.ensure_healthy()?;
        if self.phase != Phase::Closed {
            return Err(CoordinatorError::Busy);
        }
        self.submit(
            Command::Open {
                path,
                store_root,
                layout,
            },
            Phase::Opening,
        )
    }

    pub fn request_page(
        &mut self,
        page: u32,
        width: u32,
        height: u32,
    ) -> Result<PageRequestId, CoordinatorError> {
        self.ensure_healthy()?;
        if self.phase == Phase::Closed {
            return Err(CoordinatorError::NoDocument);
        }
        if self.closing || !matches!(self.phase, Phase::Idle | Phase::Rendering(_)) {
            return Err(CoordinatorError::Busy);
        }
        if self.phase == Phase::Idle && self.save_is_due() {
            return Err(CoordinatorError::Busy);
        }
        let request = match self.worker.request_page(page, width, height) {
            Ok(request) => request,
            Err(error) => return Err(self.submit_error(error)),
        };
        self.phase = Phase::Rendering(request);
        Ok(request)
    }

    pub fn cancel_page(&mut self) -> Option<PageRequestId> {
        if matches!(self.phase, Phase::Rendering(_)) {
            self.worker.cancel_page()
        } else {
            None
        }
    }

    pub fn presented(&mut self, token: DisplayToken) -> Result<(), CoordinatorError> {
        self.ensure_healthy()?;
        if self.phase != Phase::Offered(token) {
            return Err(CoordinatorError::StalePresentation);
        }
        self.submit(Command::Acknowledge(token), Phase::Acknowledging)
    }

    pub fn discard(&mut self, token: DisplayToken) -> Result<(), CoordinatorError> {
        self.ensure_healthy()?;
        if self.phase != Phase::Offered(token) {
            return Err(CoordinatorError::StalePresentation);
        }
        self.phase = Phase::Idle;
        Ok(())
    }

    pub fn request_close(&mut self) -> Result<(), CoordinatorError> {
        self.ensure_healthy()?;
        match self.phase {
            Phase::Closed => return Err(CoordinatorError::NoDocument),
            Phase::Opening | Phase::Offered(_) => return Err(CoordinatorError::Busy),
            Phase::Rendering(_) => {
                self.worker.cancel_page();
            }
            _ => {}
        }
        self.closing = true;
        Ok(())
    }

    pub fn has_unsaved_position(&self) -> bool {
        self.save_due.is_some() || matches!(self.phase, Phase::Acknowledging | Phase::Faulted)
    }

    pub fn close_pending(&self) -> bool {
        self.closing
    }

    fn save_is_due(&self) -> bool {
        self.save_due
            .is_some_and(|due| self.now >= due || (self.closing && !self.retrying))
    }

    fn drive(&mut self) -> Result<(), CoordinatorError> {
        if self.phase != Phase::Idle {
            return Ok(());
        }
        if self.save_is_due() {
            self.submit(Command::Save, Phase::Saving)?;
        } else if self.closing && self.save_due.is_none() {
            self.submit(Command::Close, Phase::Closing)?;
        }
        Ok(())
    }

    pub fn poll(&mut self, now: Duration) -> Result<Option<Event>, CoordinatorError> {
        self.ensure_healthy()?;
        if now < self.now {
            return Err(CoordinatorError::ClockReversed);
        }
        self.now = now;
        let response = match self.worker.try_response() {
            Ok(response) => response,
            Err(_) => {
                self.phase = Phase::Faulted;
                return Err(CoordinatorError::WorkerStopped);
            }
        };
        let Some(response) = response else {
            self.drive()?;
            return Ok(None);
        };
        let event = match (self.phase, response) {
            (
                Phase::Opening,
                Ok(Reply::Opened {
                    status,
                    page_count,
                    selected_page,
                }),
            ) => {
                self.phase = Phase::Idle;
                Event::Opened {
                    status,
                    page_count,
                    selected_page,
                }
            }
            (Phase::Opening, Err(error)) => {
                self.phase = Phase::Closed;
                Event::OpenFailed(error)
            }
            (Phase::Rendering(expected), Ok(Reply::PageFinished { request, result }))
                if expected == request =>
            {
                match result {
                    Ok(pixels) if !self.closing => {
                        self.phase = Phase::Offered(pixels.token);
                        Event::PageReady { request, pixels }
                    }
                    Ok(_) => {
                        self.phase = Phase::Idle;
                        Event::Cancelled(request)
                    }
                    Err(error) => {
                        self.phase = Phase::Idle;
                        Event::RenderFailed { request, error }
                    }
                }
            }
            (Phase::Rendering(expected), Ok(Reply::PageCancelled(request)))
                if expected == request =>
            {
                self.phase = Phase::Idle;
                Event::Cancelled(request)
            }
            (Phase::Acknowledging, Ok(Reply::Acknowledged)) => {
                self.phase = Phase::Idle;
                if !self.retrying {
                    self.save_due = Some(self.now.saturating_add(self.policy.debounce));
                }
                Event::Presented
            }
            (Phase::Acknowledging, Err(error)) => {
                self.phase = Phase::Faulted;
                Event::AcknowledgeFailed(error)
            }
            (Phase::Saving, Ok(Reply::Saved(status))) => {
                self.phase = Phase::Idle;
                self.save_due = None;
                self.retrying = false;
                Event::SaveComplete(status)
            }
            (Phase::Saving, Err(error)) => {
                self.phase = Phase::Idle;
                self.save_due = Some(self.now.saturating_add(self.policy.retry));
                self.retrying = true;
                Event::SaveFailed(error)
            }
            (Phase::Closing, Ok(Reply::Closed)) => {
                self.phase = Phase::Closed;
                self.closing = false;
                Event::Closed
            }
            (Phase::Closing, Err(error)) => {
                self.phase = Phase::Faulted;
                Event::CloseFailed(error)
            }
            _ => {
                self.phase = Phase::Faulted;
                return Err(CoordinatorError::Protocol);
            }
        };
        Ok(Some(event))
    }

    pub fn is_closed(&self) -> bool {
        self.phase == Phase::Closed
    }

    pub fn shutdown(self) -> Result<std::thread::Result<()>, Box<Self>> {
        if !self.is_closed() {
            return Err(Box::new(self));
        }
        Ok(self.worker.shutdown())
    }

    pub fn abort(self) -> std::thread::Result<()> {
        self.worker.shutdown()
    }
}

#[cfg(test)]
mod tests;
