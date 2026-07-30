//! Layout and rendering (Phases 3–4).
//!
//! ```text
//! ┌ telemetry bar ─────────────────────────────────┐
//! │ sidebar          │ viewport                    │
//! └ status line ───────────────────────────────────┘
//! ```
//!
//! The viewport paints per *span*, not per regex-on-view: token classes come
//! from the parse (opcode/input/target spans recorded by Phase 2), def-use
//! highlighting comes from the parsed graph's def/use maps (TODO 4.4), and
//! search matches overlay both. Styling is a per-byte class array per visible
//! line, run-length encoded into ratatui spans — O(line length), no
//! allocation proportional to the file.

use std::collections::HashSet;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

use crate::app::{App, COMMANDS, Pane, Prompt, Row, SourceState, SplitDir, event_summary};
use crate::model::{EventKind, LineInfo, NodeId, ParsedCompilation, PhaseKind, Tier};
use crate::parse::maglev::line_text;
use crate::view::{RowKind, ViewModel, is_control_opcode, is_guard_opcode, is_phi_opcode};

const BAR_BG: Color = Color::Indexed(236);
const DIM: Color = Color::Indexed(244);
const ACCENT: Color = Color::Cyan;
const CURSOR_BG: Color = Color::Indexed(237);

pub fn render(frame: &mut Frame, app: &mut App) {
    let [telemetry, body, status] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    let sidebar_width = (frame.area().width / 3).clamp(24, 44);
    let [sidebar, viewport] =
        Layout::horizontal([Constraint::Length(sidebar_width), Constraint::Min(1)]).areas(body);

    // Pane areas: the split halves the viewport region (TODO 7.1).
    let (area0, area1) = match app.split {
        None => (viewport, None),
        Some(SplitDir::Vertical) => {
            let [a, b] = Layout::horizontal([Constraint::Percentage(50), Constraint::Min(1)])
                .areas(viewport);
            (a, Some(b))
        }
        Some(SplitDir::Horizontal) => {
            let [a, b] =
                Layout::vertical([Constraint::Percentage(50), Constraint::Min(1)]).areas(viewport);
            (a, Some(b))
        }
    };

    // Heights recorded for paging, rects for mouse routing: they are
    // properties of the frame, and the frame is the only thing that knows
    // them.
    let rect_of = |r: Rect| crate::app::PaneRect {
        x: r.x,
        y: r.y,
        width: r.width,
        height: r.height,
    };
    app.sidebar_height = sidebar.height as usize;
    app.sidebar_rect = rect_of(sidebar);
    app.viewport_rect = rect_of(area0);
    app.viewport2_rect = area1.map(rect_of).unwrap_or_default();
    let active_area = if app.active_pane == 1 {
        area1.unwrap_or(area0)
    } else {
        area0
    };
    // Split panes spend one row on their title border; paging math must use
    // the content height (found in review).
    app.viewport_height =
        (active_area.height as usize).saturating_sub(if app.split.is_some() { 1 } else { 0 });

    // The diff model revalidates its two sides; losing one (a pane moved off
    // a graph phase) drops out of diff mode rather than showing a stale diff.
    let diff_model = app.diff_model();
    if app.diff && diff_model.is_none() {
        app.diff = false;
    }

    // One view model per frame: it drives the viewport, the status line, and
    // the follow pin.
    let vm = app.view_model();
    if app.follow && !app.diff {
        app.cursor = vm.len().saturating_sub(1);
    }

    frame.render_widget(telemetry_bar(app), telemetry);
    render_sidebar(frame, app, sidebar);

    match (&diff_model, area1) {
        (Some(model), Some(area1)) => render_diff(frame, app, model, area0, area1),
        _ => {
            let mut state = app.view_state();
            render_viewport(
                frame,
                app,
                &vm,
                active_area,
                &mut state,
                true,
                app.split.map(|_| app.active_pane),
            );
            app.set_view_state(state);
            if let Some(a1) = area1 {
                let inactive_area = if app.active_pane == 1 { area0 } else { a1 };
                let mut other = app.other_view;
                let ovm = app.view_model_for(other.selected);
                let inactive_pane = 1 - app.active_pane;
                render_viewport(
                    frame,
                    app,
                    &ovm,
                    inactive_area,
                    &mut other,
                    false,
                    Some(inactive_pane),
                );
                app.other_view = other;
            }
        }
    }

    frame.render_widget(status_line(app, &vm, diff_model.as_deref()), status);

    if app.help {
        render_help(frame, app, frame.area());
    }
}

// ---------------------------------------------------------------------------
// Telemetry bar (TODO 3.2)
// ---------------------------------------------------------------------------

