use crate::document::{Block, Document, Inline, SourceMapEntry, SourceSpan};
use crate::note::unicode_math_command;

/// Zero-width glue placed before every newline of paragraph text in the compiled source, so each
/// generated content line yields a SyncTeX glue record stamped with its own line (glue records
/// are always written; kerns are dropped when first in a list, and a paragraph's tail glue becomes
/// `\penalty10000`). The `{}` stops `\hskip` from consuming the following end-of-line space as
/// its optional unit space; glue (unlike a math node) keeps the preceding word hyphenatable and
/// leaves the space factor alone, and sits at the same breakpoint as the interword glue after it.
const TEXT_ANCHOR: &str = r"\hskip0pt{}";

/// The display-math form of [`TEXT_ANCHOR`]: spaces are ignored in math mode, so `\relax` only
/// ends keyword scanning after the dimension.
const MATH_ANCHOR: &str = r"\hskip0pt\relax";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LatexDocument {
    source: String,
    body: String,
    source_map: Vec<SourceMapEntry>,
    note: String,
}

impl LatexDocument {
    /// The complete compiled source, including the template and SyncTeX line anchors.
    pub fn source(&self) -> &str {
        &self.source
    }

    /// The display body shown in the LaTeX pane. Its lines match [`Self::source`]'s body lines.
    pub fn body(&self) -> &str {
        &self.body
    }

    pub fn source_map(&self) -> &[SourceMapEntry] {
        &self.source_map
    }

    /// The note text this document was generated from.
    pub fn note(&self) -> &str {
        &self.note
    }

    pub fn source_span_for_output_line(&self, line: usize) -> Option<SourceSpan> {
        let (start, end) = self.output_line_bounds(line)?;

        self.source_map
            .iter()
            .find(|entry| entry.output.start <= end && entry.output.end >= start)
            .or_else(|| {
                self.source_map
                    .iter()
                    .rev()
                    .find(|entry| entry.output.end <= start)
            })
            .map(|entry| entry.source.clone())
    }

    /// Whether a 1-based line of [`Self::source`] holds typeset note material, i.e. is non-empty
    /// and overlaps a source-mapped output range. Template, separator, and `\[`/`\]` lines are not.
    pub fn is_content_line(&self, line: usize) -> bool {
        self.content_entry(line).is_some()
    }

    /// Every content line (see [`Self::is_content_line`]) in ascending order, in one pass over
    /// the source: source-map output ranges are emitted in order and never overlap.
    pub fn content_lines(&self) -> Vec<usize> {
        let mut lines = Vec::new();
        let mut entries = self.source_map.iter().peekable();
        let mut start = 0;
        for (index, text) in self.source.split('\n').enumerate() {
            let end = start + text.len();
            while entries.next_if(|entry| entry.output.end <= start).is_some() {}
            if start < end && entries.peek().is_some_and(|entry| entry.output.start < end) {
                lines.push(index + 1);
            }
            start = end + 1;
        }
        lines
    }

    /// The 1-based line of [`Self::source`] that typesets the note byte `byte`, if it is a
    /// content line. Bytes on blank note lines and display fences map to nothing.
    pub fn output_line_for_source_byte(&self, byte: usize) -> Option<usize> {
        let entry = self
            .source_map
            .iter()
            .rev()
            .find(|entry| entry.source.start <= byte)?;
        if byte > entry.source.end {
            return None;
        }

        let note = self.note.as_bytes();
        let end = byte.min(note.len());
        let start = entry.source.start.min(end);
        let within_note = count_newlines(&note[start..end]);

        let source = self.source.as_bytes();
        let output_start = entry.output.start.min(source.len());
        let output_end = entry.output.end.min(source.len()).max(output_start);
        let within_output = count_newlines(&source[output_start..output_end]);
        let line = 1 + count_newlines(&source[..output_start]) + within_note.min(within_output);

        self.is_content_line(line).then_some(line)
    }

    /// The note byte at which the content of a 1-based line of [`Self::source`] begins.
    pub fn source_byte_for_output_line(&self, line: usize) -> Option<usize> {
        let (line_start, _) = self.output_line_bounds(line)?;
        let entry = self.content_entry(line)?;

        let newlines = if line_start > entry.output.start {
            count_newlines(&self.source.as_bytes()[entry.output.start..line_start])
        } else {
            0
        };

        let note = self.note.as_bytes();
        let end = entry.source.end.min(note.len());
        let mut byte = entry.source.start.min(end);
        let mut remaining = newlines;
        while remaining > 0 && byte < end {
            if note[byte] == b'\n' {
                remaining -= 1;
            }
            byte += 1;
        }
        Some(byte)
    }

    fn content_entry(&self, line: usize) -> Option<&SourceMapEntry> {
        let (start, end) = self.output_line_bounds(line)?;
        if start == end {
            return None;
        }
        self.source_map
            .iter()
            .find(|entry| entry.output.start < end && entry.output.end > start)
    }

