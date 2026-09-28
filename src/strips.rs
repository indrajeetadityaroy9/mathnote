//! Terminal preview image split into horizontal strips of whole cell rows.
//!
//! Each strip is its own terminal image. When a new preview canvas differs from the shown one
//! only in a few rows (typing on one line, moving the sync highlight), only those strips are
//! encoded and sent to the terminal; every other strip keeps its already transmitted image.
//! Encoding runs on a background thread, and a new set of strips replaces the shown set only
//! once all of its changed strips are ready, so the preview never shows a half-updated page.
//!
//! Kitty-protocol terminals (Kitty, Ghostty, WezTerm) get zlib-compressed RGB data placed with
//! Unicode placeholders; a mostly white page row compresses to a few kilobytes. Other terminals
//! go through ratatui-image (iTerm2 PNG, Sixel, or half blocks).

use std::env;
use std::hash::{BuildHasher, RandomState};
use std::io::Write;
use std::num::NonZeroU16;
use std::sync::mpsc;
use std::thread;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use flate2::Compression;
use flate2::write::ZlibEncoder;
use image::{DynamicImage, RgbaImage, imageops};
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

/// The encoded changed strips of one install, in the order they were requested.
pub(crate) struct EncodedStrips {
    generation: u64,
    images: Vec<Result<StripImage, ImageError>>,
}

pub(crate) struct EncodeJob {
    generation: u64,
    /// Pixels, size in cells, and Kitty image id of each changed strip.
    strips: Vec<(RgbaImage, Size, u32)>,
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
    pixels: RgbaImage,
    rows: u16,
    image: StripImage,
}

struct Staged<K> {
    generation: u64,
    key: K,
    pixels: Vec<RgbaImage>,
    /// Indices of the strips being encoded, in job order.
    changed: Vec<usize>,
}

pub(crate) struct StripPreview<K> {
    encoder: mpsc::Sender<EncodeJob>,
    font: FontSize,
    /// Pixel rows per strip, a whole number of cell rows.
    strip_height: u32,
    /// Strip `i` always uses Kitty image id `kitty_ids + i`, so a changed strip replaces its
    /// predecessor in the terminal's image store instead of accumulating new images.
    kitty_ids: u32,
    generation: u64,
    shown_key: Option<K>,
    shown: Vec<Strip>,
    staged: Option<Staged<K>>,
}

