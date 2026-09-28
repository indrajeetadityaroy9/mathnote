use std::ops::Range;

use ropey::Rope;

use crate::note::{NoteError, NoteLine, NoteSegment, normalize_math};

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

    pub fn is_empty(&self) -> bool {
        self.rope.len_chars() == 0
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
    pub fn new(range: Range<usize>) -> Self {
        Self {
            start: range.start,
            end: range.end,
        }
    }

    pub fn range(&self) -> Range<usize> {
        self.start..self.end
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Inline {
    Text {
        text: String,
        source: SourceSpan,
    },
    Math {
        source: String,
        latex: String,
        span: SourceSpan,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Block {
    Paragraph {
        inlines: Vec<Inline>,
        source: SourceSpan,
    },
    DisplayMath {
        source: String,
        latex: String,
        span: SourceSpan,
    },
    Blank {
        source: SourceSpan,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceMapEntry {
    pub source: SourceSpan,
    pub output: SourceSpan,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Document {
    source: String,
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
        let mut display_content = String::new();

        for line in input.split_inclusive('\n') {
            let line_start = offset;
            offset += line.len();
            let line_no_newline = line.strip_suffix('\n').unwrap_or(line);
            let trimmed = line_no_newline.trim();

            if display_start.is_some() {
                if trimmed == "$$" {
                    let leading = display_content.len() - display_content.trim_start().len();
                    let trailing = display_content.len() - display_content.trim_end().len();
                    let source = display_content.trim().to_owned();
                    let latex =
                        normalize_math(&source).map_err(|message| NoteError::InvalidMath {
                            byte: display_content_start + leading,
                            message,
                        })?;
                    blocks.push(Block::DisplayMath {
                        source,
                        latex,
                        span: SourceSpan {
                            start: display_content_start + leading,
                            end: display_content_start + display_content.len() - trailing,
                        },
                    });
                    display_start = None;
                    display_content.clear();
                } else {
                    display_content.push_str(line);
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
                let content_start = line_start + leading + 2;
                let content_end = line_start + trailing;
                let raw_source = &input[content_start..content_end];
                let leading = raw_source.len() - raw_source.trim_start().len();
                let trailing = raw_source.len() - raw_source.trim_end().len();
                let source = raw_source.trim().to_owned();
                let latex = normalize_math(&source).map_err(|message| NoteError::InvalidMath {
                    byte: content_start + leading,
                    message,
                })?;
                blocks.push(Block::DisplayMath {
                    source,
                    latex,
                    span: SourceSpan {
                        start: content_start + leading,
                        end: content_end - trailing,
                    },
                });
            } else if trimmed.is_empty() {
                flush_paragraph(&mut blocks, &mut paragraph, &mut paragraph_start)?;
                blocks.push(Block::Blank {
                    source: SourceSpan {
                        start: line_start,
                        end: offset,
                    },
                });
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
            source: input.to_owned(),
            blocks,
        })
    }

    pub fn source(&self) -> &str {
        &self.source
    }

    pub fn blocks(&self) -> &[Block] {
        &self.blocks
    }
}

fn flush_paragraph(
    blocks: &mut Vec<Block>,
    paragraph: &mut String,
    paragraph_start: &mut Option<usize>,
) -> Result<(), NoteError> {
    let Some(start) = paragraph_start.take() else {
        return Ok(());
    };
    let end = start + paragraph.len();
    let parsed = NoteLine::parse(paragraph)?;
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
                    source: span,
                });
            }
            NoteSegment::Math(math) => {
                inlines.push(Inline::Math {
                    source: math.source().to_owned(),
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
    blocks.push(Block::Paragraph {
        inlines,
        source: SourceSpan { start, end },
    });
    paragraph.clear();
    Ok(())
}