fn telemetry_bar(app: &App) -> Paragraph<'static> {
    let source = app.active_source();
    let idx = &source.index;

    let state = match &source.state {
        SourceState::Loading => Span::styled(" reading ", Style::new().fg(Color::Yellow)),
        SourceState::Complete => Span::styled(" ok ", Style::new().fg(Color::Green)),
        SourceState::Failed(_) => Span::styled(" failed ", Style::new().fg(Color::Red)),
    };

    let mut maglev = 0usize;
    let mut turbolev = 0usize;
    let mut turbofan = 0usize;
    let mut ignition = 0usize;
    let mut other = 0usize;
    let mut osr = 0usize;
    for c in &idx.compilations {
        match c.key.tier {
            Tier::Maglev => maglev += 1,
            Tier::Turbolev => turbolev += 1,
            Tier::Turbofan => turbofan += 1,
            Tier::Ignition => ignition += 1,
            _ => other += 1,
        }
        if c.osr.is_some() {
            osr += 1;
        }
    }
    let deopts = idx
        .events
        .iter()
        .filter(|e| matches!(e.kind, crate::model::EventKind::DeoptBegin { .. }))
        .count();

    let mut spans = vec![
        Span::styled(" garage ", Style::new().add_modifier(Modifier::BOLD)),
        Span::styled(source.label.clone(), Style::new().fg(ACCENT)),
        state,
    ];

    let mut stats = String::new();
    if !idx.compilations.is_empty() {
        stats.push_str(&format!(" {} compilations", idx.compilations.len()));
        let mut tiers = Vec::new();
        if maglev > 0 {
            tiers.push(format!("{maglev} Maglev"));
        }
        if turbolev > 0 {
            tiers.push(format!("{turbolev} Turbolev"));
        }
        if turbofan > 0 {
            tiers.push(format!("{turbofan} Turbofan"));
        }
        if ignition > 0 {
            tiers.push(format!("{ignition} Ignition"));
        }
        if other > 0 {
            tiers.push(format!("{other} other"));
        }
        if !tiers.is_empty() {
            stats.push_str(&format!(" ({})", tiers.join(", ")));
        }
        if osr > 0 {
            stats.push_str(&format!(" · {osr} OSR"));
        }
    }
    if deopts > 0 {
        stats.push_str(&format!(" · {deopts} deopts"));
    }
    if let Some(v) = &idx.detected_version {
        stats.push_str(&format!(" · V8 {v}"));
    }
    spans.push(Span::raw(stats));

    if app.sources.len() > 1 {
        let hint = app
            .keys
            .chord_hint(crate::config::Action::NextSource)
            .unwrap_or_default();
        spans.push(Span::styled(
            format!("  [{}/{} {hint}]", app.active + 1, app.sources.len()),
            Style::new().fg(DIM),
        ));
    }

    Paragraph::new(Line::from(spans)).block(Block::new().style(Style::new().bg(BAR_BG)))
}

// ---------------------------------------------------------------------------
// Sidebar (TODO 3.3, 4.7)
// ---------------------------------------------------------------------------

fn render_sidebar(frame: &mut Frame, app: &mut App, area: Rect) {
    let focused = app.focus == Pane::Sidebar;
    let rows = app.rows();

    let height = area.height as usize;
    if height > 0 {
        if app.selected < app.sidebar_scroll {
            app.sidebar_scroll = app.selected;
        }
        if app.selected >= app.sidebar_scroll + height {
            app.sidebar_scroll = app.selected + 1 - height;
        }
    }

    let mut lines = Vec::with_capacity(height);
    for (at, row) in rows
        .iter()
        .enumerate()
        .skip(app.sidebar_scroll)
        .take(height)
    {
        lines.push(sidebar_row(app, row, at == app.selected, focused));
    }
    if rows.is_empty() {
        let text = if app.sidebar_filter.is_some() {
            " (nothing matches the filter)"
        } else {
            match app.active_source().state {
                SourceState::Loading => " waiting for input…",
                _ => " (no sections)",
            }
        };
        lines.push(Line::from(Span::styled(text, Style::new().fg(DIM))));
    }

    let border_style = if focused {
        Style::new().fg(ACCENT)
    } else {
        Style::new().fg(DIM)
    };
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::new()
                .borders(Borders::RIGHT)
                .border_style(border_style),
        ),
        area,
    );

    render_scrollbar(frame, area, app.sidebar_scroll, rows.len(), height);
}

/// A minimal proportional scrollbar on the right edge (TODO 3.3).
fn render_scrollbar(frame: &mut Frame, area: Rect, scroll: usize, total: usize, height: usize) {
    if total <= height || height == 0 || area.width == 0 {
        return;
    }
    let bar_x = area.right() - 1;
    let thumb_len = ((height * height) / total).max(1) as u16;
    let denom = total - height;
    let thumb_top =
        area.top() + (((area.height - thumb_len) as usize * scroll.min(denom)) / denom) as u16;
    for y in thumb_top..(thumb_top + thumb_len).min(area.bottom()) {
        frame.render_widget(
            Paragraph::new(Span::styled("█", Style::new().fg(ACCENT))),
            Rect::new(bar_x, y, 1, 1),
        );
    }
}

