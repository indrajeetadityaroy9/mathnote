//! The live preview pipeline on one background thread of the editor: the snapshot driver.
//!
//! The thread owns the snapshot engine ([`TexDriver`]), the incremental XDV document, the font
//! store and SyncTeX. It receives each revision's LaTeX and the page the preview pane shows, and
//! reports typesetting progress, rendered pages, and the SyncTeX index of every finished run.

use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use image::RgbaImage;

use crate::app::LoopEvent;
use crate::cache::{cache_is_warmed, default_cache_dir, ensure_cache_warm};
use crate::driver::{DriverConfig, EngineState, TexDriver, TexMessage};
use crate::dvi::{BundleFontFiles, FontStore, XdvDocument, render_page};
use crate::latex::{BODY_SNAPSHOT_OFFSET, TEMPLATE_MATH_FONTS};
use crate::synctex::SyncIndex;

/// How long one engine step may serve queries before new commands are looked at.
const ENGINE_STEP: Duration = Duration::from_millis(8);

pub(crate) enum PreviewCommand {
    /// The complete LaTeX source of a revision.
    Document { revision: u64, source: String },
    /// Keep `page` rendered at `width` pixels whenever its content changes.
    View { page: usize, width: u32 },
}

pub(crate) enum PreviewEvent {
    /// The local LaTeX resources are being prepared before the first run.
    Preparing,
    /// The pipeline cannot run at all.
    Failed(String),
    Page(RenderedPage),
    /// The engine finished `revision`: its pages are final and its SyncTeX data is current.
    Finished {
        revision: u64,
        pages: usize,
        elapsed: Duration,
        messages: Vec<TexMessage>,
        sync: Option<Arc<SyncIndex>>,
    },
    /// Problems the page renderer hit (missing fonts or glyphs, unreadable output), each
    /// reported once.
    Warnings(Vec<String>),
}

/// One page rendered for the preview pane.
pub(crate) struct RenderedPage {
    pub index: usize,
    /// Pages the document has so far.
    pub pages: usize,
    pub fingerprint: u64,
    /// Pixels per big point.
    pub scale: f32,
    pub image: Arc<RgbaImage>,
}

pub(crate) fn preview_thread(
    commands: mpsc::Receiver<PreviewCommand>,
    events: mpsc::Sender<LoopEvent>,
) {
    let send = |event| events.send(LoopEvent::Preview(event)).is_ok();
    let cache_dir = default_cache_dir();
    if !cache_is_warmed(&cache_dir) && !send(PreviewEvent::Preparing) {
        return;
    }
    let setup = ensure_cache_warm(&cache_dir)
        .map_err(|error| error.to_string())
        .and_then(|()| BundleFontFiles::open(&cache_dir))
        .and_then(|files| {
            let engine_exe = std::env::current_exe().map_err(|error| error.to_string())?;
            let mut driver = TexDriver::new(DriverConfig {
                engine_exe,
                cache_dir: cache_dir.clone(),
            });
            // Snapshot every fresh engine at the end of the shared preamble, so even the first
            // edit after launch resumes there instead of typesetting from scratch.
            driver.set_root_snapshot(BODY_SNAPSHOT_OFFSET);
            Ok((driver, FontStore::new(files)))
        });
    let (driver, fonts) = match setup {
        Ok(setup) => setup,
        Err(message) => {
            send(PreviewEvent::Failed(message));
            return;
        }
    };

    let mut pipeline = Pipeline {
        events: &events,
        driver,
        fonts,
        xdv: XdvDocument::new(),
        revision: 0,
        received: Instant::now(),
        reported: None,
        view: None,
        rendered: None,
        fonts_preloaded: false,
    };
    pipeline.run(&commands);
}

struct Pipeline<'a> {
    events: &'a mpsc::Sender<LoopEvent>,
    driver: TexDriver,
    fonts: FontStore,
    xdv: XdvDocument,
    /// Newest revision and when it arrived.
    revision: u64,
    received: Instant,
    /// The revision whose finish was reported.
    reported: Option<u64>,
    /// Page and pixel width the pane shows.
    view: Option<(usize, u32)>,
    /// What was last rendered for the view: page, width, fingerprint.
    rendered: Option<(usize, u32, u64)>,
    /// Whether the template's math fonts have been loaded ahead of use.
    fonts_preloaded: bool,
}

