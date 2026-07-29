//! Phase 1 scaffolding viewport.
//!
//! Deliberately thin. Phase 3 replaces this with the real layout (telemetry
//! bar, sidebar, viewport, status line); what it needs to do today is prove the
//! event loop draws the right thing at the right time, and give something to
//! test resize and streaming against.
//!
//! Lines are rendered from the buffer on every frame rather than kept as a
//! `Vec<String>`: at a few thousand visible lines that is free, and it is the
//! shape the lazy-parse design needs anyway — the viewport asks for the range
//! it can see and nothing else touches the other 500 MB.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};

use crate::app::{App, SourceState};

pub fn render(frame: &mut Frame, app: &mut App) {
    let [header, body, status] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    // Recorded for paging. Done here because the viewport height is a property
    // of the frame, and the frame is the only thing that knows it.
    app.viewport_height = body.height as usize;

    frame.render_widget(header_line(app), header);
    frame.render_widget(body_lines(app, body.height as usize), body);
    frame.render_widget(status_line(app), status);
}

fn header_line(app: &App) -> Paragraph<'static> {
    let source = app.active_source();

    let state = match &source.state {
        SourceState::Loading => Span::styled(" reading ", Style::new().fg(Color::Yellow)),
        SourceState::Complete => Span::styled(" ok ", Style::new().fg(Color::Green)),
        SourceState::Failed(_) => Span::styled(" failed ", Style::new().fg(Color::Red)),
    };

    let mut spans = vec![
        Span::styled(
            format!(" {} ", source.label),
            Style::new().add_modifier(Modifier::BOLD),
        ),
        state,
        Span::raw(format!(
            "{} lines, {}",
            source.buffer.line_count(),
            human_bytes(source.buffer.len())
        )),
    ];

    if !source.index.compilations.is_empty() {
        spans.push(Span::raw(format!(
            "   {} compilations, {} events",
            source.index.compilations.len(),
            source.index.events.len()
        )));
    }

    if app.sources.len() > 1 {
        spans.push(Span::raw(format!(
            "   [{}/{}] Tab to switch",
            app.active + 1,
            app.sources.len()
        )));
    }

    Paragraph::new(Line::from(spans))
        .block(Block::new().style(Style::new().bg(Color::Indexed(236))))
}

fn body_lines(app: &App, height: usize) -> Paragraph<'static> {
    let buffer = &app.active_source().buffer;
    let mut lines = Vec::with_capacity(height);

    for i in app.top..app.top.saturating_add(height) {
        let Some(raw) = buffer.line(i) else { break };
        lines.push(Line::from(crate::ansi::to_display_string(raw)));
    }

    if lines.is_empty() {
        lines.push(Line::from(Span::styled(
            match app.active_source().state {
                SourceState::Loading => "  waiting for input…",
                _ => "  (empty)",
            },
            Style::new().fg(Color::DarkGray),
        )));
    }

    Paragraph::new(lines)
}

fn status_line(app: &App) -> Paragraph<'static> {
    let total = app.line_count();
    let shown = (app.top + app.viewport_height).min(total);

    // Position only. The failure text belongs in the message area on the right,
    // which `App` already puts it in.
    let mut left = format!(" {shown}/{total}");
    if app.follow {
        left.push_str("  FOLLOW");
    }

    Paragraph::new(Line::from(vec![
        Span::styled(left, Style::new().add_modifier(Modifier::BOLD)),
        Span::raw("   "),
        Span::styled(app.status.clone(), Style::new().fg(Color::Gray)),
    ]))
    .block(Block::new().style(Style::new().bg(Color::Indexed(236))))
}

fn human_bytes(n: usize) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::human_bytes;

    #[test]
    fn byte_counts_are_readable() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(2048), "2.0 KiB");
        assert_eq!(human_bytes(3 * 1024 * 1024), "3.0 MiB");
    }
}
