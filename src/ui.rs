//! The Phase 3 layout (TODO 3.1–3.4, 3.6).
//!
//! ```text
//! ┌ telemetry bar ─────────────────────────────────┐
//! │ sidebar          │ viewport                    │
//! └ status line ───────────────────────────────────┘
//! ```
//!
//! Rendering never allocates per *file* line — the viewport materialises only
//! the rows on screen, which is the shape the lazy-parse design needs anyway:
//! a 500 MB source costs the same to draw as a 5 KB one.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

use crate::app::{App, Pane, Row, SourceState};
use crate::model::{PhaseKind, Tier};

const BAR_BG: Color = Color::Indexed(236);
const DIM: Color = Color::Indexed(244);
const ACCENT: Color = Color::Cyan;

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

    // Heights recorded for paging: they are properties of the frame, and the
    // frame is the only thing that knows them.
    app.sidebar_height = sidebar.height as usize;
    app.viewport_height = viewport.height as usize;

    frame.render_widget(telemetry_bar(app), telemetry);
    render_sidebar(frame, app, sidebar);
    render_viewport(frame, app, viewport);
    frame.render_widget(status_line(app), status);

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
    let mut turbofan = 0usize;
    let mut other = 0usize;
    let mut osr = 0usize;
    for c in &idx.compilations {
        match c.key.tier {
            Tier::Maglev => maglev += 1,
            Tier::Turbofan => turbofan += 1,
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
        if turbofan > 0 {
            tiers.push(format!("{turbofan} Turbofan"));
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
        spans.push(Span::styled(
            format!("  [{}/{} Tab]", app.active + 1, app.sources.len()),
            Style::new().fg(DIM),
        ));
    }

    Paragraph::new(Line::from(spans)).block(Block::new().style(Style::new().bg(BAR_BG)))
}

// ---------------------------------------------------------------------------
// Sidebar (TODO 3.3)
// ---------------------------------------------------------------------------

fn render_sidebar(frame: &mut Frame, app: &mut App, area: Rect) {
    let focused = app.focus == Pane::Sidebar;
    let rows = app.rows();

    // Keep the selection on screen.
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
        lines.push(Line::from(Span::styled(
            match app.active_source().state {
                SourceState::Loading => " waiting for input…",
                _ => " (no sections)",
            },
            Style::new().fg(DIM),
        )));
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
// Viewport (TODO 3.4)
// ---------------------------------------------------------------------------

fn render_viewport(frame: &mut Frame, app: &mut App, area: Rect) {
    let range = app.view_range();
    let height = area.height as usize;

    // Keep the cursor on screen; `top` trails it.
    app.cursor = app
        .cursor
        .clamp(range.start, range.end.saturating_sub(1).max(range.start));
    if app.top < range.start || app.top >= range.end {
        app.top = range.start;
    }
    if app.cursor < app.top {
        app.top = app.cursor;
    }
    if height > 0 && app.cursor >= app.top + height {
        app.top = app.cursor + 1 - height;
    }

    let source = app.active_source();
    let number_width = digits(range.end.max(1));
    let mut lines = Vec::with_capacity(height);

    for line_no in app.top..range.end.min(app.top + height) {
        let raw = source.buffer.line(line_no).unwrap_or(b"");
        let mut text = crate::ansi::to_display_string(raw);
        if !app.wrap && app.scroll_x > 0 {
            text = text.chars().skip(app.scroll_x).collect();
        }

        let cursor_here = line_no == app.cursor && app.focus == Pane::Viewport;
        let number_style = if cursor_here {
            Style::new().fg(ACCENT)
        } else {
            Style::new().fg(DIM)
        };
        let mut line = Line::from(vec![
            Span::styled(format!("{:>number_width$} ", line_no + 1), number_style),
            Span::raw(text),
        ]);
        if cursor_here {
            line = line.style(Style::new().bg(Color::Indexed(237)));
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

fn digits(mut n: usize) -> usize {
    let mut width = 1;
    while n >= 10 {
        n /= 10;
        width += 1;
    }
    width
}

// ---------------------------------------------------------------------------
// Status line
// ---------------------------------------------------------------------------

fn status_line(app: &App) -> Paragraph<'static> {
    let range = app.view_range();
    let total = range.len();
    let at = (app.cursor + 1).saturating_sub(range.start);

    let mut left = format!(" L{at}/{total}");
    if app.follow {
        left.push_str("  FOLLOW");
    }
    if app.wrap {
        left.push_str("  WRAP");
    }
    if let Some(re) = &app.function_filter {
        left.push_str(&format!("  --function {}", re.as_str()));
    }

    Paragraph::new(Line::from(vec![
        Span::styled(left, Style::new().add_modifier(Modifier::BOLD)),
        Span::raw("   "),
        Span::styled(app.status.clone(), Style::new().fg(Color::Gray)),
    ]))
    .block(Block::new().style(Style::new().bg(BAR_BG)))
}

// ---------------------------------------------------------------------------
// Help modal (TODO 3.6)
// ---------------------------------------------------------------------------

fn render_help(frame: &mut Frame, app: &App, screen: Rect) {
    let rows = app.keys.help_rows();
    let height = (rows.len() as u16 + 4).min(screen.height.saturating_sub(2));
    let width = 58u16.min(screen.width.saturating_sub(4));
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
    let mut lines = Vec::with_capacity(rows.len() + 1);
    for (keys, action) in &rows {
        let pad = key_width.saturating_sub(keys.chars().count());
        lines.push(Line::from(vec![
            Span::styled(
                format!(" {}{} ", " ".repeat(pad), keys),
                Style::new().fg(ACCENT),
            ),
            Span::raw(action.describe().to_string()),
        ]));
    }
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