fn sidebar_row(app: &App, row: &Row, selected: bool, focused: bool) -> Line<'static> {
    let source = app.active_source();
    let idx = &source.index;

    let mut spans: Vec<Span<'static>> = Vec::new();
    match row {
        Row::Function {
            sfi,
            name_comp,
            count,
        } => {
            let c = &idx.compilations[*name_comp];
            let open = app.group_expanded(sfi.0);
            spans.push(Span::raw(if open { "▾ " } else { "▸ " }));
            spans.push(Span::styled(
                c.display_name().to_string(),
                Style::new().add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::styled(format!(" ×{count}"), Style::new().fg(DIM)));
        }
        Row::Compilation(i) => {
            let c = &idx.compilations[*i];
            let open = app.compilation_expanded(*i);
            let indent = if app.grouped { "  " } else { "" };
            spans.push(Span::raw(format!(
                "{indent}{} ",
                if open { "▾" } else { "▸" }
            )));
            spans.push(Span::raw(c.display_name().to_string()));
            spans.push(Span::styled(
                format!(" {} #{}", c.key.tier.label(), c.key.ordinal),
                Style::new().fg(DIM),
            ));
            if let Some(osr) = &c.osr {
                let text = match osr.offset {
                    Some(offset) => format!(" OSR@{offset}"),
                    None => " OSR".to_string(),
                };
                spans.push(Span::styled(text, Style::new().fg(Color::Yellow)));
            }
            // J5: a renamed banner degrades to an unrecognised phase, and the
            // sidebar says so instead of hiding it.
            if c.phases
                .iter()
                .any(|p| matches!(p.kind, PhaseKind::Graph { known: false }))
            {
                spans.push(Span::styled(" [unparsed]", Style::new().fg(Color::Red)));
            }
        }
        Row::Phase { comp, phase } => {
            let p = &idx.compilations[*comp].phases[*phase];
            let indent = if app.grouped { "      " } else { "    " };
            let (marker, style) = match &p.kind {
                PhaseKind::Graph { known: true } => ("· ", Style::new()),
                PhaseKind::Graph { known: false } => ("? ", Style::new().fg(Color::Red)),
                PhaseKind::Bytecode => ("≡ ", Style::new().fg(DIM)),
                PhaseKind::Inlining { .. } => ("↳ ", Style::new().fg(DIM)),
                PhaseKind::Listing => ("≣ ", Style::new().fg(DIM)),
            };
            let label = match &p.kind {
                PhaseKind::Inlining { callee, .. } => format!("inline {callee}"),
                _ => p.name.clone(),
            };
            spans.push(Span::raw(indent));
            spans.push(Span::styled(format!("{marker}{label}"), style));
        }
        Row::Raw(i) => {
            let r = &idx.raw[*i];
            let label = if r.label.is_empty() {
                format!("(raw, {} lines)", r.lines.len())
            } else {
                r.label.clone()
            };
            spans.push(Span::styled("~ ", Style::new().fg(DIM)));
            spans.push(Span::styled(label, Style::new().fg(DIM)));
        }
        Row::Event(i) => {
            // Severity colours (TODO 6.3): deopts red, completions green,
            // OSR yellow, bookkeeping dim.
            let event = &idx.events[*i];
            let style = match &event.kind {
                EventKind::DeoptBegin { .. } => {
                    Style::new().fg(Color::Red).add_modifier(Modifier::BOLD)
                }
                EventKind::DeoptEnd { invalidated: true } => Style::new().fg(Color::Red),
                EventKind::DeoptEnd { invalidated: false } => Style::new().fg(DIM),
                EventKind::CompileDone { .. } => Style::new().fg(Color::Green),
                EventKind::Osr { .. } => Style::new().fg(Color::Yellow),
                EventKind::Marking { .. } | EventKind::CompileStart { .. } => Style::new().fg(DIM),
            };
            spans.push(Span::styled(
                format!("#{:03} ", i + 1),
                Style::new().fg(DIM),
            ));
            spans.push(Span::styled(event_summary(&event.kind), style));
        }
    }

    let mut line = Line::from(spans);
    if selected {
        let bg = if focused {
            Color::Indexed(238)
        } else {
            Color::Indexed(236)
        };
        line = line.style(Style::new().bg(bg));
    }
    line
}

// ---------------------------------------------------------------------------
// Token classes and palette (TODO 4.1)
// ---------------------------------------------------------------------------

/// Byte-level style class. Painted in layers: base classes from the parse,
/// then def-use, then search — later layers win.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum Class {
    Base,
    Dim,
    Annotation,
    Banner,
    BlockHeader,
    BytecodeLine,
    FrameLine,
    Opcode,
    Guard,
    ControlOp,
    ConstantOp,
    PhiOp,
    CallOp,
    NodeDef,
    NodeRef,
    BlockRef,
    DefHl,
    InputHl,
    ConsumerHl,
    SearchHl,
}

/// Colour-depth detection (TODO 4.1): 256-colour indexes where available,
/// named ANSI colours otherwise. Truecolor terminals accept both.
#[derive(Debug, Clone, Copy)]
pub struct Palette {
    indexed: bool,
}

impl Palette {
    pub fn detect() -> Self {
        let term = std::env::var("TERM").unwrap_or_default();
        let colorterm = std::env::var("COLORTERM").unwrap_or_default();
        Palette {
            indexed: term.contains("256") || term.contains("color") || !colorterm.is_empty(),
        }
    }

    fn dim(&self) -> Color {
        if self.indexed { DIM } else { Color::DarkGray }
    }

