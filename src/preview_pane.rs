//! The preview pane: which page of the typeset document is shown, how far it is scrolled, the
//! highlighted sync target, and the strips encoded for the terminal.
//!
//! The pane holds no reference to the editor. Whatever it needs from the note (the sync target
//! of the cursor, the SyncTeX index, the typeset document) is passed in, and requests for a
//! different page or width go out through a callback that sends a [`PreviewCommand::View`].

use std::sync::Arc;

use ratatui::buffer::Buffer;
use ratatui::layout::{Margin, Position, Rect, Size};
use ratatui_image::picker::Picker;

use crate::latex::LatexDocument;
use crate::preview::{PreviewCommand, RenderedPage};
use crate::strips::{Canvas, EncodedStrips, StripPreview};
use crate::synctex::{SyncIndex, SyncTarget};
use crate::theme::Theme;

/// Preview warnings kept for the help overlay; older ones are dropped first.
pub(crate) const MAX_PREVIEW_WARNINGS: usize = 20;

/// Everything that determines the terminal image of the preview pane. An unchanged key means
/// nothing is re-encoded.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct PreviewViewport {
    fingerprint: u64,
    page_index: usize,
    image_width: u32,
    pane_width: u16,
    pane_height: u16,
    pub(crate) crop_y: u32,
    /// Pixels per big point of the page image.
    scale: f32,
    pub(crate) highlight: Option<(u32, u32)>,
}

pub(crate) struct PreviewPane {
    picker: Picker,
    strips: StripPreview<PreviewViewport>,
    page_count: usize,
    page_index: usize,
    /// The rendered image of `page_index`, when it has arrived.
    page: Option<RenderedPage>,
    requested_view: Option<(usize, u32)>,
    /// Typeset extent of the cursor's note line.
    sync_target: Option<SyncTarget>,
    /// `sync_target` in page image pixel rows `[top, bottom)`.
    highlight: Option<(u32, u32)>,
    /// Problems the page renderer reported, oldest first, shown in the help overlay.
    warnings: Vec<String>,
    /// Inner size of the pane in cells.
    size: Size,
    scroll_rows: u16,
    /// The pane's outer area, border included.
    area: Rect,
}

impl PreviewPane {
    /// A pane drawing through `picker`'s protocol. `deliver` hands encoded strips back to the UI
    /// loop, which passes them to [`Self::accept_encoded`].
    pub(crate) fn new(
        picker: Picker,
        deliver: impl Fn(EncodedStrips) -> bool + Send + 'static,
    ) -> Self {
        let strips = StripPreview::new(picker.clone(), deliver);
        Self {
            picker,
            strips,
            page_count: 0,
            page_index: 0,
            page: None,
            requested_view: None,
            sync_target: None,
            highlight: None,
            warnings: Vec::new(),
            size: Size::default(),
            scroll_rows: 0,
            area: Rect::default(),
        }
    }

    /// Swap in encoded strips. Returns whether the shown preview changed.
    pub(crate) fn accept_encoded(
        &mut self,
        encoded: EncodedStrips,
    ) -> Result<bool, ratatui_image::errors::Errors> {
        self.strips.accept(encoded)
    }

    /// Keep new renderer warnings, each once, dropping the oldest beyond the limit.
    pub(crate) fn add_warnings(&mut self, warnings: Vec<String>) {
        for warning in warnings {
            if !self.warnings.contains(&warning) {
                self.warnings.push(warning);
            }
        }
        let excess = self.warnings.len().saturating_sub(MAX_PREVIEW_WARNINGS);
        self.warnings.drain(..excess);
    }

    pub(crate) fn warnings(&self) -> &[String] {
        &self.warnings
    }

    /// A rendered page arrived; show it if it is the selected one.
    pub(crate) fn accept_page(&mut self, page: RenderedPage) {
        self.page_count = page.pages;
        if page.index == self.page_index {
            self.page = Some(page);
            self.present();
        }
    }

    /// The typeset document now has `pages` pages; step back onto the last one if the selected
    /// page is gone.
    pub(crate) fn set_page_count(&mut self, pages: usize, send: impl FnOnce(PreviewCommand)) {
        self.page_count = pages;
        if pages > 0 && self.page_index >= pages {
            self.select_page(pages - 1, send);
        }
    }

    /// Point the preview at `target`, the cursor's typeset line, switching page if needed.
    pub(crate) fn follow(&mut self, target: Option<SyncTarget>, send: impl FnOnce(PreviewCommand)) {
        self.sync_target = target;
        if let Some(target) = target
            && target.page != self.page_index
            && target.page < self.page_count
        {
            self.select_page(target.page, send);
        }
        self.present();
    }

