use std::collections::BTreeMap;
use std::io;
use std::sync::mpsc::RecvTimeoutError;
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use crossterm::terminal::{BeginSynchronizedUpdate, EndSynchronizedUpdate};
use crossterm::{execute, queue};
use image::{ImageBuffer, Rgba, RgbaImage, imageops};
use ratatui::DefaultTerminal;
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect, Size};
use ratatui_image::picker::Picker;

use crate::document::{Document, SourceSpan, TextBuffer, map_offset_across_edit};
use crate::latex::{LatexDocument, emit_latex};
use crate::preview::{PreviewCommand, PreviewEvent, RenderedPage, preview_thread};
use crate::strips::{EncodedStrips, StripPreview};
use crate::synctex::{SyncIndex, SyncTarget};
use crate::theme::Theme;
use crate::ui;
use crate::worker::{BuildRequest, WorkerEvent, build_worker};

const SPINNER_INTERVAL: Duration = Duration::from_millis(80);
const SPINNER_FRAMES: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

/// Everything that determines the terminal image of the preview pane. An unchanged key means
/// nothing is re-encoded.
#[derive(Debug, Clone, Copy, PartialEq)]
struct PreviewViewport {
    fingerprint: u64,
    page_index: usize,
    image_width: u32,
    pane_width: u16,
    pane_height: u16,
    crop_y: u32,
    /// Pixels per big point of the page image.
    scale: f32,
    highlight: Option<(u32, u32)>,
}

/// The editor: the note, its generated LaTeX, and the live preview of the typeset page. The
/// typesetting pipeline (texpresso's driver) runs on a background thread.
pub struct App {
    buffer: TextBuffer,
    should_quit: bool,
    show_help: bool,
    zen_mode: bool,
    focus: PaneFocus,
    revision: u64,
    generated_revision: Option<u64>,
    generated: Option<LatexDocument>,
    submitted_revision: Option<u64>,
    worker_tx: mpsc::Sender<BuildRequest>,
    events_tx: mpsc::Sender<LoopEvent>,
    events_rx: mpsc::Receiver<LoopEvent>,
    terminal_error: Option<io::Error>,
    status: PipelineStatus,
    diagnostic_span: Option<SourceSpan>,
    spinner_frame: usize,
    last_spinner_tick: Instant,
    source_characters: usize,
    source_words: usize,

    pipeline: PipelineLink,
    /// Documents sent to the pipeline by revision, kept until a newer one has been typeset.
    sent: BTreeMap<u64, LatexDocument>,
    /// The newest revision the pipeline finished; `sync` belongs to it.
    typeset_revision: Option<u64>,
    sync: Option<Arc<SyncIndex>>,
    page_count: usize,
    page_index: usize,
    /// The rendered image of `page_index`, when it has arrived.
    page: Option<RenderedPage>,
    requested_view: Option<(usize, u32)>,
    picker: Picker,
    strips: StripPreview<PreviewViewport>,
    /// Typeset extent of the cursor's note line.
    sync_target: Option<SyncTarget>,
    /// `sync_target` in page image pixel rows `[top, bottom)`.
    highlight: Option<(u32, u32)>,
    /// Terminal cell of the editing caret for the frame being drawn.
    caret: Option<Position>,

    source_size: Size,
    source_scroll_y: usize,
    source_scroll_x: u16,
    latex_size: Size,
    latex_scroll_rows: u16,
    preview_size: Size,
    preview_scroll_rows: u16,
    source_area: Rect,
    latex_area: Rect,
    preview_area: Rect,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PaneFocus {
    Source,
    Latex,
    Preview,
}

impl PaneFocus {
    fn next(self) -> Self {
        match self {
            Self::Source => Self::Latex,
            Self::Latex => Self::Preview,
            Self::Preview => Self::Source,
        }
    }

    fn previous(self) -> Self {
        match self {
            Self::Source => Self::Preview,
            Self::Latex => Self::Source,
            Self::Preview => Self::Latex,
        }
    }
}

#[derive(Debug, Clone)]
enum PipelineStatus {
    Empty,
    Waiting,
    Preparing,
    Typesetting,
    Ready { elapsed: Duration, warnings: usize },
    Error(String),
}

/// Everything that can wake the UI loop, delivered through one channel.
pub(crate) enum LoopEvent {
    Terminal(Event),
    TerminalFailed(io::Error),
    Worker(WorkerEvent),
    Preview(PreviewEvent),
    PreviewEncoded(EncodedStrips),
}

/// Where revisions and views go.
enum PipelineLink {
    /// No typesetting: unit tests.
    Detached,
    Thread(mpsc::Sender<PreviewCommand>),
    /// Records what would be sent, for tests.
    #[cfg(test)]
    Recording(Vec<PreviewCommand>),
}

impl PipelineLink {
    fn send(&mut self, command: PreviewCommand) {
        match self {
            Self::Detached => {}
            Self::Thread(commands) => {
                let _ = commands.send(command);
            }
            #[cfg(test)]
            Self::Recording(sent) => sent.push(command),
        }
    }
}

impl Default for App {
    /// An editor without a typesetting pipeline.
    fn default() -> Self {
        Self::with_pipeline(Picker::halfblocks(), |_| PipelineLink::Detached)
    }
}

impl App {
    /// An editor with its typesetting pipeline, drawing pages through `picker`'s protocol.
    pub fn new(picker: Picker) -> Self {
        Self::with_pipeline(picker, |events| {
            let (commands_tx, commands) = mpsc::channel();
            let events = events.clone();
            thread::spawn(move || preview_thread(commands, events));
            PipelineLink::Thread(commands_tx)
        })
    }

