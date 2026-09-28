//! Incremental DVI/XDV reader.
//!
//! Semantics follow DVItype; the XDV extensions (`define_native_font` 252, `set_glyphs` 253,
//! `set_text_and_glyphs` 254) use the byte layouts XeTeX writes (`xetex-ext.c` `make_font_def`,
//! `xetex-shipout.c`).
//!
//! [`XdvDocument::update`] is fed the whole engine output every time. Only complete pages
//! (`bop..eop`) become [`XdvPage`]s; an unfinished page is scanned as far as the bytes go and
//! resumed from there on the next call. Everything that persists across pages (font
//! definitions, the colour stack, the page size) is checkpointed after every page, so a
//! truncation drops the pages ending after it and resumes from the last surviving one.
//!
//! Character advances of TFM fonts need the font's metrics, which live in the bundle. A page
//! therefore keeps its own bytes plus the state in effect at its `bop`, and its display list is
//! built on first render, when a [`FontStore`] is at hand.

use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, OnceLock};

use super::fonts::FontStore;

/// A4 in big points, the page size when no special sets one.
pub(crate) const DEFAULT_PAGE_SIZE: (f32, f32) = (595.2756, 841.8898);
/// The DVI origin sits one inch from the page's top-left corner.
const ORIGIN_BP: f64 = 72.0;
/// Bytes of `bop` including the opcode: ten counts and the previous-page pointer.
const BOP_SIZE: usize = 45;

/// sRGB colour, 0–255 per channel.
pub(crate) type Color = [u8; 3];
const BLACK: Color = [0, 0, 0];

/// A font as the DVI file defines it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum FontDef {
    /// `fnt_def`: a TeX font, metrics from `<name>.tfm`.
    Tfm {
        name: String,
        checksum: u32,
        /// Scaled size `s` in DVI units.
        scale: i32,
        design_size: i32,
    },
    /// XDV `define_native_font`: an OpenType/TrueType file drawn by glyph id.
    Native {
        name: String,
        index: u32,
        /// Size in DVI units (TeX `Fixed` points).
        size: i32,
        flags: u16,
        rgba: Option<u32>,
        extend: Option<i32>,
        slant: Option<i32>,
        embolden: Option<i32>,
    },
}

/// One drawing operation of a page, in bp from the page's top-left corner.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Item {
    /// A TFM character code or a native glyph id, positioned at its baseline origin.
    Glyph {
        font: Arc<FontDef>,
        code: u32,
        x: f32,
        y: f32,
        color: Color,
    },
    /// A filled rectangle; `(x, y)` is its top-left corner.
    Rule {
        x: f32,
        y: f32,
        width: f32,
        height: f32,
        color: Color,
    },
}

/// A complete page (`bop..eop`).
pub struct XdvPage {
    /// Page width in bp.
    pub width: f32,
    /// Page height in bp.
    pub height: f32,
    /// Hash of the page's bytes, the fonts it selects and its starting colour; equal
    /// fingerprints render identically.
    pub fingerprint: u64,
    bytes: Box<[u8]>,
    start: Carry,
    dvi_to_bp: f64,
    display: OnceLock<Vec<Item>>,
}

impl std::fmt::Debug for XdvPage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("XdvPage")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("fingerprint", &self.fingerprint)
            .field("bytes", &self.bytes.len())
            .finish()
    }
}

impl XdvPage {
    /// The page's drawing operations, interpreted once with `fonts` for TFM advances.
    pub(crate) fn display_list(&self, fonts: &mut FontStore) -> &[Item] {
        self.display.get_or_init(|| self.interpret(fonts))
    }

    /// Big points per DVI unit (from the preamble's `num`/`den`/`mag`).
    pub(crate) fn dvi_to_bp(&self) -> f64 {
        self.dvi_to_bp
    }

