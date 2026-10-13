//! Terminal preview image split into horizontal strips of whole cell rows.
//!
//! Each strip is its own terminal image. When a new preview differs from the shown one only in
//! a few rows (typing on one line, moving the sync highlight), only those strips are encoded and
//! sent to the terminal; every other strip keeps its already transmitted image.
//!
//! The UI thread only hands over a [`Canvas`] description (a shared page image, the visible rows
//! and the highlight). A background thread composes the strips from the page, compares them with
//! the shown ones and encodes the changed ones; a new set of strips replaces the shown set only
//! once all of its changed strips are ready, so the preview never shows a half-updated page.
//!
//! Kitty-protocol terminals (Kitty, Ghostty, WezTerm) get zlib-compressed RGB data placed with
//! Unicode placeholders; a mostly white page row compresses to a few kilobytes. Other terminals
//! go through ratatui-image (iTerm2 PNG, Sixel, or half blocks).

use std::env;
use std::hash::{BuildHasher, RandomState};
use std::io::Write;
use std::num::NonZeroU16;
use std::sync::{Arc, mpsc};
use std::thread;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use flate2::Compression;
use flate2::write::ZlibEncoder;
use image::{DynamicImage, ImageBuffer, Rgba, RgbaImage, imageops};
use ratatui::buffer::{Buffer, CellDiffOption};
use ratatui::layout::{Rect, Size};
use ratatui::style::{Color, Style};
use ratatui::widgets::Widget;
use ratatui_image::errors::Errors as ImageError;
use ratatui_image::picker::{Picker, ProtocolType};
use ratatui_image::protocol::Protocol;
use ratatui_image::{FontSize, Image, Resize};

/// Sixel images are painted in bands of this many pixel rows.
const SIXEL_BAND_HEIGHT: u32 = 6;
/// Kitty's limit for the base64 payload of one graphics command.
const KITTY_CHUNK: usize = 4096;
/// Kitty Unicode placeholder, and the diacritic numbering row, column and id byte 0.
const PLACEHOLDER: char = '\u{10EEEE}';
const DIACRITIC_ZERO: char = '\u{0305}';

/// What the preview pane shows: part of a rendered page, with the sync highlight.
pub struct Canvas {
    pub page: Arc<RgbaImage>,
    /// First page pixel row shown.
    pub top: u32,
    /// Visible height in pixels, a whole number of cell rows; rows past the page are white.
    pub height: u32,
    /// Highlighted page pixel rows `[start, end)`, multiplied by `tint`.
    pub highlight: Option<(u32, u32)>,
    pub tint: Option<[u8; 3]>,
    /// Pane width in pixels; a wider page (rendered for an older, wider pane) is scaled down.
    pub max_width: u32,
}

/// The strips of one install: every strip's pixels, and the encoded changed ones.
pub struct EncodedStrips {
    generation: u64,
    pixels: Vec<Arc<RgbaImage>>,
    images: Vec<(usize, Result<StripImage, ImageError>)>,
}

impl EncodedStrips {
    /// Number of strips that changed and were encoded.
    pub fn changed_strips(&self) -> usize {
        self.images.len()
    }

    /// Bytes of terminal graphics commands the changed strips will send, for Kitty-protocol
    /// strips (other protocols encode inside ratatui-image and report 0).
    pub fn kitty_bytes(&self) -> usize {
        self.images
            .iter()
            .filter_map(|(_, image)| match image {
                Ok(StripImage::Kitty(kitty)) => kitty.transmit.as_ref().map(String::len),
                _ => None,
            })
            .sum()
    }
}

struct EncodeJob {
    generation: u64,
    canvas: Canvas,
    /// Pixels of the strips on screen when the job was made, shared with the UI thread.
    shown: Vec<Arc<RgbaImage>>,
}

enum StripImage {
    Kitty(KittyStrip),
    Other(Protocol),
}

/// One strip placed through Kitty Unicode placeholders.
struct KittyStrip {
    id: u32,
    columns: u16,
    /// The graphics commands that transmit the pixels; sent with the first frame drawing it.
    transmit: Option<String>,
}

