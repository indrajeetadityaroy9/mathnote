//! Font files, metrics and cached glyph outlines for XDV pages.
//!
//! * TFM fonts (classic TeX math): widths from `<name>.tfm`, outlines from the Type1 file that
//!   `pdftex.map` names, with the map's encoding and `SlantFont`/`ExtendFont` effects.
//! * Native fonts (XeTeX OpenType text): the file named by the XDV definition, resolved by
//!   basename, drawn by glyph id.
//!
//! Outlines are cached once per (face, glyph) in em units, y up, so every size of a face shares
//! them. A missing file, face or glyph is reported once on stderr and then skipped.

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::Path;
use std::sync::Arc;

use kurbo::BezPath;
use skrifa::instance::{LocationRef, Size};
use skrifa::outline::{DrawSettings, OutlinePen};
use skrifa::raw::ps::type1::Type1Font;
use skrifa::{FontRef, GlyphId, MetadataProvider};
use tectonic::io::{IoProvider, OpenResult};
use tectonic::status::NoopStatusBackend;
use tectonic_bundles::Bundle;

use super::fontmap::{FontMap, parse_encoding};
use super::tfm::Tfm;
use super::xdv::FontDef;
use crate::cache::open_bundle;

/// Source of font-related files (TFM, Type1, OpenType, map and encoding files) by name.
pub trait FontFiles {
    fn read(&mut self, name: &str) -> Option<Arc<[u8]>>;
}

/// [`FontFiles`] served from the verified, cache-only Tectonic bundle.
pub struct BundleFontFiles {
    bundle: Box<dyn Bundle>,
}

impl BundleFontFiles {
    pub fn open(cache_dir: &Path) -> Result<Self, String> {
        open_bundle(cache_dir, true)
            .map(|bundle| Self { bundle })
            .map_err(|error| error.to_string())
    }
}