    fn style(&self, class: Class) -> Style {
        match class {
            Class::Base => Style::new(),
            Class::Dim => Style::new().fg(self.dim()),
            Class::Annotation => Style::new().fg(self.dim()).add_modifier(Modifier::ITALIC),
            Class::Banner => Style::new().add_modifier(Modifier::BOLD),
            Class::BlockHeader => Style::new().fg(Color::Blue).add_modifier(Modifier::BOLD),
            Class::BytecodeLine => Style::new()
                .fg(if self.indexed {
                    Color::Indexed(108)
                } else {
                    Color::Green
                })
                .add_modifier(Modifier::DIM),
            Class::FrameLine => Style::new()
                .fg(if self.indexed {
                    Color::Indexed(131)
                } else {
                    Color::Red
                })
                .add_modifier(Modifier::DIM),
            Class::Opcode => Style::new().add_modifier(Modifier::BOLD),
            Class::Guard => Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
            Class::ControlOp => Style::new().fg(Color::Blue).add_modifier(Modifier::BOLD),
            Class::ConstantOp => Style::new().fg(Color::Magenta),
            Class::PhiOp => Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            Class::CallOp => Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            Class::NodeDef => Style::new().fg(Color::Green).add_modifier(Modifier::BOLD),
            Class::NodeRef => Style::new().fg(Color::Green),
            Class::BlockRef => Style::new().fg(Color::Blue),
            Class::DefHl => Style::new()
                .fg(Color::Black)
                .bg(Color::Green)
                .add_modifier(Modifier::BOLD),
            Class::InputHl => Style::new()
                .fg(Color::Black)
                .bg(Color::Blue)
                .add_modifier(Modifier::BOLD),
            Class::ConsumerHl => Style::new()
                .fg(Color::Black)
                .bg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
            Class::SearchHl => Style::new().add_modifier(Modifier::REVERSED),
        }
    }
}

/// Opcode → class, by name shape. The vocabulary is open (V8 adds opcodes
/// freely), so this matches on structure, not a fixed list. The guard /
/// control / phi shape tests live in `view` so the `:checks` and `:phi`
/// lenses count exactly what gets coloured here.
fn opcode_class(name: &str) -> Class {
    if name.starts_with("GapMove") || name.starts_with("ConstantGapMove") {
        return Class::Dim;
    }
    if is_phi_opcode(name) {
        return Class::PhiOp;
    }
    if is_guard_opcode(name) {
        return Class::Guard;
    }
    if is_control_opcode(name) {
        return Class::ControlOp;
    }
    if name.contains("Constant") {
        return Class::ConstantOp;
    }
    if name.starts_with("Call") || name.starts_with("Construct") {
        return Class::CallOp;
    }
    Class::Opcode
}

/// Shape-based styling for lines the graph parser never sees — raw sections,
/// which is where lifecycle events and the `--trace-deopt-verbose` frame
/// dumps live (TODO 6.5). Display-only: a stray program line that happens to
/// match one of these shapes just picks up a harmless colour.
fn raw_line_class(text: &str) -> Option<Class> {
    let t = text.trim_start();
    if t.starts_with("[bailout (") {
        return Some(Class::Guard);
    }
    if t.starts_with("[bailout end") {
        return Some(Class::FrameLine);
    }
    if t.starts_with(";;; deoptimize at") {
        return Some(Class::Annotation);
    }
    if t.starts_with("reading input frame")
        || t.starts_with("reading output frame")
        || t.starts_with("translating ")
        || t.starts_with("materialization ")
    {
        return Some(Class::Banner);
    }
    if t.starts_with("[marking ")
        || t.starts_with("[compiling ")
        || t.starts_with("[completed ")
        || t.starts_with("[OSR ")
    {
        return Some(Class::Dim);
    }
    // `      3: 0x… ; x5 0x… <HeapNumber 5000050000.0>` — frame input rows.
    if let Some((index, rest)) = t.split_once(": ")
        && !index.is_empty()
        && index.bytes().all(|b| b.is_ascii_digit())
        && (rest.starts_with("0x") || rest.starts_with("(optimized out)"))
    {
        return Some(Class::BytecodeLine);
    }
    // `    0x…: [top + 96] <- value ; comment` — frame translation rows.
    if t.starts_with("0x") && t.contains("] <- ") {
        return Some(Class::Dim);
    }
    None
}

/// The cursor node's def-use context (TODO 4.4), computed once per frame.
struct DefUse {
    node: NodeId,
    inputs: HashSet<NodeId>,
}

// ---------------------------------------------------------------------------
// Viewport (TODO 3.4, 4.x)
// ---------------------------------------------------------------------------

