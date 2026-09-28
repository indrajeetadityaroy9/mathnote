//! TeX font metric (TFM) files: the character widths `set_char` advances by.
//!
//! Mirrors texpresso's `tex_tfm.c`: the header words give the table sizes, the char-info word of
//! each code in `bc..=ec` indexes the width table, and every width is a `fix_word` scaled to the
//! DVI font's size with DVItype's exact integer algorithm.

/// Parsed widths of one TFM file. Only what the DVI interpreter needs is kept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Tfm {
    first_char: u16,
    /// Width-table index of each code in `first_char..=last_char`.
    width_index: Vec<u8>,
    /// Width table as `fix_word`s (design-size units, 20 fractional bits).
    widths: Vec<i32>,
}

impl Tfm {
    pub(crate) fn parse(data: &[u8]) -> Result<Self, String> {
        let word16 = |index: usize| -> Result<usize, String> {
            data.get(index * 2..index * 2 + 2)
                .map(|bytes| usize::from(u16::from_be_bytes([bytes[0], bytes[1]])))
                .ok_or_else(|| String::from("truncated TFM header"))
        };
        let lf = word16(0)?;
        let lh = word16(1)?;
        let bc = word16(2)?;
        let ec = word16(3)?;
        let nw = word16(4)?;
        let [nh, nd, ni, nl, nk, ne, np] = [
            word16(5)?,
            word16(6)?,
            word16(7)?,
            word16(8)?,
            word16(9)?,
            word16(10)?,
            word16(11)?,
        ];

        if ec > 255 || bc > ec + 1 || ne > 256 {
            return Err(format!("character codes out of range (bc={bc}, ec={ec})"));
        }
        let char_count = ec + 1 - bc;
        if 6 + lh + char_count + nw + nh + nd + ni + nl + nk + ne + np != lf {
            return Err(String::from("inconsistent TFM length values"));
        }
        if lh < 2 {
            return Err(String::from("TFM header is too small"));
        }
        if data.len() < lf * 4 {
            return Err(String::from("truncated TFM body"));
        }

        let word32 = |word: usize| -> i32 {
            let at = word * 4;
            i32::from_be_bytes([data[at], data[at + 1], data[at + 2], data[at + 3]])
        };
        let char_base = 6 + lh;
        let width_base = char_base + char_count;
        let width_index = (0..char_count)
            .map(|index| data[(char_base + index) * 4])
            .collect();
        let widths = (0..nw).map(|index| word32(width_base + index)).collect();

        Ok(Self {
            first_char: bc as u16,
            width_index,
            widths,
        })
    }

    /// Width of `code` as a `fix_word`; 0 outside `bc..=ec` or for a missing character.
    pub(crate) fn width_fix_word(&self, code: u32) -> i32 {
        let Some(offset) = code.checked_sub(u32::from(self.first_char)) else {
            return 0;
        };
        self.width_index
            .get(offset as usize)
            .and_then(|&index| self.widths.get(usize::from(index)))
            .copied()
            .unwrap_or(0)
    }

    /// Width of `code` in DVI units for a font loaded at scale `scale` (DVI units).
    pub(crate) fn scaled_width(&self, code: u32, scale: i32) -> i32 {
        scale_fix_word(self.width_fix_word(code), scale)
    }
}

/// DVItype's `store_scaled`: multiply a `fix_word` by the font scale `s` exactly as TeX does, so
/// that accumulated advances match the positions TeX computed.
pub(crate) fn scale_fix_word(fix_word: i32, scale: i32) -> i32 {
    let mut z = i64::from(scale.max(0));
    let mut alpha: i64 = 16;
    while z >= 0o40000000 {
        z /= 2;
        alpha += alpha;
    }
    let beta = 256 / alpha;
    let alpha = alpha * z;

    let [a, b, c, d] = fix_word.to_be_bytes().map(i64::from);
    let width = (((d * z) / 256 + c * z) / 256 + b * z) / beta;
    match a {
        0 => width as i32,
        255 => (width - alpha) as i32,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal TFM: lh = 2, codes 65..=66, three widths (0, 0.5, -0.25 design units).
    fn sample() -> Vec<u8> {
        let (lh, bc, ec, nw) = (2u16, 65u16, 66u16, 3u16);
        let lf = 6 + lh + (ec - bc + 1) + nw;
        let mut data = Vec::new();
        for word in [lf, lh, bc, ec, nw, 0, 0, 0, 0, 0, 0, 0] {
            data.extend_from_slice(&word.to_be_bytes());
        }
        data.extend_from_slice(&0u32.to_be_bytes()); // checksum
        data.extend_from_slice(&(10i32 << 20).to_be_bytes()); // design size
        data.extend_from_slice(&[1, 0, 0, 0]); // 'A' -> width 1
        data.extend_from_slice(&[2, 0, 0, 0]); // 'B' -> width 2
        for width in [0i32, 1 << 19, -(1 << 18)] {
            data.extend_from_slice(&width.to_be_bytes());
        }
        data
    }

    #[test]
    fn widths_scale_with_the_dvi_font_size_and_vanish_outside_the_code_range() {
        let tfm = Tfm::parse(&sample()).unwrap();
        let ten_pt = 10 * 65536;
        assert_eq!(tfm.scaled_width(65, ten_pt), 5 * 65536);
        assert_eq!(tfm.scaled_width(66, ten_pt), -(10 * 65536) / 4);
        assert_eq!(tfm.scaled_width(64, ten_pt), 0);
        assert_eq!(tfm.scaled_width(67, ten_pt), 0);
    }

    #[test]
    fn inconsistent_lengths_are_rejected() {
        let mut data = sample();
        data[1] += 1;
        assert!(Tfm::parse(&data).is_err());
    }

    #[test]
    fn large_scales_follow_dvitype_rounding() {
        // s >= 2^23 halves z until it fits, the result must still be w * s / 2^20 within 1 unit.
        let fix = (3 << 20) / 7;
        let scale = 200 * 65536;
        let exact = (i64::from(fix) * i64::from(scale)) >> 20;
        assert!((i64::from(scale_fix_word(fix, scale)) - exact).abs() <= 1);
    }
}
