use std::ops::Range;

use ropey::Rope;

use crate::note::{NoteError, NoteParagraph, NoteSegment, normalize_math};

#[derive(Debug, Clone)]
pub struct TextBuffer {
    rope: Rope,
    cursor: usize,
    preferred_column: Option<usize>,
    non_whitespace_chars: usize,
}

impl Default for TextBuffer {
    fn default() -> Self {
        Self::new("")
    }
}

impl TextBuffer {
    pub fn new(text: &str) -> Self {
        let rope = Rope::from_str(text);
        let cursor = rope.len_chars();
        Self {
            rope,
            cursor,
            preferred_column: None,
            non_whitespace_chars: text
                .chars()
                .filter(|character| !character.is_whitespace())
                .count(),
        }
    }

    pub fn text(&self) -> String {
        self.rope.to_string()
    }

    pub fn is_blank(&self) -> bool {
        self.non_whitespace_chars == 0
    }

    pub fn len_chars(&self) -> usize {
        self.rope.len_chars()
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// The cursor as a UTF-8 byte offset into [`Self::text`].
    pub fn cursor_byte(&self) -> usize {
        self.rope.char_to_byte(self.cursor)
    }

    /// Move the cursor to the character containing `byte` (clamped to the end of the text).
    pub fn set_cursor_byte(&mut self, byte: usize) {
        self.cursor = self.rope.byte_to_char(byte.min(self.rope.len_bytes()));
        self.preferred_column = None;
    }

    pub fn len_lines(&self) -> usize {
        self.rope.len_lines()
    }

    pub fn line(&self, line: usize) -> String {
        self.rope
            .get_line(line)
            .map(|slice| slice.to_string().trim_end_matches(['\r', '\n']).to_owned())
            .unwrap_or_default()
    }

    pub fn cursor_line_column(&self) -> (usize, usize) {
        let line = self.rope.char_to_line(self.cursor);
        (line, self.cursor - self.rope.line_to_char(line))
    }

    /// The line holding `byte` (clamped to the end of the text).
    pub fn line_of_byte(&self, byte: usize) -> usize {
        self.rope.byte_to_line(byte.min(self.rope.len_bytes()))
    }

    pub fn cursor_display_column(&self) -> usize {
        let line = self.rope.char_to_line(self.cursor);
        let line_start = self.rope.line_to_char(line);
        self.rope
            .slice(line_start..self.cursor)
            .chars()
            .map(|character| unicode_width::UnicodeWidthChar::width(character).unwrap_or(0))
            .sum()
    }

    pub fn set_cursor_line_column(&mut self, line: usize, column: usize) {
        let line = line.min(self.rope.len_lines().saturating_sub(1));
        self.cursor = self.rope.line_to_char(line) + column.min(self.line_content_len(line));
        self.preferred_column = None;
    }

    pub fn insert_char(&mut self, character: char) {
        self.rope.insert_char(self.cursor, character);
        self.cursor += 1;
        if !character.is_whitespace() {
            self.non_whitespace_chars += 1;
        }
        self.preferred_column = None;
    }

    pub fn insert_str(&mut self, text: &str) {
        self.rope.insert(self.cursor, text);
        self.cursor += text.chars().count();
        self.non_whitespace_chars += text
            .chars()
            .filter(|character| !character.is_whitespace())
            .count();
        self.preferred_column = None;
    }

    pub fn backspace(&mut self) -> bool {
        if self.cursor == 0 {
            return false;
        }
        if !self.rope.char(self.cursor - 1).is_whitespace() {
            self.non_whitespace_chars -= 1;
        }
        self.rope.remove(self.cursor - 1..self.cursor);
        self.cursor -= 1;
        self.preferred_column = None;
        true
    }

    pub fn delete(&mut self) -> bool {
        if self.cursor == self.rope.len_chars() {
            return false;
        }
        if !self.rope.char(self.cursor).is_whitespace() {
            self.non_whitespace_chars -= 1;
        }
        self.rope.remove(self.cursor..self.cursor + 1);
        self.preferred_column = None;
        true
    }

    pub fn clear(&mut self) {
        self.rope = Rope::new();
        self.cursor = 0;
        self.preferred_column = None;
        self.non_whitespace_chars = 0;
    }

    pub fn move_left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
        self.preferred_column = None;
    }

    pub fn move_right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.rope.len_chars());
        self.preferred_column = None;
    }

    pub fn move_home(&mut self) {
        let (line, _) = self.cursor_line_column();
        self.cursor = self.rope.line_to_char(line);
        self.preferred_column = None;
    }

    pub fn move_end(&mut self) {
        let (line, _) = self.cursor_line_column();
        self.cursor = self.rope.line_to_char(line) + self.line_content_len(line);
        self.preferred_column = None;
    }

    pub fn move_up(&mut self) {
        self.move_vertical(-1);
    }

    pub fn move_down(&mut self) {
        self.move_vertical(1);
    }

    fn move_vertical(&mut self, delta: isize) {
        let (line, column) = self.cursor_line_column();
        let preferred = *self.preferred_column.get_or_insert(column);
        let target = line
            .saturating_add_signed(delta)
            .min(self.rope.len_lines() - 1);
        self.cursor = self.rope.line_to_char(target) + preferred.min(self.line_content_len(target));
    }

    fn line_content_len(&self, line: usize) -> usize {
        let slice = self.rope.line(line);
        let mut len = slice.len_chars();
        while len > 0 && matches!(slice.char(len - 1), '\n' | '\r') {
            len -= 1;
        }
        len
    }
}

