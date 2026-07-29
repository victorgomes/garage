//! Application state and the event loop (Phase 3: TODO 3.1–3.5).
//!
//! Two panes — sidebar and viewport — with a focus model instead of modes:
//! `h`/`l` (or arrows) move focus, movement keys act on the focused pane, and
//! every binding resolves through the remappable [`Keymap`] (TODO 3.5).
//!
//! The sidebar is a row list rebuilt on demand from the index: cheap (hundreds
//! of rows, not lines), and always in sync with a still-streaming source.
//! Selection *is* the view: moving the sidebar selection immediately points
//! the viewport at that section, like `less` following a file list. Enter
//! expands/collapses; it never "opens" anything, because everything is already
//! open — lazily (TODO 2.3): the parse of a compilation happens the first time
//! the viewport actually shows it.

use std::collections::HashSet;
use std::ops::Range;
use std::sync::mpsc::Receiver;

use anyhow::Result;
use crossterm::event::{Event as CtEvent, KeyEvent, KeyEventKind};
use regex::Regex;

use crate::config::{Action, Keymap};
use crate::event::Event;
use crate::index::TraceIndex;
use crate::model::Addr;
use crate::parse::ParseCache;
use crate::source::{LogBuffer, LogSource, SourceEvent};
use crate::terminal::TerminalGuard;

/// How many queued events one loop iteration absorbs before redrawing.
/// A fast producer can deliver chunks faster than the terminal repaints;
/// draining a bounded batch keeps it at one redraw per batch while never
/// starving keystrokes.
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
    pub index: TraceIndex,
    pub parses: ParseCache,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pane {
    Sidebar,
    Viewport,
}

/// One sidebar row. Rebuilt per frame from the index — identity is positional
/// within the *current* row list, which is why selection is clamped after
/// every rebuild rather than stored as an id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Row {
    /// Grouped mode: one header per function (keyed on SFI — names can be
    /// empty or duplicated, spike-findings.md §11).
    Function {
        sfi: Addr,
        name_comp: usize,
        count: usize,
    },
    Compilation(usize),
    Phase {
        comp: usize,
        phase: usize,
    },
    Raw(usize),
}

pub struct App {
    pub sources: Vec<Source>,
    pub active: usize,
    pub focus: Pane,
    /// Sidebar mode: chronological ⇄ grouped-by-function (TODO 3.3).
    pub grouped: bool,
    pub selected: usize,
    pub sidebar_scroll: usize,
    /// Expanded compilations, per (source, compilation).
    expanded: HashSet<(usize, usize)>,
    /// Expanded function groups, per (source, sfi).
    expanded_groups: HashSet<(usize, u64)>,
    /// Cursor: absolute buffer line, kept inside the viewed section.
    pub cursor: usize,
    /// First visible line of the viewport.
    pub top: usize,
    pub scroll_x: usize,
    pub wrap: bool,
    pub follow: bool,
    pub help: bool,
    pub quit: bool,
    pub status: String,
    pub viewport_height: usize,
    pub sidebar_height: usize,
    pub keys: Keymap,
    pub function_filter: Option<Regex>,
}

impl App {
    pub fn new(sources: &[LogSource], function_filter: Option<Regex>, keys: Keymap) -> Self {
        Self {
            sources: sources
                .iter()
                .map(|s| Source {
                    label: s.label(),
                    buffer: LogBuffer::new(),
                    state: SourceState::Loading,
                    index: TraceIndex::new(function_filter.clone()),
                    parses: ParseCache::default(),
                })
                .collect(),
            active: 0,
            focus: Pane::Sidebar,
            grouped: false,
            selected: 0,
            sidebar_scroll: 0,
            expanded: HashSet::new(),
            expanded_groups: HashSet::new(),
            cursor: 0,
            top: 0,
            scroll_x: 0,
            wrap: false,
            // Following by default only makes sense for a live stream; files
            // open at the top.
            follow: matches!(sources.first(), Some(LogSource::Stdin)),
            help: false,
            quit: false,
            status: "? help".to_string(),
            viewport_height: 1,
            sidebar_height: 1,
            keys,
            function_filter,
        }
    }

    pub fn active_source(&self) -> &Source {
        &self.sources[self.active]
    }

    pub fn compilation_expanded(&self, comp: usize) -> bool {
        self.expanded.contains(&(self.active, comp))
    }

    pub fn group_expanded(&self, sfi: u64) -> bool {
        self.expanded_groups.contains(&(self.active, sfi))
    }

