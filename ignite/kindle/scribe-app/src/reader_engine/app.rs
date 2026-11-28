use std::fs;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use super::coordinator::{CoordinatorError, Event, ReaderCoordinator, SavePolicy};
use super::session::PageLayout;
use super::worker::{DisplayToken, PageRequestId};

pub const DEFAULT_READER_ROOT: &str = "/mnt/us/ignite-reader";

#[derive(Debug)]
pub struct ReaderPaths {
    books: PathBuf,
    positions: PathBuf,
}

impl ReaderPaths {
    pub fn new(root: &Path, protected: &[&Path]) -> Result<Self, String> {
        let root = checked_directory(root, false)?;
        for path in protected {
            let path = checked_directory(path, true)?;
            if root.starts_with(&path) || path.starts_with(&root) {
                return Err("reader root overlaps protected application storage".into());
            }
        }
        let books = checked_directory(&root.join("books"), false)?;
        let positions = checked_directory(&root.join("positions"), true)?;
        Ok(Self { books, positions })
    }

    pub fn book(&self, name: &Path) -> Result<PathBuf, String> {
        let mut components = name.components();
        let Some(Component::Normal(leaf)) = components.next() else {
            return Err("reader book must be a single filename".into());
        };
        if components.next().is_some() || name.as_os_str() != leaf {
            return Err("reader book must be a single filename".into());
        }
        Ok(self.books.join(leaf))
    }

    pub fn positions(&self) -> &Path {
        &self.positions
    }
}

fn checked_directory(path: &Path, may_be_missing: bool) -> Result<PathBuf, String> {
    if !path.is_absolute() {
        return Err("reader and protected storage roots must be absolute".into());
    }
    let mut current = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir | Component::CurDir => {
                return Err("storage root contains a relative component".into());
            }
            _ => current.push(component.as_os_str()),
        }
        if matches!(component, Component::Prefix(_) | Component::RootDir) {
            continue;
        }
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_dir() => {}
            Err(error) if may_be_missing && error.kind() == std::io::ErrorKind::NotFound => {}
            Ok(_) => return Err(format!("unsafe storage directory: {}", current.display())),
            Err(error) => return Err(format!("storage directory {}: {error}", current.display())),
        }
    }
    Ok(current)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModeState {
    Closed,
    Opening,
    Active,
    Leaving,
    Faulted,
}

#[derive(Debug)]
pub enum ModeError {
    Path(String),
    Coordinator(CoordinatorError),
    InvalidState,
}

impl From<CoordinatorError> for ModeError {
    fn from(error: CoordinatorError) -> Self {
        Self::Coordinator(error)
    }
}

pub struct ReaderMode {
    paths: ReaderPaths,
    coordinator: ReaderCoordinator,
    state: ModeState,
}

impl ReaderMode {
    pub fn start(paths: ReaderPaths, policy: SavePolicy) -> std::io::Result<Self> {
        Ok(Self {
            paths,
            coordinator: ReaderCoordinator::start(policy)?,
            state: ModeState::Closed,
        })
    }

    pub fn state(&self) -> ModeState {
        self.state
    }

    pub fn enter(&mut self, name: &Path, layout: PageLayout) -> Result<(), ModeError> {
        if self.state != ModeState::Closed {
            return Err(ModeError::InvalidState);
        }
        let book = self.paths.book(name).map_err(ModeError::Path)?;
        self.coordinator
            .open(book, self.paths.positions().to_path_buf(), layout)?;
        self.state = ModeState::Opening;
        Ok(())
    }

    fn require_active(&self) -> Result<(), ModeError> {
        if self.state == ModeState::Active {
            Ok(())
        } else {
            Err(ModeError::InvalidState)
        }
    }

    pub fn request_page(
        &mut self,
        page: u32,
        width: u32,
        height: u32,
    ) -> Result<PageRequestId, ModeError> {
        self.require_active()?;
        Ok(self.coordinator.request_page(page, width, height)?)
    }

    pub fn presented(&mut self, token: DisplayToken) -> Result<(), ModeError> {
        self.require_active()?;
        Ok(self.coordinator.presented(token)?)
    }

    pub fn discard(&mut self, token: DisplayToken) -> Result<(), ModeError> {
        self.require_active()?;
        Ok(self.coordinator.discard(token)?)
    }

    pub fn leave(&mut self) -> Result<(), ModeError> {
        match self.state {
            ModeState::Closed | ModeState::Leaving => return Ok(()),
            ModeState::Faulted => return Err(ModeError::InvalidState),
            ModeState::Opening => {}
            ModeState::Active => self.coordinator.request_close()?,
        }
        self.state = ModeState::Leaving;
        Ok(())
    }

    pub fn poll(&mut self, now: Duration) -> Result<Option<Event>, ModeError> {
        let event = match self.coordinator.poll(now) {
            Ok(event) => event,
            Err(error) => {
                if error != CoordinatorError::ClockReversed {
                    self.state = ModeState::Faulted;
                }
                return Err(error.into());
            }
        };
        match &event {
            Some(Event::Opened { .. }) if self.state == ModeState::Leaving => {
                if let Err(error) = self.coordinator.request_close() {
                    self.state = ModeState::Faulted;
                    return Err(error.into());
                }
            }
            Some(Event::Opened { .. }) => self.state = ModeState::Active,
            Some(Event::OpenFailed(_) | Event::Closed) => self.state = ModeState::Closed,
            Some(Event::AcknowledgeFailed(_) | Event::CloseFailed(_)) => {
                self.state = ModeState::Faulted;
            }
            _ => {}
        }
        Ok(event)
    }

    pub fn shutdown(self) -> Result<std::thread::Result<()>, Box<Self>> {
        if self.state != ModeState::Closed {
            return Err(Box::new(self));
        }
        match self.coordinator.shutdown() {
            Ok(result) => Ok(result),
            Err(coordinator) => Err(Box::new(Self {
                paths: self.paths,
                coordinator: *coordinator,
                state: ModeState::Faulted,
            })),
        }
    }
}

#[cfg(unix)]
pub mod diagnostic;
pub mod display;

#[cfg(test)]
mod tests;