    fn interpret(&self, fonts: &mut FontStore) -> Vec<Item> {
        let mut items = Vec::new();
        let mut carry = self.start.clone();
        let mut regs = Registers::default();
        let mut stack: Vec<Registers> = Vec::new();
        let mut font: Option<Arc<FontDef>> = None;
        let k = self.dvi_to_bp;
        let to_x = |h: i32| (ORIGIN_BP + f64::from(h) * k) as f32;
        let to_y = |v: i32| (ORIGIN_BP + f64::from(v) * k) as f32;

        let mut at = BOP_SIZE;
        while let Ok(Some((op, size))) = decode(&self.bytes[at..]) {
            at += size;
            match op {
                Op::Char { code, set } => {
                    let Some(def) = &font else { continue };
                    if !matches!(**def, FontDef::Tfm { .. }) {
                        continue;
                    }
                    items.push(Item::Glyph {
                        font: Arc::clone(def),
                        code,
                        x: to_x(regs.h),
                        y: to_y(regs.v),
                        color: carry.color,
                    });
                    if set {
                        regs.h = regs.h.wrapping_add(fonts.tfm_width(def, code));
                    }
                }
                Op::Rule { height, width, set } => {
                    if height > 0 && width > 0 {
                        let top = to_y(regs.v.wrapping_sub(height));
                        let left = to_x(regs.h);
                        items.push(Item::Rule {
                            x: left,
                            y: top,
                            width: to_x(regs.h.wrapping_add(width)) - left,
                            height: to_y(regs.v) - top,
                            color: carry.color,
                        });
                    }
                    if set {
                        regs.h = regs.h.wrapping_add(width);
                    }
                }
                Op::Glyphs { width, count, data } => {
                    let Some(def) = &font else {
                        regs.h = regs.h.wrapping_add(width);
                        continue;
                    };
                    if matches!(**def, FontDef::Native { .. }) {
                        let count = usize::from(count);
                        let (positions, ids) = data.split_at(count * 8);
                        for index in 0..count {
                            let dx = be_i32(&positions[index * 8..]);
                            let dy = be_i32(&positions[index * 8 + 4..]);
                            let gid = u16::from_be_bytes([ids[index * 2], ids[index * 2 + 1]]);
                            items.push(Item::Glyph {
                                font: Arc::clone(def),
                                code: u32::from(gid),
                                x: to_x(regs.h.wrapping_add(dx)),
                                y: to_y(regs.v.wrapping_add(dy)),
                                color: carry.color,
                            });
                        }
                    }
                    regs.h = regs.h.wrapping_add(width);
                }
                Op::Push => stack.push(regs),
                Op::Pop => regs = stack.pop().unwrap_or_default(),
                Op::Right(d) => regs.h = regs.h.wrapping_add(d),
                Op::W(d) => {
                    if let Some(d) = d {
                        regs.w = d;
                    }
                    regs.h = regs.h.wrapping_add(regs.w);
                }
                Op::X(d) => {
                    if let Some(d) = d {
                        regs.x = d;
                    }
                    regs.h = regs.h.wrapping_add(regs.x);
                }
                Op::Down(d) => regs.v = regs.v.wrapping_add(d),
                Op::Y(d) => {
                    if let Some(d) = d {
                        regs.y = d;
                    }
                    regs.v = regs.v.wrapping_add(regs.y);
                }
                Op::Z(d) => {
                    if let Some(d) = d {
                        regs.z = d;
                    }
                    regs.v = regs.v.wrapping_add(regs.z);
                }
                Op::Font(number) => font = carry.fonts.get(&number).cloned(),
                Op::FontDef(number, def) => carry.define(number, def),
                Op::Special(text) => carry.special(text),
                Op::Eop => break,
                Op::Nop | Op::Bop | Op::Pre | Op::Post | Op::PostPost => {}
            }
        }
        items
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct Registers {
    h: i32,
    v: i32,
    w: i32,
    x: i32,
    y: i32,
    z: i32,
}

/// State that outlives a page: font definitions, colour stack and page size.
#[derive(Debug, Clone, Default)]
struct Carry {
    fonts: HashMap<u32, Arc<FontDef>>,
    color: Color,
    color_stack: Vec<Color>,
    page_size: Option<(f32, f32)>,
}

impl Carry {
    fn define(&mut self, number: u32, def: FontDef) {
        if self.fonts.get(&number).is_none_or(|old| **old != def) {
            self.fonts.insert(number, Arc::new(def));
        }
    }

    fn special(&mut self, text: &[u8]) {
        let Ok(text) = std::str::from_utf8(text) else {
            return;
        };
        let text = text.trim();
        if let Some(rest) = text.strip_prefix("color") {
            let rest = rest.trim_start();
            if rest == "pop" {
                self.color = self.color_stack.pop().unwrap_or(BLACK);
            } else if let Some(spec) = rest.strip_prefix("push") {
                self.color_stack.push(self.color);
                if let Some(color) = parse_color(spec) {
                    self.color = color;
                }
            } else if let Some(color) = parse_color(rest) {
                self.color_stack.clear();
                self.color = color;
            }
        } else if let Some(size) = parse_page_size(text) {
            self.page_size = size;
        }
    }
}

/// `rgb r g b`, `gray g` or `cmyk c m y k` with components in 0..=1.
fn parse_color(spec: &str) -> Option<Color> {
    let mut words = spec.split_whitespace();
    let model = words.next()?;
    let values = words
        .map(str::parse::<f32>)
        .collect::<Result<Vec<_>, _>>()
        .ok()?;
    let channel = |value: f32| (value.clamp(0.0, 1.0) * 255.0).round() as u8;
    match (model, values.as_slice()) {
        ("rgb", &[r, g, b]) => Some([channel(r), channel(g), channel(b)]),
        ("gray", &[g]) => Some([channel(g); 3]),
        ("cmyk", &[c, m, y, k]) => Some([
            channel((1.0 - c) * (1.0 - k)),
            channel((1.0 - m) * (1.0 - k)),
            channel((1.0 - y) * (1.0 - k)),
        ]),
        _ => None,
    }
}

/// `pdf:pagesize width <dim> height <dim>`, `pdf:pagesize default` and `papersize=<w>,<h>`.
/// The outer `Option` says whether this is a page-size special; `Some(None)` resets to the
/// default.
fn parse_page_size(text: &str) -> Option<Option<(f32, f32)>> {
    if let Some(rest) = text.strip_prefix("pdf:") {
        let mut words = rest.split_whitespace();
        if words.next()? != "pagesize" {
            return None;
        }
        let words = words.collect::<Vec<_>>();
        return match words.as_slice() {
            ["default", ..] => Some(None),
            ["width", width, "height", height, ..] => {
                Some(Some((parse_dimen(width)?, parse_dimen(height)?)))
            }
            _ => None,
        };
    }
    let rest = text.strip_prefix("papersize=")?;
    let (width, height) = rest.split_once(',')?;
    Some(Some((parse_dimen(width)?, parse_dimen(height)?)))
}

/// A TeX dimension such as `597.50787pt` or `210truemm`, in bp.
fn parse_dimen(text: &str) -> Option<f32> {
    let text = text.trim();
    let split = text
        .find(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-' || c == '+'))
        .unwrap_or(text.len());
    let (number, unit) = text.split_at(split);
    let value: f64 = number.parse().ok()?;
    let unit = unit.trim();
    let unit = unit.strip_prefix("true").unwrap_or(unit);
    let bp_per_unit = match unit {
        "pt" => 72.0 / 72.27,
        "bp" => 1.0,
        "mm" => 72.0 / 25.4,
        "cm" => 72.0 / 2.54,
        "in" => 72.0,
        "pc" => 12.0 * 72.0 / 72.27,
        "dd" => 1238.0 / 1157.0 * 72.0 / 72.27,
        "cc" => 12.0 * 1238.0 / 1157.0 * 72.0 / 72.27,
        "sp" => 72.0 / 72.27 / 65536.0,
        _ => return None,
    };
    Some((value * bp_per_unit) as f32)
}

/// Parsing state saved after each page.
#[derive(Debug, Clone)]
struct Checkpoint {
    end: usize,
    carry: Carry,
}

/// A page whose `eop` has not arrived yet, resumable at `at`.
#[derive(Debug)]
struct PartialPage {
    start: usize,
    at: usize,
    start_carry: Carry,
    carry: Carry,
    selected: Vec<u32>,
}

/// Incremental reader of a growing DVI/XDV stream.
#[derive(Debug, Default)]
pub struct XdvDocument {
    /// Byte length of the preamble and the DVI-unit-to-bp factor, once parsed.
    preamble: Option<(usize, f64)>,
    pages: Vec<XdvPage>,
    checkpoints: Vec<Checkpoint>,
    /// End of the last consumed instruction outside any page.
    pos: usize,
    carry: Carry,
    partial: Option<PartialPage>,
    complete: bool,
    /// Set at an undecodable byte; parsing waits for a truncation before it.
    broken: bool,
}

impl XdvDocument {
    pub fn new() -> Self {
        Self::default()
    }

    /// Catch up with `data`, the engine's whole output so far. `truncated_to` reports that the
    /// output was cut back to that length (and possibly regrown) since the previous call.
    pub fn update(&mut self, data: &[u8], truncated_to: Option<usize>) {
        let limit = truncated_to.map_or(data.len(), |length| length.min(data.len()));
        if truncated_to.is_some() || data.len() < self.consumed() {
            self.rewind(limit);
        }
        self.parse(data);
    }

    /// Number of complete pages.
    pub fn page_count(&self) -> usize {
        self.pages.len()
    }

    pub fn page(&self, index: usize) -> Option<&XdvPage> {
        self.pages.get(index)
    }

    /// Whether the postamble has been reached.
    pub fn is_complete(&self) -> bool {
        self.complete
    }

    fn consumed(&self) -> usize {
        self.partial.as_ref().map_or(self.pos, |partial| partial.at)
    }

    /// Drop everything that depends on bytes at or after `length`.
    fn rewind(&mut self, length: usize) {
        let keep = self
            .checkpoints
            .iter()
            .take_while(|checkpoint| checkpoint.end <= length)
            .count();
        self.pages.truncate(keep);
        self.checkpoints.truncate(keep);
        self.partial = None;
        self.complete = false;
        self.broken = false;
        match (self.checkpoints.last(), self.preamble) {
            (Some(checkpoint), _) => {
                self.pos = checkpoint.end;
                self.carry = checkpoint.carry.clone();
            }
            (None, Some((end, _))) if end <= length => {
                self.pos = end;
                self.carry = Carry::default();
            }
            (None, _) => {
                self.preamble = None;
                self.pos = 0;
                self.carry = Carry::default();
            }
        }
    }

    fn parse(&mut self, data: &[u8]) {
        if self.broken || self.complete {
            return;
        }
        if self.preamble.is_none() {
            match parse_preamble(data) {
                Ok(Some(preamble)) => {
                    self.pos = preamble.0;
                    self.preamble = Some(preamble);
                }
                Ok(None) => return,
                Err(()) => {
                    self.fail(0);
                    return;
                }
            }
        }

        loop {
            if self.partial.is_some() {
                match self.continue_page(data) {
                    Ok(true) => continue,
                    Ok(false) => return,
                    Err(at) => {
                        self.fail(at);
                        return;
                    }
                }
            }
            let (op, size) = match decode(&data[self.pos..]) {
                Ok(Some(decoded)) => decoded,
                Ok(None) => return,
                Err(()) => {
                    self.fail(self.pos);
                    return;
                }
            };
            match op {
                Op::Bop => {
                    self.partial = Some(PartialPage {
                        start: self.pos,
                        at: self.pos + size,
                        start_carry: self.carry.clone(),
                        carry: self.carry.clone(),
                        selected: Vec::new(),
                    });
                    continue;
                }
                Op::FontDef(number, def) => self.carry.define(number, def),
                Op::Special(text) => self.carry.special(text),
                Op::Nop => {}
                Op::Post => {
                    self.complete = true;
                    return;
                }
                _ => {
                    self.fail(self.pos);
                    return;
                }
            }
            self.pos += size;
        }
    }

    /// Scan the unfinished page further. `Ok(true)` when its `eop` was reached.
    fn continue_page(&mut self, data: &[u8]) -> Result<bool, usize> {
        let Some(partial) = self.partial.as_mut() else {
            return Ok(false);
        };
        loop {
            let (op, size) = match decode(&data[partial.at..]) {
                Ok(Some(decoded)) => decoded,
                Ok(None) => return Ok(false),
                Err(()) => return Err(partial.at),
            };
            partial.at += size;
            match op {
                Op::Eop => break,
                Op::FontDef(number, def) => partial.carry.define(number, def),
                Op::Special(text) => partial.carry.special(text),
                Op::Font(number) => {
                    if !partial.selected.contains(&number) {
                        partial.selected.push(number);
                    }
                }
                Op::Bop | Op::Pre | Op::Post | Op::PostPost => return Err(partial.at - size),
                _ => {}
            }
        }

        let partial = self.partial.take().expect("partial page present");
        let (width, height) = partial.carry.page_size.unwrap_or(DEFAULT_PAGE_SIZE);
        let dvi_to_bp = self.preamble.map_or(0.0, |(_, factor)| factor);
        let bytes: Box<[u8]> = data[partial.start..partial.at].into();

        let mut hasher = DefaultHasher::new();
        bytes.hash(&mut hasher);
        dvi_to_bp.to_bits().hash(&mut hasher);
        partial.start_carry.color.hash(&mut hasher);
        (width.to_bits(), height.to_bits()).hash(&mut hasher);
        for number in &partial.selected {
            number.hash(&mut hasher);
            partial.start_carry.fonts.get(number).hash(&mut hasher);
        }

        self.pages.push(XdvPage {
            width,
            height,
            fingerprint: hasher.finish(),
            bytes,
            start: partial.start_carry,
            dvi_to_bp,
            display: OnceLock::new(),
        });
        self.pos = partial.at;
        self.carry = partial.carry;
        self.checkpoints.push(Checkpoint {
            end: self.pos,
            carry: self.carry.clone(),
        });
        Ok(true)
    }

    fn fail(&mut self, at: usize) {
        eprintln!("mathnote: undecodable XDV instruction at byte {at}; waiting for a rewrite");
        self.broken = true;
    }
}

/// `pre`: returns its length and the DVI-unit-to-bp factor.
fn parse_preamble(data: &[u8]) -> Result<Option<(usize, f64)>, ()> {
    let Some(&first) = data.first() else {
        return Ok(None);
    };
    if first != PRE {
        return Err(());
    }
    if data.len() < 15 {
        return Ok(None);
    }
    let length = 15 + usize::from(data[14]);
    if data.len() < length {
        return Ok(None);
    }
    let num = f64::from(be_u32(&data[2..]));
    let den = f64::from(be_u32(&data[6..]));
    let mag = f64::from(be_u32(&data[10..]));
    if den == 0.0 {
        return Err(());
    }
    Ok(Some((length, num / den * mag / 1000.0 * 72.0 / 254_000.0)))
}

const SET1: u8 = 128;
const SET_RULE: u8 = 132;
const PUT1: u8 = 133;
const PUT_RULE: u8 = 137;
const NOP: u8 = 138;
const BOP: u8 = 139;
const EOP: u8 = 140;
const PUSH: u8 = 141;
const POP: u8 = 142;
const RIGHT1: u8 = 143;
const W0: u8 = 147;
const X0: u8 = 152;
const DOWN1: u8 = 157;
const Y0: u8 = 161;
const Z0: u8 = 166;
const FNT_NUM_0: u8 = 171;
const FNT_NUM_63: u8 = 234;
const FNT1: u8 = 235;
const XXX1: u8 = 239;
const FNT_DEF1: u8 = 243;
const PRE: u8 = 247;
const POST: u8 = 248;
const POST_POST: u8 = 249;
const BEGIN_REFLECT: u8 = 250;
const END_REFLECT: u8 = 251;
const NATIVE_FONT_DEF: u8 = 252;
const SET_GLYPHS: u8 = 253;
const SET_TEXT_AND_GLYPHS: u8 = 254;

const FLAG_COLORED: u16 = 0x0200;
const FLAG_VARIATIONS: u16 = 0x0800;
const FLAG_EXTEND: u16 = 0x1000;
const FLAG_SLANT: u16 = 0x2000;
const FLAG_EMBOLDEN: u16 = 0x4000;

/// One decoded instruction, borrowing variable-length payloads.
#[derive(Debug)]
enum Op<'a> {
    Char {
        code: u32,
        set: bool,
    },
    Rule {
        height: i32,
        width: i32,
        set: bool,
    },
    /// `set_glyphs`/`set_text_and_glyphs`: `data` is `count` (dx, dy) i32 pairs followed by
    /// `count` u16 glyph ids.
    Glyphs {
        width: i32,
        count: u16,
        data: &'a [u8],
    },
    Nop,
    Bop,
    Eop,
    Push,
    Pop,
    Right(i32),
    W(Option<i32>),
    X(Option<i32>),
    Down(i32),
    Y(Option<i32>),
    Z(Option<i32>),
    Font(u32),
    Special(&'a [u8]),
    FontDef(u32, FontDef),
    Pre,
    Post,
    PostPost,
}

fn be_u32(bytes: &[u8]) -> u32 {
    u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

fn be_i32(bytes: &[u8]) -> i32 {
    be_u32(bytes) as i32
}

fn be_u16(bytes: &[u8]) -> u16 {
    u16::from_be_bytes([bytes[0], bytes[1]])
}

/// Unsigned big-endian integer of `n` bytes.
fn unsigned(bytes: &[u8], n: usize) -> u32 {
    bytes[..n]
        .iter()
        .fold(0u32, |value, &byte| (value << 8) | u32::from(byte))
}

/// Signed big-endian integer of `n` bytes.
fn signed(bytes: &[u8], n: usize) -> i32 {
    let shift = 32 - 8 * n as u32;
    ((unsigned(bytes, n) << shift) as i32) >> shift
}

/// Decode the instruction at the start of `buf`: `Ok(None)` when it is incomplete, `Err` when
/// the opcode is not DVI/XDV.
fn decode(buf: &[u8]) -> Result<Option<(Op<'_>, usize)>, ()> {
    macro_rules! need {
        ($n:expr) => {
            if buf.len() < $n {
                return Ok(None);
            }
        };
    }
    need!(1);
    let op = buf[0];
    let arg = &buf[1..];
    let decoded = match op {
        0..=127 => (
            Op::Char {
                code: u32::from(op),
                set: true,
            },
            1,
        ),
        SET1..=131 | PUT1..=136 => {
            let set = op < SET_RULE;
            let n = usize::from(if set { op - SET1 } else { op - PUT1 }) + 1;
            need!(1 + n);
            (
                Op::Char {
                    code: unsigned(arg, n),
                    set,
                },
                1 + n,
            )
        }
        SET_RULE | PUT_RULE => {
            need!(9);
            (
                Op::Rule {
                    height: be_i32(arg),
                    width: be_i32(&arg[4..]),
                    set: op == SET_RULE,
                },
                9,
            )
        }
        NOP => (Op::Nop, 1),
        BOP => {
            need!(BOP_SIZE);
            (Op::Bop, BOP_SIZE)
        }
        EOP => (Op::Eop, 1),
        PUSH => (Op::Push, 1),
        POP => (Op::Pop, 1),
        RIGHT1..=146 | DOWN1..=160 => {
            let (base, horizontal) = if op < W0 {
                (RIGHT1, true)
            } else {
                (DOWN1, false)
            };
            let n = usize::from(op - base) + 1;
            need!(1 + n);
            let d = signed(arg, n);
            (
                if horizontal {
                    Op::Right(d)
                } else {
                    Op::Down(d)
                },
                1 + n,
            )
        }
        W0..=151 | X0..=156 | Y0..=165 | Z0..=170 => {
            let base = match op {
                W0..=151 => W0,
                X0..=156 => X0,
                Y0..=165 => Y0,
                _ => Z0,
            };
            let n = usize::from(op - base);
            need!(1 + n);
            let d = (n > 0).then(|| signed(arg, n));
            let decoded = match base {
                W0 => Op::W(d),
                X0 => Op::X(d),
                Y0 => Op::Y(d),
                _ => Op::Z(d),
            };
            (decoded, 1 + n)
        }
        FNT_NUM_0..=FNT_NUM_63 => (Op::Font(u32::from(op - FNT_NUM_0)), 1),
        FNT1..=238 => {
            let n = usize::from(op - FNT1) + 1;
            need!(1 + n);
            (Op::Font(unsigned(arg, n)), 1 + n)
        }
        XXX1..=242 => {
            let n = usize::from(op - XXX1) + 1;
            need!(1 + n);
            let length = unsigned(arg, n) as usize;
            let size = 1 + n + length;
            need!(size);
            (Op::Special(&arg[n..n + length]), size)
        }
        FNT_DEF1..=246 => {
            let n = usize::from(op - FNT_DEF1) + 1;
            need!(1 + n + 14);
            let fields = &arg[n..];
            let area = usize::from(fields[12]);
            let name = usize::from(fields[13]);
            let size = 1 + n + 14 + area + name;
            need!(size);
            let def = FontDef::Tfm {
                name: String::from_utf8_lossy(&fields[14 + area..14 + area + name]).into_owned(),
                checksum: be_u32(fields),
                scale: be_i32(&fields[4..]),
                design_size: be_i32(&fields[8..]),
            };
            (Op::FontDef(unsigned(arg, n), def), size)
        }
        PRE => {
            need!(15);
            let size = 15 + usize::from(arg[13]);
            need!(size);
            (Op::Pre, size)
        }
        POST => {
            need!(29);
            (Op::Post, 29)
        }
        POST_POST => {
            need!(6);
            (Op::PostPost, 6)
        }
        BEGIN_REFLECT | END_REFLECT => (Op::Nop, 1),
        NATIVE_FONT_DEF => {
            // font_num[4] size[4] flags[2] name_len[1] name[name_len] face_index[4]
            // then rgba[4], extend[4], slant[4], embolden[4] as flagged.
            need!(12);
            let flags = be_u16(&arg[8..]);
            let name_len = usize::from(arg[10]);
            let mut size = 12 + name_len + 4;
            need!(size);
            let name = String::from_utf8_lossy(&arg[11..11 + name_len]).into_owned();
            let index = be_u32(&arg[11 + name_len..]);
            let mut optional = |flag: u16| -> Result<Option<u32>, ()> {
                if flags & flag == 0 {
                    return Ok(None);
                }
                size += 4;
                if buf.len() < size {
                    return Err(());
                }
                Ok(Some(be_u32(&buf[size - 4..])))
            };
            let (Ok(rgba), Ok(extend), Ok(slant), Ok(embolden)) = (
                optional(FLAG_COLORED),
                optional(FLAG_EXTEND),
                optional(FLAG_SLANT),
                optional(FLAG_EMBOLDEN),
            ) else {
                return Ok(None);
            };
            if flags & FLAG_VARIATIONS != 0 {
                need!(size + 2);
                size += 2 + 4 * usize::from(be_u16(&buf[size..]));
                need!(size);
            }
            let def = FontDef::Native {
                name,
                index,
                size: be_i32(&arg[4..]),
                flags,
                rgba,
                extend: extend.map(|value| value as i32),
                slant: slant.map(|value| value as i32),
                embolden: embolden.map(|value| value as i32),
            };
            (Op::FontDef(be_u32(arg), def), size)
        }
        SET_GLYPHS => {
            // width[4] n[2] (dx[4] dy[4])*n glyph[2]*n
            need!(7);
            let count = be_u16(&arg[4..]);
            let size = 7 + 10 * usize::from(count);
            need!(size);
            (
                Op::Glyphs {
                    width: be_i32(arg),
                    count,
                    data: &arg[6..size - 1],
                },
                size,
            )
        }
        SET_TEXT_AND_GLYPHS => {
            // text_len[2] text[2]*text_len width[4] n[2] (dx[4] dy[4])*n glyph[2]*n
            need!(3);
            let text = 2 * usize::from(be_u16(arg));
            let head = 1 + 2 + text;
            need!(head + 6);
            let count = be_u16(&buf[head + 4..]);
            let size = head + 6 + 10 * usize::from(count);
            need!(size);
            (
                Op::Glyphs {
                    width: be_i32(&buf[head..]),
                    count,
                    data: &buf[head + 6..size],
                },
                size,
            )
        }
        _ => return Err(()),
    };
    Ok(Some(decoded))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dimensions_convert_to_big_points() {
        assert_eq!(parse_dimen("72.27pt"), Some(72.0));
        assert_eq!(parse_dimen("1in"), Some(72.0));
        assert_eq!(parse_dimen("25.4mm"), Some(72.0));
        assert_eq!(parse_dimen("2.54truecm"), Some(72.0));
        assert_eq!(parse_dimen("10bp"), Some(10.0));
        assert_eq!(parse_dimen("10furlong"), None);
    }

    #[test]
    fn page_size_specials_are_recognised() {
        let size = parse_page_size("pdf:pagesize width 597.50787pt height 845.04684pt")
            .unwrap()
            .unwrap();
        assert!((size.0 - DEFAULT_PAGE_SIZE.0).abs() < 1e-3);
        assert!((size.1 - DEFAULT_PAGE_SIZE.1).abs() < 1e-3);
        assert_eq!(
            parse_page_size("papersize=72bp,1in"),
            Some(Some((72.0, 72.0)))
        );
        assert_eq!(parse_page_size("pdf:pagesize default"), Some(None));
        assert_eq!(
            parse_page_size("pdfcolorstackinit 1 page direct (0 g 0 G)"),
            None
        );
    }

    #[test]
    fn colour_specials_push_pop_and_reset() {
        let mut carry = Carry::default();
        carry.special(b"color push rgb 1 0 0");
        assert_eq!(carry.color, [255, 0, 0]);
        carry.special(b"color push gray 0.5");
        assert_eq!(carry.color, [128; 3]);
        carry.special(b"color push Unknown");
        assert_eq!(carry.color, [128; 3]);
        carry.special(b"color pop");
        carry.special(b"color pop");
        assert_eq!(carry.color, [255, 0, 0]);
        carry.special(b"color cmyk 0 0 0 1");
        assert_eq!((carry.color, carry.color_stack.len()), ([0; 3], 0));
        carry.special(b"color pop");
        assert_eq!(carry.color, BLACK);
    }

    #[test]
    fn signed_arguments_sign_extend() {
        assert_eq!(signed(&[0xff], 1), -1);
        assert_eq!(signed(&[0x80, 0x00], 2), -32768);
        assert_eq!(signed(&[0x7f, 0xff, 0xff], 3), 0x7fffff);
        assert_eq!(unsigned(&[0xff, 0xff], 2), 0xffff);
    }
}