    /// Install the visible preview, first scrolling just enough to show the whole sync target
    /// and marking it for highlighting.
    fn present(&mut self) {
        self.highlight = None;
        if let (Some(target), Some(page)) = (self.sync_target, &self.page)
            && target.page == self.page_index
        {
            let height = page.image.height();
            let top = (target.top * page.scale).floor().max(0.0) as u32;
            let bottom = ((target.bottom * page.scale).ceil().max(0.0) as u32).min(height);
            let font_height = self.font_height();
            let viewport = self.viewport_height();
            let max_scroll = height.saturating_sub(viewport);
            let max_rows = max_scroll.div_ceil(font_height);
            let crop = (u32::from(self.scroll_rows) * font_height).min(max_scroll);

            let rows = if top < crop || bottom.saturating_sub(top) > viewport {
                Some(top / font_height)
            } else if bottom > crop + viewport {
                Some((bottom - viewport).div_ceil(font_height))
            } else {
                None
            };
            if let Some(rows) = rows {
                self.scroll_rows = rows.min(max_rows).min(u32::from(u16::MAX)) as u16;
            }
            self.highlight = Some((top, bottom));
        }
        self.install_visible();
    }

    /// Hand the visible rows of the page and the highlight to the strip encoder, which composes,
    /// compares and encodes them off the UI thread.
    fn install_visible(&mut self) {
        let Some(page) = &self.page else {
            return;
        };
        if self.size.height == 0 {
            return;
        }
        let Some(max_width) = self.target_width() else {
            return;
        };
        let image = &page.image;
        let font_height = self.font_height();
        let viewport_height = self.viewport_height();
        let max_y = image.height().saturating_sub(viewport_height);
        let y = (u32::from(self.scroll_rows) * font_height).min(max_y);
        self.scroll_rows = (y / font_height) as u16;

        let key = PreviewViewport {
            fingerprint: page.fingerprint,
            page_index: page.index,
            image_width: image.width(),
            pane_width: self.size.width,
            pane_height: self.size.height,
            crop_y: y,
            scale: page.scale,
            highlight: self.highlight,
        };
        if self.strips.holds(&key) {
            return;
        }

        self.strips.install(
            key,
            Canvas {
                page: Arc::clone(&page.image),
                top: y,
                height: viewport_height,
                highlight: self.highlight,
                tint: Some(Theme::default().highlight_tint),
                max_width,
            },
        );
    }

    pub(crate) fn scroll(&mut self, rows: i16) {
        let Some(page) = &self.page else {
            return;
        };
        let font_height = self.font_height();
        let max_rows = page
            .image
            .height()
            .saturating_sub(self.viewport_height())
            .div_ceil(font_height) as u16;
        self.scroll_rows = self.scroll_rows.saturating_add_signed(rows).min(max_rows);
        self.install_visible();
    }

    pub(crate) fn change_page(&mut self, delta: isize, send: impl FnOnce(PreviewCommand)) {
        if self.page_count == 0 {
            return;
        }
        let next = self
            .page_index
            .saturating_add_signed(delta)
            .min(self.page_count - 1);
        self.sync_target = None;
        self.highlight = None;
        self.select_page(next, send);
    }

    fn select_page(&mut self, index: usize, send: impl FnOnce(PreviewCommand)) {
        if index == self.page_index {
            return;
        }
        self.page_index = index;
        self.scroll_rows = 0;
        // The shown strips stay until the new page's image arrives, so nothing flashes blank.
        self.page = None;
        self.request_view(send);
    }

    /// The height of one terminal cell in pixels, at least 1.
    fn font_height(&self) -> u32 {
        u32::from(self.picker.font_size().height.max(1))
    }

    /// The preview pane's height in pixels, counting at least one row.
    fn viewport_height(&self) -> u32 {
        u32::from(self.size.height.max(1)) * self.font_height()
    }

    /// The preview pane's width in pixels, or `None` while the pane has no area.
    fn target_width(&self) -> Option<u32> {
        (self.size.width > 0)
            .then(|| u32::from(self.size.width) * u32::from(self.picker.font_size().width))
    }

    /// Tell the pipeline which page to keep rendered, at the pane's width.
    fn request_view(&mut self, send: impl FnOnce(PreviewCommand)) {
        let Some(width) = self.target_width() else {
            return;
        };
        let view = (self.page_index, width);
        if self.requested_view != Some(view) {
            self.requested_view = Some(view);
            send(PreviewCommand::View {
                page: view.0,
                width: view.1,
            });
        }
    }

    /// Set the pane's inner size; a new size asks for the page at the new width.
    pub(crate) fn resize(&mut self, size: Size, send: impl FnOnce(PreviewCommand)) {
        if size == self.size {
            return;
        }
        self.size = size;
        self.request_view(send);
        self.install_visible();
    }

    pub(crate) fn area(&self) -> Rect {
        self.area
    }

    pub(crate) fn set_area(&mut self, area: Rect) {
        self.area = area;
    }