/// Renders one viewport pane. `state` is the pane's own navigation state
/// (clamping writes back into it); `active` says whether this pane owns the
/// cursor; `pane_title` labels split panes with what they show.
fn render_viewport(
    frame: &mut Frame,
    app: &App,
    vm: &ViewModel,
    area: Rect,
    state: &mut crate::app::ViewState,
    active: bool,
    pane_title: Option<usize>,
) {
    // Split panes get a title bar naming their content; single view doesn't
    // spend the row.
    let area = match pane_title {
        Some(_) => {
            let title = format!(
                " {}{} ",
                app.view_title_for(state.selected),
                if active { " ●" } else { "" }
            );
            let block = Block::new()
                .borders(Borders::TOP)
                .border_style(Style::new().fg(if active { ACCENT } else { DIM }))
                .title(title);
            let inner = block.inner(area);
            frame.render_widget(block, area);
            inner
        }
        None => area,
    };

    let height = area.height as usize;
    let len = vm.len();
    state.cursor = state.cursor.min(len.saturating_sub(1));

    if state.top >= len {
        state.top = 0;
    }
    if state.cursor < state.top {
        state.top = state.cursor;
    }
    if height > 0 && state.cursor >= state.top + height {
        state.top = state.cursor + 1 - height;
    }

    let palette = Palette::detect();
    let defuse = if active {
        crate::app::node_at(vm, state.cursor).map(|node| DefUse {
            node: node.id,
            inputs: node.inputs.iter().map(|r| r.node).collect(),
        })
    } else {
        None
    };

    let source = app.active_source();
    let last_line = vm.line_at(len.saturating_sub(1)).unwrap_or(0);
    let number_width = digits(last_line + 1);
    let mut lines = Vec::with_capacity(height);

    for row_idx in state.top..len.min(state.top + height) {
        let Some(row) = vm.row(row_idx) else { break };
        let cursor_here = active && row_idx == state.cursor && app.focus == Pane::Viewport;

        let number_style = if cursor_here {
            Style::new().fg(ACCENT)
        } else {
            Style::new().fg(DIM)
        };
        let mut spans = vec![Span::styled(
            format!("{:>number_width$} ", row.line + 1),
            number_style,
        )];

        match row.kind {
            RowKind::BlockFold { block, hidden } => {
                spans.push(Span::styled(
                    format!("[+] b{block} — {hidden} lines hidden"),
                    palette.style(Class::BlockHeader),
                ));
            }
            RowKind::AnnotationFold { count } => {
                spans.push(Span::styled(
                    format!(
                        "[+] {count} trace line{} (t to show)",
                        if count == 1 { "" } else { "s" }
                    ),
                    palette.style(Class::Annotation),
                ));
            }
            RowKind::Text => {
                let text = line_text(&source.buffer, row.line);
                let info = row.info.and_then(|(p, i)| {
                    vm.parsed()
                        .and_then(|parsed| parsed.phases.get(p))
                        .and_then(|phase| phase.infos.get(i))
                });
                spans.extend(paint_line(
                    &text,
                    info,
                    vm.parsed().map(|p| p.as_ref()),
                    defuse.as_ref(),
                    app.search.as_ref(),
                    &palette,
                    state.scroll_x,
                ));
            }
        }

        let mut line = Line::from(spans);
        if cursor_here {
            line = line.style(Style::new().bg(CURSOR_BG));
        }
        lines.push(line);
    }

    if lines.is_empty() {
        lines.push(Line::from(Span::styled(
            "  (empty section)",
            Style::new().fg(DIM),
        )));
    }

    let mut paragraph = Paragraph::new(lines);
    if app.wrap {
        paragraph = paragraph.wrap(Wrap { trim: false });
    }
    frame.render_widget(paragraph, area);
}

// ---------------------------------------------------------------------------
// Phase diff rendering (TODO 7.4)
// ---------------------------------------------------------------------------

/// Row background tints for diff statuses. 256-colour only; the 16-colour
/// fallback relies on the gutter symbols alone.
fn diff_tint(status: &crate::diff::RowStatus, palette: &Palette) -> Option<Color> {
    use crate::diff::RowStatus;
    if !palette.indexed {
        return None;
    }
    match status {
        RowStatus::Same => None,
        RowStatus::Added => Some(Color::Indexed(22)),
        RowStatus::Deleted => Some(Color::Indexed(52)),
        RowStatus::Changed { .. } => Some(Color::Indexed(58)),
        RowStatus::Replaced { .. } => Some(Color::Indexed(53)),
        RowStatus::Moved { .. } => Some(Color::Indexed(23)),
    }
}

fn diff_gutter_style(status: &crate::diff::RowStatus) -> Style {
    use crate::diff::RowStatus;
    match status {
        RowStatus::Same => Style::new().fg(DIM),
        RowStatus::Added => Style::new().fg(Color::Green).add_modifier(Modifier::BOLD),
        RowStatus::Deleted => Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
        RowStatus::Changed { .. } => Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        RowStatus::Replaced { .. } => Style::new().fg(Color::Magenta).add_modifier(Modifier::BOLD),
        RowStatus::Moved { .. } => Style::new().fg(Color::Cyan),
    }
}