impl<K: Copy + PartialEq> StripPreview<K> {
    /// Start the encoder thread. `deliver` hands each encoded install back to the UI loop and
    /// returns `false` once nobody listens any more.
    pub(crate) fn new(
        picker: Picker,
        deliver: impl Fn(EncodedStrips) -> bool + Send + 'static,
    ) -> Self {
        let font = picker.font_size();
        let font_height = u32::from(font.height.max(1));
        let strip_height = match picker.protocol_type() {
            // Sixel strips span whole bands so a strip never paints into the next one's rows.
            ProtocolType::Sixel => {
                SIXEL_BAND_HEIGHT * font_height / gcd(font_height, SIXEL_BAND_HEIGHT)
            }
            ProtocolType::Halfblocks | ProtocolType::Kitty | ProtocolType::Iterm2 => font_height,
        };

        let (encoder, jobs) = mpsc::channel::<EncodeJob>();
        thread::spawn(move || {
            let inside_tmux = inside_tmux();
            while let Ok(mut job) = jobs.recv() {
                // A newer install supersedes every queued one.
                while let Ok(newer) = jobs.try_recv() {
                    job = newer;
                }
                let images = job
                    .strips
                    .into_iter()
                    .map(|(pixels, size, id)| match picker.protocol_type() {
                        ProtocolType::Kitty => Ok(StripImage::Kitty(KittyStrip::encode(
                            &pixels,
                            size.width,
                            id,
                            inside_tmux,
                        ))),
                        ProtocolType::Halfblocks | ProtocolType::Sixel | ProtocolType::Iterm2 => {
                            picker
                                .new_protocol(
                                    DynamicImage::ImageRgba8(pixels),
                                    size,
                                    Resize::Fit(None),
                                )
                                .map(StripImage::Other)
                        }
                    })
                    .collect();
                if !deliver(EncodedStrips {
                    generation: job.generation,
                    images,
                }) {
                    return;
                }
            }
        });

        // Ids stay below 2^24: the top byte is 0, so placeholders need no third diacritic.
        let kitty_ids = (RandomState::new().hash_one(0u8) as u32 & 0x00ff_ffff).max(1);
        Self {
            encoder,
            font,
            strip_height,
            kitty_ids,
            generation: 0,
            shown_key: None,
            shown: Vec::new(),
            staged: None,
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

    pub(crate) fn is_staging(&self) -> bool {
        self.staged.is_some()
    }

    /// Indices of the strips the pending install is encoding.
    #[cfg(test)]
    pub(crate) fn staged_strips(&self) -> Option<&[usize]> {
        self.staged.as_ref().map(|staged| staged.changed.as_slice())
    }

    /// Show `canvas` under `key`, encoding only the strips whose pixels differ from the shown
    /// ones. The canvas height must be a whole number of cell rows.
    pub(crate) fn install(&mut self, key: K, canvas: &RgbaImage) {
        let pixels = (0..canvas.height())
            .step_by(self.strip_height as usize)
            .map(|top| {
                let height = self.strip_height.min(canvas.height() - top);
                imageops::crop_imm(canvas, 0, top, canvas.width(), height).to_image()
            })
            .collect::<Vec<_>>();
        let changed = (0..pixels.len())
            .filter(|index| {
                self.shown
                    .get(*index)
                    .is_none_or(|shown| shown.pixels != pixels[*index])
            })
            .collect::<Vec<_>>();

        self.generation = self.generation.wrapping_add(1);
        if changed.is_empty() && self.shown.len() == pixels.len() {
            self.shown_key = Some(key);
            self.staged = None;
            return;
        }

        let job = EncodeJob {
            generation: self.generation,
            strips: changed
                .iter()
                .map(|index| {
                    let strip = &pixels[*index];
                    (
                        strip.clone(),
                        self.cells(strip),
                        (self.kitty_ids + *index as u32) & 0x00ff_ffff,
                    )
                })
                .collect(),
        };
        self.staged = Some(Staged {
            generation: self.generation,
            key,
            pixels,
            changed,
        });
        // The encoder only stops when the UI loop's receiver is gone, i.e. during shutdown.
        let _ = self.encoder.send(job);
    }

    /// Swap in the strips of the pending install once they are encoded; results of superseded
    /// installs are dropped. Returns whether the shown preview changed.
    pub(crate) fn accept(&mut self, encoded: EncodedStrips) -> Result<bool, ImageError> {
        let Some(staged) = self
            .staged
            .take_if(|staged| staged.generation == encoded.generation)
        else {
            return Ok(false);
        };

        let mut images = staged.pixels.iter().map(|_| None).collect::<Vec<_>>();
        for (index, image) in staged.changed.iter().zip(encoded.images) {
            images[*index] = Some(image?);
        }
        let mut previous = std::mem::take(&mut self.shown)
            .into_iter()
            .map(|strip| Some(strip.image))
            .collect::<Vec<_>>();

        self.shown = staged
            .pixels
            .into_iter()
            .zip(images)
            .enumerate()
            .filter_map(|(index, (pixels, image))| {
                // Unchanged strips keep the image already transmitted to the terminal.
                let image = image.or_else(|| previous.get_mut(index).and_then(Option::take))?;
                Some(Strip {
                    rows: self.cells(&pixels).height,
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

    fn cells(&self, strip: &RgbaImage) -> Size {
        Size::new(
            strip.width().div_ceil(u32::from(self.font.width.max(1))) as u16,
            strip.height().div_ceil(u32::from(self.font.height.max(1))) as u16,
        )
    }
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
