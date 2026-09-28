//! Incremental XDV rendering: texpresso's `src/dvi/*` plus the MuPDF drawing it relied on.
//!
//! * [`XdvDocument`] reads the growing XDV output of the engine and exposes complete pages.
//! * [`FontStore`] loads TFM metrics, `pdftex.map` Type1 fonts and XeTeX native fonts through a
//!   [`FontFiles`] source ([`BundleFontFiles`] serves them from the Tectonic bundle).
//! * [`render_page`] rasterizes a page's glyphs and rules with vello_cpu.
//!
//! Coordinates are big points (bp) from the page's top-left corner, with the DVI origin at
//! (72 bp, 72 bp), the same convention as [`crate::synctex::SyncIndex`].

mod fontmap;
mod fonts;
mod render;
mod tfm;
mod xdv;

pub use fonts::{BundleFontFiles, FontFiles, FontStore};
pub use render::render_page;
pub use xdv::{XdvDocument, XdvPage};