    /// The byte of `document`'s note typeset under a click at `column`, `row`.
    ///
    /// The click is mapped back through the width fit `install_visible` applied to the shown
    /// canvas (drawn from the pane's top-left corner), then from page pixels to points.
    pub(crate) fn note_byte_at(
        &self,
        column: u16,
        row: u16,
        sync: &SyncIndex,
        document: &LatexDocument,
    ) -> Option<usize> {
        let view = self.strips.shown_key()?;
        let inner = self.area.inner(Margin::new(1, 1));
        if !self.strips.is_visible()
            || view.scale <= 0.0
            || !inner.contains(Position::new(column, row))
        {
            return None;
        }

        let font = self.picker.font_size();
        let (font_width, font_height) = (f64::from(font.width), f64::from(font.height));
        let pane_width = f64::from(inner.width) * font_width;
        let ratio = (pane_width / f64::from(view.image_width.max(1))).min(1.0);
        let x = (f64::from(column - inner.x) + 0.5) * font_width / ratio;
        let y = (f64::from(row - inner.y) + 0.5) * font_height / ratio;
        if x >= f64::from(view.image_width) {
            return None;
        }

        let scale = f64::from(view.scale);
        sync.backward(
            view.page_index,
            (x / scale) as f32,
            ((y + f64::from(view.crop_y)) / scale) as f32,
            |line| document.is_content_line(line),
        )
        .and_then(|line| document.source_byte_for_output_line(line))
    }

    /// Whether encoded strips are still on their way.
    pub(crate) fn is_staging(&self) -> bool {
        self.strips.is_staging()
    }

    pub(crate) fn has_preview(&self) -> bool {
        self.strips.is_visible()
    }

    pub(crate) fn page_label(&self) -> String {
        if self.page_count == 0 {
            String::from("no page")
        } else {
            format!("page {}/{}", self.page_index + 1, self.page_count)
        }
    }

    pub(crate) fn protocol_label(&self) -> String {
        format!("{:?}", self.picker.protocol_type())
    }

    pub(crate) fn render(&mut self, area: Rect, buf: &mut Buffer) {
        self.strips.render(area, buf);
    }

    #[cfg(test)]
    pub(crate) fn font_size(&self) -> ratatui_image::FontSize {
        self.picker.font_size()
    }

    #[cfg(test)]
    pub(crate) fn size(&self) -> Size {
        self.size
    }

    #[cfg(test)]
    pub(crate) fn scroll_rows(&self) -> u16 {
        self.scroll_rows
    }

    #[cfg(test)]
    pub(crate) fn highlight(&self) -> Option<(u32, u32)> {
        self.highlight
    }

    #[cfg(test)]
    pub(crate) fn shown_viewport(&self) -> Option<PreviewViewport> {
        self.strips.shown_key()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::mpsc;
    use std::time::Duration;

    use image::{ImageBuffer, Rgba};

    use super::*;

    pub(crate) fn white_page(
        index: usize,
        width: u32,
        height: u32,
        fingerprint: u64,
    ) -> RenderedPage {
        RenderedPage {
            index,
            pages: 1,
            fingerprint,
            scale: 1.0,
            image: Arc::new(ImageBuffer::from_pixel(
                width,
                height,
                Rgba([255, 255, 255, 255]),
            )),
        }
    }

    fn pane() -> (PreviewPane, mpsc::Receiver<EncodedStrips>) {
        let (encoded_tx, encoded) = mpsc::channel();
        let mut picker = Picker::halfblocks();
        picker.set_background_color(Some(Rgba([255, 255, 255, 255])));
        let mut pane = PreviewPane::new(picker, move |strips| encoded_tx.send(strips).is_ok());
        pane.resize(Size::new(20, 8), |_| {});
        (pane, encoded)
    }

    fn wait_for_encoded_strips(pane: &mut PreviewPane, encoded: &mpsc::Receiver<EncodedStrips>) {
        while pane.is_staging() {
            let strips = encoded
                .recv_timeout(Duration::from_secs(1))
                .expect("timed out waiting for terminal preview encoding");
            pane.accept_encoded(strips).expect("strips encode");
        }
    }

    #[test]
    fn a_change_within_one_cell_row_re_encodes_only_that_strip() {
        let (mut pane, encoded) = pane();
        let font = pane.font_size();
        let (width, height) = (20 * u32::from(font.width), 8 * u32::from(font.height));
        pane.accept_page(white_page(0, width, height, 1));
        wait_for_encoded_strips(&mut pane, &encoded);
        assert!(pane.has_preview());
        assert_eq!(
            pane.strips.last_encoded().len(),
            8,
            "the first install encodes every strip"
        );

        let mut page = ImageBuffer::from_pixel(width, height, Rgba([255, 255, 255, 255]));
        let row_top = 3 * u32::from(font.height);
        for y in row_top + 1..row_top + 4 {
            for x in 0..width / 2 {
                page.put_pixel(x, y, Rgba([0, 0, 0, 255]));
            }
        }
        let mut changed = white_page(0, width, height, 2);
        changed.image = Arc::new(page);
        pane.accept_page(changed);

        assert!(pane.strips.is_staging());
        assert!(
            pane.has_preview(),
            "the shown strips stay while the new ones are encoded"
        );
        wait_for_encoded_strips(&mut pane, &encoded);
        assert_eq!(pane.strips.last_encoded(), &[3]);
        assert!(pane.has_preview());
    }
}