impl FontFiles for BundleFontFiles {
    fn read(&mut self, name: &str) -> Option<Arc<[u8]>> {
        let mut status = NoopStatusBackend::default();
        match self.bundle.input_open_name(name, &mut status) {
            OpenResult::Ok(handle) => {
                // Read the inner stream: the handle itself SHA-256s everything read through it
                // for TeX's rerun detection, which font files do not need.
                let mut data = Vec::new();
                handle.into_inner().read_to_end(&mut data).ok()?;
                Some(data.into())
            }
            OpenResult::NotAvailable | OpenResult::Err(_) => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum FaceKey {
    Tfm(String),
    Native(String, u32),
}

enum Face {
    Type1 {
        font: Box<Type1Font>,
        /// Glyph of each 8-bit character code.
        codes: Box<[Option<GlyphId>; 256]>,
        units_per_em: f64,
        slant: f64,
        extend: f64,
    },
    Sfnt {
        data: Arc<[u8]>,
        index: u32,
        units_per_em: f64,
    },
}

/// Loaded fonts and outline cache, shared by every page of a document.
pub struct FontStore {
    files: Box<dyn FontFiles>,
    /// `pdftex.map`, read on first use of a TFM font (`Some(None)` when it is unavailable).
    map: Option<Option<FontMap>>,
    tfms: HashMap<String, Option<Arc<Tfm>>>,
    faces: HashMap<FaceKey, Option<usize>>,
    loaded: Vec<Face>,
    outlines: HashMap<(usize, u32), Option<Arc<BezPath>>>,
    warned: HashSet<String>,
}

impl FontStore {
    pub fn new(files: Box<dyn FontFiles>) -> Self {
        Self {
            files,
            map: None,
            tfms: HashMap::new(),
            faces: HashMap::new(),
            loaded: Vec::new(),
            outlines: HashMap::new(),
            warned: HashSet::new(),
        }
    }

    /// Advance of `code` in DVI units; 0 for native fonts and missing metrics.
    pub(crate) fn tfm_width(&mut self, def: &FontDef, code: u32) -> i32 {
        let FontDef::Tfm { name, scale, .. } = def else {
            return 0;
        };
        self.tfm(name)
            .map_or(0, |tfm| tfm.scaled_width(code, *scale))
    }

    /// Outline of a TFM character code or native glyph id, in em units with y up.
    pub(crate) fn outline(&mut self, def: &FontDef, code: u32) -> Option<Arc<BezPath>> {
        let face = self.face(def)?;
        if let Some(outline) = self.outlines.get(&(face, code)) {
            return outline.clone();
        }
        let outline = draw_outline(&self.loaded[face], code).map(Arc::new);
        if outline.is_none() {
            let name = match def {
                FontDef::Tfm { name, .. } | FontDef::Native { name, .. } => name,
            };
            self.warn(format!(
                "glyph {code} of font {name} is missing; skipping it"
            ));
        }
        self.outlines.insert((face, code), outline.clone());
        outline
    }

    fn tfm(&mut self, name: &str) -> Option<Arc<Tfm>> {
        if let Some(tfm) = self.tfms.get(name) {
            return tfm.clone();
        }
        let file = format!("{name}.tfm");
        let tfm = match self.files.read(&file).map(|data| Tfm::parse(&data)) {
            Some(Ok(tfm)) => Some(Arc::new(tfm)),
            Some(Err(error)) => {
                self.warn(format!("{file}: {error}"));
                None
            }
            None => {
                self.warn(format!("{file} is not available"));
                None
            }
        };
        self.tfms.insert(name.to_owned(), tfm.clone());
        tfm
    }

    fn face(&mut self, def: &FontDef) -> Option<usize> {
        let key = match def {
            FontDef::Tfm { name, .. } => FaceKey::Tfm(name.clone()),
            FontDef::Native { name, index, .. } => FaceKey::Native(name.clone(), *index),
        };
        if let Some(face) = self.faces.get(&key) {
            return *face;
        }
        let loaded = match &key {
            FaceKey::Tfm(name) => self.load_type1(name),
            FaceKey::Native(name, index) => self.load_sfnt(name, *index),
        };
        let face = match loaded {
            Ok(face) => {
                self.loaded.push(face);
                Some(self.loaded.len() - 1)
            }
            Err(error) => {
                self.warn(error);
                None
            }
        };
        self.faces.insert(key, face);
        face
    }

    fn load_type1(&mut self, tfm_name: &str) -> Result<Face, String> {
        if self.map.is_none() {
            self.map = Some(self.files.read("pdftex.map").map(FontMap::new));
        }
        let Some(Some(map)) = &self.map else {
            return Err(String::from("pdftex.map is not available"));
        };
        let entry = map
            .lookup(tfm_name)
            .ok_or_else(|| format!("no pdftex.map entry for {tfm_name}"))?;
        let file = entry
            .font_file
            .ok_or_else(|| format!("pdftex.map gives no font file for {tfm_name}"))?;
        let data = self
            .files
            .read(&file)
            .ok_or_else(|| format!("{file} (for {tfm_name}) is not available"))?;
        let font = Type1Font::new(&data)
            .map_err(|error| format!("{file} is not a Type1 font: {error:?}"))?;

        let mut codes = Box::new([None; 256]);
        match entry.encoding {
            Some(encoding) => {
                let names = self
                    .files
                    .read(&encoding)
                    .map(|data| parse_encoding(&data))
                    .ok_or_else(|| format!("{encoding} (for {tfm_name}) is not available"))?;
                let glyphs = font
                    .glyph_names()
                    .map(|(gid, name)| (name, gid))
                    .collect::<HashMap<_, _>>();
                for (slot, name) in codes.iter_mut().zip(&names) {
                    *slot = glyphs.get(name.as_str()).copied();
                }
            }
            None => {
                if let Some(encoding) = font.encoding() {
                    for (code, slot) in codes.iter_mut().enumerate() {
                        *slot = encoding.map(code as u8);
                    }
                }
            }
        }

        Ok(Face::Type1 {
            units_per_em: f64::from(font.upem().max(1)),
            font: Box::new(font),
            codes,
            slant: f64::from(entry.slant),
            extend: f64::from(entry.extend),
        })
    }

    fn load_sfnt(&mut self, name: &str, index: u32) -> Result<Face, String> {
        let data = native_candidates(name)
            .iter()
            .find_map(|candidate| self.files.read(candidate))
            .ok_or_else(|| format!("font file {name} is not available"))?;
        let font = FontRef::from_index(&data, index)
            .map_err(|error| format!("{name}: cannot read face {index}: {error}"))?;
        let units_per_em = f64::from(
            font.metrics(Size::unscaled(), LocationRef::default())
                .units_per_em
                .max(1),
        );
        Ok(Face::Sfnt {
            data: Arc::clone(&data),
            index,
            units_per_em,
        })
    }

    fn warn(&mut self, message: String) {
        if self.warned.insert(message.clone()) {
            eprintln!("mathnote: {message}");
        }
    }
}

/// Bundle names to try for an XDV native font: the basename, with an extension when it has
/// none (texpresso's resource manager does the same).
fn native_candidates(name: &str) -> Vec<String> {
    let name = name.trim_matches(['[', ']', '"']);
    let base = name.rsplit('/').next().unwrap_or(name);
    let lower = base.to_ascii_lowercase();
    if [".otf", ".ttf", ".ttc", ".otc"]
        .iter()
        .any(|extension| lower.ends_with(extension))
    {
        vec![base.to_owned()]
    } else {
        [".otf", ".ttf", ".ttc", ""]
            .iter()
            .map(|extension| format!("{base}{extension}"))
            .collect()
    }
}

fn draw_outline(face: &Face, code: u32) -> Option<BezPath> {
    match face {
        Face::Type1 {
            font,
            codes,
            units_per_em,
            slant,
            extend,
        } => {
            let gid = (*codes.get(code as usize)?)?;
            let mut pen = EmPen::new(1.0 / units_per_em, *extend, *slant);
            font.draw(gid, None, &mut pen).ok()?;
            Some(pen.path)
        }
        Face::Sfnt {
            data,
            index,
            units_per_em,
        } => {
            let font = FontRef::from_index(data, *index).ok()?;
            let glyph = font.outline_glyphs().get(GlyphId::new(code))?;
            let mut pen = EmPen::new(1.0 / units_per_em, 1.0, 0.0);
            glyph
                .draw(
                    DrawSettings::unhinted(Size::unscaled(), LocationRef::default()),
                    &mut pen,
                )
                .ok()?;
            Some(pen.path)
        }
    }
}

/// Collects an outline in em units (y up), applying horizontal extend and slant.
struct EmPen {
    path: BezPath,
    scale: f64,
    extend: f64,
    slant: f64,
}

impl EmPen {
    fn new(scale: f64, extend: f64, slant: f64) -> Self {
        Self {
            path: BezPath::new(),
            scale,
            extend,
            slant,
        }
    }

    fn point(&self, x: f32, y: f32) -> (f64, f64) {
        let (x, y) = (f64::from(x) * self.scale, f64::from(y) * self.scale);
        (x * self.extend + y * self.slant, y)
    }
}

impl OutlinePen for EmPen {
    fn move_to(&mut self, x: f32, y: f32) {
        let point = self.point(x, y);
        self.path.move_to(point);
    }

    fn line_to(&mut self, x: f32, y: f32) {
        let point = self.point(x, y);
        self.path.line_to(point);
    }

    fn quad_to(&mut self, cx0: f32, cy0: f32, x: f32, y: f32) {
        let (control, point) = (self.point(cx0, cy0), self.point(x, y));
        self.path.quad_to(control, point);
    }

    fn curve_to(&mut self, cx0: f32, cy0: f32, cx1: f32, cy1: f32, x: f32, y: f32) {
        let (first, second, point) = (self.point(cx0, cy0), self.point(cx1, cy1), self.point(x, y));
        self.path.curve_to(first, second, point);
    }

    fn close(&mut self) {
        self.path.close_path();
    }
}