    /// The sidebar rows for the active source, in display order.
    pub fn rows(&self) -> Vec<Row> {
        let source = &self.sources[self.active];
        let idx = &source.index;
        let mut rows = Vec::new();

        if self.grouped {
            // First-seen order of SFIs; compilations nested under each,
            // collapsed by default — this is what scales to thousands of
            // compilations (PLAN §5.1).
            let mut seen: Vec<Addr> = Vec::new();
            for c in idx.compilations.iter() {
                if !c.filtered_out && !seen.contains(&c.key.sfi) {
                    seen.push(c.key.sfi);
                }
            }
            for sfi in seen {
                let members: Vec<usize> = idx
                    .compilations
                    .iter()
                    .enumerate()
                    .filter(|(_, c)| !c.filtered_out && c.key.sfi == sfi)
                    .map(|(i, _)| i)
                    .collect();
                rows.push(Row::Function {
                    sfi,
                    name_comp: members[0],
                    count: members.len(),
                });
                if self.expanded_groups.contains(&(self.active, sfi.0)) {
                    for i in members {
                        self.push_compilation_rows(&mut rows, i);
                    }
                }
            }
            for (i, _) in idx.raw.iter().enumerate() {
                rows.push(Row::Raw(i));
            }
        } else {
            // Chronological: compilations and raw sections merged by start
            // line, which is simply their interleaved order in the stream.
            let mut comps = idx.compilations.iter().enumerate().peekable();
            let mut raws = idx.raw.iter().enumerate().peekable();
            loop {
                let next_comp = comps.peek().map(|(_, c)| c.lines.start);
                let next_raw = raws.peek().map(|(_, r)| r.lines.start);
                match (next_comp, next_raw) {
                    (Some(c), Some(r)) if c <= r => {
                        let (i, c) = comps.next().unwrap();
                        if !c.filtered_out {
                            self.push_compilation_rows(&mut rows, i);
                        }
                    }
                    (Some(_), Some(_)) | (None, Some(_)) => {
                        let (i, _) = raws.next().unwrap();
                        rows.push(Row::Raw(i));
                    }
                    (Some(_), None) => {
                        let (i, c) = comps.next().unwrap();
                        if !c.filtered_out {
                            self.push_compilation_rows(&mut rows, i);
                        }
                    }
                    (None, None) => break,
                }
            }
        }
        rows
    }

    fn push_compilation_rows(&self, rows: &mut Vec<Row>, comp: usize) {
        rows.push(Row::Compilation(comp));
        if self.expanded.contains(&(self.active, comp)) {
            let n = self.sources[self.active].index.compilations[comp]
                .phases
                .len();
            for phase in 0..n {
                rows.push(Row::Phase { comp, phase });
            }
        }
    }

    /// The buffer line range the viewport shows: the selected row's section,
    /// or the whole buffer before anything is indexed.
    pub fn view_range(&self) -> Range<usize> {
        let source = &self.sources[self.active];
        let rows = self.rows();

        match rows.get(self.selected) {
            Some(Row::Compilation(i)) => source.index.compilations[*i].lines.clone(),
            Some(Row::Phase { comp, phase }) => source.index.compilations[*comp].phases[*phase]
                .lines
                .clone(),
            Some(Row::Raw(i)) => source.index.raw[*i].lines.clone(),
            Some(Row::Function { name_comp, .. }) => {
                source.index.compilations[*name_comp].lines.clone()
            }
            None => 0..source.buffer.line_count(),
        }
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
        // Windows reports press and release; without this every key acts twice.
        if key.kind == KeyEventKind::Release {
            return;
        }

        let Some(action) = self.keys.lookup(&key) else {
            return;
        };

        if self.help {
            // Any key closes the modal — including `q`, which would otherwise
            // quit the whole app from inside a help screen.
            self.help = false;
            return;
        }

        match action {
            Action::Quit => self.quit = true,
            Action::Back => {
                // Esc backs out of viewport focus, then quits.
                if self.focus == Pane::Viewport {
                    self.focus = Pane::Sidebar;
                } else {
                    self.quit = true;
                }
            }
            Action::Help => self.help = true,
            Action::FocusSidebar => self.focus = Pane::Sidebar,
            Action::FocusViewport => self.focus = Pane::Viewport,
            Action::NextSource if self.sources.len() > 1 => {
                self.active = (self.active + 1) % self.sources.len();
                self.selected = 0;
                self.sidebar_scroll = 0;
                self.reset_view();
            }
            Action::Select => self.toggle_expand(),
            Action::ToggleGrouping => {
                self.grouped = !self.grouped;
                self.selected = 0;
                self.sidebar_scroll = 0;
                self.status = format!(
                    "sidebar: {}",
                    if self.grouped {
                        "grouped by function"
                    } else {
                        "chronological"
                    }
                );
            }
            Action::ToggleWrap => {
                self.wrap = !self.wrap;
                self.scroll_x = 0;
                self.status = format!("wrap {}", if self.wrap { "on" } else { "off" });
            }
            Action::ToggleFollow => {
                self.follow = !self.follow;
                if self.follow {
                    self.jump_bottom();
                }
                self.status = format!("follow {}", if self.follow { "on" } else { "off" });
            }
            Action::ScrollLeft => self.scroll_x = self.scroll_x.saturating_sub(8),
            Action::ScrollRight => self.scroll_x += 8,

            Action::Up => self.move_by(-1),
            Action::Down => self.move_by(1),
            Action::HalfPageDown => self.move_by(self.half_page()),
            Action::HalfPageUp => self.move_by(-self.half_page()),
            Action::PageDown => self.move_by(self.page()),
            Action::PageUp => self.move_by(-self.page()),
            Action::Top => self.jump_top(),
            Action::Bottom => self.jump_bottom(),

            // Phase 4 actions; bound but inert until then.
            _ => {}
        }
    }