/// The aligned two-column diff view: both panes walk the same row list, so
/// scrolling is synced by construction (PLAN §7.4). The shared cursor lives
/// in the app's flat fields.
fn render_diff(
    frame: &mut Frame,
    app: &mut App,
    model: &crate::diff::DiffModel,
    area0: Rect,
    area1: Rect,
) {
    let palette = Palette::detect();
    let len = model.rows.len();
    app.cursor = app.cursor.min(len.saturating_sub(1));

    // Clamp against the smaller pane so the cursor stays visible in both.
    let height = (area0.height.min(area1.height) as usize).saturating_sub(1);
    if app.top >= len {
        app.top = 0;
    }
    if app.cursor < app.top {
        app.top = app.cursor;
    }
    if height > 0 && app.cursor >= app.top + height {
        app.top = app.cursor + 1 - height;
    }

    for (pane, area) in [(0, area0), (1, area1)] {
        let (side, own) = if pane == 0 {
            (
                &model.left,
                model.rows.iter().map(|r| r.left).collect::<Vec<_>>(),
            )
        } else {
            (
                &model.right,
                model.rows.iter().map(|r| r.right).collect::<Vec<_>>(),
            )
        };
        let own = own.as_slice();
        let active = pane == app.active_pane;
        let title_selected = app.pane_state(pane).selected;

        let title = format!(
            " {}{} ",
            app.view_title_for(title_selected),
            if active { " ●" } else { "" }
        );
        let block = Block::new()
            .borders(Borders::TOP)
            .border_style(Style::new().fg(if active { ACCENT } else { DIM }))
            .title(title);
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let defuse = app.diff_cursor_node(model, pane).map(|node| DefUse {
            node: node.id,
            inputs: node.inputs.iter().map(|r| r.node).collect(),
        });

        let source = app.active_source();
        let number_width = digits(source.buffer.line_count().max(1));
        let mut lines = Vec::with_capacity(inner.height as usize);

        let visible = app.top..len.min(app.top + inner.height as usize);
        for (i, own_idx) in own[visible.clone()].iter().enumerate() {
            let i = i + visible.start;
            let own_idx = *own_idx;
            let diff_row = &model.rows[i];
            let cursor_here = i == app.cursor && app.focus == Pane::Viewport;
            let mut spans = vec![Span::styled(
                crate::app::diff_gutter(&diff_row.status).to_string(),
                diff_gutter_style(&diff_row.status),
            )];

            match own_idx.and_then(|idx| side.rows.get(idx)) {
                Some(row) => {
                    spans.push(Span::styled(
                        format!("{:>number_width$} ", row.line + 1),
                        Style::new().fg(if cursor_here { ACCENT } else { DIM }),
                    ));
                    let text = line_text(&source.buffer, row.line);
                    let info = row.info.and_then(|(p, j)| {
                        side.parsed
                            .phases
                            .get(p)
                            .and_then(|phase| phase.infos.get(j))
                    });
                    spans.extend(paint_line(
                        &text,
                        info,
                        Some(side.parsed.as_ref()),
                        defuse.as_ref(),
                        app.search.as_ref(),
                        &palette,
                        app.scroll_x,
                    ));
                }
                None => {
                    spans.push(Span::styled(
                        "·".repeat(inner.width.saturating_sub(2) as usize),
                        Style::new().fg(Color::Indexed(238)),
                    ));
                }
            }

            let mut line = Line::from(spans);
            if cursor_here {
                line = line.style(Style::new().bg(CURSOR_BG));
            } else if let Some(tint) = diff_tint(&diff_row.status, &palette) {
                line = line.style(Style::new().bg(tint));
            }
            lines.push(line);
        }

        if lines.is_empty() {
            lines.push(Line::from(Span::styled(
                "  (no rows)",
                Style::new().fg(DIM),
            )));
        }
        frame.render_widget(Paragraph::new(lines), inner);
    }
}

/// Builds the styled spans for one line: a per-byte class array painted in
/// layers, then run-length encoded at char granularity.
fn paint_line(
    text: &str,
    info: Option<&LineInfo>,
    parsed: Option<&ParsedCompilation>,
    defuse: Option<&DefUse>,
    search: Option<&regex::Regex>,
    palette: &Palette,
    skip_chars: usize,
) -> Vec<Span<'static>> {
    let mut paint = vec![Class::Base; text.len()];

    let fill = |range: &std::ops::Range<u32>, class: Class, paint: &mut Vec<Class>| {
        let start = range.start as usize;
        let end = (range.end as usize).min(paint.len());
        if start < end {
            paint[start..end].fill(class);
        }
    };

    match info {
        Some(LineInfo::Node(node)) => {
            fill(&node.def_span, Class::NodeDef, &mut paint);
            let opcode_name = parsed
                .and_then(|p| p.opcodes.get(node.opcode as usize))
                .map(String::as_str)
                .unwrap_or("");
            fill(&node.opcode_span, opcode_class(opcode_name), &mut paint);
            for input in &node.inputs {
                fill(&input.span, Class::NodeRef, &mut paint);
            }
            for target in &node.targets {
                fill(&target.span, Class::BlockRef, &mut paint);
            }
        }
        Some(LineInfo::Frame { refs, .. }) => {
            paint.fill(Class::FrameLine);
            for r in refs {
                fill(&r.span, Class::NodeRef, &mut paint);
            }
        }
        Some(LineInfo::PhiMove { refs }) => {
            paint.fill(Class::Dim);
            for r in refs {
                fill(&r.span, Class::NodeRef, &mut paint);
            }
        }
        Some(LineInfo::Bytecode { .. }) => paint.fill(Class::BytecodeLine),
        Some(LineInfo::BlockHeader { .. }) => paint.fill(Class::BlockHeader),
        Some(LineInfo::Banner) => paint.fill(Class::Banner),
        Some(LineInfo::SfiContext | LineInfo::Control | LineInfo::VirtualObjects) => {
            paint.fill(Class::Dim)
        }
        Some(LineInfo::Disasm {
            mnemonic,
            target,
            comment,
            ..
        }) => {
            // Address and encoding columns dim, mnemonic emphasised, branch
            // target and reloc comment picked out — everything else (the
            // operands) as-emitted (TODO 8.2).
            fill(&(0..mnemonic.start), Class::Dim, &mut paint);
            fill(mnemonic, Class::Opcode, &mut paint);
            if let Some(target) = target {
                fill(target, Class::BlockRef, &mut paint);
            }
            if let Some(comment) = comment {
                fill(comment, Class::Annotation, &mut paint);
            }
        }
        Some(LineInfo::Annotation { .. }) => paint.fill(Class::Annotation),
        None => {
            if let Some(class) = raw_line_class(text) {
                paint.fill(class);
            }
        }
    }

    // Def-use overlay (TODO 4.4): definition, inputs, and consumers of the
    // cursor node in distinct styles — from the parsed graph, not regex.
    if let (Some(du), Some(LineInfo::Node(node))) = (defuse, info) {
        if node.id == du.node {
            fill(&node.def_span, Class::DefHl, &mut paint);
        } else if du.inputs.contains(&node.id) {
            fill(&node.def_span, Class::InputHl, &mut paint);
        }
    }
    if let Some(du) = defuse {
        let refs: Vec<crate::model::Span> = match info {
            Some(LineInfo::Node(node)) => node
                .inputs
                .iter()
                .filter(|r| r.node == du.node)
                .map(|r| r.span.clone())
                .collect(),
            Some(LineInfo::Frame { refs, .. } | LineInfo::PhiMove { refs }) => refs
                .iter()
                .filter(|r| r.node == du.node)
                .map(|r| r.span.clone())
                .collect(),
            _ => Vec::new(),
        };
        for span in refs {
            fill(&span, Class::ConsumerHl, &mut paint);
        }
    }

    // Search overlay (TODO 4.6) — wins over everything.
    if let Some(re) = search {
        for m in re.find_iter(text) {
            let range = m.start() as u32..m.end() as u32;
            fill(&range, Class::SearchHl, &mut paint);
        }
    }

    // RLE at char granularity, honouring horizontal scroll.
    let mut spans = Vec::new();
    let mut current: Option<(Class, String)> = None;
    for (skipped, (at, ch)) in text.char_indices().enumerate() {
        if skipped < skip_chars {
            continue;
        }
        // Control characters would corrupt the terminal; width-preserving
        // replacement keeps spans aligned.
        let ch = if ch.is_control() { '\u{fffd}' } else { ch };
        let class = paint[at];
        match &mut current {
            Some((c, buf)) if *c == class => buf.push(ch),
            _ => {
                if let Some((c, buf)) = current.take() {
                    spans.push(Span::styled(buf, palette.style(c)));
                }
                current = Some((class, ch.to_string()));
            }
        }
    }
    if let Some((c, buf)) = current.take() {
        spans.push(Span::styled(buf, palette.style(c)));
    }
    spans
}