impl KittyStrip {
    fn encode(pixels: &RgbaImage, columns: u16, id: u32, inside_tmux: bool) -> Self {
        let rgb = pixels
            .as_raw()
            .as_chunks::<4>()
            .0
            .iter()
            .flat_map(|pixel| [pixel[0], pixel[1], pixel[2]])
            .collect::<Vec<_>>();
        let mut zlib = ZlibEncoder::new(Vec::new(), Compression::fast());
        zlib.write_all(&rgb).expect("writing to memory");
        let payload = BASE64.encode(zlib.finish().expect("writing to memory"));

        let (start, escape, end) = if inside_tmux {
            ("\x1bPtmux;", "\x1b\x1b", "\x1b\\")
        } else {
            ("", "\x1b", "")
        };
        let mut transmit = String::with_capacity(payload.len() + 128);
        let chunks = payload.as_bytes().chunks(KITTY_CHUNK).collect::<Vec<_>>();
        for (index, chunk) in chunks.iter().enumerate() {
            let more = u8::from(index + 1 < chunks.len());
            transmit.push_str(start);
            transmit.push_str(escape);
            if index == 0 {
                transmit.push_str(&format!(
                    "_Gq=2,a=T,U=1,f=24,o=z,t=d,i={id},s={},v={},m={more};",
                    pixels.width(),
                    pixels.height()
                ));
            } else {
                transmit.push_str(&format!("_Gm={more};"));
            }
            transmit.push_str(std::str::from_utf8(chunk).expect("base64 is ASCII"));
            transmit.push_str(escape);
            transmit.push('\\');
            transmit.push_str(end);
        }
        Self {
            id,
            columns,
            transmit: Some(transmit),
        }
    }

    /// One row of placeholders in the first cell (the id is the foreground colour), the rest of
    /// the row skipped so ratatui does not overwrite it.
    fn render(&mut self, area: Rect, buf: &mut Buffer) {
        let columns = self.columns.min(area.width);
        if columns == 0 {
            return;
        }
        let mut symbol = self.transmit.take().unwrap_or_default();
        symbol.push(PLACEHOLDER);
        symbol.push(DIACRITIC_ZERO);
        symbol.push(DIACRITIC_ZERO);
        symbol.extend(std::iter::repeat_n(PLACEHOLDER, usize::from(columns) - 1));
        let [_, red, green, blue] = self.id.to_be_bytes();
        if let Some(cell) = buf.cell_mut((area.x, area.y)) {
            cell.set_symbol(&symbol)
                .set_style(Style::default().fg(Color::Rgb(red, green, blue)))
                .set_diff_option(CellDiffOption::ForcedWidth(NonZeroU16::MIN));
        }
        for x in 1..columns {
            if let Some(cell) = buf.cell_mut((area.x + x, area.y)) {
                cell.set_diff_option(CellDiffOption::Skip);
            }
        }
    }
}

struct Strip {
    pixels: Arc<RgbaImage>,
    rows: u16,
    image: StripImage,
}

struct Staged<K> {
    generation: u64,
    key: K,
}

pub struct StripPreview<K> {
    encoder: mpsc::Sender<EncodeJob>,
    font: FontSize,
    generation: u64,
    shown_key: Option<K>,
    shown: Vec<Strip>,
    staged: Option<Staged<K>>,
    /// Indices of the strips the last accepted install re-encoded.
    #[cfg(test)]
    last_encoded: Vec<usize>,
}

impl<K: Copy + PartialEq> StripPreview<K> {
    /// Start the encoder thread. `deliver` hands each encoded install back to the UI loop and
    /// returns `false` once nobody listens any more.
    pub fn new(picker: Picker, deliver: impl Fn(EncodedStrips) -> bool + Send + 'static) -> Self {
        let font = picker.font_size();
        let font_height = u32::from(font.height.max(1));
        let strip_height = match picker.protocol_type() {
            // Sixel strips span whole bands so a strip never paints into the next one's rows.
            ProtocolType::Sixel => {
                SIXEL_BAND_HEIGHT * font_height / gcd(font_height, SIXEL_BAND_HEIGHT)
            }
            ProtocolType::Halfblocks | ProtocolType::Kitty | ProtocolType::Iterm2 => font_height,
        };
        // Strip `i` always uses Kitty image id `kitty_ids + i`, so a changed strip replaces its
        // predecessor in the terminal's image store instead of accumulating new images. Ids stay
        // below 2^24: the top byte is 0, so placeholders need no third diacritic.
        let kitty_ids = (RandomState::new().hash_one(0u8) as u32 & 0x00ff_ffff).max(1);

        let (encoder, jobs) = mpsc::channel::<EncodeJob>();
        thread::spawn(move || {
            let inside_tmux = inside_tmux();
            while let Ok(mut job) = jobs.recv() {
                // A newer install supersedes every queued one.
                while let Ok(newer) = jobs.try_recv() {
                    job = newer;
                }
                let pixels = compose_strips(&job.canvas, strip_height, font_height)
                    .into_iter()
                    .map(Arc::new)
                    .collect::<Vec<_>>();
                let images = pixels
                    .iter()
                    .enumerate()
                    .filter(|(index, strip)| {
                        job.shown
                            .get(*index)
                            .is_none_or(|shown| shown.as_raw() != strip.as_raw())
                    })
                    .map(|(index, strip)| {
                        let size = cells(strip, font);
                        let image = match picker.protocol_type() {
                            ProtocolType::Kitty => Ok(StripImage::Kitty(KittyStrip::encode(
                                strip,
                                size.width,
                                (kitty_ids + index as u32) & 0x00ff_ffff,
                                inside_tmux,
                            ))),
                            ProtocolType::Halfblocks
                            | ProtocolType::Sixel
                            | ProtocolType::Iterm2 => picker
                                .new_protocol(
                                    DynamicImage::ImageRgba8(RgbaImage::clone(strip)),
                                    size,
                                    Resize::Fit(None),
                                )
                                .map(StripImage::Other),
                        };
                        (index, image)
                    })
                    .collect();
                if !deliver(EncodedStrips {
                    generation: job.generation,
                    pixels,
                    images,
                }) {
                    return;
                }
            }
        });

        Self {
            encoder,
            font,
            generation: 0,
            shown_key: None,
            shown: Vec::new(),
            staged: None,
            #[cfg(test)]
            last_encoded: Vec::new(),
        }
    }

