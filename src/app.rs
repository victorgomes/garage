//! Application state and the event loop (Phases 3–4).
//!
//! Two panes — sidebar and viewport — with a focus model instead of modes:
//! `h`/`l` move focus, movement keys act on the focused pane, and every
//! binding resolves through the remappable [`Keymap`] (TODO 3.5). The one real
//! mode is the input line (`/` search, `f` filter), which captures keys until
//! Enter or Esc.
//!
//! The viewport cursor is a **display row**, not a buffer line: folding (4.2)
//! and annotation collapsing (4.8) mean the visible rows are not a contiguous
//! line range. [`App::view_model`] builds the row list for the selected
//! section — and, for compilations, triggers the lazy parse (TODO 2.3) whose
//! results drive styling, def-use highlighting, and node jumps.

use std::collections::HashSet;
use std::sync::mpsc::Receiver;

use anyhow::Result;
use crossterm::event::{
    Event as CtEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use regex::Regex;

use crate::config::{Action, Keymap};
use crate::event::Event;
use crate::index::TraceIndex;
use crate::model::{Addr, EventKind, IRNode, LineInfo, NodeId, PhaseKind, SCHEDULE_ONLY};
use crate::parse::ParseCache;
use crate::parse::maglev::line_text;
use crate::source::{LogBuffer, LogSource, SourceEvent};
use crate::terminal::TerminalGuard;
use crate::view::{FoldKey, Lens, MODEL_LIMIT, RowKind, ViewModel, model_rows};

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

/// Screen-space rectangle of a pane, recorded at render time — the frame is
/// the only thing that knows the layout, and mouse events arrive in screen
/// coordinates.
#[derive(Debug, Clone, Copy, Default)]
pub struct PaneRect {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
}

impl PaneRect {
    pub fn contains(&self, x: u16, y: u16) -> bool {
        x >= self.x && x < self.x + self.width && y >= self.y && y < self.y + self.height
    }
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
    /// Timeline mode: one row per [`TimelineEvent`] (TODO 6.3).
    Event(usize),
}

/// What the input line is collecting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Prompt {
    Search,
    Filter,
    /// `E`: filename to export the current view to (TODO 5.3).
    Export,
    /// `:` — the command palette (TODO 6.1).
    Command,
}

/// The palette's command table, in completion order. The second column is the
/// help/candidate hint. Kept as data so completion, dispatch, and the help
/// modal can never drift apart.
pub const COMMANDS: &[(&str, &str)] = &[
    ("checks", "lens: guard nodes, with a count"),
    ("clear", "clear the lens and timeline filter"),
    ("copy", "copy the visible section"),
    ("deopts", "timeline, deopt events only"),
    ("export", "export the view to <file>"),
    ("function", "filter the sidebar to <regex>"),
    ("megamorphic", "lens: megamorphic feedback and ICs"),
    ("phi", "lens: control/phi backbone"),
    ("spill", "lens: regalloc spills and reloads"),
    ("timeline", "toggle the timeline view"),
];

#[derive(Debug)]
pub struct InputLine {
    pub prompt: Prompt,
    pub buffer: String,
}

/// `u`/`i` cycling state: anchored to the node the cycle started from, so
/// jumping to a consumer does not re-anchor the cycle on the consumer.
#[derive(Debug, Clone)]
struct Cycle {
    node: NodeId,
    targets: Vec<usize>, // buffer lines
    at: usize,
}

/// One jump-history entry. Stores the sidebar *mode* alongside the position:
/// a deopt→graph jump leaves the timeline, and Ctrl+O must land back on the
/// event row, not interpret a timeline row index against the compilation list.
#[derive(Debug, Clone, Copy)]
struct Jump {
    timeline: bool,
    selected: usize,
    line: usize,
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
    /// Viewport cursor: a display-row index into the current view model.
    pub cursor: usize,
    /// First visible display row.
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

    // Phase 4 state.
    /// Folded basic blocks; persisted across navigation (TODO 4.2).
    folded_blocks: HashSet<FoldKey>,
    /// `t`: show trace annotations inline instead of folded (TODO 4.8).
    pub show_annotations: bool,
    /// The committed search pattern (TODO 4.6).
    pub search: Option<Regex>,
    /// Sidebar quick filter (TODO 4.7).
    pub sidebar_filter: Option<Regex>,
    /// Active input line, if any.
    pub input: Option<InputLine>,
    /// Jump history for Ctrl+O / Ctrl+I.
    jumps: Vec<Jump>,
    jump_at: usize,
    cycle: Option<Cycle>,

    // Phase 6 state.
    /// Timeline mode: the sidebar lists events instead of sections (TODO 6.3).
    pub timeline: bool,
    /// The other mode's selection, restored when toggling back.
    timeline_selected: usize,
    /// `:deopts`: timeline narrowed to deopt events.
    pub timeline_deopts_only: bool,
    /// Active semantic lens (TODO 6.2).
    pub lens: Option<Lens>,

    /// Pane geometry from the last frame, for mouse routing.
    pub sidebar_rect: PaneRect,
    pub viewport_rect: PaneRect,
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
            folded_blocks: HashSet::new(),
            show_annotations: false,
            search: None,
            sidebar_filter: None,
            input: None,
            jumps: Vec::new(),
            jump_at: 0,
            cycle: None,
            timeline: false,
            timeline_selected: 0,
            timeline_deopts_only: false,
            lens: None,
            sidebar_rect: PaneRect::default(),
            viewport_rect: PaneRect::default(),
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

    fn filter_matches(&self, text: &str) -> bool {
        match &self.sidebar_filter {
            Some(re) => re.is_match(text),
            None => true,
        }
    }