fn digits(mut n: usize) -> usize {
    let mut width = 1;
    while n >= 10 {
        n /= 10;
        width += 1;
    }
    width
}

// ---------------------------------------------------------------------------
// Status line (with the input prompt and cursor-node tracking, TODO 4.3)
// ---------------------------------------------------------------------------

fn status_line(
    app: &App,
    vm: &ViewModel,
    diff: Option<&crate::diff::DiffModel>,
) -> Paragraph<'static> {
    // An active input line takes over the whole status bar.
    if let Some(input) = &app.input {
        let prompt = match input.prompt {
            Prompt::Search => "/",
            Prompt::Filter => "filter: ",
            Prompt::Export => "export to: ",
            Prompt::Command => ":",
        };
        let mut spans = vec![
            Span::styled(
                format!(" {prompt}{}", input.buffer),
                Style::new().add_modifier(Modifier::BOLD),
            ),
            Span::styled("█", Style::new().fg(ACCENT)),
        ];
        // The palette shows its matching commands live (TODO 6.1); a unique
        // match shows that command's one-line description instead.
        if input.prompt == Prompt::Command && !input.buffer.contains(' ') {
            let matches: Vec<&(&str, &str)> = COMMANDS
                .iter()
                .filter(|(name, _)| name.starts_with(input.buffer.as_str()))
                .collect();
            let hint = match matches.as_slice() {
                [] => "no such command".to_string(),
                [(name, describe)] => format!("{name} — {describe}"),
                several => several
                    .iter()
                    .map(|(name, _)| *name)
                    .collect::<Vec<_>>()
                    .join(" "),
            };
            spans.push(Span::styled(format!("  {hint}"), Style::new().fg(DIM)));
        }
        spans.push(Span::styled(
            "  Enter apply · Esc cancel".to_string(),
            Style::new().fg(DIM),
        ));
        return Paragraph::new(Line::from(spans))
            .block(Block::new().style(Style::new().bg(BAR_BG)));
    }

    // Diff mode swaps the whole layout: summary + the cursor row's story.
    if let Some(model) = diff {
        let mut spans = vec![Span::styled(
            format!(" DIFF  {}", model.summary.describe()),
            Style::new().add_modifier(Modifier::BOLD),
        )];
        if let Some(detail) = model.describe_row(app.cursor) {
            spans.push(Span::styled(
                format!("   {detail}"),
                Style::new().fg(ACCENT),
            ));
        }
        spans.push(Span::styled(
            "   d exits · Ctrl+W other side".to_string(),
            Style::new().fg(DIM),
        ));
        return Paragraph::new(Line::from(spans))
            .block(Block::new().style(Style::new().bg(BAR_BG)));
    }

    let line = vm.line_at(app.cursor).unwrap_or(0);
    let mut left = format!(" L{}", line + 1);
    if app.timeline {
        left.push_str("  TIMELINE");
        if app.timeline_deopts_only {
            left.push_str(":deopts");
        }
    }
    if let Some(lens) = app.lens {
        left.push_str(&format!("  lens::{}", lens.name()));
    }
    if app.follow {
        left.push_str("  FOLLOW");
    }
    if app.wrap {
        left.push_str("  WRAP");
    }
    if let Some(re) = &app.sidebar_filter {
        left.push_str(&format!("  filter:{}", re.as_str()));
    }
    if let Some(re) = &app.search {
        left.push_str(&format!("  /{}", re.as_str()));
    }

    // Cursor node tracking (TODO 4.3).
    let mut node_info = String::new();
    if let Some(node) = app.cursor_node(vm) {
        if let Some(parsed) = vm.parsed() {
            let opcode = parsed
                .opcodes
                .get(node.opcode as usize)
                .map(String::as_str)
                .unwrap_or("?");
            node_info = format!("n{} {opcode} · {} in", node.id, node.inputs.len());
            if let Some(uses) = node.uses {
                node_info.push_str(&format!(" · {uses} uses"));
            }
            node_info.push_str("   ");
        }
    }

    Paragraph::new(Line::from(vec![
        Span::styled(left, Style::new().add_modifier(Modifier::BOLD)),
        Span::raw("   "),
        Span::styled(node_info, Style::new().fg(ACCENT)),
        Span::styled(app.status.clone(), Style::new().fg(Color::Gray)),
    ]))
    .block(Block::new().style(Style::new().bg(BAR_BG)))
}

