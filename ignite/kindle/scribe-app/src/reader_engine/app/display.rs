use std::time::Duration;

use kindle_platform::FbGeometry;

use super::{ModeError, ModeState, ReaderMode};
use crate::reader_engine::coordinator::{CoordinatorError, Event};
use crate::reader_engine::session::SaveStatus;
use crate::reader_engine::worker::{PagePixels, PageRequestId};
use crate::ui_gfx::{Canvas, Rect, BLACK, WHITE};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Previous,
    Home,
    Next,
}

#[derive(Clone, Copy, Debug)]
pub struct Viewport {
    width: u32,
    height: u32,
    bar: u32,
}

impl Viewport {
    pub fn new(geo: &FbGeometry) -> Result<Self, String> {
        if geo.xres < 60 || geo.yres < 56 {
            return Err("reader viewport is too small".into());
        }
        Ok(Self {
            width: geo.xres,
            height: geo.yres,
            bar: (geo.yres / 8).min(100),
        })
    }

    pub fn render_box(self) -> (u32, u32) {
        (self.width, self.height - self.bar)
    }

    pub fn action(self, x: u32, y: u32) -> Option<Action> {
        if x >= self.width || y >= self.bar {
            return None;
        }
        if x < self.width / 3 {
            Some(Action::Previous)
        } else if x < (self.width / 3) * 2 {
            Some(Action::Home)
        } else {
            Some(Action::Next)
        }
    }
}

pub fn compose(
    fb: &mut [u8],
    geo: &FbGeometry,
    page: &PagePixels,
    total: u32,
) -> Result<(), String> {
    let view = Viewport::new(geo)?;
    let (width, height) = view.render_box();
    let expected = (page.width as usize)
        .checked_mul(page.height as usize)
        .ok_or("reader pixels overflow")?;
    if page.width == 0
        || page.height == 0
        || page.width > width
        || page.height > height
        || page.pixels.len() != expected
        || page.page >= total
    {
        return Err("invalid reader page pixels".into());
    }
    if geo.xoffset.checked_add(geo.xres).is_none()
        || geo.yoffset.checked_add(geo.yres).is_none()
    {
        return Err("reader framebuffer geometry overflow".into());
    }
    {
        let mut canvas = Canvas::new(fb, geo)?;
        canvas.fill_all(WHITE);
        let third = width / 3;
        let scale = (view.bar / 14).clamp(1, 6);
        canvas.text_centered(Rect::new(0, 0, third, view.bar), scale, "PREV", BLACK);
        let label = format!("HOME {}/{}", page.page + 1, total);
        canvas.text_centered(
            Rect::new(third, 0, third, view.bar),
            scale,
            &label,
            BLACK,
        );
        canvas.text_centered(
            Rect::new(third * 2, 0, width - third * 2, view.bar),
            scale,
            "NEXT",
            BLACK,
        );
    }
    let x = geo.xoffset + (width - page.width) / 2;
    let y = geo.yoffset + view.bar + (height - page.height) / 2;
    for (row, pixels) in page.pixels.chunks_exact(page.width as usize).enumerate() {
        let start = (y as usize + row) * geo.line_length as usize + x as usize;
        fb[start..start + pixels.len()].copy_from_slice(pixels);
    }
    Ok(())
}

#[derive(Debug)]
pub enum Notice {
    OpenFailed(String),
    RenderFailed(String),
    DisplayFailed(String),
    SaveFailed(String),
    SaveComplete(SaveStatus),
    Closed,
}

pub struct ReaderScreen {
    mode: ReaderMode,
    view: Viewport,
    total: u32,
    desired: Option<u32>,
    pending: Option<u32>,
    expected: Option<PageRequestId>,
    leaving: bool,
}

impl ReaderScreen {
    pub fn new(mode: ReaderMode, view: Viewport) -> Self {
        Self {
            mode,
            view,
            total: 0,
            desired: None,
            pending: None,
            expected: None,
            leaving: false,
        }
    }

    pub fn action(&mut self, action: Action) -> Result<(), ModeError> {
        if action == Action::Home {
            return self.leave();
        }
        if self.leaving || self.mode.state() != ModeState::Active {
            return Ok(());
        }
        let Some(current) = self.desired else {
            return Ok(());
        };
        let target = match action {
            Action::Previous => current.saturating_sub(1),
            Action::Next => current.saturating_add(1).min(self.total.saturating_sub(1)),
            Action::Home => unreachable!(),
        };
        if target != current {
            self.desired = Some(target);
            self.pending = Some(target);
        }
        Ok(())
    }

    pub fn leave(&mut self) -> Result<(), ModeError> {
        self.mode.leave()?;
        self.leaving = true;
        self.pending = None;
        Ok(())
    }

    pub fn is_closed(&self) -> bool {
        self.mode.state() == ModeState::Closed
    }

    fn dispatch(&mut self) -> Result<(), ModeError> {
        let Some(page) = self.pending else {
            return Ok(());
        };
        if self.leaving {
            return Ok(());
        }
        let (width, height) = self.view.render_box();
        match self.mode.request_page(page, width, height) {
            Ok(request) => {
                self.expected = Some(request);
                self.pending = None;
            }
            Err(ModeError::Coordinator(CoordinatorError::Busy)) => {}
            Err(error) => return Err(error),
        }
        Ok(())
    }

    pub fn tick(
        &mut self,
        now: Duration,
        mut present: impl FnMut(&PagePixels, u32) -> Result<(), String>,
    ) -> Result<Option<Notice>, ModeError> {
        self.dispatch()?;
        match self.mode.poll(now)? {
            Some(Event::Opened {
                selected_page,
                page_count,
                ..
            }) if !self.leaving => {
                self.total = page_count;
                self.desired = selected_page;
                self.pending = selected_page;
                if selected_page.is_none() {
                    self.leave()?;
                    return Ok(Some(Notice::OpenFailed("no safe resume page".into())));
                }
            }
            Some(Event::PageReady { request, pixels }) => {
                if self.leaving
                    || self.expected != Some(request)
                    || self.pending.is_some()
                    || self.desired != Some(pixels.page)
                {
                    self.mode.discard(pixels.token)?;
                } else if let Err(error) = present(&pixels, self.total) {
                    self.mode.discard(pixels.token)?;
                    self.leave()?;
                    return Ok(Some(Notice::DisplayFailed(error)));
                } else {
                    self.mode.presented(pixels.token)?;
                }
            }
            Some(Event::OpenFailed(error)) => {
                return Ok(Some(Notice::OpenFailed(format!("{error:?}"))));
            }
            Some(Event::RenderFailed { request, error }) if self.expected == Some(request) => {
                if self.pending.is_none() {
                    self.leave()?;
                    return Ok(Some(Notice::RenderFailed(format!("{error:?}"))));
                }
            }
            Some(Event::SaveFailed(error)) => {
                return Ok(Some(Notice::SaveFailed(format!("{error:?}"))));
            }
            Some(Event::SaveComplete(status)) => return Ok(Some(Notice::SaveComplete(status))),
            Some(Event::Closed) => return Ok(Some(Notice::Closed)),
            Some(Event::AcknowledgeFailed(_) | Event::CloseFailed(_)) => {
                return Err(ModeError::InvalidState);
            }
            _ => {}
        }
        self.dispatch()?;
        Ok(None)
    }

    pub fn shutdown(self) -> Result<(), String> {
        self.mode
            .shutdown()
            .map_err(|_| "reader screen did not close".to_owned())?
            .map_err(|_| "reader worker panicked".to_owned())
    }
}

#[cfg(test)]
mod tests;
