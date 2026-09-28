use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, BorderType, Borders, Clear, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState,
    Wrap,
};

use crate::App;
use crate::app::PaneFocus;
use crate::layout;
use crate::theme::Theme;

pub(crate) fn render(frame: &mut Frame, app: &mut App) {
    let theme = Theme::default();
    frame.render_widget(
        Block::new().style(Style::default().bg(theme.background)),
        frame.area(),
    );

    let [content_area, status_area] = frame.area().layout(&Layout::vertical([
        Constraint::Fill(1),
        Constraint::Length(1),
    ]));
    let panes = layout::split(content_area, app.focus(), app.zen_mode());

    app.configure_pane_areas(panes.source, panes.latex, panes.preview);
    let source_inner = render_source(frame, app, panes.source, &theme);
    app.set_caret(caret_position(app, source_inner));
    let latex_inner = render_generated_latex(frame, app, panes.latex, &theme);
    let preview_inner = render_preview(frame, app, panes.preview, &theme);
    app.configure_layout(
        source_inner.into(),
        latex_inner.into(),
        preview_inner.into(),
    );
    render_status(frame, app, status_area, &theme);

    if app.show_help() {
        render_help(frame, &theme);
    }
}

fn render_source(frame: &mut Frame, app: &App, area: Rect, theme: &Theme) -> Rect {
    let focused = app.focus() == PaneFocus::Source;
    let block = panel_block(" ◈ NATURAL NOTE ", focused, theme);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return inner;
    }

    let gutter_width = source_gutter_width(app.source_line_count(), inner.width);
    let [gutter, content] = inner.layout(&Layout::horizontal([
        Constraint::Length(gutter_width),
        Constraint::Min(1),
    ]));

    let diagnostic_line = app.diagnostic_line();
    let (scroll_y, scroll_x) = app.source_scroll();
    let first_line = usize::from(scroll_y);
    let end_line = first_line
        .saturating_add(usize::from(inner.height))
        .min(app.source_line_count());
    let lines: Vec<Line<'static>> = (first_line..end_line)
        .map(|line_index| {
            let style = if diagnostic_line == Some(line_index) {
                Style::default().fg(Color::White).bg(theme.error)
            } else {
                Style::default().fg(theme.foreground)
            };
            Line::styled(app.source_line(line_index), style)
        })
        .collect();
    let gutter_lines: Vec<Line<'static>> = (first_line..end_line)
        .map(|line_index| {
            let style = if diagnostic_line == Some(line_index) {
                Style::default()
                    .fg(theme.background)
                    .bg(theme.error)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(if line_index == app.cursor_line_column().0 {
                    theme.cursor
                } else {
                    theme.muted
                })
            };
            Line::styled(
                format!(
                    "{:>width$} ",
                    line_index + 1,
                    width = usize::from(gutter_width.saturating_sub(1))
                ),
                style,
            )
        })
        .collect();
    frame.render_widget(
        Paragraph::new(Text::from(gutter_lines)).style(Style::default().bg(theme.selection)),
        gutter,
    );
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .style(Style::default().bg(theme.background))
            .scroll((0, scroll_x)),
        content,
    );

    render_scrollbar(
        frame,
        area,
        app.source_line_count(),
        usize::from(scroll_y),
        theme,
    );
    content
}

/// The terminal cell of the editing caret when the source pane has focus and the caret is inside
/// the visible text area.
fn caret_position(app: &App, content: Rect) -> Option<Position> {
    if app.focus() != PaneFocus::Source {
        return None;
    }
    let (x, y) = app.cursor_screen_position();
    (x < content.width && y < content.height).then(|| Position::new(content.x + x, content.y + y))
}

fn render_generated_latex(frame: &mut Frame, app: &App, area: Rect, theme: &Theme) -> Rect {
    let block = panel_block(
        " λ GENERATED LATEX ",
        app.focus() == PaneFocus::Latex,
        theme,
    );
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return inner;
    }

    let body = app.generated_body();
    let paragraph = if body.trim().is_empty() {
        Paragraph::new(Line::styled(
            "The generated document body will appear here.",
            Style::default().fg(theme.muted),
        ))
    } else {
        Paragraph::new(body).style(Style::default().fg(theme.foreground))
    };
    frame.render_widget(
        paragraph
            .style(Style::default().fg(theme.foreground))
            .wrap(Wrap { trim: false })
            .scroll((app.latex_scroll(), 0)),
        inner,
    );
    inner
}