    fn page(&self) -> isize {
        match self.focus {
            Pane::Sidebar => self.sidebar_height.saturating_sub(1).max(1) as isize,
            Pane::Viewport => self.viewport_height.saturating_sub(2).max(1) as isize,
        }
    }

    fn half_page(&self) -> isize {
        (self.page() / 2).max(1)
    }

    fn move_by(&mut self, delta: isize) {
        match self.focus {
            Pane::Sidebar => {
                let rows = self.rows().len();
                if rows == 0 {
                    return;
                }
                let target = self.selected as isize + delta;
                self.selected = target.clamp(0, rows as isize - 1) as usize;
                self.reset_view();
            }
            Pane::Viewport => {
                let range = self.view_range();
                if range.is_empty() {
                    return;
                }
                let target = self.cursor as isize + delta;
                self.cursor = target.clamp(range.start as isize, range.end as isize - 1) as usize;
                self.follow = self.follow && self.cursor + 1 == range.end;
            }
        }
    }

    fn jump_top(&mut self) {
        match self.focus {
            Pane::Sidebar => {
                self.selected = 0;
                self.reset_view();
            }
            Pane::Viewport => {
                self.cursor = self.view_range().start;
                self.follow = false;
            }
        }
    }

    fn jump_bottom(&mut self) {
        match self.focus {
            Pane::Sidebar => {
                self.selected = self.rows().len().saturating_sub(1);
                self.reset_view();
            }
            Pane::Viewport => {
                let range = self.view_range();
                self.cursor = range.end.saturating_sub(1).max(range.start);
            }
        }
    }

    fn toggle_expand(&mut self) {
        let rows = self.rows();
        match rows.get(self.selected) {
            Some(Row::Compilation(i)) => {
                let key = (self.active, *i);
                if !self.expanded.remove(&key) {
                    self.expanded.insert(key);
                }
            }
            Some(Row::Function { sfi, .. }) => {
                let key = (self.active, sfi.0);
                if !self.expanded_groups.remove(&key) {
                    self.expanded_groups.insert(key);
                }
            }
            Some(Row::Phase { .. } | Row::Raw(_)) => self.focus = Pane::Viewport,
            None => {}
        }
    }

    /// Selection moved: pin the cursor into the new section. Deliberate
    /// sidebar navigation also breaks follow — otherwise the next chunk from a
    /// live stream yanks the selection straight back to the newest section.
    fn reset_view(&mut self) {
        let range = self.view_range();
        self.cursor = range.start;
        self.scroll_x = 0;
        self.follow = false;
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
            SourceEvent::Mapped { map, .. } => {
                target.buffer.adopt_map(map);
                target.index.ingest(&target.buffer, false);
            }
            SourceEvent::Chunk { bytes, .. } => {
                target.buffer.append(&bytes);
                target.index.ingest(&target.buffer, false);
            }
            SourceEvent::Eof { .. } => {
                target.buffer.finish();
                target.index.ingest(&target.buffer, true);
                target.state = SourceState::Complete;
            }
            SourceEvent::Failed { error, .. } => {
                // `SourceError` already names the path or "stdin"; prefixing
                // the label here would print it twice.
                target.state = SourceState::Failed(error.clone());
                self.status = error;
            }
        }

