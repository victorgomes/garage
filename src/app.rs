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
use crossterm::event::{Event as CtEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use regex::Regex;

use crate::config::{Action, Keymap};
use crate::event::Event;
use crate::index::TraceIndex;
use crate::model::{Addr, IRNode, LineInfo, NodeId};
use crate::parse::ParseCache;
use crate::parse::maglev::line_text;
use crate::source::{LogBuffer, LogSource, SourceEvent};
use crate::terminal::TerminalGuard;
use crate::view::{FoldKey, MODEL_LIMIT, RowKind, ViewModel, model_rows};

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

/// What the input line is collecting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Prompt {
    Search,
    Filter,
    /// `E`: filename to export the current view to (TODO 5.3).
    Export,
}

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
    /// Jump history for Ctrl+O / Ctrl+I: (selected row, cursor row).
    jumps: Vec<(usize, usize)>,
    jump_at: usize,
    cycle: Option<Cycle>,
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
            None => {
                return ViewModel::Plain {
                    range: 0..self.sources[self.active].buffer.line_count(),
                };
            }
        };

        let source = &mut self.sources[self.active];
        let section = &source.index.compilations[comp];
        if section.lines.len() > MODEL_LIMIT {
            return ViewModel::Plain {
                range: section.lines.clone(),
            };
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
        );
        ViewModel::Modeled { rows, parsed, comp }
    }

    /// The node defined on the cursor row, for tracking and jumps (TODO 4.3).
    pub fn cursor_node(&self, vm: &ViewModel) -> Option<IRNode> {
        let row = vm.row(self.cursor)?;
        let (p, i) = row.info?;
        let parsed = vm.parsed()?;
        match parsed.phases.get(p)?.infos.get(i)? {
            LineInfo::Node(node) => Some(node.clone()),
            _ => None,
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

        if self.input.is_some() {
            self.handle_input_key(key);
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
            if re.is_match(&line_text(buffer, line)) {
                self.cursor = at as usize;
                self.follow = false;
                self.status = format!("match at L{}", line + 1);
                return;
            }
        }
        self.status = format!("no match for {}", re.as_str());
    }

    // -----------------------------------------------------------------------
    // Node jumps (TODO 4.5)
    // -----------------------------------------------------------------------

    /// `i`: jump to the definition of the cursor node's inputs, cycling
    /// through them on repeat.
    fn jump_to_input(&mut self) {
        let vm = self.view_model();
        let Some(node) = self.cursor_node(&vm) else {
            self.status = "no node under cursor".to_string();
            return;
        };
        let Some(row) = vm.row(self.cursor) else {
            return;
        };
        let Some((p, _)) = row.info else { return };
        let Some(parsed) = vm.parsed() else { return };

        let phase = &parsed.phases[p];
        let phase_start = self.phase_start(&vm, p);
        let targets: Vec<usize> = node
            .inputs
            .iter()
            .filter_map(|r| phase.defs.get(&r.node))
            .map(|&info_idx| phase_start + info_idx as usize)
            .collect();
        if targets.is_empty() {
            self.status = format!("n{} has no inputs defined in this phase", node.id);
            return;
        }
        self.cycle_jump(node.id, targets, "input");
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

    fn push_history(&mut self) {
        self.jumps.truncate(self.jump_at);
        self.jumps.push((self.selected, self.cursor));
        self.jump_at = self.jumps.len();
    }

    fn history_step(&mut self, direction: isize) {
        if direction < 0 {
            if self.jump_at == 0 {
                self.status = "at oldest jump".to_string();
                return;
            }
            // Record where we are so Ctrl+I can come back.
            if self.jump_at == self.jumps.len() {
                self.jumps.push((self.selected, self.cursor));
            }
            self.jump_at -= 1;
        } else {
            if self.jump_at + 1 >= self.jumps.len() {
                self.status = "at newest jump".to_string();
                return;
            }
            self.jump_at += 1;
        }
        let (selected, cursor) = self.jumps[self.jump_at];
        self.selected = selected;
        self.cursor = cursor;
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
                // Streaming: stick to the newest section's end. The cursor
                // pin to the last display row happens at render time, where
                // the view model is built anyway.
                self.selected = rows.saturating_sub(1);
            }
        }
    }
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
        key(&mut app, KeyCode::Char('i'));
        // Cursor is now on `1: Foo`; a fresh cycle from n1's line... the
        // cursor node changed, so `i` restarts from Foo — which has no inputs.
        // Instead test Ctrl+O returns to the jump origin.
        ctrl(&mut app, 'o');
        let vm = app.view_model();
        assert_eq!(vm.line_at(app.cursor), Some(9), "Ctrl+O returns");

        // Consumers of n1: Bar (line 5) and Quux (line 9).
        app.cursor = 3; // on `1: Foo`
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
}
