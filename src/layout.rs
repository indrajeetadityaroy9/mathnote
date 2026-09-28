use ratatui::layout::{Constraint, Direction, Layout, Rect};

use crate::app::PaneFocus;

pub(crate) struct PaneAreas {
    pub source: Rect,
    pub latex: Rect,
    pub preview: Rect,
}

const COLLAPSED: u16 = 1;
const STACK_WIDTH: u16 = 70;
const SINGLE_WIDTH: u16 = 46;
const SINGLE_HEIGHT: u16 = 14;

pub(crate) fn split(area: Rect, focus: PaneFocus, zen_mode: bool) -> PaneAreas {
    if zen_mode || area.width < SINGLE_WIDTH || area.height < SINGLE_HEIGHT {
        return single(area, focus);
    }
    let direction = if area.width < STACK_WIDTH {
        Direction::Vertical
    } else {
        Direction::Horizontal
    };
    let (source, latex, preview) = constraints(focus);
    let panes = Layout::default()
        .direction(direction)
        .spacing(0)
        .constraints([source, latex, preview])
        .split(area);
    PaneAreas {
        source: panes[0],
        latex: panes[1],
        preview: panes[2],
    }
}

fn single(area: Rect, focus: PaneFocus) -> PaneAreas {
    let zero = Rect::new(area.x, area.y, 0, 0);
    match focus {
        PaneFocus::Source => PaneAreas {
            source: area,
            latex: zero,
            preview: zero,
        },
        PaneFocus::Latex => PaneAreas {
            source: zero,
            latex: area,
            preview: zero,
        },
        PaneFocus::Preview => PaneAreas {
            source: zero,
            latex: zero,
            preview: area,
        },
    }
}

fn constraints(focus: PaneFocus) -> (Constraint, Constraint, Constraint) {
    match focus {
        PaneFocus::Source => (
            Constraint::Fill(7),
            Constraint::Fill(5),
            Constraint::Fill(8),
        ),
        PaneFocus::Latex => (
            Constraint::Length(COLLAPSED),
            Constraint::Fill(1),
            Constraint::Fill(1),
        ),
        PaneFocus::Preview => (
            Constraint::Length(COLLAPSED),
            Constraint::Length(COLLAPSED),
            Constraint::Fill(1),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wide_layout_uses_three_columns() {
        let areas = split(Rect::new(0, 0, 120, 30), PaneFocus::Source, false);
        assert!(areas.source.width > 0);
        assert!(areas.latex.width > 0);
        assert!(areas.preview.width > 0);
        assert_eq!(areas.source.y, areas.latex.y);
        assert_eq!(areas.latex.y, areas.preview.y);
    }

    #[test]
    fn narrow_layout_stacks_full_width_panels() {
        let areas = split(Rect::new(0, 0, 60, 24), PaneFocus::Source, false);
        assert_eq!(areas.source.width, 60);
        assert_eq!(areas.latex.width, 60);
        assert_eq!(areas.preview.width, 60);
        assert!(areas.source.y < areas.latex.y);
        assert!(areas.latex.y < areas.preview.y);
    }

    #[test]
    fn tiny_layout_only_shows_the_focused_panel() {
        let area = Rect::new(0, 0, 45, 10);
        let areas = split(area, PaneFocus::Preview, false);
        assert_eq!(areas.source.width, 0);
        assert_eq!(areas.latex.width, 0);
        assert_eq!(areas.preview, area);
    }

    #[test]
    fn preview_focus_collapses_sibling_columns() {
        let areas = split(Rect::new(0, 0, 120, 30), PaneFocus::Preview, false);
        assert_eq!(areas.source.width, COLLAPSED);
        assert_eq!(areas.latex.width, COLLAPSED);
        assert_eq!(areas.preview.width, 120 - COLLAPSED * 2);
    }

    #[test]
    fn zen_mode_only_shows_the_focused_panel() {
        let area = Rect::new(0, 0, 120, 30);
        let areas = split(area, PaneFocus::Latex, true);
        assert_eq!(areas.source.width, 0);
        assert_eq!(areas.latex, area);
        assert_eq!(areas.preview.width, 0);
    }
}
