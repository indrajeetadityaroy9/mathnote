use ratatui::style::Color;

#[derive(Debug, Clone, Copy)]
pub(crate) struct Theme {
    pub background: Color,
    pub foreground: Color,
    pub accent: Color,
    pub selection: Color,
    pub border: Color,
    pub inactive: Color,
    pub muted: Color,
    pub highlight: Color,
    pub error: Color,
    pub warning: Color,
    pub success: Color,
    pub status_background: Color,
    pub panel_title: Color,
    pub cursor: Color,
    pub scrollbar: Color,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            // Shiki's Cyberpunk 2077 palette. Night City yellow is reserved
            // for focus and commands, cyan for successful output, and red for
            // diagnostics so state changes remain readable at a glance.
            background: Color::Rgb(0x10, 0x0a, 0x18),
            foreground: Color::Rgb(0xe8, 0xe6, 0xf0),
            accent: Color::Rgb(0xfc, 0xee, 0x0a),
            selection: Color::Rgb(0x22, 0x14, 0x3a),
            border: Color::Rgb(0x33, 0x20, 0x4f),
            inactive: Color::Rgb(0x55, 0x45, 0x70),
            muted: Color::Rgb(0x7d, 0x6f, 0x96),
            highlight: Color::Rgb(0xff, 0x00, 0x3c),
            error: Color::Rgb(0xff, 0x00, 0x3c),
            warning: Color::Rgb(0xfc, 0xee, 0x0a),
            success: Color::Rgb(0x00, 0xf0, 0xff),
            status_background: Color::Rgb(0x0a, 0x06, 0x10),
            panel_title: Color::Rgb(0xfc, 0xee, 0x0a),
            cursor: Color::Rgb(0x00, 0xf0, 0xff),
            scrollbar: Color::Rgb(0x22, 0x14, 0x3a),
        }
    }
}

impl Theme {
    /// The accent colour's RGB components, when it is an RGB colour.
    pub(crate) fn accent_rgb(&self) -> Option<[u8; 3]> {
        match self.accent {
            Color::Rgb(red, green, blue) => Some([red, green, blue]),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_palette_matches_shiki_cyberpunk_2077() {
        let theme = Theme::default();
        assert_eq!(theme.background, Color::Rgb(0x10, 0x0a, 0x18));
        assert_eq!(theme.accent, Color::Rgb(0xfc, 0xee, 0x0a));
        assert_eq!(theme.highlight, Color::Rgb(0xff, 0x00, 0x3c));
        assert_eq!(theme.success, Color::Rgb(0x00, 0xf0, 0xff));
        assert_eq!(theme.status_background, Color::Rgb(0x0a, 0x06, 0x10));
    }
}
