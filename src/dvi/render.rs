//! Rasterize an [`XdvPage`] with vello_cpu.

use image::RgbaImage;
use kurbo::{Affine, Rect, Stroke};
use vello_cpu::color::{AlphaColor, Srgb};
use vello_cpu::{Pixmap, RenderContext, Resources};

use super::fonts::FontStore;
use super::xdv::{Color, FontDef, Item, XdvPage};

/// Render `page` at `scale` pixels per bp onto opaque white. The image is
/// `ceil(width × scale) × ceil(height × scale)` pixels (clamped to vello's `u16` limit).
pub fn render_page(page: &XdvPage, fonts: &mut FontStore, scale: f32) -> RgbaImage {
    let dimension = |extent: f32| (extent * scale).ceil().clamp(1.0, f32::from(u16::MAX)) as u16;
    let (width, height) = (dimension(page.width), dimension(page.height));
    let s = f64::from(scale);
    let dvi_to_bp = page.dvi_to_bp();

    let mut context = RenderContext::new(width, height);
    context.set_paint(paint([255; 3]));
    context.fill_rect(&Rect::new(0.0, 0.0, f64::from(width), f64::from(height)));

    let mut current: Option<Color> = None;
    let mut set_color = |context: &mut RenderContext, color: Color| {
        if current != Some(color) {
            context.set_paint(paint(color));
            current = Some(color);
        }
    };

    for item in page.display_list(fonts) {
        match item {
            Item::Glyph {
                font,
                code,
                x,
                y,
                color,
            } => {
                let Some(outline) = fonts.outline(font, *code) else {
                    continue;
                };
                let style = GlyphStyle::of(font, dvi_to_bp, *color);
                if style.size <= 0.0 {
                    continue;
                }
                let em = s * style.size;
                context.set_transform(Affine::new([
                    em * style.extend,
                    0.0,
                    em * style.slant,
                    -em,
                    s * f64::from(*x),
                    s * f64::from(*y),
                ]));
                set_color(&mut context, style.color);
                context.fill_path(&outline);
                if style.embolden > 0.0 {
                    context.set_stroke(Stroke::new(style.embolden / style.size));
                    context.stroke_path(&outline);
                }
            }
            Item::Rule {
                x,
                y,
                width,
                height,
                color,
            } => {
                context.set_transform(Affine::IDENTITY);
                set_color(&mut context, *color);
                let (x, y) = (f64::from(*x), f64::from(*y));
                context.fill_rect(&Rect::new(
                    s * x,
                    s * y,
                    s * (x + f64::from(*width)),
                    s * (y + f64::from(*height)),
                ));
            }
        }
    }

    context.flush();
    let mut pixmap = Pixmap::new(width, height);
    context.render_to_pixmap(&mut Resources::new(), &mut pixmap);
    // Every pixel is opaque (white background), so premultiplied bytes are the unpremultiplied
    // ones and the buffer can be taken as is.
    RgbaImage::from_raw(
        u32::from(width),
        u32::from(height),
        pixmap.data_as_u8_slice().to_vec(),
    )
    .expect("pixmap holds width × height RGBA pixels")
}

fn paint(color: Color) -> AlphaColor<Srgb> {
    AlphaColor::from_rgba8(color[0], color[1], color[2], 255)
}

/// Size and synthetic effects of a glyph, in bp.
struct GlyphStyle {
    size: f64,
    extend: f64,
    slant: f64,
    /// Stroke width of XeTeX's `embolden` (bp); 0 when absent.
    embolden: f64,
    color: Color,
}

impl GlyphStyle {
    fn of(font: &FontDef, dvi_to_bp: f64, color: Color) -> Self {
        let fixed = |value: Option<i32>, default: f64| {
            value.map_or(default, |value| f64::from(value) / 65536.0)
        };
        match font {
            FontDef::Tfm { scale, .. } => Self {
                size: f64::from(*scale) * dvi_to_bp,
                extend: 1.0,
                slant: 0.0,
                embolden: 0.0,
                color,
            },
            FontDef::Native {
                size,
                rgba,
                extend,
                slant,
                embolden,
                ..
            } => Self {
                size: f64::from(*size) * dvi_to_bp,
                extend: fixed(*extend, 1.0),
                slant: fixed(*slant, 0.0),
                // XeTeX stores `embolden` as a stroke width in points.
                embolden: fixed(*embolden, 0.0) * 72.0 / 72.27,
                color: rgba.map_or(color, |rgba| {
                    let [r, g, b, _] = rgba.to_be_bytes();
                    [r, g, b]
                }),
            },
        }
    }
}