    /// Byte start and end (excluding the newline) of a 1-based line of [`Self::source`].
    fn output_line_bounds(&self, line: usize) -> Option<(usize, usize)> {
        if line == 0 {
            return None;
        }
        let start = if line == 1 {
            0
        } else {
            self.source
                .match_indices('\n')
                .nth(line - 2)
                .map(|(byte, _)| byte + 1)?
        };
        let end = self.source[start..]
            .find('\n')
            .map_or(self.source.len(), |offset| start + offset);
        Some((start, end))
    }
}

fn count_newlines(bytes: &[u8]) -> usize {
    bytes.iter().filter(|byte| **byte == b'\n').count()
}

pub fn emit_latex(document: &Document) -> LatexDocument {
    let mut emitter = Emitter {
        display: String::new(),
        compiled: String::new(),
        source_map: Vec::new(),
    };

    for block in document.blocks() {
        match block {
            Block::Paragraph { inlines, .. } => {
                for inline in inlines {
                    match inline {
                        Inline::Text { text, source } => {
                            emitter.push_anchored(&escape_prose(text), source.clone(), TEXT_ANCHOR);
                        }
                        Inline::Math { latex, span, .. } => {
                            emitter.push("\\(", None);
                            emitter.push(latex, Some(span.clone()));
                            emitter.push("\\)", None);
                        }
                    }
                }
                if !emitter.compiled.ends_with('\n') {
                    emitter.push_compiled(TEXT_ANCHOR);
                }
                emitter.push("\n\n", None);
            }
            Block::DisplayMath { latex, span, .. } => {
                emitter.push("\\[\n", None);
                emitter.push_anchored(latex, span.clone(), MATH_ANCHOR);
                emitter.push_compiled(MATH_ANCHOR);
                emitter.push("\n\\]\n\n", None);
            }
            Block::Blank { .. } => {
                emitter.push("\n", None);
            }
        }
    }

    let mut source_map = emitter.source_map;
    for entry in &mut source_map {
        entry.output.start += TEMPLATE_PREFIX.len();
        entry.output.end += TEMPLATE_PREFIX.len();
    }

    LatexDocument {
        source: format!("{TEMPLATE_PREFIX}{}{TEMPLATE_SUFFIX}", emitter.compiled),
        body: emitter.display,
        source_map,
        note: document.source().to_owned(),
    }
}

pub fn escape_prose(input: &str) -> String {
    let mut escaped = String::new();
    for character in input.chars() {
        if let Some(command) = unicode_math_command(character) {
            escaped.push_str(r"\ensuremath{");
            escaped.push_str(command);
            escaped.push('}');
            continue;
        }
        match character {
            '\\' => escaped.push_str(r"\textbackslash{}"),
            '{' => escaped.push_str(r"\{"),
            '}' => escaped.push_str(r"\}"),
            '$' => escaped.push_str(r"\$"),
            '&' => escaped.push_str(r"\&"),
            '%' => escaped.push_str(r"\%"),
            '#' => escaped.push_str(r"\#"),
            '_' => escaped.push_str(r"\_"),
            '^' => escaped.push_str(r"\textasciicircum{}"),
            '~' => escaped.push_str(r"\textasciitilde{}"),
            '<' => escaped.push_str(r"\textless{}"),
            '>' => escaped.push_str(r"\textgreater{}"),
            _ => escaped.push(character),
        }
    }
    escaped
}

/// Writes the display body and the compiled body side by side. Source-map entries always refer
/// to the compiled stream, whose lines stay aligned with the display stream because anchors never
/// add newlines.
struct Emitter {
    display: String,
    compiled: String,
    source_map: Vec<SourceMapEntry>,
}

impl Emitter {
    fn push(&mut self, text: &str, source: Option<SourceSpan>) {
        let start = self.compiled.len();
        self.display.push_str(text);
        self.compiled.push_str(text);
        if let Some(source) = source {
            self.record(source, start);
        }
    }

    fn push_anchored(&mut self, text: &str, source: SourceSpan, anchor: &str) {
        let start = self.compiled.len();
        self.display.push_str(text);
        for (index, line) in text.split('\n').enumerate() {
            if index > 0 {
                self.compiled.push_str(anchor);
                self.compiled.push('\n');
            }
            self.compiled.push_str(line);
        }
        self.record(source, start);
    }

    fn push_compiled(&mut self, text: &str) {
        self.compiled.push_str(text);
    }

    fn record(&mut self, source: SourceSpan, start: usize) {
        self.source_map.push(SourceMapEntry {
            source,
            output: SourceSpan {
                start,
                end: self.compiled.len(),
            },
        });
    }
}

const TEMPLATE_PREFIX: &str = r"\documentclass[a4paper,11pt]{article}
\usepackage[margin=20mm]{geometry}
\pagestyle{plain}
\setlength{\parindent}{0pt}
\setlength{\parskip}{0.6em}
\emergencystretch=2em
\begin{document}
\null

";

const TEMPLATE_SUFFIX: &str = r"\end{document}
";
