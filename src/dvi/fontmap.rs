//! `pdftex.map`: which Type1 file, encoding and effects draw a TFM font.
//!
//! Mirrors texpresso's `tex_fontmap.c` line grammar:
//! `tfmname [psname] ["snippet"] [<[encfile.enc] [<fontfile.pfb]`, where `<<` and `<[` mark
//! the same files, `%` starts a comment line, and any other token rejects the line. The map is
//! several megabytes, so it is indexed once by first token and a line is only parsed when its
//! TFM name is requested.

use std::collections::HashMap;
use std::ops::Range;
use std::sync::Arc;

/// One parsed map line.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct MapEntry {
    pub font_file: Option<String>,
    pub encoding: Option<String>,
    /// `SlantFont` factor (0 when absent).
    pub slant: f32,
    /// `ExtendFont` factor (1 when absent).
    pub extend: f32,
}

/// First-token index over the raw map text.
pub(crate) struct FontMap {
    text: Arc<[u8]>,
    lines: HashMap<Box<str>, Vec<Range<usize>>>,
}

impl FontMap {
    pub(crate) fn new(text: Arc<[u8]>) -> Self {
        let mut lines: HashMap<Box<str>, Vec<Range<usize>>> = HashMap::new();
        let mut start = 0;
        while start < text.len() {
            let end = text[start..]
                .iter()
                .position(|&byte| byte == b'\n')
                .map_or(text.len(), |offset| start + offset);
            let line = &text[start..end];
            let token_start = line
                .iter()
                .position(|&byte| !is_space(byte))
                .unwrap_or(line.len());
            let token_end = line[token_start..]
                .iter()
                .position(|&byte| is_space(byte))
                .map_or(line.len(), |offset| token_start + offset);
            if token_start < token_end
                && line[token_start] != b'%'
                && let Ok(name) = std::str::from_utf8(&line[token_start..token_end])
            {
                lines
                    .entry(name.into())
                    .or_default()
                    .push(start + token_end..end);
            }
            start = end + 1;
        }
        Self { text, lines }
    }

    /// The first well-formed line for `tfm_name`.
    pub(crate) fn lookup(&self, tfm_name: &str) -> Option<MapEntry> {
        self.lines
            .get(tfm_name)?
            .iter()
            .find_map(|range| parse_rest(&self.text[range.clone()]))
    }
}

fn is_space(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\r')
}

/// Parse everything after the TFM name.
fn parse_rest(rest: &[u8]) -> Option<MapEntry> {
    let rest = std::str::from_utf8(rest).ok()?;
    let mut entry = MapEntry {
        font_file: None,
        encoding: None,
        slant: 0.0,
        extend: 1.0,
    };
    let mut cursor = rest.trim_start_matches([' ', '\t', '\r']);

    // Optional PostScript name.
    if !cursor.is_empty() && !cursor.starts_with(['<', '"']) {
        let end = cursor.find([' ', '\t', '\r']).unwrap_or(cursor.len());
        cursor = cursor[end..].trim_start_matches([' ', '\t', '\r']);
    }

    while !cursor.is_empty() {
        if let Some(snippet) = cursor.strip_prefix('"') {
            let end = snippet.find('"')?;
            apply_snippet(&mut entry, &snippet[..end]);
            cursor = &snippet[end + 1..];
        } else {
            // Anything but a snippet or a `<file` rejects the line, as in texpresso.
            let file = cursor.strip_prefix('<')?;
            let file = file.trim_start_matches([' ', '\t']);
            let file = file.strip_prefix(['[', '<']).unwrap_or(file);
            let file = file.trim_start_matches([' ', '\t']);
            let end = file.find([' ', '\t', '\r']).unwrap_or(file.len());
            let name = &file[..end];
            if name.is_empty() {
                return None;
            }
            if name.ends_with(".enc") {
                entry.encoding = Some(name.to_owned());
            } else {
                entry.font_file = Some(name.to_owned());
            }
            cursor = &file[end..];
        }
        cursor = cursor.trim_start_matches([' ', '\t', '\r']);
    }
    Some(entry)
}

/// PostScript effects: `<number> SlantFont`, `<number> ExtendFont`.
fn apply_snippet(entry: &mut MapEntry, snippet: &str) {
    let mut previous: Option<f32> = None;
    for token in snippet.split_whitespace() {
        match token {
            "SlantFont" => entry.slant = previous.unwrap_or(0.0),
            "ExtendFont" => entry.extend = previous.unwrap_or(1.0),
            _ => {}
        }
        previous = token.parse().ok();
    }
}

/// Glyph names of a PostScript encoding vector file (`/Name [ /a /b ... ] def`).
pub(crate) fn parse_encoding(data: &[u8]) -> Vec<String> {
    let text = String::from_utf8_lossy(data);
    let mut names = Vec::new();
    let mut inside = false;
    for line in text.lines() {
        let line = line.split('%').next().unwrap_or("");
        let mut rest = line;
        while !rest.is_empty() {
            if !inside {
                match rest.find('[') {
                    Some(at) => {
                        inside = true;
                        rest = &rest[at + 1..];
                    }
                    None => break,
                }
                continue;
            }
            let (chunk, closed) = match rest.find(']') {
                Some(at) => (&rest[..at], Some(at)),
                None => (rest, None),
            };
            names.extend(
                chunk
                    .split(|c: char| c.is_whitespace() || c == '/')
                    .filter(|name| !name.is_empty())
                    .map(str::to_owned),
            );
            match closed {
                Some(_) => return names,
                None => break,
            }
        }
    }
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(text: &str) -> FontMap {
        FontMap::new(Arc::from(text.as_bytes()))
    }

    #[test]
    fn map_lines_give_font_file_encoding_and_effects() {
        let map = map("% comment\n\
             cmr10 CMR10 <cmr10.pfb\n\
             ptmro8r Times-Roman \" .167 SlantFont TeXBase1Encoding ReEncodeFont \" <8r.enc <utmr8a.pfb\n\
             pncr8rn NewCenturySchlbk-Roman \"0.82 ExtendFont\" <[8r.enc <<uncr8a.pfb\n\
             bad10 BAD10 4 <bad10.pfb\n");

        let cmr = map.lookup("cmr10").unwrap();
        assert_eq!(cmr.font_file.as_deref(), Some("cmr10.pfb"));
        assert_eq!(cmr.encoding, None);
        assert_eq!((cmr.slant, cmr.extend), (0.0, 1.0));

        let times = map.lookup("ptmro8r").unwrap();
        assert_eq!(times.font_file.as_deref(), Some("utmr8a.pfb"));
        assert_eq!(times.encoding.as_deref(), Some("8r.enc"));
        assert_eq!(times.slant, 0.167);

        let century = map.lookup("pncr8rn").unwrap();
        assert_eq!(century.font_file.as_deref(), Some("uncr8a.pfb"));
        assert_eq!(century.encoding.as_deref(), Some("8r.enc"));
        assert_eq!(century.extend, 0.82);

        assert_eq!(map.lookup("bad10"), None);
        assert_eq!(map.lookup("comment"), None);
        assert_eq!(map.lookup("cmr1"), None);
    }

    #[test]
    fn encoding_vectors_list_names_in_code_order() {
        let names = parse_encoding(
            b"% header\n/TeXBase1Encoding [\n/.notdef /dotaccent % 1\n/fi/fl\n] def\n",
        );
        assert_eq!(names, [".notdef", "dotaccent", "fi", "fl"]);
    }
}
