//! Application state and the event loop.
//!
//! Phase 1 scope only: get bytes on screen and prove the infrastructure works
//! end to end — event-driven redraw, streaming input, keys from `/dev/tty`,
//! clean teardown. The real layout (sidebar, telemetry bar, viewport) is
//! Phase 3, and the raw viewport here is scaffolding it will replace.

use std::sync::mpsc::Receiver;

use anyhow::Result;
use crossterm::event::{Event as CtEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use regex::Regex;

use crate::event::Event;
use crate::source::{LogBuffer, LogSource, SourceEvent};
use crate::terminal::TerminalGuard;

/// How many queued events one loop iteration will absorb before redrawing.
///
/// A fast producer (`d8` writing hundreds of MB through a pipe) can deliver
/// chunks faster than the terminal can be repainted. Draining a bounded batch
/// keeps the UI at one redraw per batch instead of one per chunk, while the
/// bound stops a flooding source from starving keystrokes.
const MAX_EVENTS_PER_DRAW: usize = 512;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceState {
    Loading,
    Complete,
    Failed(String),
}

pub struct Source {
    pub label: String,
    pub buffer: LogBuffer,
    pub state: SourceState,
}

pub struct App {
    pub sources: Vec<Source>,
    pub active: usize,
    /// Index of the first visible line.
    pub top: usize,
    /// Stick to the end of the trace as new bytes arrive.
    pub follow: bool,
    pub quit: bool,
    pub status: String,
    /// Recorded at draw time so paging knows how far a page is.
    pub viewport_height: usize,
    /// Recorded, not yet applied: the prefilter runs in the indexer (Phase 2).
    pub function_filter: Option<Regex>,
}

impl App {
    pub fn new(sources: &[LogSource], function_filter: Option<Regex>) -> Self {
        let status = match &function_filter {
            Some(re) => format!(
                "--function {} recorded; it filters once the indexer lands",
                re.as_str()
            ),
            None => "? help (Phase 3)   q quit".to_string(),
        };

        Self {
            sources: sources
                .iter()
                .map(|s| Source {
                    label: s.label(),
                    buffer: LogBuffer::new(),
                    state: SourceState::Loading,
                })
                .collect(),
            active: 0,
            top: 0,
            // Following by default only makes sense for a live stream, and a
            // stream is exactly the case where the interesting output is the
            // most recent. Files open at the top.
            follow: matches!(sources.first(), Some(LogSource::Stdin)),
            quit: false,
            status,
            viewport_height: 1,
            function_filter,
        }
    }

    pub fn active_source(&self) -> &Source {
        &self.sources[self.active]
    }

    pub fn line_count(&self) -> usize {
        self.active_source().buffer.line_count()
    }

    /// Largest `top` that still fills the viewport.
    fn max_top(&self) -> usize {
        self.line_count().saturating_sub(self.viewport_height)
    }

    fn scroll_down(&mut self, n: usize) {
        self.top = (self.top + n).min(self.max_top());
        self.follow = self.top == self.max_top() && self.follow;
    }

    fn scroll_up(&mut self, n: usize) {
        self.top = self.top.saturating_sub(n);
        self.follow = false;
    }

    fn jump_to_top(&mut self) {
        self.top = 0;
        self.follow = false;
    }

    fn jump_to_bottom(&mut self) {
        self.top = self.max_top();
        self.follow = true;
    }

    pub fn handle(&mut self, event: Event) {
        match event {
            Event::Input(CtEvent::Key(key)) => self.handle_key(key),
            // Resize needs no state change; the redraw that follows is enough.
            Event::Input(_) => {}
            Event::Source(e) => self.handle_source(e),
            Event::InputClosed => {
                tracing::warn!("terminal input closed; quitting");
                self.quit = true;
            }
        }
    }

    fn handle_key(&mut self, key: KeyEvent) {
        // Windows reports both press and release; without this every key acts
        // twice there.
        if key.kind == KeyEventKind::Release {
            return;
        }

        let page = self.viewport_height.saturating_sub(2).max(1);
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

        match key.code {
            KeyCode::Char('c') if ctrl => self.quit = true,
            KeyCode::Char('q') | KeyCode::Esc => self.quit = true,

            KeyCode::Char('j') | KeyCode::Down => self.scroll_down(1),
            KeyCode::Char('k') | KeyCode::Up => self.scroll_up(1),
            KeyCode::Char('d') if ctrl => self.scroll_down(page / 2),
            KeyCode::Char('u') if ctrl => self.scroll_up(page / 2),
            KeyCode::PageDown | KeyCode::Char(' ') => self.scroll_down(page),
            KeyCode::PageUp => self.scroll_up(page),
            KeyCode::Home | KeyCode::Char('g') => self.jump_to_top(),
            KeyCode::End | KeyCode::Char('G') => self.jump_to_bottom(),

            KeyCode::Char('F') => {
                self.follow = !self.follow;
                if self.follow {
                    self.top = self.max_top();
                }
                self.status = format!("follow {}", if self.follow { "on" } else { "off" });
            }

            KeyCode::Tab if self.sources.len() > 1 => {
                self.active = (self.active + 1) % self.sources.len();
                self.top = 0;
            }

            _ => {}
        }
    }

    fn handle_source(&mut self, event: SourceEvent) {
        let index = match &event {
            SourceEvent::Mapped { source, .. }
            | SourceEvent::Chunk { source, .. }
            | SourceEvent::Eof { source }
            | SourceEvent::Failed { source, .. } => *source,
        };

        let Some(target) = self.sources.get_mut(index) else {
            tracing::error!(index, "event for an unknown source");
            return;
        };

        match event {
            SourceEvent::Mapped { map, .. } => target.buffer.adopt_map(map),
            SourceEvent::Chunk { bytes, .. } => target.buffer.append(&bytes),
            SourceEvent::Eof { .. } => {
                target.buffer.finish();
                target.state = SourceState::Complete;
            }
            SourceEvent::Failed { error, .. } => {
                // `SourceError` already names the path or "stdin"; prefixing
                // the label here would print it twice.
                target.state = SourceState::Failed(error.clone());
                self.status = error;
            }
        }

        if self.follow && index == self.active {
            self.top = self.max_top();
        }
    }
}

/// Blocks on the event channel, updates, redraws. One redraw per batch of
/// events, and no CPU at all while nothing is happening.
/// The caller must keep its own `Sender` alive for the duration, otherwise
/// `recv` starts failing as soon as the last reader finishes.
pub fn run(guard: &mut TerminalGuard, app: &mut App, rx: Receiver<Event>) -> Result<()> {
    guard.tui().draw(|frame| crate::ui::render(frame, app))?;

    while !app.quit {
        // Blocking: a `RecvError` means every sender is gone, which cannot
        // normally happen because the caller holds one, so treat it as quit.
        let Ok(first) = rx.recv() else { break };
        app.handle(first);

        let mut drained = 0;
        while drained < MAX_EVENTS_PER_DRAW {
            match rx.try_recv() {
                Ok(next) => {
                    app.handle(next);
                    drained += 1;
                }
                Err(_) => break,
            }
        }

        if app.quit {
            break;
        }
        guard.tui().draw(|frame| crate::ui::render(frame, app))?;
    }

    Ok(())
}