fn render_preview(frame: &mut Frame, app: &mut App, area: Rect, theme: &Theme) -> Rect {
    let title = format!(
        " ▣ DOCUMENT PREVIEW  │  {}  │  {} ",
        app.page_label(),
        app.protocol_label()
    );
    let block = panel_block(title, app.focus() == PaneFocus::Preview, theme);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return inner;
    }

    frame.render_widget(Clear, inner);
    frame.render_widget(Block::new().style(Style::default().bg(Color::White)), inner);
    if app.has_preview() {
        app.render_preview(inner, frame.buffer_mut());
    } else {
        frame.render_widget(
            Paragraph::new(app.preview_placeholder())
                .style(Style::default().fg(theme.muted).bg(Color::White))
                .alignment(Alignment::Center),
            inner,
        );
    }
    inner
}

fn render_status(frame: &mut Frame, app: &App, area: Rect, theme: &Theme) {
    frame.render_widget(
        Block::new().style(Style::default().bg(theme.status_background)),
        area,
    );

    let hints = if area.width >= 100 {
        match app.focus() {
            PaneFocus::Source => " F1 HELP │ F2 ZEN │ F6 PANES │ TAB INDENT ",
            PaneFocus::Latex => " F1 HELP │ H/L PANES │ J/K SCROLL │ HOME/END ",
            PaneFocus::Preview => " F1 HELP │ H/L PANES │ J/K SCROLL │ PGUP/PGDN PAGE ",
        }
    } else if area.width >= 72 {
        " F1 HELP │ F2 ZEN │ F6 PANES "
    } else if area.width >= 50 {
        " F1 HELP │ F6 PANES "
    } else {
        ""
    };
    let hint_width = u16::try_from(hints.len())
        .unwrap_or(u16::MAX)
        .min(area.width);
    let [left, right] = area.layout(&Layout::horizontal([
        Constraint::Min(1),
        Constraint::Length(hint_width),
    ]));

    let status = app.status_line();
    let status_color = if status.starts_with("error:") {
        theme.error
    } else if status.starts_with("ready") {
        theme.success
    } else if status == "type a note to begin" {
        theme.muted
    } else {
        theme.warning
    };
    let separator = || Span::styled(" │ ", Style::default().fg(theme.inactive));
    let mut spans = vec![Span::styled(
        if app.zen_mode() { " ZEN " } else { " EDIT " },
        Style::default()
            .fg(theme.background)
            .bg(theme.accent)
            .add_modifier(Modifier::BOLD),
    )];
    spans.push(Span::raw(" "));
    spans.push(Span::styled(
        app.focus_label(),
        Style::default()
            .fg(theme.accent)
            .add_modifier(Modifier::BOLD),
    ));
    if area.width >= 70 {
        let (line, column) = app.cursor_line_column();
        spans.push(separator());
        spans.push(Span::styled(
            format!("LN {}  COL {}", line + 1, column + 1),
            Style::default().fg(theme.foreground),
        ));
    }
    if area.width >= 112 {
        let (characters, words, lines) = app.source_stats();
        spans.push(separator());
        spans.push(Span::styled(
            words.map_or_else(
                || format!("{characters} CHARS · {lines} LINES"),
                |words| format!("{characters} CHARS · {words} WORDS · {lines} LINES"),
            ),
            Style::default().fg(theme.muted),
        ));
    }
    spans.push(separator());
    spans.push(Span::styled(status, Style::default().fg(status_color)));
    let line = Line::from(spans);
    frame.render_widget(Paragraph::new(line), left);
    if !hints.is_empty() {
        frame.render_widget(
            Paragraph::new(hints)
                .alignment(Alignment::Right)
                .style(Style::default().fg(theme.muted)),
            right,
        );
    }
}

fn render_help(frame: &mut Frame, theme: &Theme) {
    let area = centered_rect(
        frame.area(),
        frame.area().width.saturating_mul(4) / 5,
        frame.area().height,
    );
    frame.render_widget(Clear, area);
    let help = Text::from(vec![
        help_heading("GLOBAL", theme),
        help_binding("F1", "toggle command reference", theme),
        help_binding("F2", "toggle distraction-free zen mode", theme),
        help_binding("F6 / Shift-F6", "cycle panes forward / backward", theme),
        help_binding("Esc / Ctrl-C", "quit mathnote", theme),
        help_heading("EDITOR", theme),
        help_binding("Tab", "insert four spaces", theme),
        help_binding(
            "Arrows / Home / End",
            "move the cursor; the preview follows its line",
            theme,
        ),
        help_binding("PageUp / PageDown", "change preview page", theme),
        help_binding("Ctrl-↑ / Ctrl-↓", "scroll the preview", theme),
        help_binding("Ctrl-U", "clear the note", theme),
        help_heading("INSPECTORS", theme),
        help_binding("h / l", "move between panes", theme),
        help_binding("j / k or arrows", "scroll the focused pane", theme),
        help_binding(
            "PageUp / PageDown",
            "scroll LaTeX or change preview page",
            theme,
        ),
        help_heading("MOUSE", theme),
        help_binding(
            "Click",
            "focus a pane, place the cursor, or jump from the preview",
            theme,
        ),
        help_binding("Wheel", "scroll the pane under the pointer", theme),
    ]);
    let block = panel_block(" ⌨ COMMAND REFERENCE  │  F1 / ESC CLOSE ", true, theme);
    frame.render_widget(
        Paragraph::new(help)
            .block(block)
            .style(Style::default().fg(theme.foreground).bg(theme.background))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn centered_rect(area: Rect, preferred_width: u16, preferred_height: u16) -> Rect {
    let width = preferred_width.min(area.width.saturating_sub(2)).max(1);
    let height = preferred_height.min(area.height.saturating_sub(2)).max(1);
    Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    )
}

fn panel_block<'a>(title: impl Into<Line<'a>>, focused: bool, theme: &Theme) -> Block<'a> {
    Block::default()
        .title(title)
        .title_style(
            Style::default()
                .fg(theme.panel_title)
                .add_modifier(Modifier::BOLD),
        )
        .borders(Borders::ALL)
        .border_type(if focused {
            BorderType::Thick
        } else {
            BorderType::Plain
        })
        .border_style(Style::default().fg(if focused { theme.accent } else { theme.border }))
        .style(Style::default().bg(theme.background).fg(theme.foreground))
}