    fn with_pipeline(
        mut picker: Picker,
        pipeline: impl FnOnce(&mpsc::Sender<LoopEvent>) -> PipelineLink,
    ) -> Self {
        picker.set_background_color(Some(Rgba([255, 255, 255, 255])));
        let (worker_tx, worker_requests) = mpsc::channel();
        let (events_tx, events_rx) = mpsc::channel();
        let worker_events = events_tx.clone();
        thread::spawn(move || build_worker(worker_requests, worker_events));
        let encoded_events = events_tx.clone();
        let strips = StripPreview::new(picker.clone(), move |encoded| {
            encoded_events
                .send(LoopEvent::PreviewEncoded(encoded))
                .is_ok()
        });
        let pipeline = pipeline(&events_tx);

        let mut app = Self {
            buffer: TextBuffer::default(),
            should_quit: false,
            show_help: false,
            zen_mode: false,
            focus: PaneFocus::Source,
            revision: 1,
            generated_revision: None,
            generated: None,
            submitted_revision: None,
            worker_tx,
            events_tx,
            events_rx,
            terminal_error: None,
            status: PipelineStatus::Waiting,
            diagnostic_span: None,
            spinner_frame: 0,
            last_spinner_tick: Instant::now(),
            source_characters: 0,
            source_words: 0,
            pipeline,
            sent: BTreeMap::new(),
            typeset_revision: None,
            sync: None,
            page_count: 0,
            page_index: 0,
            page: None,
            requested_view: None,
            picker,
            strips,
            sync_target: None,
            highlight: None,
            caret: None,
            source_size: Size::default(),
            source_scroll_y: 0,
            source_scroll_x: 0,
            latex_size: Size::default(),
            latex_scroll_rows: 0,
            preview_size: Size::default(),
            preview_scroll_rows: 0,
            source_area: Rect::default(),
            latex_area: Rect::default(),
            preview_area: Rect::default(),
        };
        app.refresh_source_state();
        app
    }

    pub fn run(mut self, terminal: &mut DefaultTerminal) -> io::Result<()> {
        spawn_terminal_reader(self.events_tx.clone());
        let mut needs_draw = true;
        while !self.should_quit {
            needs_draw |= self.maybe_submit_build();
            if needs_draw {
                self.draw(terminal)?;
                needs_draw = false;
            }

            let event = if self.pipeline_is_active() {
                match self.events_rx.recv_timeout(SPINNER_INTERVAL) {
                    Ok(event) => Some(event),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => return Ok(()),
                }
            } else {
                match self.events_rx.recv() {
                    Ok(event) => Some(event),
                    Err(_) => return Ok(()),
                }
            };
            if let Some(event) = event {
                needs_draw |= self.handle_loop_event(event);
            }
            needs_draw |= self.process_background_events();
            needs_draw |= self.advance_spinner();
            if let Some(error) = self.terminal_error.take() {
                return Err(error);
            }
        }
        Ok(())
    }

    /// Draw one frame as a single synchronized update with the caret hidden while cells are
    /// written, then place and show it. Ratatui would otherwise show the caret before moving it,
    /// so each frame briefly flashed it at the last cell written.
    fn draw(&mut self, terminal: &mut DefaultTerminal) -> io::Result<()> {
        queue!(terminal.backend_mut(), BeginSynchronizedUpdate, Hide)?;
        terminal.draw(|frame| ui::render(frame, self))?;
        if let Some(caret) = self.caret {
            queue!(terminal.backend_mut(), MoveTo(caret.x, caret.y), Show)?;
        }
        execute!(terminal.backend_mut(), EndSynchronizedUpdate)
    }

    pub(crate) fn handle_event(&mut self, event: Event) -> bool {
        let before = (self.revision, self.buffer.cursor());
        let redraw = match event {
            Event::Key(key) => {
                self.handle_key(key);
                true
            }
            Event::Paste(text) => {
                self.buffer.insert_str(&text);
                self.mark_edited();
                true
            }
            Event::Mouse(mouse) => {
                self.handle_mouse(mouse);
                true
            }
            Event::Resize(_, _) => true,
            _ => false,
        };
        if (self.revision, self.buffer.cursor()) != before {
            self.follow_cursor();
        }
        redraw
    }

    fn handle_loop_event(&mut self, event: LoopEvent) -> bool {
        match event {
            LoopEvent::Terminal(event) => self.handle_event(event),
            LoopEvent::TerminalFailed(error) => {
                self.terminal_error = Some(error);
                true
            }
            LoopEvent::Worker(event) => {
                self.handle_worker_event(event);
                true
            }
            LoopEvent::Preview(event) => {
                self.handle_preview_event(event);
                true
            }
            LoopEvent::PreviewEncoded(encoded) => match self.strips.accept(encoded) {
                Ok(changed) => changed,
                Err(error) => {
                    self.status = PipelineStatus::Error(format!("terminal image error: {error}"));
                    true
                }
            },
        }
    }

    pub(crate) fn handle_key(&mut self, key: KeyEvent) {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return;
        }

        if key.code == KeyCode::F(1) {
            self.show_help = !self.show_help;
            return;
        }
        if self.show_help {
            if matches!(key.code, KeyCode::Esc | KeyCode::Char('q')) {
                self.show_help = false;
            }
            return;
        }

        if key.code == KeyCode::F(2) {
            self.zen_mode = !self.zen_mode;
            return;
        }