    /// Whether `key` is the newest requested preview: being encoded, or shown with nothing
    /// pending.
    pub(crate) fn holds(&self, key: &K) -> bool {
        match &self.staged {
            Some(staged) => staged.key == *key,
            None => self.shown_key.as_ref() == Some(key),
        }
    }

    pub(crate) fn shown_key(&self) -> Option<K> {
        self.shown_key
    }

    pub(crate) fn is_visible(&self) -> bool {
        !self.shown.is_empty()
    }

    pub fn is_staging(&self) -> bool {
        self.staged.is_some()
    }

    /// Indices of the strips the last accepted install re-encoded.
    #[cfg(test)]
    pub(crate) fn last_encoded(&self) -> &[usize] {
        &self.last_encoded
    }

    /// Show `canvas` under `key`. The strips are composed, compared with the shown ones and
    /// encoded on the background thread; the UI thread only queues the job.
    pub fn install(&mut self, key: K, canvas: Canvas) {
        self.generation = self.generation.wrapping_add(1);
        // The shown set changes only by accepting the staged generation, so these are still the
        // strips on screen when this job's result is accepted.
        let job = EncodeJob {
            generation: self.generation,
            canvas,
            shown: self
                .shown
                .iter()
                .map(|strip| Arc::clone(&strip.pixels))
                .collect(),
        };
        self.staged = Some(Staged {
            generation: self.generation,
            key,
        });
        // The encoder only stops when the UI loop's receiver is gone, i.e. during shutdown.
        let _ = self.encoder.send(job);
    }

    /// Swap in the strips of the pending install once they are encoded; results of superseded
    /// installs are dropped. Returns whether the shown preview changed.
    pub fn accept(&mut self, encoded: EncodedStrips) -> Result<bool, ImageError> {
        let Some(staged) = self
            .staged
            .take_if(|staged| staged.generation == encoded.generation)
        else {
            return Ok(false);
        };

        let mut images = encoded.pixels.iter().map(|_| None).collect::<Vec<_>>();
        #[cfg(test)]
        {
            self.last_encoded = encoded.images.iter().map(|(index, _)| *index).collect();
        }
        for (index, image) in encoded.images {
            images[index] = Some(image?);
        }
        let mut previous = std::mem::take(&mut self.shown)
            .into_iter()
            .map(|strip| Some(strip.image))
            .collect::<Vec<_>>();

        self.shown = encoded
            .pixels
            .into_iter()
            .zip(images)
            .enumerate()
            .filter_map(|(index, (pixels, image))| {
                // Unchanged strips keep the image already transmitted to the terminal.
                let image = image.or_else(|| previous.get_mut(index).and_then(Option::take))?;
                Some(Strip {
                    rows: cells(&pixels, self.font).height,
                    pixels,
                    image,
                })
            })
            .collect();
        self.shown_key = Some(staged.key);
        Ok(true)
    }

    /// Draw the shown strips top to bottom from the top-left corner of `area`.
    pub(crate) fn render(&mut self, area: Rect, buf: &mut Buffer) {
        let mut y = area.y;
        for strip in &mut self.shown {
            if y + strip.rows > area.bottom() {
                return;
            }
            let rect = Rect::new(area.x, y, area.width, strip.rows);
            match &mut strip.image {
                StripImage::Kitty(kitty) => kitty.render(rect, buf),
                StripImage::Other(protocol) => Image::new(protocol).render(rect, buf),
            }
            y += strip.rows;
        }
    }
}

/// Size of a strip in terminal cells.
fn cells(strip: &RgbaImage, font: FontSize) -> Size {
    Size::new(
        strip.width().div_ceil(u32::from(font.width.max(1))) as u16,
        strip.height().div_ceil(u32::from(font.height.max(1))) as u16,
    )
}