pub(crate) fn source_gutter_width(line_count: usize, available: u16) -> u16 {
    let digits = line_count.max(1).ilog10() as u16 + 1;
    digits.saturating_add(1).min(available.saturating_sub(1))
}

fn render_scrollbar(frame: &mut Frame, area: Rect, total: usize, position: usize, theme: &Theme) {
    if total <= usize::from(area.height.saturating_sub(2)) {
        return;
    }
    let mut state = ScrollbarState::new(total).position(position);
    let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
        .begin_symbol(None)
        .end_symbol(None)
        .track_symbol(None)
        .style(Style::default().fg(theme.scrollbar));
    frame.render_stateful_widget(scrollbar, area, &mut state);
}

fn help_heading(label: &'static str, theme: &Theme) -> Line<'static> {
    Line::from(Span::styled(
        format!("── {label} ──"),
        Style::default()
            .fg(theme.highlight)
            .add_modifier(Modifier::BOLD),
    ))
}

fn help_binding(key: &'static str, label: &'static str, theme: &Theme) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!("  {key:>18}  "),
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(label, Style::default().fg(theme.foreground)),
    ])
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;

    fn render_screen(width: u16, height: u16, show_help: bool) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let mut app = App::default();
        if show_help {
            app.handle_key(KeyEvent::new(KeyCode::F(1), KeyModifiers::NONE));
        }
        terminal
            .draw(|frame| render(frame, &mut app))
            .expect("render succeeds");

        let buffer = terminal.backend().buffer();
        (0..height)
            .map(|y| {
                (0..width)
                    .filter_map(|x| buffer.cell((x, y)))
                    .map(|cell| cell.symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn status_bar_replaces_the_static_header() {
        let screen = render_screen(100, 24, false);
        assert!(screen.contains("NATURAL NOTE"));
        assert!(screen.contains("GENERATED LATEX"));
        assert!(screen.contains("DOCUMENT PREVIEW"));
        assert!(screen.contains("EDIT"));
        assert!(screen.contains("SOURCE"));
        assert!(screen.contains("F1 HELP"));
        assert!(screen.contains("TAB INDENT"));
        assert!(!screen.contains("natural-language mathematics"));
    }

    #[test]
    fn help_overlay_is_visible_in_the_rendered_buffer() {
        let screen = render_screen(100, 24, true);
        assert!(screen.contains("COMMAND REFERENCE"));
        assert!(screen.contains("F6 / Shift-F6"));
        assert!(screen.contains("jump from the preview"));
        assert!(screen.contains("MOUSE"));
    }

    #[test]
    fn narrow_layout_keeps_all_panes_available() {
        let screen = render_screen(50, 18, false);
        assert!(screen.contains("NATURAL NOTE"));
        assert!(screen.contains("GENERATED LATEX"));
        assert!(screen.contains("DOCUMENT PREVIEW"));
    }

    #[test]
    fn rendered_cells_use_cyberpunk_surface_and_focus_colors() {
        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let mut app = App::default();
        terminal
            .draw(|frame| render(frame, &mut app))
            .expect("render succeeds");

        let theme = Theme::default();
        let buffer = terminal.backend().buffer();
        let focused_border = buffer.cell((0, 0)).expect("focused border cell");
        assert_eq!(focused_border.fg, theme.accent);
        assert_eq!(focused_border.bg, theme.background);
        let status = buffer.cell((99, 23)).expect("status cell");
        assert_eq!(status.bg, theme.status_background);
    }
}