    /// The sidebar rows for the active source, in display order.
    pub fn rows(&self) -> Vec<Row> {
        let source = &self.sources[self.active];
        let idx = &source.index;
        let mut rows = Vec::new();

        if self.timeline {
            // Timeline mode (TODO 6.3): one row per event, ordinal order —
            // which is stream order, because the indexer records them in one
            // pass.
            for (i, e) in idx.events.iter().enumerate() {
                if self.timeline_deopts_only && !matches!(e.kind, EventKind::DeoptBegin { .. }) {
                    continue;
                }
                rows.push(Row::Event(i));
            }
            return rows;
        }

        let comp_visible = |i: usize| {
            let c = &idx.compilations[i];
            !c.filtered_out && self.filter_matches(c.display_name())
        };

        if self.grouped {
            // First-seen order of SFIs; compilations nested under each,
            // collapsed by default — this is what scales to thousands of
            // compilations (PLAN §5.1).
            let mut seen: Vec<Addr> = Vec::new();
            let mut members_of: std::collections::HashMap<Addr, Vec<usize>> =
                std::collections::HashMap::new();
            for (i, c) in idx.compilations.iter().enumerate() {
                if comp_visible(i) {
                    let members = members_of.entry(c.key.sfi).or_default();
                    if members.is_empty() {
                        seen.push(c.key.sfi);
                    }
                    members.push(i);
                }
            }
            for sfi in seen {
                let members = members_of.remove(&sfi).unwrap_or_default();
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
            for (i, r) in idx.raw.iter().enumerate() {
                if self.filter_matches(&r.label) {
                    rows.push(Row::Raw(i));
                }
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
                        let (i, _) = comps.next().unwrap();
                        if comp_visible(i) {
                            self.push_compilation_rows(&mut rows, i);
                        }
                    }
                    (Some(_), Some(_)) | (None, Some(_)) => {
                        let (i, r) = raws.next().unwrap();
                        if self.filter_matches(&r.label) {
                            rows.push(Row::Raw(i));
                        }
                    }
                    (Some(_), None) => {
                        let (i, _) = comps.next().unwrap();
                        if comp_visible(i) {
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

    /// Builds the display rows for the selected section. Compilations get the
    /// modeled (parsed, foldable) form; raw sections and oversized sections
    /// stay a plain O(1) window.
    pub fn view_model(&mut self) -> ViewModel {
        let rows = self.rows();
        let (comp, only_phase) = match rows.get(self.selected) {
            Some(Row::Compilation(i)) => (*i, None),
            Some(Row::Phase { comp, phase }) => (*comp, Some(*phase)),
            Some(Row::Function { name_comp, .. }) => (*name_comp, None),
            Some(Row::Raw(i)) => {
                let range = self.sources[self.active].index.raw[*i].lines.clone();
                return ViewModel::Plain { range };
            }
            Some(Row::Event(i)) => {
                return ViewModel::Plain {
                    range: self.event_view_range(*i),
                };
            }
            None => {
                return ViewModel::Plain {
                    range: 0..self.sources[self.active].buffer.line_count(),
                };
            }
        };

        let source = &mut self.sources[self.active];
        let section = &source.index.compilations[comp];
        if section.lines.len() > MODEL_LIMIT {
            // Oversized: no row modeling, but a phase selection must still
            // show *that phase*, not the whole compilation.
            let range = match only_phase {
                Some(p) => section.phases[p].lines.clone(),
                None => section.lines.clone(),
            };
            return ViewModel::Plain { range };
        }
        let parsed = source.parses.get_or_parse(&source.buffer, section, comp);
        let only_lines = match only_phase {
            Some(p) => section.phases[p].lines.clone(),
            None => section.lines.clone(),
        };
        let rows = model_rows(
            &parsed,
            section,
            &only_lines,
            self.active,
            comp,
            &self.folded_blocks,
            self.show_annotations,
            &source.buffer,
            self.lens,
        );
        ViewModel::Modeled { rows, parsed, comp }
    }

    /// What the viewport shows for a selected timeline event: for a deopt,
    /// the whole bailout block (through the matching `[bailout end]` — under
    /// `--trace-deopt-verbose` that is the frame-unwinding dump, TODO 6.5);
    /// for anything else, the enclosing section as context. Ranges are capped
    /// to the enclosing section so an interleaved stream cannot leak another
    /// section into the panel.
    fn event_view_range(&self, ev: usize) -> std::ops::Range<usize> {
        let idx = &self.sources[self.active].index;
        let Some(event) = idx.events.get(ev) else {
            return 0..self.sources[self.active].buffer.line_count();
        };
        let section = self
            .enclosing_section_range(event.line)
            .unwrap_or(event.line..event.line + 1);

        if matches!(event.kind, EventKind::DeoptBegin { .. }) {
            let end = idx.events[ev + 1..]
                .iter()
                .find(|e| matches!(e.kind, EventKind::DeoptEnd { .. }))
                .map(|e| e.line + 1)
                .unwrap_or(section.end)
                .min(section.end);
            return event.line..end.max(event.line + 1);
        }
        section
    }

    /// The section (raw or compilation) containing a buffer line. Both lists
    /// are sorted by start line and the sections partition the file, so in
    /// each list only the last section starting at or before the line can
    /// contain it.
    fn enclosing_section_range(&self, line: usize) -> Option<std::ops::Range<usize>> {
        let idx = &self.sources[self.active].index;
        let at = idx.raw.partition_point(|r| r.lines.start <= line);
        if let Some(r) = at.checked_sub(1).map(|i| &idx.raw[i])
            && r.lines.contains(&line)
        {
            return Some(r.lines.clone());
        }
        let at = idx.compilations.partition_point(|c| c.lines.start <= line);
        if let Some(c) = at.checked_sub(1).map(|i| &idx.compilations[i])
            && c.lines.contains(&line)
        {
            return Some(c.lines.clone());
        }
        None
    }

    /// The node defined on the cursor row, for tracking and jumps (TODO 4.3).
    /// Schedule-only lines (GapMove & co) share the [`SCHEDULE_ONLY`]
    /// sentinel and define nothing — treating them as "a node" once
    /// highlighted every gap move on screen at the same time.
    pub fn cursor_node(&self, vm: &ViewModel) -> Option<IRNode> {
        let row = vm.row(self.cursor)?;
        let (p, i) = row.info?;
        let parsed = vm.parsed()?;
        match parsed.phases.get(p)?.infos.get(i)? {
            LineInfo::Node(node) if node.id != SCHEDULE_ONLY => Some(node.clone()),
            _ => None,
        }
    }

    pub fn handle(&mut self, event: Event) {
        match event {
            Event::Input(CtEvent::Key(key)) => self.handle_key(key),
            Event::Input(CtEvent::Mouse(mouse)) => self.handle_mouse(mouse),
            // Resize needs no state change; the redraw that follows is enough.
            Event::Input(_) => {}
            Event::Source(e) => self.handle_source(e),
            Event::InputClosed => {
                tracing::warn!("terminal input closed; quitting");
                self.quit = true;
            }
        }
    }

    /// Mouse: the wheel scrolls the pane under the pointer (three rows, like
    /// vim's default), a left click focuses the pane and places the
    /// selection/cursor — which is what lights up def-use highlighting on the
    /// clicked node. Coordinates route through the pane rects the last frame
    /// recorded. With wrap on, one logical row can occupy several screen
    /// rows, so a click lands on the row *starting* at that screen line —
    /// approximate, and documented as such.
    fn handle_mouse(&mut self, mouse: MouseEvent) {
        if self.help {
            self.help = false;
            return;
        }
        // The input line owns the interaction; a stray click must not move
        // the cursor out from under an incremental search.
        if self.input.is_some() {
            return;
        }

        let at_pane = if self.sidebar_rect.contains(mouse.column, mouse.row) {
            Some(Pane::Sidebar)
        } else if self.viewport_rect.contains(mouse.column, mouse.row) {
            Some(Pane::Viewport)
        } else {
            None
        };

        match mouse.kind {
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let Some(pane) = at_pane else { return };
                self.focus = pane;
                let delta = if mouse.kind == MouseEventKind::ScrollUp {
                    -3
                } else {
                    3
                };
                self.move_by(delta);
            }
            MouseEventKind::Down(MouseButton::Left) => match at_pane {
                Some(Pane::Sidebar) => {
                    self.focus = Pane::Sidebar;
                    let offset = (mouse.row - self.sidebar_rect.y) as usize;
                    let rows = self.rows().len();
                    if rows == 0 {
                        return;
                    }
                    let target = (self.sidebar_scroll + offset).min(rows - 1);
                    if target != self.selected {
                        self.selected = target;
                        self.reset_view();
                    }
                }
                Some(Pane::Viewport) => {
                    self.focus = Pane::Viewport;
                    let offset = (mouse.row - self.viewport_rect.y) as usize;
                    let len = self.view_model().len();
                    if len == 0 {
                        return;
                    }
                    self.cursor = (self.top + offset).min(len - 1);
                    self.follow = self.follow && self.cursor + 1 == len;
                    self.cycle = None;
                }
                None => {}
            },
            _ => {}
        }
    }

    fn handle_key(&mut self, key: KeyEvent) {
        // Windows reports press and release; without this every key acts twice.
        if key.kind == KeyEventKind::Release {
            return;
        }

        if self.input.is_some() {
            self.handle_input_key(key);
            return;
        }

        // Before the keymap lookup, so that genuinely *any* key closes the
        // modal — unbound keys included, and `q` closes rather than quits.
        if self.help {
            self.help = false;
            return;
        }

        let Some(action) = self.keys.lookup(&key) else {
            return;
        };

        match action {
            Action::Quit => self.quit = true,
            Action::Back => {
                if self.focus == Pane::Viewport {
                    self.focus = Pane::Sidebar;
                } else {
                    self.quit = true;
                }
            }
            Action::Help => self.help = true,
            Action::FocusSidebar => self.focus = Pane::Sidebar,
            Action::FocusViewport => self.focus = Pane::Viewport,
            Action::NextSource => {
                if self.sources.len() > 1 {
                    self.active = (self.active + 1) % self.sources.len();
                    self.selected = 0;
                    self.sidebar_scroll = 0;
                    self.reset_view();
                }
            }
            Action::Select => self.toggle_expand(),
            Action::ToggleGrouping => {
                self.grouped = !self.grouped;
                self.selected = 0;
                self.sidebar_scroll = 0;
                // Follow means "pin to the newest section", and grouped mode
                // does not order rows by recency — the pin has no honest
                // target there, so switching modes breaks follow.
                self.follow = false;
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
                    let len = self.view_model().len();
                    self.cursor = len.saturating_sub(1);
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

            Action::FoldBlock => self.fold_block(),
            Action::ToggleAnnotations => {
                self.show_annotations = !self.show_annotations;
                self.status = format!(
                    "annotations {}",
                    if self.show_annotations {
                        "inline"
                    } else {
                        "folded"
                    }
                );
            }
            Action::Search => {
                self.input = Some(InputLine {
                    prompt: Prompt::Search,
                    buffer: String::new(),
                });
            }
            Action::Filter => {
                self.input = Some(InputLine {
                    prompt: Prompt::Filter,
                    buffer: self
                        .sidebar_filter
                        .as_ref()
                        .map(|r| r.as_str().to_string())
                        .unwrap_or_default(),
                });
            }
            Action::SearchNext => self.search_step(1),
            Action::SearchPrev => self.search_step(-1),
            Action::JumpToInput => self.jump_to_input(),
            Action::CycleConsumers => self.cycle_consumers(),
            Action::JumpBack => self.history_step(-1),
            Action::JumpForward => self.history_step(1),

            Action::Yank => self.yank_line(),
            Action::YankSection => self.yank_section(),
            Action::Export => {
                self.input = Some(InputLine {
                    prompt: Prompt::Export,
                    buffer: String::new(),
                });
            }
            Action::CommandPalette => {
                self.input = Some(InputLine {
                    prompt: Prompt::Command,
                    buffer: String::new(),
                });
            }
            Action::ToggleTimeline => self.toggle_timeline(),
        }
    }

    fn handle_input_key(&mut self, key: KeyEvent) {
        let Some(input) = &mut self.input else { return };
        match key.code {
            KeyCode::Esc => {
                let prompt = input.prompt;
                self.input = None;
                if prompt == Prompt::Search {
                    self.search = None;
                }
            }
            KeyCode::Enter => {
                let input = self.input.take().expect("checked above");
                match input.prompt {
                    Prompt::Search => {
                        // The pattern was compiled incrementally; Enter just
                        // commits and jumps to the first match.
                        if self.search.is_some() {
                            self.search_step(1);
                        }
                    }
                    Prompt::Filter => {
                        self.sidebar_filter = if input.buffer.is_empty() {
                            None
                        } else {
                            match Regex::new(&input.buffer) {
                                Ok(re) => Some(re),
                                Err(_) => {
                                    self.status = format!("bad filter regex: {}", input.buffer);
                                    None
                                }
                            }
                        };
                        self.selected = 0;
                        self.reset_view();
                    }
                    Prompt::Export => {
                        if input.buffer.is_empty() {
                            self.status = "export cancelled (empty filename)".to_string();
                        } else {
                            self.export_to(std::path::PathBuf::from(&input.buffer));
                        }
                    }
                    Prompt::Command => self.run_command(input.buffer.trim().to_string()),
                }
            }
            KeyCode::Tab if input.prompt == Prompt::Command => {
                // Completion (TODO 6.1): extend to the longest common prefix
                // of the matching commands; unique match completes fully. The
                // candidate list itself is rendered live in the status line.
                let typed = input.buffer.clone();
                if typed.contains(' ') {
                    return; // arguments have no completion
                }
                let matches: Vec<&str> = COMMANDS
                    .iter()
                    .map(|(name, _)| *name)
                    .filter(|name| name.starts_with(&typed))
                    .collect();
                match matches.as_slice() {
                    [] => {}
                    [only] => input.buffer = only.to_string(),
                    several => {
                        let mut prefix = several[0].to_string();
                        for name in &several[1..] {
                            let common = prefix
                                .chars()
                                .zip(name.chars())
                                .take_while(|(a, b)| a == b)
                                .count();
                            prefix.truncate(common);
                        }
                        input.buffer = prefix;
                    }
                }
            }
            KeyCode::Backspace => {
                input.buffer.pop();
                self.recompile_input();
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                input.buffer.push(c);
                self.recompile_input();
            }
            _ => {}
        }
    }

    /// Incremental search: recompile on each keystroke; invalid patterns are
    /// simply "no match yet" while typing.
    fn recompile_input(&mut self) {
        let Some(input) = &self.input else { return };
        if input.prompt == Prompt::Search {
            self.search = Regex::new(&input.buffer)
                .ok()
                .filter(|_| !input.buffer.is_empty());
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
                let len = self.view_model().len();
                if len == 0 {
                    return;
                }
                let target = self.cursor as isize + delta;
                self.cursor = target.clamp(0, len as isize - 1) as usize;
                self.follow = self.follow && self.cursor + 1 == len;
                self.cycle = None;
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
                self.cursor = 0;
                self.follow = false;
                self.cycle = None;
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
                self.cursor = self.view_model().len().saturating_sub(1);
                self.cycle = None;
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
            Some(Row::Event(i)) => self.event_jump(*i),
            None => {}
        }
    }

    /// Selection moved: pin the cursor into the new section. Deliberate
    /// sidebar navigation also breaks follow — otherwise the next chunk from a
    /// live stream yanks the selection straight back to the newest section.
    fn reset_view(&mut self) {
        self.cursor = 0;
        self.top = 0;
        self.scroll_x = 0;
        self.follow = false;
        self.cycle = None;
        self.sync_event_cursor();
    }

    /// In timeline mode the view is the event's context, and the cursor
    /// belongs on the event's own line, not on the top of that context.
    fn sync_event_cursor(&mut self) {
        if !self.timeline {
            return;
        }
        let rows = self.rows();
        let Some(Row::Event(i)) = rows.get(self.selected) else {
            return;
        };
        let line = self.sources[self.active].index.events[*i].line;
        let vm = self.view_model();
        if let Some(row) = row_showing(&vm, line) {
            self.cursor = row;
        }
    }

    // -----------------------------------------------------------------------
    // Folding (TODO 4.2)
    // -----------------------------------------------------------------------

    /// `Space`: fold/unfold the block containing the cursor.
    fn fold_block(&mut self) {
        let vm = self.view_model();
        let Some(row) = vm.row(self.cursor) else {
            return;
        };
        let (Some((p, i)), Some(parsed), ViewModel::Modeled { comp, .. }) =
            (row.info, vm.parsed(), &vm)
        else {
            return;
        };
        let comp = *comp;

        // Walk back from the cursor to the enclosing block header.
        let infos = &parsed.phases[p].infos;
        let header = (0..=i.min(infos.len().saturating_sub(1)))
            .rev()
            .find_map(|j| match infos.get(j) {
                Some(LineInfo::BlockHeader { block }) => Some((j, *block)),
                _ => None,
            });
        let Some((header_idx, block)) = header else {
            self.status = "no block here".to_string();
            return;
        };

        let key = (self.active, comp, p, block);
        let folded = if self.folded_blocks.remove(&key) {
            false
        } else {
            self.folded_blocks.insert(key);
            true
        };

        // Land the cursor on the header row of the toggled block.
        let header_line = self.sources[self.active].index.compilations[comp].phases[p]
            .lines
            .start
            + header_idx;
        let vm = self.view_model();
        if let Some(row) = row_showing(&vm, header_line) {
            self.cursor = row;
        }
        self.status = format!("b{block} {}", if folded { "folded" } else { "unfolded" });
    }

    // -----------------------------------------------------------------------
    // Search (TODO 4.6)
    // -----------------------------------------------------------------------

    /// `n`/`N`: move the cursor to the next/previous display row whose text
    /// matches the search. Works in parsed and raw sections alike, wraps.
    fn search_step(&mut self, direction: isize) {
        let Some(re) = self.search.clone() else {
            self.status = "no search — press /".to_string();
            return;
        };
        self.focus = Pane::Viewport;
        let vm = self.view_model();
        let len = vm.len();
        if len == 0 {
            return;
        }

        let buffer = &self.sources[self.active].buffer;
        let mut at = self.cursor as isize;
        for _ in 0..len {
            at += direction;
            // Wrap around.
            if at < 0 {
                at = len as isize - 1;
            } else if at >= len as isize {
                at = 0;
            }
            let Some(line) = vm.line_at(at as usize) else {
                continue;
            };
            if line_matches(buffer, line, &re) {
                self.cursor = at as usize;
                self.follow = false;
                // A search jump re-targets any i/u cycle to the new cursor.
                self.cycle = None;
                self.status = format!("match at L{}", line + 1);
                return;
            }
        }
        self.status = format!("no match for {}", re.as_str());
    }

    // -----------------------------------------------------------------------
    // Command palette (TODO 6.1, 6.2)
    // -----------------------------------------------------------------------

    /// Executes one committed `:` command. The status line is the message
    /// line: every branch ends in a message, including the unknown-command
    /// case.
    fn run_command(&mut self, text: String) {
        let (name, arg) = match text.split_once(char::is_whitespace) {
            Some((n, a)) => (n, a.trim()),
            None => (text.as_str(), ""),
        };
        match name {
            "" => self.status = "empty command".to_string(),
            "checks" => self.toggle_lens(Lens::Checks),
            "phi" => self.toggle_lens(Lens::Phi),
            "spill" => self.toggle_lens(Lens::Spill),
            "megamorphic" => self.toggle_lens(Lens::Megamorphic),
            "deopts" => {
                if !self.timeline {
                    self.toggle_timeline();
                }
                self.timeline_deopts_only = true;
                let rows = self.rows();
                self.selected = self.selected.min(rows.len().saturating_sub(1));
                self.reset_view();
                self.status = match rows.len() {
                    0 => "no deopt events in this trace".to_string(),
                    1 => "1 deopt event — Enter jumps to the compilation".to_string(),
                    n => format!("{n} deopt events — Enter jumps to the compilation"),
                };
            }
            "function" => {
                if arg.is_empty() {
                    self.sidebar_filter = None;
                    self.status = "sidebar filter cleared".to_string();
                } else {
                    match Regex::new(arg) {
                        Ok(re) => {
                            self.status = format!("sidebar filtered to /{arg}/");
                            self.sidebar_filter = Some(re);
                            self.selected = 0;
                            self.reset_view();
                        }
                        Err(_) => self.status = format!("bad regex: {arg}"),
                    }
                }
            }
            "copy" => self.yank_section(),
            "export" => {
                if arg.is_empty() {
                    self.input = Some(InputLine {
                        prompt: Prompt::Export,
                        buffer: String::new(),
                    });
                } else {
                    self.export_to(std::path::PathBuf::from(arg));
                }
            }
            "timeline" => self.toggle_timeline(),
            "clear" => {
                self.lens = None;
                self.timeline_deopts_only = false;
                self.status = "lens and timeline filter cleared".to_string();
            }
            other => self.status = format!("unknown command :{other} (Tab lists commands)"),
        }
    }

    /// `:checks` & friends: same lens toggles off, different lens replaces.
    /// The match count reported is for the *current* view, which is the count
    /// the user is looking at.
    fn toggle_lens(&mut self, lens: Lens) {
        if self.lens == Some(lens) {
            self.lens = None;
            self.status = format!("lens :{} cleared", lens.name());
            return;
        }
        self.lens = Some(lens);
        self.cursor = 0;
        self.top = 0;
        self.cycle = None;
        let vm = self.view_model();
        let matches = match &vm {
            ViewModel::Modeled { rows, parsed, .. } => {
                let buffer = &self.sources[self.active].buffer;
                rows.iter()
                    .filter(|row| {
                        row.kind == RowKind::Text && {
                            let info = row
                                .info
                                .and_then(|(p, i)| parsed.phases.get(p)?.infos.get(i));
                            lens.matches(info, parsed, buffer, row.line)
                        }
                    })
                    .count()
            }
            ViewModel::Plain { .. } => {
                self.status = format!(
                    "lens :{} set — applies to parsed compilations (this view is raw)",
                    lens.name()
                );
                return;
            }
        };
        self.status = format!(
            "lens :{} — {matches} match{} in this view (:clear resets)",
            lens.name(),
            if matches == 1 { "" } else { "es" }
        );
    }

    // -----------------------------------------------------------------------
    // Timeline & the deopt→graph jump (TODO 6.3, 6.4)
    // -----------------------------------------------------------------------

    /// `Tab`: compilation list ⇄ timeline. Each mode keeps its own selection.
    fn toggle_timeline(&mut self) {
        self.timeline = !self.timeline;
        std::mem::swap(&mut self.selected, &mut self.timeline_selected);
        self.timeline_deopts_only = false;
        self.sidebar_scroll = 0;
        self.focus = Pane::Sidebar;
        self.reset_view();
        if self.timeline {
            let n = self.rows().len();
            self.selected = self.selected.min(n.saturating_sub(1));
            self.status = match n {
                0 => "timeline — no events (needs --trace-opt / --trace-deopt)".to_string(),
                n => format!("timeline — {n} events · Enter jumps to the compilation"),
            };
        } else {
            let n = self.rows().len();
            self.selected = self.selected.min(n.saturating_sub(1));
            self.status = "compilation list".to_string();
        }
    }

    /// Enter on a timeline event: jump to the correlated compilation, per
    /// docs/correlation-keys.md — `(sfi, tier)` + stream position, never a
    /// guess. Unresolvable events stay timeline entries with a message.
    fn event_jump(&mut self, ev: usize) {
        let event = self.sources[self.active].index.events[ev].clone();
        // Deopts bind strictly backwards (the code being torn down was
        // compiled earlier); marking/compile events may precede their dump,
        // so they are allowed to bind forwards.
        let (sfi, tier, offset, forward_ok) = match &event.kind {
            EventKind::DeoptBegin {
                sfi: Some(sfi),
                tier,
                bytecode_offset,
                ..
            } => (*sfi, tier.clone(), *bytecode_offset, false),
            EventKind::Marking {
                sfi: Some(sfi),
                target,
                ..
            }
            | EventKind::CompileStart {
                sfi: Some(sfi),
                target,
                ..
            }
            | EventKind::CompileDone {
                sfi: Some(sfi),
                target,
                ..
            } => (*sfi, target.clone(), None, true),
            _ => {
                self.status =
                    "this event has no SFI to correlate on — timeline entry only".to_string();
                return;
            }
        };

        let comps = &self.sources[self.active].index.compilations;
        let matching = |c: &crate::model::CompilationSection| {
            c.key.sfi == sfi && c.key.tier == tier && !c.filtered_out
        };
        // Most recent instance opened before the event line (rule 2).
        let mut found = comps
            .iter()
            .enumerate()
            .filter(|(_, c)| matching(c) && c.lines.start < event.line)
            .map(|(i, _)| i)
            .next_back();
        if found.is_none() && forward_ok {
            found = comps
                .iter()
                .enumerate()
                .filter(|(_, c)| matching(c) && c.lines.start >= event.line)
                .map(|(i, _)| i)
                .next();
        }
        let Some(comp) = found else {
            self.status = format!(
                "unresolved: no {} graph for sfi {sfi} in this trace — timeline entry only",
                tier.label()
            );
            return;
        };

        self.push_history();
        self.open_compilation(comp);
        if let Some(offset) = offset {
            self.jump_to_bytecode_offset(comp, offset);
        }
    }

    /// Leaves the timeline (if on) and selects a compilation row, expanding
    /// its group in grouped mode so the row exists.
    fn open_compilation(&mut self, comp: usize) {
        if self.timeline {
            self.timeline = false;
            std::mem::swap(&mut self.selected, &mut self.timeline_selected);
        }
        let sfi = self.sources[self.active].index.compilations[comp].key.sfi;
        if self.grouped {
            self.expanded_groups.insert((self.active, sfi.0));
        }
        let mut rows = self.rows();
        let mut at = rows
            .iter()
            .position(|r| matches!(r, Row::Compilation(c) if *c == comp));
        if at.is_none() && self.sidebar_filter.is_some() {
            // The target exists but the quick filter hides it. Jumping
            // somewhere else would be a lie; clearing a display filter is the
            // honest resolution, and the status says so.
            self.sidebar_filter = None;
            rows = self.rows();
            at = rows
                .iter()
                .position(|r| matches!(r, Row::Compilation(c) if *c == comp));
            self.status = "sidebar filter cleared to show the jump target".to_string();
        }
        if let Some(at) = at {
            self.selected = at;
        }
        self.reset_view();
        self.focus = Pane::Viewport;
    }

    /// Lands the cursor on `bytecode offset N` inside a compilation, per the
    /// correlation spec: prefer the earliest *graph* phase containing the
    /// offset (later phases drop dead bytecode), then a deopt frame at that
    /// offset, then the bytecode-array dump, then stay at the top.
    fn jump_to_bytecode_offset(&mut self, comp: usize, offset: u32) {
        let section = &self.sources[self.active].index.compilations[comp];
        if section.lines.len() > MODEL_LIMIT {
            self.status = format!("@{offset}: section too large to model — cursor at the top");
            return;
        }
        let section = section.clone();
        let source = &mut self.sources[self.active];
        let parsed = source.parses.get_or_parse(&source.buffer, &section, comp);

        let mut target: Option<(usize, &'static str)> = None;
        let find_bytecode = |p: usize| {
            parsed.phases.get(p).and_then(|phase| {
                phase.infos.iter().position(
                    |info| matches!(info, LineInfo::Bytecode { offset: o } if *o == offset),
                )
            })
        };
        for (p, phase_section) in section.phases.iter().enumerate() {
            if !matches!(phase_section.kind, PhaseKind::Graph { .. }) {
                continue;
            }
            if let Some(i) = find_bytecode(p) {
                target = Some((phase_section.lines.start + i, "graph"));
                break;
            }
        }
        if target.is_none() {
            'frames: for (p, phase_section) in section.phases.iter().enumerate() {
                let Some(phase) = parsed.phases.get(p) else {
                    continue;
                };
                for (i, info) in phase.infos.iter().enumerate() {
                    if let LineInfo::Frame { frame, .. } = info
                        && parsed
                            .frames
                            .get(*frame as usize)
                            .is_some_and(|f| f.bytecode_offset == Some(offset as i32))
                    {
                        target = Some((phase_section.lines.start + i, "deopt frame"));
                        break 'frames;
                    }
                }
            }
        }
        if target.is_none() {
            for (p, phase_section) in section.phases.iter().enumerate() {
                if !matches!(phase_section.kind, PhaseKind::Bytecode) {
                    continue;
                }
                if let Some(i) = find_bytecode(p) {
                    target = Some((phase_section.lines.start + i, "bytecode array"));
                    break;
                }
            }
        }

        match target {
            Some((line, where_)) => {
                self.goto_line(line);
                self.status = format!("bytecode offset {offset} ({where_})");
            }
            None => {
                self.status =
                    format!("bytecode offset {offset} not in this dump — cursor at the top");
            }
        }
    }

    // -----------------------------------------------------------------------
    // Node jumps (TODO 4.5)
    // -----------------------------------------------------------------------

    /// `i`: jump to the definition of the cursor node's inputs, cycling
    /// through them on repeat. Like `u`, the cycle stays anchored to the node
    /// it started from — the first jump moves the cursor onto an input, and
    /// without the anchor a second `i` would ask for *that* node's inputs
    /// instead of the next input of the original.
    fn jump_to_input(&mut self) {
        let vm = self.view_model();
        let Some(row) = vm.row(self.cursor) else {
            return;
        };
        let Some((p, _)) = row.info else { return };
        let Some(parsed) = vm.parsed() else { return };

        let anchor = match (&self.cycle, self.cursor_node(&vm)) {
            (Some(cycle), _) => cycle.node,
            (None, Some(node)) => node.id,
            (None, None) => {
                self.status = "no node under cursor".to_string();
                return;
            }
        };

        let phase = &parsed.phases[p];
        // The anchor's inputs come from its definition line, which may not be
        // the cursor line any more.
        let Some(anchor_node) = phase
            .defs
            .get(&anchor)
            .and_then(|&idx| phase.infos.get(idx as usize))
            .and_then(|info| match info {
                LineInfo::Node(node) => Some(node),
                _ => None,
            })
        else {
            self.status = format!("n{anchor} is not defined in this phase");
            return;
        };

        let phase_start = self.phase_start(&vm, p);
        let targets: Vec<usize> = anchor_node
            .inputs
            .iter()
            .filter_map(|r| phase.defs.get(&r.node))
            .map(|&info_idx| phase_start + info_idx as usize)
            .collect();
        if targets.is_empty() {
            self.status = format!("n{anchor} has no inputs defined in this phase");
            return;
        }
        self.cycle_jump(anchor, targets, "input");
    }

    /// `u`: cycle through the consumers of the cursor node (TODO 4.5). The
    /// cycle stays anchored to the node it started from.
    fn cycle_consumers(&mut self) {
        let vm = self.view_model();
        let Some(row) = vm.row(self.cursor) else {
            return;
        };
        let Some((p, _)) = row.info else { return };
        let Some(parsed) = vm.parsed() else { return };

        // Continue an existing cycle even though the cursor moved off the
        // anchor node; otherwise `u u` would cycle the first consumer's users.
        let anchor = match (&self.cycle, self.cursor_node(&vm)) {
            (Some(cycle), _) => cycle.node,
            (None, Some(node)) => node.id,
            (None, None) => {
                self.status = "no node under cursor".to_string();
                return;
            }
        };

        let phase = &parsed.phases[p];
        let phase_start = self.phase_start(&vm, p);
        let targets: Vec<usize> = phase
            .users
            .get(&anchor)
            .map(|users| {
                users
                    .iter()
                    .map(|&info_idx| phase_start + info_idx as usize)
                    .collect()
            })
            .unwrap_or_default();
        if targets.is_empty() {
            self.status = format!("n{anchor} has no consumers in this phase");
            return;
        }
        self.cycle_jump(anchor, targets, "consumer");
    }

    /// Shared cycle mechanics: same anchor advances, new anchor restarts.
    fn cycle_jump(&mut self, node: NodeId, targets: Vec<usize>, what: &str) {
        let at = match &self.cycle {
            Some(c) if c.node == node && c.targets == targets => (c.at + 1) % targets.len(),
            _ => 0,
        };
        let line = targets[at];
        self.push_history();
        self.goto_line(line);
        self.cycle = Some(Cycle {
            node,
            targets: targets.clone(),
            at,
        });
        self.status = format!("n{node} {what} {}/{}", at + 1, targets.len());
    }

    /// Buffer line of a phase's first line within the current compilation.
    fn phase_start(&self, vm: &ViewModel, p: usize) -> usize {
        match vm {
            ViewModel::Modeled { comp, .. } => {
                self.sources[self.active].index.compilations[*comp].phases[p]
                    .lines
                    .start
            }
            ViewModel::Plain { .. } => 0,
        }
    }

    /// Moves the cursor to a buffer line, unfolding whatever hides it.
    fn goto_line(&mut self, line: usize) {
        let vm = self.view_model();
        if let Some(row) = row_showing(&vm, line) {
            self.cursor = row;
            self.follow = false;
            return;
        }
        // Hidden: inside a folded block, or in a collapsed annotation run.
        if let ViewModel::Modeled { comp, parsed, .. } = &vm {
            let comp = *comp;
            let section = &self.sources[self.active].index.compilations[comp];
            for (p, phase_section) in section.phases.iter().enumerate() {
                if !phase_section.lines.contains(&line) {
                    continue;
                }
                let idx = line - phase_section.lines.start;
                let infos = &parsed.phases[p].infos;
                if let Some((_, block)) = (0..=idx.min(infos.len().saturating_sub(1)))
                    .rev()
                    .find_map(|j| match infos.get(j) {
                        Some(LineInfo::BlockHeader { block }) => Some((j, *block)),
                        _ => None,
                    })
                {
                    self.folded_blocks.remove(&(self.active, comp, p, block));
                }
            }
        }
        // Unfolding the block may already be enough; only reach for the wider
        // hammers if the target is still hidden.
        let vm = self.view_model();
        if let Some(row) = row_showing(&vm, line) {
            self.cursor = row;
            self.follow = false;
            return;
        }
        // A lens can hide the target (e.g. `i` from a guard to a plain value
        // node under `:checks`); the jump wins over the lens.
        if self.lens.is_some() {
            self.lens = None;
            self.status = "lens cleared by jump".to_string();
            let vm = self.view_model();
            if let Some(row) = row_showing(&vm, line) {
                self.cursor = row;
                self.follow = false;
                return;
            }
        }
        self.show_annotations = true;
        let vm = self.view_model();
        if let Some(row) = row_showing(&vm, line) {
            self.cursor = row;
            self.follow = false;
        }
    }

    // -----------------------------------------------------------------------
    // Jump history (TODO 4.5)
    // -----------------------------------------------------------------------

    /// History entries store **buffer lines**, not display rows: a jump can
    /// unfold blocks or expand annotations, which shifts every later display
    /// row — a stored row index would land Ctrl+O off by the hidden count
    /// (found in review).
    fn push_history(&mut self) {
        let here = self.history_position();
        self.jumps.truncate(self.jump_at);
        self.jumps.push(here);
        self.jump_at = self.jumps.len();
    }

    fn history_position(&mut self) -> Jump {
        let line = self.view_model().line_at(self.cursor).unwrap_or(0);
        Jump {
            timeline: self.timeline,
            selected: self.selected,
            line,
        }
    }

    fn history_step(&mut self, direction: isize) {
        if direction < 0 {
            if self.jump_at == 0 {
                self.status = "at oldest jump".to_string();
                return;
            }
            // Record where we are so Ctrl+I can come back.
            if self.jump_at == self.jumps.len() {
                let here = self.history_position();
                self.jumps.push(here);
            }
            self.jump_at -= 1;
        } else {
            if self.jump_at + 1 >= self.jumps.len() {
                self.status = "at newest jump".to_string();
                return;
            }
            self.jump_at += 1;
        }
        let jump = self.jumps[self.jump_at];
        // Restore the sidebar mode the entry was recorded in; the stored
        // selection is only meaningful in that mode.
        self.timeline = jump.timeline;
        self.selected = jump.selected;
        self.goto_line(jump.line);
        self.follow = false;
        self.cycle = None;
    }

    // -----------------------------------------------------------------------
    // Clipboard and export (TODO 5.2, 5.3)
    // -----------------------------------------------------------------------

    /// A short description of what the viewport is showing, for export
    /// headers and messages.
    pub fn view_title(&self) -> String {
        let source = &self.sources[self.active];
        match self.rows().get(self.selected) {
            Some(Row::Compilation(i)) | Some(Row::Function { name_comp: i, .. }) => {
                let c = &source.index.compilations[*i];
                format!(
                    "{} · {} #{}",
                    c.display_name(),
                    c.key.tier.label(),
                    c.key.ordinal
                )
            }
            Some(Row::Phase { comp, phase }) => {
                let c = &source.index.compilations[*comp];
                format!(
                    "{} · {} #{} · {}",
                    c.display_name(),
                    c.key.tier.label(),
                    c.key.ordinal,
                    c.phases[*phase].name
                )
            }
            Some(Row::Raw(i)) => format!("raw · {}", source.index.raw[*i].label),
            Some(Row::Event(i)) => {
                let event = &source.index.events[*i];
                format!("event #{} · {}", i + 1, event_summary(&event.kind))
            }
            None => source.label.clone(),
        }
    }

    /// The visible rows of the current view as plain text — folds render as
    /// their markers, exactly like the screen (PLAN §7.9: "current view").
    fn visible_view_text(&mut self) -> Vec<String> {
        let vm = self.view_model();
        let buffer = &self.sources[self.active].buffer;
        (0..vm.len())
            .filter_map(|r| vm.row(r))
            .map(|row| match row.kind {
                RowKind::Text => line_text(buffer, row.line),
                RowKind::BlockFold { block, hidden } => {
                    format!("[+] b{block} — {hidden} lines hidden")
                }
                RowKind::AnnotationFold { count } => format!("[+] {count} trace lines"),
            })
            .collect()
    }

    fn yank_line(&mut self) {
        let vm = self.view_model();
        let Some(line) = vm.line_at(self.cursor) else {
            return;
        };
        let text = line_text(&self.sources[self.active].buffer, line);
        match crate::clipboard::copy(&text) {
            Ok(how) => self.status = format!("line {how}"),
            Err(e) => self.status = e.to_string(),
        }
    }

    fn yank_section(&mut self) {
        // Refuse oversized sections *before* materialising anything: the size
        // of a plain window is O(1) from the line-offset index, and without
        // this check a Y on a multi-million-line raw section froze the UI to
        // build a gigabyte of Strings whose only possible fate was the same
        // refusal (found in review).
        let vm = self.view_model();
        if let ViewModel::Plain { range } = &vm {
            let bytes = self.sources[self.active].buffer.span_bytes(range.clone());
            if bytes > crate::clipboard::MAX_COPY {
                self.status = format!(
                    "section is {} KB; the clipboard path tops out at {} KB — use export instead",
                    bytes / 1024,
                    crate::clipboard::MAX_COPY / 1024
                );
                return;
            }
        }
        let text = self.visible_view_text().join("\n");
        match crate::clipboard::copy(&text) {
            Ok(how) => self.status = format!("section {how}"),
            Err(e) => self.status = e.to_string(),
        }
    }

    fn export_to(&mut self, path: std::path::PathBuf) {
        let title = self.view_title();
        let lines = self.visible_view_text();
        match crate::clipboard::export(&path, &title, &lines) {
            Ok(()) => self.status = format!("exported {} lines to {}", lines.len(), path.display()),
            Err(e) => self.status = e.to_string(),
        }
    }

    fn handle_source(&mut self, event: SourceEvent) {
        let index = match &event {
            SourceEvent::Mapped { source, .. }
            | SourceEvent::Chunk { source, .. }
            | SourceEvent::Eof { source }
            | SourceEvent::Failed { source, .. } => *source,
        };

        if self.sources.get(index).is_none() {
            tracing::error!(index, "event for an unknown source");
            return;
        }
        // Captured before ingest mutates the index, so the selection can be
        // re-located by identity afterwards.
        let prev_row = (index == self.active)
            .then(|| self.rows().get(self.selected).cloned())
            .flatten();
        let target = &mut self.sources[index];

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
            let rows = self.rows();
            // Re-locate the selection by identity, not by index: grouped mode
            // inserts new Function rows *before* the raw rows, so a kept index
            // silently lands on a different item as the stream grows (found in
            // review). Chronological mode is append-only, where this is a
            // no-op.
            if let Some(prev) = prev_row {
                if let Some(at) = rows.iter().position(|r| same_row(r, &prev)) {
                    self.selected = at;
                }
            }
            self.selected = self.selected.min(rows.len().saturating_sub(1));
            if self.follow && !self.grouped && !self.timeline {
                // Streaming, chronological: the last row is the newest
                // section (the merge is ordered by start line). The cursor
                // pin to the last display row happens at render time. In
                // grouped mode there is no "newest" row to pin — toggling
                // grouping breaks follow instead.
                self.selected = rows.len().saturating_sub(1);
            }
        }
    }
}

/// One-line human summary of an event, shared by the timeline sidebar and
/// the view title.
pub fn event_summary(kind: &EventKind) -> String {
    let display = |name: &str| {
        if name.is_empty() {
            "<toplevel>".to_string()
        } else {
            name.to_string()
        }
    };
    match kind {
        EventKind::Marking {
            name,
            target,
            reason,
            ..
        } => format!("mark {} → {} ({reason})", display(name), target.label()),
        EventKind::CompileStart {
            name, target, osr, ..
        } => format!(
            "compile {} {}{}",
            display(name),
            target.label(),
            if *osr { " OSR" } else { "" }
        ),
        EventKind::CompileDone {
            name, target, osr, ..
        } => format!(
            "done {} {}{}",
            display(name),
            target.label(),
            if *osr { " OSR" } else { "" }
        ),
        EventKind::DeoptBegin {
            kind,
            reason,
            name,
            tier,
            bytecode_offset,
            ..
        } => {
            let at = bytecode_offset
                .map(|o| format!(" @{o}"))
                .unwrap_or_default();
            format!("{kind} {} {}{at} — {reason}", display(name), tier.label())
        }
        EventKind::DeoptEnd { invalidated } => format!(
            "bailout end ({})",
            if *invalidated {
                "code invalidated"
            } else {
                "code unaffected"
            }
        ),
        EventKind::Osr {
            what,
            name,
            osr_offset,
            ..
        } => {
            let at = osr_offset.map(|o| format!(" @{o}")).unwrap_or_default();
            format!("OSR {what} {}{at}", display(name))
        }
    }
}

/// Row identity across rebuilds: `Function` rows compare by SFI only — their
/// `count` grows as the stream does, which is exactly when re-location
/// matters.
fn same_row(a: &Row, b: &Row) -> bool {
    match (a, b) {
        (Row::Function { sfi: a, .. }, Row::Function { sfi: b, .. }) => a == b,
        _ => a == b,
    }
}

/// Regex match on one line without materialising a String when avoidable:
/// escape-free valid-UTF-8 lines (all of a plain-text trace) match borrowed.
/// d8's colored graph lines still pay the strip, but `n` over a large raw
/// section stops allocating per line (found in review: ~56 ms per keypress
/// on a 2M-line section).
fn line_matches(buffer: &LogBuffer, line: usize, re: &Regex) -> bool {
    let bytes = buffer.line(line).unwrap_or(b"");
    if !bytes.contains(&0x1b)
        && let Ok(text) = std::str::from_utf8(bytes)
    {
        return re.is_match(text);
    }
    re.is_match(&line_text(buffer, line))
}

/// The display row showing a buffer line, regardless of row kind (fold
/// markers included).
fn row_showing(vm: &ViewModel, line: usize) -> Option<usize> {
    match vm {
        ViewModel::Plain { range } => range.contains(&line).then(|| line - range.start),
        ViewModel::Modeled { rows, .. } => rows.iter().position(|r| r.line == line),
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
        let started = std::time::Instant::now();
        guard.tui().draw(|frame| crate::ui::render(frame, app))?;
        // The PLAN §12 cursor-latency budget, made observable: any frame that
        // blows it lands in the debug log with what was on screen.
        let elapsed = started.elapsed();
        if elapsed.as_millis() >= 16 {
            tracing::debug!(
                millis = elapsed.as_millis(),
                selected = app.selected,
                "slow frame"
            );
        }
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

    fn ctrl(app: &mut App, c: char) {
        app.handle(Event::Input(CtEvent::Key(KeyEvent::new(
            KeyCode::Char(c),
            KeyModifiers::CONTROL,
        ))));
    }

    const TRACE: &str = "\
warmup line
Compiling 0x1 <JSFunction f (sfi = 0x10)> with Maglev
----- Maglev graph building -----
 Block b0
   1: Foo
   2: Bar [n1]
 Block b1
   3: Baz [n2]
   regalloc trace line
   4: Quux [n1, n3]
Compiling 0x2 <JSFunction g (sfi = 0x20)> with Maglev
----- Maglev graph building -----
   5: Zap
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
    fn compilation_view_models_and_folds_annotations() {
        let mut app = app_with(TRACE);
        app.follow = false;
        app.selected = 1;
        let vm = app.view_model();
        // 9 section lines (anchor + banner + 2 headers + 4 nodes + 1
        // annotation-as-marker): 9 rows.
        assert_eq!(vm.len(), 9);
        assert!(matches!(
            vm.row(7).unwrap().kind,
            RowKind::AnnotationFold { count: 1 }
        ));
        key(&mut app, KeyCode::Char('t'));
        let vm = app.view_model();
        assert_eq!(vm.len(), 9, "single annotation, same count");
        assert!(matches!(vm.row(7).unwrap().kind, RowKind::Text));
        assert!(app.show_annotations);
    }

    #[test]
    fn space_folds_the_block_under_the_cursor() {
        let mut app = app_with(TRACE);
        app.follow = false;
        app.selected = 1;
        app.focus = Pane::Viewport;
        // Cursor onto node `2: Bar` (display row: 0 anchor,1 banner,2 b0,3 Foo,4 Bar).
        app.cursor = 4;
        key(&mut app, KeyCode::Char(' '));
        let vm = app.view_model();
        assert!(
            matches!(
                vm.row(2).unwrap().kind,
                RowKind::BlockFold {
                    block: 0,
                    hidden: 2
                }
            ),
            "b0 folded with its two nodes hidden"
        );
        assert_eq!(app.cursor, 2, "cursor landed on the fold marker");
        key(&mut app, KeyCode::Char(' '));
        assert_eq!(app.view_model().len(), 9, "unfolded again");
    }

    #[test]
    fn search_moves_the_cursor_and_wraps() {
        let mut app = app_with(TRACE);
        app.follow = false;
        app.selected = 1;
        app.focus = Pane::Viewport;
        key(&mut app, KeyCode::Char('/'));
        for c in "Ba[rz]".chars() {
            key(&mut app, KeyCode::Char(c));
        }
        key(&mut app, KeyCode::Enter);
        let vm = app.view_model();
        assert_eq!(vm.line_at(app.cursor), Some(5), "first match: Bar");
        key(&mut app, KeyCode::Char('n'));
        let vm = app.view_model();
        assert_eq!(vm.line_at(app.cursor), Some(7), "next match: Baz");
        key(&mut app, KeyCode::Char('n'));
        let vm = app.view_model();
        assert_eq!(vm.line_at(app.cursor), Some(5), "wrapped");
    }

    #[test]
    fn input_jump_and_consumer_cycle_with_history() {
        let mut app = app_with(TRACE);
        app.follow = false;
        app.selected = 1;
        app.focus = Pane::Viewport;
        // Row 8: `4: Quux [n1, n3]` (the annotation above it is a marker row).
        app.cursor = 8;
        key(&mut app, KeyCode::Char('i'));
        let vm = app.view_model();
        assert_eq!(vm.line_at(app.cursor), Some(4), "def of n1 (first input)");
        // The cycle stays anchored to Quux, so the second `i` reaches the
        // *second* input's definition (review finding: it used to restart
        // from the node under the cursor and report "no inputs").
        key(&mut app, KeyCode::Char('i'));
        let vm = app.view_model();
        assert_eq!(vm.line_at(app.cursor), Some(7), "def of n3 (second input)");
        // History unwinds jump by jump, in buffer-line coordinates.
        ctrl(&mut app, 'o');
        let vm = app.view_model();
        assert_eq!(vm.line_at(app.cursor), Some(4), "back to the first jump");
        ctrl(&mut app, 'o');
        let vm = app.view_model();
        assert_eq!(vm.line_at(app.cursor), Some(9), "back to the origin");

        // Consumers of n1: Bar (line 5) and Quux (line 9). Move onto Foo via
        // keys so the stale cycle is cleared the way real navigation clears
        // it.
        key(&mut app, KeyCode::Char('g'));
        for _ in 0..3 {
            key(&mut app, KeyCode::Char('j'));
        }
        let vm = app.view_model();
        assert_eq!(vm.line_at(app.cursor), Some(4), "on `1: Foo`");
        key(&mut app, KeyCode::Char('u'));
        let vm = app.view_model();
        assert_eq!(vm.line_at(app.cursor), Some(5), "first consumer");
        key(&mut app, KeyCode::Char('u'));
        let vm = app.view_model();
        assert_eq!(
            vm.line_at(app.cursor),
            Some(9),
            "cycle stays anchored to n1"
        );
    }

    #[test]
    fn jump_into_a_folded_block_unfolds_it() {
        let mut app = app_with(TRACE);
        app.follow = false;
        app.selected = 1;
        app.focus = Pane::Viewport;
        // Fold b0 (rows: 0 anchor, 1 banner, 2 b0, ...).
        app.cursor = 2;
        key(&mut app, KeyCode::Char(' '));
        // From Quux, i → def of n1 which is inside folded b0.
        let vm = app.view_model();
        let quux_row = (0..vm.len())
            .find(|&r| vm.line_at(r) == Some(9))
            .expect("Quux visible");
        app.cursor = quux_row;
        key(&mut app, KeyCode::Char('i'));
        let vm = app.view_model();
        assert_eq!(vm.line_at(app.cursor), Some(4), "unfolded and landed");
    }

    #[test]
    fn sidebar_filter_narrows_rows() {
        let mut app = app_with(TRACE);
        app.follow = false;
        key(&mut app, KeyCode::Char('f'));
        key(&mut app, KeyCode::Char('g'));
        key(&mut app, KeyCode::Enter);
        let rows = app.rows();
        assert_eq!(rows, vec![Row::Compilation(1)], "only g matches");
        // Clearing the filter restores everything.
        key(&mut app, KeyCode::Char('f'));
        key(&mut app, KeyCode::Backspace);
        key(&mut app, KeyCode::Enter);
        assert_eq!(app.rows().len(), 3);
    }

    #[test]
    fn grouping_creates_function_headers() {
        let mut app = app_with(TRACE);
        app.follow = false;
        key(&mut app, KeyCode::Char('c'));
        let rows = app.rows();
        // Two functions (f, g) collapsed + one raw section.
        assert_eq!(rows.len(), 3);
        assert!(matches!(rows[0], Row::Function { count: 1, .. }));
        key(&mut app, KeyCode::Enter);
        assert_eq!(app.rows().len(), 4, "group expanded");
    }

    #[test]
    fn follow_sticks_to_the_newest_section() {
        let app = app_with(TRACE);
        assert!(app.follow, "stdin defaults to follow");
        let rows = app.rows();
        assert_eq!(app.selected, rows.len() - 1);
    }

    #[test]
    fn help_modal_opens_and_any_key_closes() {
        let mut app = app_with(TRACE);
        key(&mut app, KeyCode::Char('?'));
        assert!(app.help);
        key(&mut app, KeyCode::Char('q'));
        assert!(!app.help);
        assert!(!app.quit, "q inside help closes the modal, not the app");
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

    fn mouse(app: &mut App, kind: MouseEventKind, column: u16, row: u16) {
        app.handle(Event::Input(CtEvent::Mouse(MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        })));
    }

    /// Lay out panes the way a frame would: sidebar columns 0..30, viewport
    /// 30..120, both rows 1..41.
    fn with_layout(app: &mut App) {
        app.sidebar_rect = PaneRect {
            x: 0,
            y: 1,
            width: 30,
            height: 40,
        };
        app.viewport_rect = PaneRect {
            x: 30,
            y: 1,
            width: 90,
            height: 40,
        };
        app.sidebar_height = 40;
        app.viewport_height = 40;
    }

    #[test]
    fn wheel_scrolls_the_pane_under_the_pointer() {
        let mut app = app_with(TRACE);
        app.follow = false;
        app.selected = 1;
        app.focus = Pane::Sidebar;
        with_layout(&mut app);

        // Wheel over the viewport moves the cursor, not the selection…
        app.cursor = 0;
        mouse(&mut app, MouseEventKind::ScrollDown, 60, 5);
        assert_eq!(app.focus, Pane::Viewport);
        assert_eq!(app.cursor, 3, "three rows per tick");
        assert_eq!(app.selected, 1, "selection untouched");
        mouse(&mut app, MouseEventKind::ScrollUp, 60, 5);
        assert_eq!(app.cursor, 0);

        // …and over the sidebar it moves the selection.
        mouse(&mut app, MouseEventKind::ScrollUp, 5, 5);
        assert_eq!(app.focus, Pane::Sidebar);
        assert_eq!(app.selected, 0);

        // Outside both panes: ignored.
        let before = app.selected;
        mouse(&mut app, MouseEventKind::ScrollDown, 60, 0);
        assert_eq!(app.selected, before);
    }

    #[test]
    fn click_places_selection_and_cursor() {
        let mut app = app_with(TRACE);
        app.follow = false;
        with_layout(&mut app);

        // Click the second sidebar row (screen row 2 = y 1 + offset 1).
        mouse(&mut app, MouseEventKind::Down(MouseButton::Left), 5, 2);
        assert_eq!(app.focus, Pane::Sidebar);
        assert_eq!(app.selected, 1);

        // Click viewport screen row 5 → display row top+4 → its node lights
        // up as the cursor node.
        mouse(&mut app, MouseEventKind::Down(MouseButton::Left), 60, 5);
        assert_eq!(app.focus, Pane::Viewport);
        assert_eq!(app.cursor, 4);
        let vm = app.view_model();
        assert_eq!(app.cursor_node(&vm).map(|n| n.id), Some(2), "clicked Bar");

        // A click past the end of the section clamps to the last row.
        mouse(&mut app, MouseEventKind::Down(MouseButton::Left), 60, 39);
        let vm = app.view_model();
        assert_eq!(app.cursor, vm.len() - 1);
    }

    #[test]
    fn mouse_ignored_during_prompts_and_closes_help() {
        let mut app = app_with(TRACE);
        app.follow = false;
        with_layout(&mut app);

        key(&mut app, KeyCode::Char('?'));
        mouse(&mut app, MouseEventKind::ScrollDown, 60, 5);
        assert!(!app.help, "any mouse action closes the modal");

        key(&mut app, KeyCode::Char('/'));
        let cursor = app.cursor;
        mouse(&mut app, MouseEventKind::Down(MouseButton::Left), 60, 20);
        assert_eq!(app.cursor, cursor, "prompt owns the interaction");
        assert!(app.input.is_some());
    }

    #[test]
    fn cursor_node_tracks_the_cursor_row() {
        let mut app = app_with(TRACE);
        app.follow = false;
        app.selected = 1;
        app.cursor = 4;
        let vm = app.view_model();
        let node = app.cursor_node(&vm).expect("node on row 4");
        assert_eq!(node.id, 2);
    }

    /// A trace with lifecycle events, a graph with an interleaved bytecode
    /// line, and a verbose deopt block — the Phase 6 material.
    const EVENTS_TRACE: &str = "\
[marking 0x09b8 <JSFunction f (sfi = 0x10)> for optimization to MAGLEV, ConcurrencyMode::kConcurrent, reason: hot and stable]
Compiling 0x1 <JSFunction f (sfi = 0x10)> with Maglev
----- Maglev graph building -----
 Block b0
   2 : 0b 04             Ldar a1
   1: CheckSmth [n0]
[bailout (kind: deopt-eager, reason: not a Smi): begin. deoptimizing 0x09b8 <JSFunction f (sfi = 0x10)>, 0x031a <Code MAGLEV>, opt id 1, bytecode offset 2, deopt exit 0, FP to SP delta 32, caller SP 0x0001, pc 0x0002]
            ;;; deoptimize at <test.js:14:1>
  reading input frame  => bytecode_offset=2, args=1, height=6, retval=0(#0); inputs:
      0: 0x09b8 ;  [fp -  16]  0x09b8 <JSFunction (sfi = 0x10)>
[bailout end. code_invalidation: unaffected, took 0.024 ms]
";

    #[test]
    fn command_palette_completes_and_toggles_a_lens() {
        let mut app = app_with(EVENTS_TRACE);
        app.follow = false;
        app.selected = 1; // the compilation

        key(&mut app, KeyCode::Char(':'));
        for c in "che".chars() {
            key(&mut app, KeyCode::Char(c));
        }
        key(&mut app, KeyCode::Tab);
        assert_eq!(app.input.as_ref().unwrap().buffer, "checks");
        key(&mut app, KeyCode::Enter);
        assert_eq!(app.lens, Some(Lens::Checks));
        assert!(app.status.contains("1 match"), "{}", app.status);

        // The lens keeps the skeleton + the guard; the bytecode line is gone.
        let vm = app.view_model();
        let lines: Vec<usize> = (0..vm.len()).filter_map(|r| vm.line_at(r)).collect();
        assert_eq!(lines, vec![1, 2, 3, 5]);

        // Ambiguous prefix completes to the common prefix and stays open.
        key(&mut app, KeyCode::Char(':'));
        key(&mut app, KeyCode::Char('c'));
        key(&mut app, KeyCode::Tab);
        assert_eq!(
            app.input.as_ref().unwrap().buffer,
            "c",
            "checks/clear/copy share only c"
        );
        for c in "lear".chars() {
            key(&mut app, KeyCode::Char(c));
        }
        key(&mut app, KeyCode::Enter);
        assert_eq!(app.lens, None);
    }

    #[test]
    fn unknown_command_reports_and_does_nothing() {
        let mut app = app_with(EVENTS_TRACE);
        app.follow = false;
        key(&mut app, KeyCode::Char(':'));
        for c in "bogus".chars() {
            key(&mut app, KeyCode::Char(c));
        }
        key(&mut app, KeyCode::Enter);
        assert!(
            app.status.contains("unknown command :bogus"),
            "{}",
            app.status
        );
        assert!(app.input.is_none());
    }

    #[test]
    fn timeline_lists_events_and_enter_jumps_to_the_offset() {
        let mut app = app_with(EVENTS_TRACE);
        app.follow = false;

        key(&mut app, KeyCode::Tab);
        assert!(app.timeline);
        let rows = app.rows();
        assert_eq!(rows.len(), 3, "marking, bailout, bailout end");

        // Move onto the deopt event; the viewport shows the bailout block
        // with the cursor on the event line (the 6.5 panel).
        key(&mut app, KeyCode::Char('j'));
        assert_eq!(app.rows()[app.selected], Row::Event(1));
        let vm = app.view_model();
        match &vm {
            ViewModel::Plain { range } => assert_eq!(
                range.clone(),
                6..11,
                "bailout begin through bailout end, verbose block included"
            ),
            _ => panic!("event views are plain"),
        }
        assert_eq!(vm.line_at(app.cursor), Some(6));

        // Enter: correlation puts us in f's Maglev graph at bytecode offset 2.
        key(&mut app, KeyCode::Enter);
        assert!(!app.timeline);
        assert_eq!(app.focus, Pane::Viewport);
        let vm = app.view_model();
        assert_eq!(vm.line_at(app.cursor), Some(4), "the `2 :` bytecode line");
        assert!(app.status.contains("offset 2"), "{}", app.status);

        // Ctrl+O returns to the timeline, on the event row.
        ctrl(&mut app, 'o');
        assert!(app.timeline);
        assert_eq!(app.rows()[app.selected], Row::Event(1));
    }

    #[test]
    fn deopts_command_filters_the_timeline() {
        let mut app = app_with(EVENTS_TRACE);
        app.follow = false;
        key(&mut app, KeyCode::Char(':'));
        for c in "deopts".chars() {
            key(&mut app, KeyCode::Char(c));
        }
        key(&mut app, KeyCode::Enter);
        assert!(app.timeline && app.timeline_deopts_only);
        assert_eq!(app.rows(), vec![Row::Event(1)]);
        assert!(app.status.contains("1 deopt event"), "{}", app.status);

        // Tab back out clears the narrowing.
        key(&mut app, KeyCode::Tab);
        key(&mut app, KeyCode::Tab);
        assert_eq!(app.rows().len(), 3);
    }

    #[test]
    fn function_command_filters_the_sidebar() {
        let mut app = app_with(TRACE);
        app.follow = false;
        key(&mut app, KeyCode::Char(':'));
        for c in "function g".chars() {
            key(&mut app, KeyCode::Char(c));
        }
        key(&mut app, KeyCode::Enter);
        assert_eq!(app.rows(), vec![Row::Compilation(1)]);
        key(&mut app, KeyCode::Char(':'));
        for c in "function".chars() {
            key(&mut app, KeyCode::Char(c));
        }
        key(&mut app, KeyCode::Enter);
        assert_eq!(app.rows().len(), 3, "no argument clears the filter");
    }

    #[test]
    fn marking_event_jump_binds_forward_to_the_dump() {
        let mut app = app_with(EVENTS_TRACE);
        app.follow = false;
        key(&mut app, KeyCode::Tab);
        // Row 0 is the marking event at line 0, before the compilation.
        key(&mut app, KeyCode::Enter);
        assert!(!app.timeline);
        let rows = app.rows();
        assert_eq!(rows[app.selected], Row::Compilation(0));
    }

    #[test]
    fn lens_cleared_when_a_jump_targets_a_hidden_line() {
        let mut app = app_with(TRACE);
        app.follow = false;
        app.selected = 1;
        app.focus = Pane::Viewport;
        // `:phi` hides everything in this graph except headers.
        app.lens = Some(Lens::Phi);
        let vm = app.view_model();
        assert!(vm.len() < 9);
        // A search for a hidden node line still lands: the jump wins.
        app.goto_line(5); // `2: Bar [n1]`
        assert_eq!(app.lens, None);
        let vm = app.view_model();
        assert_eq!(vm.line_at(app.cursor), Some(5));
    }
}