        if key.code == KeyCode::F(6) {
            self.focus = if key.modifiers.contains(KeyModifiers::SHIFT) {
                self.focus.previous()
            } else {
                self.focus.next()
            };
            self.ensure_cursor_visible();
            return;
        }

        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.should_quit = true;
            return;
        }
        if key.code == KeyCode::Char('u') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.buffer.clear();
            self.mark_edited();
            return;
        }
        if key.code == KeyCode::Up && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.scroll_preview(-3);
            return;
        }
        if key.code == KeyCode::Down && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.scroll_preview(3);
            return;
        }

        if self.focus != PaneFocus::Source && self.handle_browse_key(key) {
            return;
        }

        let edited = match key.code {
            KeyCode::Esc => {
                self.should_quit = true;
                false
            }
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.buffer.insert_char(character);
                true
            }
            KeyCode::Enter => {
                self.buffer.insert_char('\n');
                true
            }
            KeyCode::Tab => {
                self.buffer.insert_str("    ");
                true
            }
            KeyCode::Backspace => self.buffer.backspace(),
            KeyCode::Delete => self.buffer.delete(),
            KeyCode::Left => {
                self.buffer.move_left();
                false
            }
            KeyCode::Right => {
                self.buffer.move_right();
                false
            }
            KeyCode::Up => {
                self.buffer.move_up();
                false
            }
            KeyCode::Down => {
                self.buffer.move_down();
                false
            }
            KeyCode::Home => {
                self.buffer.move_home();
                false
            }
            KeyCode::End => {
                self.buffer.move_end();
                false
            }
            KeyCode::PageUp => {
                self.change_page(-1);
                false
            }
            KeyCode::PageDown => {
                self.change_page(1);
                false
            }
            _ => false,
        };

        if edited {
            self.mark_edited();
        } else {
            self.ensure_cursor_visible();
        }
    }

    /// Keys of the LaTeX and preview inspector panes.
    fn handle_browse_key(&mut self, key: KeyEvent) -> bool {
        let plain_character = |expected| {
            key.code == KeyCode::Char(expected)
                && !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        };

        if plain_character('q') {
            self.should_quit = true;
            return true;
        }
        if key.code == KeyCode::Left || plain_character('h') {
            self.focus = self.focus.previous();
            self.ensure_cursor_visible();
            return true;
        }
        if key.code == KeyCode::Right || key.code == KeyCode::Enter || plain_character('l') {
            self.focus = self.focus.next();
            self.ensure_cursor_visible();
            return true;
        }

        let up = key.code == KeyCode::Up || plain_character('k');
        let down = key.code == KeyCode::Down || plain_character('j');
        match self.focus {
            PaneFocus::Source => return false,
            PaneFocus::Latex => {
                let page = i16::try_from(self.latex_size.height.max(1)).unwrap_or(i16::MAX);
                match key.code {
                    _ if up => self.scroll_latex(-1),
                    _ if down => self.scroll_latex(1),
                    KeyCode::PageUp => self.scroll_latex(-page),
                    KeyCode::PageDown => self.scroll_latex(page),
                    KeyCode::Home => self.latex_scroll_rows = 0,
                    KeyCode::End => self.latex_scroll_rows = self.max_latex_scroll(),
                    _ => return false,
                }
            }
            PaneFocus::Preview => match key.code {
                _ if up => self.scroll_preview(-1),
                _ if down => self.scroll_preview(1),
                KeyCode::PageUp => self.change_page(-1),
                KeyCode::PageDown => self.change_page(1),
                KeyCode::Home => self.scroll_preview(i16::MIN),
                KeyCode::End => self.scroll_preview(i16::MAX),
                _ => return false,
            },
        }
        true
    }

    fn mark_edited(&mut self) {
        self.revision = self.revision.wrapping_add(1);
        self.submitted_revision = None;
        self.diagnostic_span = None;
        self.refresh_source_state();
        self.ensure_cursor_visible();
    }

    fn refresh_source_state(&mut self) {
        self.source_characters = self.buffer.len_chars();
        self.diagnostic_span = None;
        if self.buffer.is_blank() {
            self.source_words = 0;
            self.latex_scroll_rows = 0;
            self.submitted_revision = Some(self.revision);
            self.status = PipelineStatus::Empty;
            if let Ok(document) = Document::parse("") {
                self.accept_built(self.revision, emit_latex(&document));
            }
            return;
        }
        self.generated_revision = None;
        self.status = PipelineStatus::Waiting;
    }

    fn maybe_submit_build(&mut self) -> bool {
        if self.submitted_revision == Some(self.revision) {
            return false;
        }
        let request = BuildRequest {
            revision: self.revision,
            source: self.buffer.text(),
        };
        if self.worker_tx.send(request).is_ok() {
            self.submitted_revision = Some(self.revision);
        } else {
            self.status = PipelineStatus::Error(String::from("note worker stopped"));
        }
        true
    }

    pub(crate) fn process_background_events(&mut self) -> bool {
        let mut changed = false;
        while let Ok(event) = self.events_rx.try_recv() {
            changed |= self.handle_loop_event(event);
        }
        changed
    }

    fn handle_worker_event(&mut self, event: WorkerEvent) {
        match event {
            WorkerEvent::Built {
                revision,
                document,
                words,
            } if self
                .generated_revision
                .is_none_or(|generated| revision > generated) =>
            {
                self.source_words = words;
                self.accept_built(revision, document);
            }
            WorkerEvent::ParseFailed {
                revision,
                message,
                byte,
            } if revision == self.revision => {
                self.generated_revision = None;
                self.status = PipelineStatus::Error(message);
                self.diagnostic_span = Some(SourceSpan {
                    start: byte,
                    end: byte.saturating_add(1),
                });
            }
            _ => {}
        }
    }

    /// Show a newly built revision's LaTeX and hand it to the pipeline. The pipeline diffs it
    /// against the previous revision, so unchanged LaTeX costs no typesetting.
    fn accept_built(&mut self, revision: u64, document: LatexDocument) {
        self.generated = Some(document.clone());
        self.generated_revision = Some(revision);
        self.latex_scroll_rows = 0;
        self.pipeline.send(PreviewCommand::Document {
            revision,
            source: document.source().to_owned(),
        });
        self.sent.insert(revision, document);
        if !matches!(self.status, PipelineStatus::Empty) {
            self.status = PipelineStatus::Typesetting;
        }
    }

    fn handle_preview_event(&mut self, event: PreviewEvent) {
        let empty = matches!(self.status, PipelineStatus::Empty);
        match event {
            PreviewEvent::Preparing if !empty => self.status = PipelineStatus::Preparing,
            PreviewEvent::Preparing => {}
            PreviewEvent::Failed(message) => self.status = PipelineStatus::Error(message),
            PreviewEvent::Page(page) => {
                self.page_count = page.pages;
                if page.index == self.page_index {
                    self.page = Some(page);
                    self.present_preview();
                }
            }
            PreviewEvent::Finished {
                revision,
                pages,
                elapsed,
                messages,
                sync,
            } => {
                let newest = self.sent.last_key_value().map(|(newest, _)| *newest);
                self.typeset_revision = Some(revision);
                self.sync = sync;
                self.sent = self.sent.split_off(&revision);
                self.page_count = pages;
                if pages > 0 && self.page_index >= pages {
                    self.select_page(pages - 1);
                }
                if newest == Some(revision) && !empty {
                    match messages.iter().find(|message| message.error) {
                        Some(error) => {
                            self.status = PipelineStatus::Error(error.message.clone());
                            self.diagnostic_span = error
                                .tex_line
                                .and_then(|line| self.note_span_for(revision, line));
                        }
                        None => {
                            self.status = PipelineStatus::Ready {
                                elapsed,
                                warnings: messages.len(),
                            };
                        }
                    }
                }
                self.follow_cursor();
            }
        }
    }

    /// The note position that `revision`'s source line `line` typesets, moved across edits made
    /// since that revision. Lines inside a multi-line paragraph map to their own note line.
    fn note_span_for(&self, revision: u64, line: usize) -> Option<SourceSpan> {
        let document = self.sent.get(&revision)?;
        let byte = document.source_byte_for_output_line(line).or_else(|| {
            document
                .source_span_for_output_line(line)
                .map(|span| span.start)
        })?;
        let start = map_offset_across_edit(document.note(), &self.buffer.text(), byte);
        Some(SourceSpan {
            start,
            end: start.saturating_add(1),
        })
    }

    /// The document the SyncTeX data belongs to.
    fn typeset_document(&self) -> Option<&LatexDocument> {
        self.sent.get(&self.typeset_revision?)
    }

    /// Point the preview at the cursor's typeset line, switching page if needed.
    ///
    /// The typeset document may trail the text while an edit is typeset. The cursor is mapped
    /// across the pending edit into the note that document came from, so typing on a line keeps
    /// its highlight in place.
    fn follow_cursor(&mut self) {
        let target = match (&self.sync, self.typeset_document()) {
            (Some(sync), Some(document)) => {
                let byte = map_offset_across_edit(
                    &self.buffer.text(),
                    document.note(),
                    self.buffer.cursor_byte(),
                );
                document
                    .output_line_for_source_byte(byte)
                    .and_then(|line| sync.forward(line))
            }
            _ => None,
        };
        self.sync_target = target;
        if let Some(target) = target
            && target.page != self.page_index
            && target.page < self.page_count
        {
            self.select_page(target.page);
        }
        self.present_preview();
    }

    /// Install the visible preview, first scrolling just enough to show the whole sync target
    /// and marking it for highlighting.
    fn present_preview(&mut self) {
        self.highlight = None;
        if let (Some(target), Some(page)) = (self.sync_target, &self.page)
            && target.page == self.page_index
        {
            let height = page.image.height();
            let top = (target.top * page.scale).floor().max(0.0) as u32;
            let bottom = ((target.bottom * page.scale).ceil().max(0.0) as u32).min(height);
            let font_height = u32::from(self.picker.font_size().height.max(1));
            let viewport = u32::from(self.preview_size.height.max(1)) * font_height;
            let max_scroll = height.saturating_sub(viewport);
            let max_rows = max_scroll.div_ceil(font_height);
            let crop = (u32::from(self.preview_scroll_rows) * font_height).min(max_scroll);

            let rows = if top < crop || bottom.saturating_sub(top) > viewport {
                Some(top / font_height)
            } else if bottom > crop + viewport {
                Some((bottom - viewport).div_ceil(font_height))
            } else {
                None
            };
            if let Some(rows) = rows {
                self.preview_scroll_rows = rows.min(max_rows).min(u32::from(u16::MAX)) as u16;
            }
            self.highlight = Some((top, bottom));
        }
        self.install_visible_preview();
    }

    /// Crop the visible rows of the page, tint the highlighted lines, and hand the canvas to the
    /// strip encoder.
    fn install_visible_preview(&mut self) {
        let Some(page) = &self.page else {
            return;
        };
        if self.preview_size.width == 0 || self.preview_size.height == 0 {
            return;
        }
        let image = &page.image;
        let font = self.picker.font_size();
        let font_height = u32::from(font.height.max(1));
        let viewport_height = (u32::from(self.preview_size.height) * font_height).max(1);
        let max_y = image.height().saturating_sub(viewport_height);
        let y = (u32::from(self.preview_scroll_rows) * font_height).min(max_y);
        self.preview_scroll_rows = (y / font_height) as u16;

        let key = PreviewViewport {
            fingerprint: page.fingerprint,
            page_index: page.index,
            image_width: image.width(),
            pane_width: self.preview_size.width,
            pane_height: self.preview_size.height,
            crop_y: y,
            scale: page.scale,
            highlight: self.highlight,
        };
        if self.strips.holds(&key) {
            return;
        }

        let crop_height = viewport_height.min(image.height().saturating_sub(y)).max(1);
        let cropped =
            imageops::crop_imm(image.as_ref(), 0, y, image.width(), crop_height).to_image();
        let mut canvas =
            ImageBuffer::from_pixel(image.width(), viewport_height, Rgba([255, 255, 255, 255]));
        imageops::replace(&mut canvas, &cropped, 0, 0);
        if let (Some((top, bottom)), Some(accent)) = (self.highlight, Theme::default().accent_rgb())
            && bottom > y
        {
            for row in top.saturating_sub(y)..(bottom - y).min(viewport_height) {
                for column in 0..canvas.width() {
                    let pixel = canvas.get_pixel_mut(column, row);
                    for (channel, tint) in pixel.0.iter_mut().zip(accent) {
                        *channel = (u16::from(*channel) * u16::from(tint) / 255) as u8;
                    }
                }
            }
        }
        let canvas = fit_to_width(
            canvas,
            u32::from(self.preview_size.width) * u32::from(font.width),
            font_height,
        );
        self.strips.install(key, &canvas);
    }

    fn scroll_preview(&mut self, rows: i16) {
        let Some(page) = &self.page else {
            return;
        };
        let font_height = u32::from(self.picker.font_size().height.max(1));
        let viewport = u32::from(self.preview_size.height.max(1)) * font_height;
        let max_rows = page
            .image
            .height()
            .saturating_sub(viewport)
            .div_ceil(font_height) as u16;
        self.preview_scroll_rows = self
            .preview_scroll_rows
            .saturating_add_signed(rows)
            .min(max_rows);
        self.install_visible_preview();
    }

    fn change_page(&mut self, delta: isize) {
        if self.page_count == 0 {
            return;
        }
        let next = self
            .page_index
            .saturating_add_signed(delta)
            .min(self.page_count - 1);
        self.sync_target = None;
        self.highlight = None;
        self.select_page(next);
    }

    fn select_page(&mut self, index: usize) {
        if index == self.page_index {
            return;
        }
        self.page_index = index;
        self.preview_scroll_rows = 0;
        // The shown strips stay until the new page's image arrives, so nothing flashes blank.
        self.page = None;
        self.request_view();
    }

    /// The preview pane's width in pixels, or `None` while the pane has no area.
    fn target_width(&self) -> Option<u32> {
        (self.preview_size.width > 0)
            .then(|| u32::from(self.preview_size.width) * u32::from(self.picker.font_size().width))
    }

    /// Tell the pipeline which page to keep rendered, at the pane's width.
    fn request_view(&mut self) {
        let Some(width) = self.target_width() else {
            return;
        };
        let view = (self.page_index, width);
        if self.requested_view != Some(view) {
            self.requested_view = Some(view);
            self.pipeline.send(PreviewCommand::View {
                page: view.0,
                width: view.1,
            });
        }
    }

    fn scroll_latex(&mut self, rows: i16) {
        self.latex_scroll_rows = self
            .latex_scroll_rows
            .saturating_add_signed(rows)
            .min(self.max_latex_scroll());
    }

    fn max_latex_scroll(&self) -> u16 {
        let line_count = self.generated_body().lines().count().max(1);
        let viewport = usize::from(self.latex_size.height.max(1));
        line_count
            .saturating_sub(viewport)
            .min(usize::from(u16::MAX)) as u16
    }

    fn pipeline_is_active(&self) -> bool {
        matches!(
            self.status,
            PipelineStatus::Waiting | PipelineStatus::Preparing | PipelineStatus::Typesetting
        ) || self.strips.is_staging()
    }

    fn advance_spinner(&mut self) -> bool {
        if !self.pipeline_is_active() || self.last_spinner_tick.elapsed() < SPINNER_INTERVAL {
            return false;
        }
        self.spinner_frame = self.spinner_frame.wrapping_add(1);
        self.last_spinner_tick = Instant::now();
        true
    }

    fn handle_mouse(&mut self, mouse: MouseEvent) {
        if self.show_help {
            return;
        }
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if contains(self.source_area, mouse.column, mouse.row) {
                    self.focus = PaneFocus::Source;
                    self.place_source_cursor(mouse.column, mouse.row);
                } else if contains(self.latex_area, mouse.column, mouse.row) {
                    self.focus = PaneFocus::Latex;
                } else if contains(self.preview_area, mouse.column, mouse.row) {
                    self.focus = PaneFocus::Preview;
                    self.sync_from_preview_click(mouse.column, mouse.row);
                }
            }
            MouseEventKind::ScrollUp => self.scroll_at(mouse.column, mouse.row, -3),
            MouseEventKind::ScrollDown => self.scroll_at(mouse.column, mouse.row, 3),
            _ => {}
        }
    }

    /// Move the source cursor to the note line typeset under a preview click.
    ///
    /// The click is mapped back through the width fit `install_visible_preview` applied to the
    /// shown canvas (drawn from the pane's top-left corner), then from page pixels to points.
    fn sync_from_preview_click(&mut self, column: u16, row: u16) -> bool {
        let (Some(view), Some(sync), Some(document)) = (
            self.strips.shown_key(),
            self.sync.clone(),
            self.typeset_document(),
        ) else {
            return false;
        };
        let inner = self.preview_area.inner(ratatui::layout::Margin::new(1, 1));
        if !self.strips.is_visible() || view.scale <= 0.0 || !contains(inner, column, row) {
            return false;
        }

        let font = self.picker.font_size();
        let (font_width, font_height) = (f64::from(font.width), f64::from(font.height));
        let pane_width = f64::from(inner.width) * font_width;
        let ratio = (pane_width / f64::from(view.image_width.max(1))).min(1.0);
        let x = (f64::from(column - inner.x) + 0.5) * font_width / ratio;
        let y = (f64::from(row - inner.y) + 0.5) * font_height / ratio;
        if x >= f64::from(view.image_width) {
            return false;
        }

        let scale = f64::from(view.scale);
        let Some(byte) = sync
            .backward(
                view.page_index,
                (x / scale) as f32,
                ((y + f64::from(view.crop_y)) / scale) as f32,
                |line| document.is_content_line(line),
            )
            .and_then(|line| document.source_byte_for_output_line(line))
            .map(|byte| map_offset_across_edit(document.note(), &self.buffer.text(), byte))
        else {
            return false;
        };

        self.buffer.set_cursor_byte(byte);
        self.focus = PaneFocus::Source;
        self.ensure_cursor_visible();
        true
    }

    fn scroll_at(&mut self, column: u16, row: u16, rows: i16) {
        if contains(self.source_area, column, row) {
            self.focus = PaneFocus::Source;
            for _ in 0..rows.unsigned_abs() {
                if rows < 0 {
                    self.buffer.move_up();
                } else {
                    self.buffer.move_down();
                }
            }
            self.ensure_cursor_visible();
        } else if contains(self.latex_area, column, row) {
            self.focus = PaneFocus::Latex;
            self.scroll_latex(rows);
        } else if contains(self.preview_area, column, row) {
            self.focus = PaneFocus::Preview;
            self.scroll_preview(rows);
        }
    }

    fn place_source_cursor(&mut self, column: u16, row: u16) {
        let inner = self.source_area.inner(ratatui::layout::Margin::new(1, 1));
        if !contains(inner, column, row) {
            return;
        }
        let gutter = ui::source_gutter_width(self.source_line_count(), inner.width);
        let content_x = inner.x.saturating_add(gutter);
        if column < content_x {
            return;
        }
        let line_index = self
            .source_scroll_y
            .saturating_add(usize::from(row.saturating_sub(inner.y)))
            .min(self.buffer.len_lines().saturating_sub(1));
        let target_display_column = usize::from(self.source_scroll_x)
            .saturating_add(usize::from(column.saturating_sub(content_x)));
        let line = self.buffer.line(line_index);
        let mut display_column = 0usize;
        let mut character_column = 0usize;
        for character in line.chars() {
            let width = unicode_width::UnicodeWidthChar::width(character).unwrap_or(0);
            if display_column.saturating_add(width) > target_display_column {
                break;
            }
            display_column = display_column.saturating_add(width);
            character_column += 1;
        }
        self.buffer
            .set_cursor_line_column(line_index, character_column);
        self.ensure_cursor_visible();
    }

    fn ensure_cursor_visible(&mut self) {
        let (line, _) = self.buffer.cursor_line_column();
        let height = usize::from(self.source_size.height.max(1));
        if line < self.source_scroll_y {
            self.source_scroll_y = line;
        } else if line >= self.source_scroll_y + height {
            self.source_scroll_y = line + 1 - height;
        }

        let display_column = self.buffer.cursor_display_column().min(u16::MAX as usize) as u16;
        let width = self.source_size.width.max(1);
        if display_column < self.source_scroll_x {
            self.source_scroll_x = display_column;
        } else if display_column >= self.source_scroll_x.saturating_add(width) {
            self.source_scroll_x = display_column + 1 - width;
        }
    }

    pub(crate) fn configure_layout(
        &mut self,
        source_size: Size,
        latex_size: Size,
        preview_size: Size,
    ) {
        let resized = preview_size != self.preview_size;
        self.source_size = source_size;
        self.latex_size = latex_size;
        self.preview_size = preview_size;
        self.latex_scroll_rows = self.latex_scroll_rows.min(self.max_latex_scroll());
        self.ensure_cursor_visible();
        if resized {
            self.request_view();
            self.install_visible_preview();
        }
    }

    pub(crate) fn configure_pane_areas(
        &mut self,
        source_area: Rect,
        latex_area: Rect,
        preview_area: Rect,
    ) {
        self.source_area = source_area;
        self.latex_area = latex_area;
        self.preview_area = preview_area;
    }

    pub(crate) fn source_line_count(&self) -> usize {
        self.buffer.len_lines()
    }

    pub(crate) fn source_line(&self, line: usize) -> String {
        self.buffer.line(line)
    }

    pub(crate) fn source_scroll(&self) -> (u16, u16) {
        (
            self.source_scroll_y.min(u16::MAX as usize) as u16,
            self.source_scroll_x,
        )
    }

    pub(crate) fn cursor_screen_position(&self) -> (u16, u16) {
        let (line, _) = self.buffer.cursor_line_column();
        let display_column = self.buffer.cursor_display_column().min(u16::MAX as usize) as u16;
        (
            display_column.saturating_sub(self.source_scroll_x),
            (line.saturating_sub(self.source_scroll_y)).min(u16::MAX as usize) as u16,
        )
    }

    pub(crate) fn generated_body(&self) -> &str {
        self.generated.as_ref().map_or("", LatexDocument::body)
    }

    pub(crate) fn focus(&self) -> PaneFocus {
        self.focus
    }

    pub(crate) fn set_caret(&mut self, caret: Option<Position>) {
        self.caret = caret;
    }

    pub(crate) fn zen_mode(&self) -> bool {
        self.zen_mode
    }

    pub(crate) fn cursor_line_column(&self) -> (usize, usize) {
        self.buffer.cursor_line_column()
    }

    pub(crate) fn source_stats(&self) -> (usize, Option<usize>, usize) {
        (
            self.source_characters,
            (self.generated_revision == Some(self.revision)).then_some(self.source_words),
            self.buffer.len_lines(),
        )
    }

    pub(crate) fn focus_label(&self) -> &'static str {
        match self.focus {
            PaneFocus::Source => "SOURCE",
            PaneFocus::Latex => "LATEX",
            PaneFocus::Preview => "PREVIEW",
        }
    }

    pub(crate) fn show_help(&self) -> bool {
        self.show_help
    }

    pub(crate) fn latex_scroll(&self) -> u16 {
        self.latex_scroll_rows
    }

    pub(crate) fn diagnostic_line(&self) -> Option<usize> {
        let span = self.diagnostic_span.as_ref()?;
        let source = self.buffer.text();
        let byte = span.start.min(source.len());
        Some(source[..byte].bytes().filter(|byte| *byte == b'\n').count())
    }

    pub(crate) fn status_line(&self) -> String {
        let spinner = SPINNER_FRAMES[self.spinner_frame % SPINNER_FRAMES.len()];
        match &self.status {
            PipelineStatus::Empty => String::from("type a note to begin"),
            PipelineStatus::Waiting => format!("{spinner} building LaTeX…"),
            PipelineStatus::Preparing => {
                format!("{spinner} preparing local LaTeX resources…")
            }
            PipelineStatus::Typesetting => format!("{spinner} typesetting…"),
            PipelineStatus::Ready { elapsed, warnings } => match warnings {
                0 => format!("ready in {} ms", elapsed.as_millis()),
                warnings => format!(
                    "ready in {} ms with {warnings} warning(s)",
                    elapsed.as_millis()
                ),
            },
            PipelineStatus::Error(message) => format!("error: {message}"),
        }
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

    pub(crate) fn has_preview(&self) -> bool {
        self.strips.is_visible()
    }

    pub(crate) fn preview_placeholder(&self) -> &'static str {
        if matches!(self.status, PipelineStatus::Empty) {
            "Start typing to build a LaTeX document."
        } else {
            "Typesetting the document…"
        }
    }

    pub(crate) fn render_preview(&mut self, area: Rect, buf: &mut Buffer) {
        self.strips.render(area, buf);
    }
}