impl Pipeline<'_> {
    fn run(&mut self, commands: &mpsc::Receiver<PreviewCommand>) {
        loop {
            let running = self.driver.state() == EngineState::Running;
            let command = if running {
                commands.try_recv().ok()
            } else {
                // Idle after a run: load the math fonts now rather than on the first page that
                // needs them (not before the first run, which would delay it).
                if self.driver.state() == EngineState::Finished {
                    self.preload_fonts();
                    if !self.forward_warnings() {
                        return;
                    }
                }
                match commands.recv() {
                    Ok(command) => Some(command),
                    Err(_) => return,
                }
            };
            if let Some(command) = command {
                self.handle(command);
                while let Ok(command) = commands.try_recv() {
                    self.handle(command);
                }
            }

            if self.driver.state() == EngineState::Running && self.driver.step(ENGINE_STEP) {
                self.update_pages();
            }
            if !self.render_view() || !self.report_finish() || !self.forward_warnings() {
                return;
            }
        }
    }

    fn handle(&mut self, command: PreviewCommand) {
        match command {
            PreviewCommand::Document { revision, source } => {
                self.revision = revision;
                self.received = Instant::now();
                self.driver.set_document(source.as_bytes());
                self.update_pages();
            }
            PreviewCommand::View { page, width } => self.view = Some((page, width)),
        }
    }

    fn update_pages(&mut self) {
        let truncated = self.driver.take_xdv_truncation();
        self.xdv.update(self.driver.xdv(), truncated);
    }

    /// Load the classic math fonts the template uses once, while the engine is idle. The cache
    /// bootstrap fetches their files, so they are always cached; the text font is loaded by the
    /// first page render.
    fn preload_fonts(&mut self) {
        if !self.fonts_preloaded {
            self.fonts_preloaded = true;
            self.fonts.preload(TEMPLATE_MATH_FONTS);
        }
    }

    /// Send the renderer's new warnings to the editor; `false` once the editor is gone.
    fn forward_warnings(&mut self) -> bool {
        let mut warnings = self.xdv.take_warnings();
        warnings.extend(self.fonts.take_warnings());
        warnings.is_empty()
            || self
                .events
                .send(LoopEvent::Preview(PreviewEvent::Warnings(warnings)))
                .is_ok()
    }

    /// Render the viewed page when it or the view changed; `false` once the editor is gone.
    fn render_view(&mut self) -> bool {
        let Some((index, width)) = self.view else {
            return true;
        };
        let Some(page) = self.xdv.page(index) else {
            return true;
        };
        if self.rendered == Some((index, width, page.fingerprint)) || width == 0 {
            return true;
        }
        self.rendered = Some((index, width, page.fingerprint));
        let scale = width as f32 / page.width;
        let image = render_page(page, &mut self.fonts, scale);
        let rendered = RenderedPage {
            index,
            pages: self.xdv.page_count(),
            fingerprint: page.fingerprint,
            scale,
            image: Arc::new(image),
        };
        self.events
            .send(LoopEvent::Preview(PreviewEvent::Page(rendered)))
            .is_ok()
    }

    /// Report the newest revision once the engine has finished it.
    fn report_finish(&mut self) -> bool {
        if self.driver.state() != EngineState::Finished || self.reported == Some(self.revision) {
            return true;
        }
        self.update_pages();
        // The finished run may have changed the viewed page after its last render.
        if !self.render_view() {
            return false;
        }
        self.reported = Some(self.revision);
        let sync = self
            .driver
            .synctex_gz()
            .and_then(|bytes| SyncIndex::from_gzip(bytes).ok())
            .map(Arc::new);
        self.events
            .send(LoopEvent::Preview(PreviewEvent::Finished {
                revision: self.revision,
                pages: self.xdv.page_count(),
                elapsed: self.received.elapsed(),
                messages: self.driver.messages(),
                sync,
            }))
            .is_ok()
    }
}