        if index == self.active {
            let rows = self.rows().len();
            self.selected = self.selected.min(rows.saturating_sub(1));
            if self.follow {
                // Streaming: stick to the newest section's end.
                self.selected = rows.saturating_sub(1);
                let range = self.view_range();
                self.cursor = range.end.saturating_sub(1).max(range.start);
            }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyModifiers};

    fn app_with(trace: &str) -> App {
        let sources = vec![LogSource::Stdin];
        let keys = Keymap::build(&std::collections::HashMap::new()).unwrap();
        let mut app = App::new(&sources, None, keys);
        app.handle(Event::Source(SourceEvent::Chunk {
            source: 0,
            bytes: trace.as_bytes().to_vec(),
        }));
        app.handle(Event::Source(SourceEvent::Eof { source: 0 }));
        app
    }

    fn key(app: &mut App, code: KeyCode) {
        app.handle(Event::Input(CtEvent::Key(KeyEvent::new(
            code,
            KeyModifiers::NONE,
        ))));
    }

    const TRACE: &str = "\
warmup line
Compiling 0x1 <JSFunction f (sfi = 0x10)> with Maglev
----- Maglev graph building -----
   1: Foo
----- Register allocation -----
   1/1: Foo
Compiling 0x2 <JSFunction f (sfi = 0x10)> with Maglev
----- Maglev graph building -----
   2: Bar
";

    #[test]
    fn rows_interleave_raw_and_compilations_chronologically() {
        let app = app_with(TRACE);
        let rows = app.rows();
        assert_eq!(
            rows,
            vec![Row::Raw(0), Row::Compilation(0), Row::Compilation(1)]
        );
    }

    #[test]
    fn enter_expands_phases_and_selection_views_them() {
        let mut app = app_with(TRACE);
        app.follow = false;
        app.selected = 1;
        key(&mut app, KeyCode::Enter);
        let rows = app.rows();
        assert_eq!(rows.len(), 5, "two phase rows appeared");
        assert_eq!(rows[2], Row::Phase { comp: 0, phase: 0 });

        app.selected = 2;
        let range = app.view_range();
        assert_eq!(range, 2..4, "phase view is the phase's line range");
    }

    #[test]
    fn grouping_creates_function_headers() {
        let mut app = app_with(TRACE);
        app.follow = false;
        key(&mut app, KeyCode::Char('c'));
        let rows = app.rows();
        // One function (same SFI twice) collapsed + one raw section.
        assert_eq!(rows.len(), 2);
        assert!(matches!(rows[0], Row::Function { count: 2, .. }));
        key(&mut app, KeyCode::Enter);
        assert_eq!(app.rows().len(), 4, "group expanded");
    }

    #[test]
    fn viewport_cursor_stays_inside_the_section() {
        let mut app = app_with(TRACE);
        app.follow = false;
        app.selected = 1; // first compilation, lines 1..6
        app.reset_view();
        app.focus = Pane::Viewport;
        for _ in 0..20 {
            key(&mut app, KeyCode::Char('j'));
        }
        assert_eq!(app.cursor, 5, "clamped to section end");
        key(&mut app, KeyCode::Char('g'));
        assert_eq!(app.cursor, 1);
    }

    #[test]
    fn follow_sticks_to_the_stream_end() {
        let app = app_with(TRACE);
        assert!(app.follow, "stdin defaults to follow");
        let rows = app.rows();
        assert_eq!(app.selected, rows.len() - 1);
        let range = app.view_range();
        assert_eq!(app.cursor, range.end - 1);
    }

    #[test]
    fn help_modal_opens_and_any_key_closes() {
        let mut app = app_with(TRACE);
        key(&mut app, KeyCode::Char('?'));
        assert!(app.help);
        key(&mut app, KeyCode::Char('j'));
        assert!(!app.help);
        assert!(!app.quit);
    }

    #[test]
    fn esc_backs_out_of_viewport_then_quits() {
        let mut app = app_with(TRACE);
        app.focus = Pane::Viewport;
        key(&mut app, KeyCode::Esc);
        assert_eq!(app.focus, Pane::Sidebar);
        assert!(!app.quit);
        key(&mut app, KeyCode::Esc);
        assert!(app.quit);
    }
}