/// Scale a canvas wider than `max_width` pixels down to that width, then pad it with white to
/// whole cell rows so it splits into strips. Only a page rendered for an older, wider pane is
/// ever too wide.
fn fit_to_width(canvas: RgbaImage, max_width: u32, font_height: u32) -> RgbaImage {
    if canvas.width() <= max_width || max_width == 0 {
        return canvas;
    }
    let ratio = f64::from(max_width) / f64::from(canvas.width());
    let height = (f64::from(canvas.height()) * ratio).round().max(1.0) as u32;
    let scaled = imageops::resize(&canvas, max_width, height, imageops::FilterType::Triangle);
    let mut padded = ImageBuffer::from_pixel(
        max_width,
        height.div_ceil(font_height) * font_height,
        Rgba([255, 255, 255, 255]),
    );
    imageops::replace(&mut padded, &scaled, 0, 0);
    padded
}

fn contains(area: Rect, column: u16, row: u16) -> bool {
    area.width > 0
        && area.height > 0
        && column >= area.x
        && column < area.x.saturating_add(area.width)
        && row >= area.y
        && row < area.y.saturating_add(area.height)
}

fn spawn_terminal_reader(events: mpsc::Sender<LoopEvent>) {
    thread::spawn(move || {
        loop {
            let event = match event::read() {
                Ok(event) => LoopEvent::Terminal(event),
                Err(error) => {
                    let _ = events.send(LoopEvent::TerminalFailed(error));
                    return;
                }
            };
            if events.send(event).is_err() {
                return;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::driver::TexMessage;

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn recording_app() -> App {
        let mut app =
            App::with_pipeline(
                Picker::halfblocks(),
                |_| PipelineLink::Recording(Vec::new()),
            );
        app.configure_layout(Size::new(40, 20), Size::new(30, 20), Size::new(20, 8));
        app
    }

    fn recorded(app: &App) -> &[PreviewCommand] {
        match &app.pipeline {
            PipelineLink::Recording(sent) => sent,
            _ => &[],
        }
    }

    /// Submit the current text and wait until the worker has built it.
    fn build(app: &mut App) {
        app.maybe_submit_build();
        for _ in 0..200 {
            app.process_background_events();
            if app.generated_revision == Some(app.revision) {
                return;
            }
            thread::sleep(Duration::from_millis(5));
        }
        panic!("timed out waiting for the note build");
    }

    fn wait_for_encoded_strips(app: &mut App) {
        for _ in 0..200 {
            app.process_background_events();
            if !app.strips.is_staging() {
                return;
            }
            thread::sleep(Duration::from_millis(5));
        }
        panic!("timed out waiting for terminal preview encoding");
    }

    fn white_page(index: usize, width: u32, height: u32, fingerprint: u64) -> RenderedPage {
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

    fn finished(revision: u64, messages: Vec<TexMessage>, sync: Option<SyncIndex>) -> LoopEvent {
        LoopEvent::Preview(PreviewEvent::Finished {
            revision,
            pages: 1,
            elapsed: Duration::from_millis(12),
            messages,
            sync: sync.map(Arc::new),
        })
    }

    /// A built note `alpha\nbeta` whose typeset page has line boxes for `alpha` at 500 pt and
    /// `beta` at 1000 pt on a 200×2000 px page rendered at one pixel per point.
    fn synced_app() -> (App, SyncIndex, usize) {
        let mut app = recording_app();
        app.buffer.insert_str("alpha\nbeta");
        app.mark_edited();
        build(&mut app);

        let document = &app.sent[&app.revision];
        let alpha = document
            .output_line_for_source_byte(0)
            .expect("alpha is typeset");
        let beta = document
            .output_line_for_source_byte(6)
            .expect("beta is typeset");
        let sync = SyncIndex::parse(&format!(
            "SyncTeX Version:1\nInput:1:texput\nMagnification:1000\nUnit:1\nContent:\n{{1\n\
             [1,1:0,0:0,0,0\n\
             (1,{alpha}:4736287,32890880:39469056,655360,196608\n\
             g1,{alpha}:13156352,32890880\n)\n\
             (1,{beta}:4736287,65781760:39469056,655360,196608\n\
             g1,{beta}:13156352,65781760\n)\n]\n}}1\n"
        ))
        .expect("fixture parses");
        app.handle_loop_event(LoopEvent::Preview(PreviewEvent::Page(white_page(
            0, 200, 2000, 1,
        ))));
        app.handle_loop_event(finished(app.revision, Vec::new(), Some(sync.clone())));
        (app, sync, beta)
    }

    #[test]
    fn f6_cycles_focus_without_repurposing_tab() {
        let mut app = App::default();
        assert_eq!(app.focus(), PaneFocus::Source);

        app.handle_key(press(KeyCode::F(6)));
        assert_eq!(app.focus(), PaneFocus::Latex);
        app.handle_key(press(KeyCode::F(6)));
        assert_eq!(app.focus(), PaneFocus::Preview);
        app.handle_key(press(KeyCode::F(6)));
        assert_eq!(app.focus(), PaneFocus::Source);

        app.handle_key(press(KeyCode::Tab));
        assert_eq!(app.buffer.text(), "    ");
    }

    #[test]
    fn inspector_navigation_uses_h_and_l_for_focus() {
        let mut app = App::default();
        app.handle_key(press(KeyCode::F(6)));
        assert_eq!(app.focus(), PaneFocus::Latex);

        app.handle_key(press(KeyCode::Char('l')));
        assert_eq!(app.focus(), PaneFocus::Preview);
        app.handle_key(press(KeyCode::Char('h')));
        assert_eq!(app.focus(), PaneFocus::Latex);
    }

    #[test]
    fn f2_toggles_distraction_free_zen_mode() {
        let mut app = App::default();
        assert!(!app.zen_mode());
        app.handle_key(press(KeyCode::F(2)));
        assert!(app.zen_mode());
        app.handle_key(press(KeyCode::F(2)));
        assert!(!app.zen_mode());
    }

    #[test]
    fn mouse_click_focuses_panels_and_places_the_source_cursor() {
        let mut app = App::default();
        app.buffer.insert_str("abc\ndef");
        app.mark_edited();
        app.configure_pane_areas(
            Rect::new(0, 0, 40, 10),
            Rect::new(40, 0, 30, 10),
            Rect::new(70, 0, 30, 10),
        );

        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 6,
            row: 2,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(app.focus(), PaneFocus::Source);
        assert_eq!(app.cursor_line_column(), (1, 3));

        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 45,
            row: 2,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(app.focus(), PaneFocus::Latex);
    }

    #[test]
    fn edited_source_is_built_on_the_worker_not_the_typing_path() {
        let mut app = App::default();
        app.buffer.insert_str("x plus y");
        app.mark_edited();

        assert_eq!(app.generated_revision, None);
        assert!(matches!(app.status, PipelineStatus::Waiting));
        assert_eq!(app.source_stats().0, 8);

        build(&mut app);
        assert_eq!(app.source_stats().1, Some(3));
    }

    #[test]
    fn background_parse_errors_map_back_to_the_source() {
        let mut app = App::default();
        app.buffer.insert_str("$$\nx");
        app.mark_edited();
        app.maybe_submit_build();

        for _ in 0..200 {
            app.process_background_events();
            if matches!(app.status, PipelineStatus::Error(_)) {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        assert!(matches!(app.status, PipelineStatus::Error(_)));
        assert_eq!(app.diagnostic_line(), Some(0));
    }

    #[test]
    fn each_built_revision_is_sent_for_typesetting() {
        let mut app = recording_app();
        app.buffer.insert_str("alpha");
        app.mark_edited();
        build(&mut app);

        let document = recorded(&app)
            .iter()
            .rev()
            .find_map(|command| match command {
                PreviewCommand::Document { revision, source } => Some((*revision, source)),
                PreviewCommand::View { .. } => None,
            });
        let (revision, source) = document.expect("the build was sent");
        assert_eq!(revision, app.revision);
        assert!(source.contains("alpha"));
        assert!(matches!(app.status, PipelineStatus::Typesetting));
    }

    #[test]
    fn the_pane_width_is_requested_from_the_pipeline() {
        let app = recording_app();
        let width = 20 * u32::from(app.picker.font_size().width);
        assert!(recorded(&app).iter().any(|command| matches!(
            command,
            PreviewCommand::View { page: 0, width: requested } if *requested == width
        )));
    }

    #[test]
    fn finished_typesetting_reports_ready_and_highlights_the_cursor_line() {
        let (app, sync, beta) = synced_app();

        assert_eq!(app.status_line(), "ready in 12 ms");
        let target = sync.forward(beta).expect("beta has a line box");
        let (top, bottom) = (target.top.floor() as u32, target.bottom.ceil() as u32);
        assert_eq!(app.highlight, Some((top, bottom)));

        let font_height = u32::from(app.picker.font_size().height);
        let viewport = u32::from(app.preview_size.height) * font_height;
        let crop = u32::from(app.preview_scroll_rows) * font_height;
        assert!(crop > 0);
        assert!(crop <= top && bottom <= crop + viewport);
    }

    #[test]
    fn typing_on_the_highlighted_line_keeps_the_encoded_preview() {
        let (mut app, _, _) = synced_app();
        wait_for_encoded_strips(&mut app);
        let highlight = app.highlight;

        app.handle_event(Event::Key(press(KeyCode::Char('x'))));

        assert_ne!(Some(app.revision), app.typeset_revision);
        assert_eq!(app.highlight, highlight);
        assert!(!app.strips.is_staging());
    }

    #[test]
    fn clicking_the_preview_moves_the_source_cursor_to_that_line() {
        let (mut app, _, _) = synced_app();
        wait_for_encoded_strips(&mut app);
        app.buffer.set_cursor_line_column(0, 0);
        app.configure_pane_areas(
            Rect::new(0, 0, 40, 10),
            Rect::new(40, 0, 30, 10),
            Rect::new(70, 0, 22, 10),
        );

        let view = app.strips.shown_key().expect("preview is encoded");
        let (top, bottom) = view.highlight.expect("beta is highlighted");
        let font_height = u32::from(app.picker.font_size().height);
        let row = 1 + ((top + bottom) / 2 - view.crop_y) / font_height;
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 75,
            row: row as u16,
            modifiers: KeyModifiers::NONE,
        });

        assert_eq!(app.focus(), PaneFocus::Source);
        assert_eq!(app.cursor_line_column(), (1, 0));
    }

    #[test]
    fn a_change_within_one_cell_row_re_encodes_only_that_strip() {
        let mut app = recording_app();
        let font = app.picker.font_size();
        let (width, height) = (20 * u32::from(font.width), 8 * u32::from(font.height));
        app.handle_loop_event(LoopEvent::Preview(PreviewEvent::Page(white_page(
            0, width, height, 1,
        ))));
        wait_for_encoded_strips(&mut app);
        assert!(app.has_preview());

        let mut page = ImageBuffer::from_pixel(width, height, Rgba([255, 255, 255, 255]));
        let row_top = 3 * u32::from(font.height);
        for y in row_top + 1..row_top + 4 {
            for x in 0..width / 2 {
                page.put_pixel(x, y, Rgba([0, 0, 0, 255]));
            }
        }
        let mut changed = white_page(0, width, height, 2);
        changed.image = Arc::new(page);
        app.handle_loop_event(LoopEvent::Preview(PreviewEvent::Page(changed)));

        assert_eq!(app.strips.staged_strips(), Some(&[3][..]));
        wait_for_encoded_strips(&mut app);
        assert!(app.has_preview());
    }

    #[test]
    fn tex_errors_mark_their_note_line() {
        let mut app = recording_app();
        app.buffer.insert_str("alpha\nbeta");
        app.mark_edited();
        build(&mut app);
        let beta = app.sent[&app.revision]
            .output_line_for_source_byte(6)
            .expect("beta is typeset");

        app.handle_loop_event(finished(
            app.revision,
            vec![TexMessage {
                error: true,
                message: String::from("Undefined control sequence"),
                tex_line: Some(beta),
            }],
            None,
        ));

        assert_eq!(app.status_line(), "error: Undefined control sequence");
        assert_eq!(app.diagnostic_line(), Some(1));
    }

    #[test]
    fn deleting_the_last_visible_character_restores_empty_state() {
        let mut app = App::default();
        app.buffer.insert_char('x');
        app.mark_edited();
        assert!(matches!(app.status, PipelineStatus::Waiting));

        assert!(app.buffer.backspace());
        app.mark_edited();
        assert!(matches!(app.status, PipelineStatus::Empty));
        assert_eq!(app.source_stats(), (0, Some(0), 1));
    }

    #[test]
    fn whitespace_only_notes_remain_in_the_empty_state() {
        let mut app = App::default();
        app.buffer.insert_str(" \n\t");
        app.mark_edited();
        app.maybe_submit_build();

        assert!(matches!(app.status, PipelineStatus::Empty));
        assert_eq!(app.status_line(), "type a note to begin");
        assert_eq!(
            app.preview_placeholder(),
            "Start typing to build a LaTeX document."
        );
    }

    #[test]
    fn help_overlay_can_be_opened_and_closed_without_quitting() {
        let mut app = App::default();
        app.handle_key(press(KeyCode::F(1)));
        assert!(app.show_help());

        app.handle_key(press(KeyCode::Esc));
        assert!(!app.show_help());
        assert!(!app.should_quit);
    }
}
