use ratatui::style::Color;

const BLACK: Color = Color::Rgb(0x00, 0x00, 0x00);
const WHITE: Color = Color::Rgb(0xff, 0xff, 0xff);
const GRAY: Color = Color::Rgb(0x80, 0x80, 0x80);

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
    /// Multiplied into the page pixels of the preview's highlighted line.
    pub highlight_tint: [u8; 3],
}

impl Default for Theme {
    /// White on black. Gray marks secondary elements (unfocused borders, line numbers, hints);
    /// focus shows as a thick white border, and errors as inverted text.
    fn default() -> Self {
        Self {
            background: BLACK,
            foreground: WHITE,
            accent: WHITE,
            selection: BLACK,
            border: GRAY,
            inactive: GRAY,
            muted: GRAY,
            highlight: WHITE,
            error: WHITE,
            warning: WHITE,
            success: WHITE,
            status_background: BLACK,
            panel_title: WHITE,
            cursor: WHITE,
            scrollbar: GRAY,
            highlight_tint: [0xd8, 0xd8, 0xd8],
        }
    }
}