/// The strips of `canvas`, `strip_height` pixel rows each (the last may be shorter).
///
/// Rows are copied straight from the page, padded with white below it, and tinted where
/// highlighted. Only a page wider than the pane, rendered for an older and wider pane, goes
/// through one full-canvas copy to be scaled down to the pane's width first.
fn compose_strips(canvas: &Canvas, strip_height: u32, font_height: u32) -> Vec<RgbaImage> {
    let page = canvas.page.as_ref();
    if page.width() > canvas.max_width && canvas.max_width > 0 {
        let full = compose_rows(canvas, 0, canvas.height);
        let scaled = fit_to_width(full, canvas.max_width, font_height);
        return (0..scaled.height())
            .step_by(strip_height as usize)
            .map(|top| {
                let height = strip_height.min(scaled.height() - top);
                imageops::crop_imm(&scaled, 0, top, scaled.width(), height).to_image()
            })
            .collect();
    }
    (0..canvas.height)
        .step_by(strip_height as usize)
        .map(|top| compose_rows(canvas, top, strip_height.min(canvas.height - top)))
        .collect()
}

/// Canvas rows `[first, first + count)`: page rows from `canvas.top`, white past the page,
/// multiplied by the tint inside the highlight.
fn compose_rows(canvas: &Canvas, first: u32, count: u32) -> RgbaImage {
    let page = canvas.page.as_ref();
    let width = page.width();
    let row_bytes = width as usize * 4;
    let mut out = vec![255u8; row_bytes * count as usize];
    for (row, line) in out.chunks_exact_mut(row_bytes).enumerate() {
        let page_row = canvas.top + first + row as u32;
        if page_row >= page.height() {
            continue;
        }
        let start = page_row as usize * row_bytes;
        line.copy_from_slice(&page.as_raw()[start..start + row_bytes]);
        if let (Some((top, bottom)), Some(tint)) = (canvas.highlight, canvas.tint)
            && (top..bottom).contains(&page_row)
        {
            for pixel in line.as_chunks_mut::<4>().0 {
                for (channel, factor) in pixel.iter_mut().zip(tint) {
                    *channel = (u16::from(*channel) * u16::from(factor) / 255) as u8;
                }
            }
        }
    }
    RgbaImage::from_raw(width, count, out).expect("buffer matches the dimensions")
}

/// Scale a canvas wider than `max_width` pixels down to that width, then pad it with white to
/// whole cell rows so it splits into strips.
fn fit_to_width(canvas: RgbaImage, max_width: u32, font_height: u32) -> RgbaImage {
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

/// The rule ratatui-image's picker uses to decide whether image escapes need tmux passthrough.
fn inside_tmux() -> bool {
    env::var("TERM").is_ok_and(|term| term.starts_with("tmux"))
        || env::var("TERM_PROGRAM").is_ok_and(|program| program == "tmux")
}

fn gcd(mut left: u32, mut right: u32) -> u32 {
    while right != 0 {
        (left, right) = (right, left % right);
    }
    left
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 2×4 page whose rows are 10, 20, 30, 40 (gray), shown from row 1 in 4 rows.
    fn canvas(highlight: Option<(u32, u32)>, max_width: u32) -> Canvas {
        let page = RgbaImage::from_fn(2, 4, |_, y| {
            let value = 10 * (y as u8 + 1);
            Rgba([value, value, value, 255])
        });
        Canvas {
            page: Arc::new(page),
            top: 1,
            height: 4,
            highlight,
            tint: Some([255, 0, 128]),
            max_width,
        }
    }

    fn row(strips: &[RgbaImage], strip_height: u32, row: u32) -> [u8; 4] {
        strips[(row / strip_height) as usize]
            .get_pixel(0, row % strip_height)
            .0
    }

    #[test]
    fn strips_copy_the_visible_rows_pad_with_white_and_tint_the_highlight() {
        let strips = compose_strips(&canvas(Some((2, 3)), 2), 2, 2);

        assert_eq!(strips.len(), 2);
        assert!(strips.iter().all(|strip| strip.dimensions() == (2, 2)));
        assert_eq!(row(&strips, 2, 0), [20, 20, 20, 255]);
        assert_eq!(row(&strips, 2, 1), [30, 0, 15, 255], "page row 2 is tinted");
        assert_eq!(row(&strips, 2, 2), [40, 40, 40, 255]);
        assert_eq!(
            row(&strips, 2, 3),
            [255, 255, 255, 255],
            "past the page is white"
        );
    }

    #[test]
    fn a_page_wider_than_the_pane_is_scaled_to_its_width() {
        let strips = compose_strips(&canvas(None, 1), 2, 2);

        assert!(strips.iter().all(|strip| strip.width() == 1));
        assert_eq!(strips.iter().map(RgbaImage::height).sum::<u32>(), 2);
    }
}