// ---------------------------------------------------------------------------
// Help modal (TODO 3.6)
// ---------------------------------------------------------------------------

fn render_help(frame: &mut Frame, app: &App, screen: Rect) {
    // Two columns: the keymap outgrew a single 80×24-safe column, and a
    // silently truncated help modal is worse than none (found in review —
    // the split/diff keys were exactly the ones cut).
    let rows = app.keys.help_rows();
    let half = rows.len().div_ceil(2);
    let height = (half as u16 + 6).min(screen.height.saturating_sub(2));
    let width = 96u16.min(screen.width.saturating_sub(4));
    let area = Rect::new(
        screen.x + (screen.width.saturating_sub(width)) / 2,
        screen.y + (screen.height.saturating_sub(height)) / 2,
        width,
        height,
    );

    let key_width = rows
        .iter()
        .map(|(k, _)| k.chars().count())
        .max()
        .unwrap_or(8);
    let column = |keys: &String, action: &crate::config::Action| {
        let pad = key_width.saturating_sub(keys.chars().count());
        let describe: String = action.describe().chars().take(32).collect();
        vec![
            Span::styled(
                format!(" {}{} ", " ".repeat(pad), keys),
                Style::new().fg(ACCENT),
            ),
            Span::raw(format!("{describe:<33}")),
        ]
    };
    let mut lines = Vec::with_capacity(half + 3);
    for i in 0..half {
        let mut spans = column(&rows[i].0, &rows[i].1);
        if let Some((keys, action)) = rows.get(half + i) {
            spans.extend(column(keys, action));
        }
        lines.push(Line::from(spans));
    }
    lines.push(Line::from(Span::styled(
        " mouse: wheel scrolls · click places the cursor",
        Style::new().fg(DIM),
    )));
    lines.push(Line::from(vec![
        Span::styled(" : commands — ", Style::new().fg(DIM)),
        Span::styled(
            COMMANDS
                .iter()
                .map(|(name, _)| *name)
                .collect::<Vec<_>>()
                .join(" "),
            Style::new().fg(ACCENT),
        ),
    ]));
    lines.push(Line::from(Span::styled(
        " any key to close",
        Style::new().fg(DIM),
    )));

    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::new()
                    .borders(Borders::ALL)
                    .border_style(Style::new().fg(ACCENT))
                    .title(" keys "),
            )
            .style(Style::new().bg(Color::Indexed(235))),
        area,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opcode_classes_by_shape() {
        assert_eq!(opcode_class("CheckedSmiUntag"), Class::Guard);
        assert_eq!(opcode_class("Deopt"), Class::Guard);
        assert_eq!(opcode_class("BranchIfInt32Compare"), Class::ControlOp);
        assert_eq!(opcode_class("Jump"), Class::ControlOp);
        assert_eq!(opcode_class("HeapConstant"), Class::ConstantOp);
        assert_eq!(opcode_class("φᵀ"), Class::PhiOp);
        assert_eq!(opcode_class("CallRuntime"), Class::CallOp);
        assert_eq!(opcode_class("GapMove"), Class::Dim);
        assert_eq!(opcode_class("Int32AddWithOverflow"), Class::Opcode);
    }

    #[test]
    fn paint_line_slices_at_spans() {
        use crate::model::{IRNode, NodeRef};
        let text = "  11: Int32AddWithOverflow [n9, n10], 1 uses";
        let node = IRNode {
            id: 11,
            schedule_pos: None,
            def_span: 2..4,
            opcode: 0,
            opcode_span: 6..26,
            inputs: vec![
                NodeRef {
                    node: 9,
                    span: 28..30,
                },
                NodeRef {
                    node: 10,
                    span: 32..35,
                },
            ],
            uses: Some(1),
            dead: false,
            targets: vec![],
        };
        let parsed = ParsedCompilation {
            opcodes: vec!["Int32AddWithOverflow".to_string()],
            ..Default::default()
        };
        let info = LineInfo::Node(node);
        let palette = Palette { indexed: true };
        let spans = paint_line(text, Some(&info), Some(&parsed), None, None, &palette, 0);
        let joined: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(joined, text, "painting must not lose characters");
        assert!(spans.len() >= 5, "def/opcode/inputs painted separately");
    }

    #[test]
    fn horizontal_scroll_skips_chars_not_bytes() {
        let text = "│╭─►Block b4";
        let palette = Palette { indexed: true };
        let spans = paint_line(text, None, None, None, None, &palette, 2);
        let joined: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(joined, "─►Block b4");
    }
}