/// Map a byte offset in `from` to the matching offset in `to`, where the texts differ by one
/// replaced region: their longest common prefix and suffix correspond byte for byte, and any
/// offset inside the replaced region maps to the region's start in `to`.
pub fn map_offset_across_edit(from: &str, to: &str, byte: usize) -> usize {
    let byte = byte.min(from.len());
    let mut prefix = from
        .bytes()
        .zip(to.bytes())
        .take_while(|(left, right)| left == right)
        .count();
    while !from.is_char_boundary(prefix) || !to.is_char_boundary(prefix) {
        prefix -= 1;
    }
    let mut suffix = from
        .bytes()
        .rev()
        .zip(to.bytes().rev())
        .take(from.len().min(to.len()) - prefix)
        .take_while(|(left, right)| left == right)
        .count();
    while !from.is_char_boundary(from.len() - suffix) || !to.is_char_boundary(to.len() - suffix) {
        suffix -= 1;
    }

    if byte <= prefix {
        byte
    } else if byte >= from.len() - suffix {
        byte + to.len() - from.len()
    } else {
        prefix
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceSpan {
    pub start: usize,
    pub end: usize,
}

impl SourceSpan {
    pub fn range(&self) -> Range<usize> {
        self.start..self.end
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Inline {
    Text { text: String, span: SourceSpan },
    Math { latex: String, span: SourceSpan },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Block {
    Paragraph { inlines: Vec<Inline> },
    DisplayMath { latex: String, span: SourceSpan },
    Blank,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceMapEntry {
    pub source: SourceSpan,
    pub output: SourceSpan,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Document {
    note: String,
    blocks: Vec<Block>,
}

impl Document {
    pub fn parse(input: &str) -> Result<Self, NoteError> {
        let mut blocks = Vec::new();
        let mut paragraph_start = None;
        let mut paragraph = String::new();
        let mut offset = 0;
        let mut display_start: Option<usize> = None;
        let mut display_content_start = 0;

        for line in input.split_inclusive('\n') {
            let line_start = offset;
            offset += line.len();
            let line_no_newline = line.strip_suffix('\n').unwrap_or(line);
            let trimmed = line_no_newline.trim();

            if display_start.is_some() {
                if trimmed == "$$" {
                    blocks.push(display_math(input, display_content_start..line_start)?);
                    display_start = None;
                }
                continue;
            }

            if trimmed == "$$" {
                flush_paragraph(&mut blocks, &mut paragraph, &mut paragraph_start)?;
                display_start = Some(line_start);
                display_content_start = offset;
            } else if trimmed.starts_with("$$") && trimmed.ends_with("$$") && trimmed.len() >= 4 {
                flush_paragraph(&mut blocks, &mut paragraph, &mut paragraph_start)?;
                let leading = line_no_newline.find("$$").unwrap_or(0);
                let trailing = line_no_newline.rfind("$$").unwrap_or(line_no_newline.len());
                blocks.push(display_math(
                    input,
                    line_start + leading + 2..line_start + trailing,
                )?);
            } else if trimmed.is_empty() {
                flush_paragraph(&mut blocks, &mut paragraph, &mut paragraph_start)?;
                blocks.push(Block::Blank);
            } else {
                if paragraph_start.is_none() {
                    paragraph_start = Some(line_start);
                }
                paragraph.push_str(&input[line_start..offset]);
            }
        }
        if let Some(start) = display_start {
            return Err(NoteError::InvalidMath {
                byte: start,
                message: String::from("unclosed display math fence"),
            });
        }
        flush_paragraph(&mut blocks, &mut paragraph, &mut paragraph_start)?;

        Ok(Self {
            note: input.to_owned(),
            blocks,
        })
    }

    /// The note text this document was parsed from.
    pub fn note(&self) -> &str {
        &self.note
    }

    pub fn blocks(&self) -> &[Block] {
        &self.blocks
    }
}

/// Builds the display-math block whose content is `input[content]`, with the
/// span trimmed to the non-whitespace content.
fn display_math(input: &str, content: Range<usize>) -> Result<Block, NoteError> {
    let raw = &input[content.clone()];
    let leading = raw.len() - raw.trim_start().len();
    let trailing = raw.len() - raw.trim_end().len();
    let start = content.start + leading;
    let latex = normalize_math(raw.trim()).map_err(|message| NoteError::InvalidMath {
        byte: start,
        message,
    })?;
    Ok(Block::DisplayMath {
        latex,
        span: SourceSpan {
            start,
            end: content.end - trailing,
        },
    })
}

fn flush_paragraph(
    blocks: &mut Vec<Block>,
    paragraph: &mut String,
    paragraph_start: &mut Option<usize>,
) -> Result<(), NoteError> {
    let Some(start) = paragraph_start.take() else {
        return Ok(());
    };
    let parsed = NoteParagraph::parse(paragraph)?;
    let mut search_cursor = 0;
    let mut inlines = Vec::new();
    for segment in parsed.segments() {
        match segment {
            NoteSegment::Text(text) => {
                let local_start = paragraph[search_cursor..]
                    .find(text)
                    .map(|offset| search_cursor + offset)
                    .unwrap_or(search_cursor);
                let span = SourceSpan {
                    start: start + local_start,
                    end: start + local_start + text.len(),
                };
                search_cursor = local_start + text.len();
                inlines.push(Inline::Text {
                    text: text.clone(),
                    span,
                });
            }
            NoteSegment::Math(math) => {
                inlines.push(Inline::Math {
                    latex: math.latex().to_owned(),
                    span: SourceSpan {
                        start: start + math.span().start,
                        end: start + math.span().end,
                    },
                });
                search_cursor = math.span().end;
            }
        }
    }
    blocks.push(Block::Paragraph { inlines });
    paragraph.clear();
    Ok(())
}
