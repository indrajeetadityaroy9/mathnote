//! In-memory index of the SyncTeX data Tectonic writes for a compiled note.
//!
//! Only records that carry their own source line are indexed: glue, kern, math and rule records.
//! XeTeX writes all of them from `hlist_out`, so each lies inside an hbox record; the outermost
//! enclosing hbox is the typeset line (or display, header, footer box) the record belongs to.
//! `x` records are ignored because they repeat the context of the previous node, and box records
//! carry the line at which the box was packaged rather than the lines of its contents.

use std::fmt;
use std::io::Read;

use flate2::read::GzDecoder;

/// The first file XeTeX opens, the job's primary input, always receives SyncTeX tag 1.
const PRIMARY_TAG: u32 = 1;
/// 65536 sp = 1 pt and 72.27 pt = 72 bp = 1 in.
const SP_PER_POINT: f64 = 65536.0 * 72.27 / 72.0;

/// The typeset extent of a source line, in PDF points from the top-left page corner.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SyncTarget {
    pub page: usize,
    pub top: f32,
    pub bottom: f32,
}

#[derive(Debug)]
pub enum SyncError {
    Decompress(String),
    Malformed { line: usize },
}

impl fmt::Display for SyncError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Decompress(message) => write!(formatter, "SyncTeX data is not gzip: {message}"),
            Self::Malformed { line } => {
                write!(formatter, "SyncTeX data is malformed at line {line}")
            }
        }
    }
}

impl std::error::Error for SyncError {}

#[derive(Debug, Clone, PartialEq)]
pub struct SyncIndex {
    pages: Vec<SyncPage>,
}

#[derive(Debug, Clone, PartialEq, Default)]
struct SyncPage {
    boxes: Vec<LineBox>,
    records: Vec<SyncRecord>,
}

/// An outermost hbox: baseline position `(h, v)` with its width, height and depth.
#[derive(Debug, Clone, Copy, PartialEq)]
struct LineBox {
    h: f32,
    v: f32,
    width: f32,
    height: f32,
    depth: f32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct SyncRecord {
    line: usize,
    h: f32,
    line_box: usize,
}

impl LineBox {
    fn top(&self) -> f32 {
        self.v - self.height
    }

    fn bottom(&self) -> f32 {
        self.v + self.depth
    }

    fn distance(&self, x: f32, y: f32) -> f32 {
        let dx = axis_distance(x, self.h, self.h + self.width);
        let dy = axis_distance(y, self.top(), self.bottom());
        dx.hypot(dy)
    }
}

fn axis_distance(value: f32, start: f32, end: f32) -> f32 {
    if value < start {
        start - value
    } else if value > end {
        value - end
    } else {
        0.0
    }
}

struct RawBox {
    h: i64,
    v: i64,
    width: i64,
    height: i64,
    depth: i64,
}

struct RawRecord {
    line: usize,
    h: i64,
    line_box: usize,
}

#[derive(Default)]
struct RawPage {
    boxes: Vec<RawBox>,
    records: Vec<RawRecord>,
}

impl SyncIndex {
    pub fn from_gzip(bytes: &[u8]) -> Result<Self, SyncError> {
        let mut text = Vec::new();
        GzDecoder::new(bytes)
            .read_to_end(&mut text)
            .map_err(|error| SyncError::Decompress(error.to_string()))?;
        Self::parse(&String::from_utf8_lossy(&text))
    }

    /// Parse the complete line vocabulary XeTeX's `xetex-synctex.c` emits. Any other line, or a
    /// record whose fields do not parse, makes the whole index untrustworthy.
    pub fn parse(text: &str) -> Result<Self, SyncError> {
        const IGNORED_HEADERS: [&str; 9] = [
            "SyncTeX Version:",
            "Input:",
            "Output:",
            "X Offset:",
            "Y Offset:",
            "Content:",
            "Postamble:",
            "Count:",
            "Post scriptum:",
        ];

        let mut magnification = 1000.0;
        let mut unit = 1.0;
        let mut pages: Vec<RawPage> = Vec::new();
        let mut current: Option<usize> = None;
        let mut stack: Vec<Option<usize>> = Vec::new();
        let mut form_depth = 0usize;

        for (index, line) in text.lines().enumerate() {
            let malformed = SyncError::Malformed { line: index + 1 };
            if let Some(value) = line.strip_prefix("Magnification:") {
                magnification = positive(value).ok_or(malformed)?.unwrap_or(magnification);
                continue;
            }
            if let Some(value) = line.strip_prefix("Unit:") {
                unit = positive(value).ok_or(malformed)?.unwrap_or(unit);
                continue;
            }
            if IGNORED_HEADERS
                .iter()
                .any(|header| line.starts_with(header))
            {
                continue;
            }

            let mut characters = line.chars();
            let Some(kind) = characters.next() else {
                return Err(malformed);
            };
            let rest = characters.as_str();
            match kind {
                '!' | 'f' | 'x' | 'h' | 'v' => {}
                '<' => form_depth += 1,
                '>' => form_depth = form_depth.checked_sub(1).ok_or(malformed)?,
                _ if form_depth > 0 => {}
                '{' => {
                    let sheet: usize = rest.parse().map_err(|_| malformed)?;
                    let page = sheet
                        .checked_sub(1)
                        .ok_or(SyncError::Malformed { line: index + 1 })?;
                    if pages.len() <= page {
                        pages.resize_with(page + 1, RawPage::default);
                    }
                    current = Some(page);
                    stack.clear();
                }
                '}' => {
                    rest.parse::<usize>().map_err(|_| malformed)?;
                    current = None;
                    stack.clear();
                }
                '[' => {
                    current.ok_or(malformed)?;
                    stack.push(None);
                }
                '(' => {
                    let page = current.ok_or(SyncError::Malformed { line: index + 1 })?;
                    let fields = RecordFields::parse(rest).ok_or(malformed)?;
                    if stack.iter().any(Option::is_some) {
                        stack.push(None);
                    } else {
                        let [width, height, depth] = fields
                            .dims::<3>()
                            .ok_or(SyncError::Malformed { line: index + 1 })?;
                        let boxes = &mut pages[page].boxes;
                        boxes.push(RawBox {
                            h: fields.h,
                            v: fields.v,
                            width,
                            height,
                            depth,
                        });
                        stack.push(Some(boxes.len() - 1));
                    }
                }
                ']' | ')' => {
                    stack.pop().ok_or(malformed)?;
                }
                'g' | 'k' | '$' | 'r' => {
                    let page = current.ok_or(SyncError::Malformed { line: index + 1 })?;
                    let fields = RecordFields::parse(rest).ok_or(malformed)?;
                    let line_box = stack.iter().find_map(|level| *level);
                    if let (PRIMARY_TAG, Some(line_box)) = (fields.tag, line_box) {
                        pages[page].records.push(RawRecord {
                            line: fields.line,
                            h: fields.h,
                            line_box,
                        });
                    }
                }
                _ => return Err(malformed),
            }
        }

        let points = |raw: i64| (raw as f64 * unit * magnification / 1000.0 / SP_PER_POINT) as f32;
        let pages = pages
            .into_iter()
            .map(|page| SyncPage {
                boxes: page
                    .boxes
                    .iter()
                    .map(|raw| LineBox {
                        h: points(raw.h),
                        v: points(raw.v),
                        width: points(raw.width),
                        height: points(raw.height),
                        depth: points(raw.depth),
                    })
                    .collect(),
                records: page
                    .records
                    .iter()
                    .map(|raw| SyncRecord {
                        line: raw.line,
                        h: points(raw.h),
                        line_box: raw.line_box,
                    })
                    .collect(),
            })
            .collect();
        Ok(Self { pages })
    }

    pub fn page_count(&self) -> usize {
        self.pages.len()
    }

    /// The typeset extent of source line `line`: the union of the line boxes holding its records
    /// on the page where the line starts.
    pub fn forward(&self, line: usize) -> Option<SyncTarget> {
        let (page_index, page) = self
            .pages
            .iter()
            .enumerate()
            .find(|(_, page)| page.records.iter().any(|record| record.line == line))?;

        let mut boxes = page
            .records
            .iter()
            .filter(|record| record.line == line)
            .map(|record| record.line_box)
            .collect::<Vec<_>>();
        boxes.sort_unstable();
        boxes.dedup();

        let top = boxes
            .iter()
            .map(|index| page.boxes[*index].top())
            .fold(f32::INFINITY, f32::min);
        let bottom = boxes
            .iter()
            .map(|index| page.boxes[*index].bottom())
            .fold(f32::NEG_INFINITY, f32::max);
        Some(SyncTarget {
            page: page_index,
            top,
            bottom,
        })
    }

    /// The source line typeset at `(x, y)` (PDF points from the page's top-left corner).
    ///
    /// The line box containing the point, or else the nearest one, is searched for the first
    /// content record at or right of `x`: characters carry no records of their own, and the next
    /// own-stamped record (a trailing space, the line anchor, a math-off node) was created while
    /// reading the same source line.
    pub fn backward(
        &self,
        page: usize,
        x: f32,
        y: f32,
        is_content: impl Fn(usize) -> bool,
    ) -> Option<usize> {
        let page = self.pages.get(page)?;
        let target = page
            .boxes
            .iter()
            .position(|line_box| line_box.distance(x, y) == 0.0)
            .or_else(|| {
                let mut nearest: Option<(usize, f32)> = None;
                for (index, line_box) in page.boxes.iter().enumerate() {
                    let distance = line_box.distance(x, y);
                    if nearest.is_none_or(|(_, best)| distance < best) {
                        nearest = Some((index, distance));
                    }
                }
                nearest.map(|(index, _)| index)
            })?;

        let candidates = page
            .records
            .iter()
            .filter(|record| record.line_box == target && is_content(record.line))
            .collect::<Vec<_>>();
        candidates
            .iter()
            .find(|record| record.h >= x)
            .or(candidates.last())
            .map(|record| record.line)
    }
}

/// `Some(Some(value))` for a positive number, `Some(None)` for a non-positive one (keep the
/// default), `None` when the field is not a number.
fn positive(value: &str) -> Option<Option<f64>> {
    let value: f64 = value.trim().parse().ok()?;
    Some((value > 0.0).then_some(value))
}

/// `tag,line:h,v` followed by optional `:dims`.
struct RecordFields {
    tag: u32,
    line: usize,
    h: i64,
    v: i64,
    dims: Vec<i64>,
}

impl RecordFields {
    fn parse(text: &str) -> Option<Self> {
        let mut parts = text.split(':');
        let (tag, line) = parts.next()?.split_once(',')?;
        let (h, v) = parts.next()?.split_once(',')?;
        let dims = match parts.next() {
            Some(dims) => dims
                .split(',')
                .map(|value| value.parse().ok())
                .collect::<Option<Vec<_>>>()?,
            None => Vec::new(),
        };
        if parts.next().is_some() {
            return None;
        }
        Some(Self {
            tag: tag.parse().ok()?,
            line: line.parse().ok()?,
            h: h.parse().ok()?,
            v: v.parse().ok()?,
            dims,
        })
    }

    fn dims<const N: usize>(&self) -> Option<[i64; N]> {
        self.dims.as_slice().try_into().ok()
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use flate2::Compression;
    use flate2::write::GzEncoder;

    use super::*;

    const FIXTURE: &str = "SyncTeX Version:1
Input:1:texput
Input:2:/bundle/article.cls
Output:pdf
Magnification:1000
Unit:1
X Offset:0
Y Offset:0
Content:
!120
{1
[1,1:4736287,4736287:25000000,40000000,0
(1,30:4736287,13156352:25000000,655360,196608
g1,12:9000000,13156352
x1,12:9500000,13156352
g2,12:9900000,13156352
g1,13:13156352,13156352
)
(1,30:4736287,26312704:25000000,655360,196608
$1,14:6578176,26312704
k1,14:7000000,26312704:65536
)
(1,99:4736287,52625408:25000000,655360,196608
g1,40:20000000,52625408
)
]
}1
{2
[1,1:4736287,4736287:25000000,40000000,0
(1,31:4736287,6578176:25000000,655360,196608
g1,20:6578176,6578176
)
]
}2
Postamble:
Count:9
Post scriptum:
";

    fn pt(raw: i64) -> f32 {
        (raw as f64 / SP_PER_POINT) as f32
    }

    fn band(v: i64) -> (f32, f32) {
        (pt(v) - pt(655360), pt(v) + pt(196608))
    }

    fn index() -> SyncIndex {
        SyncIndex::parse(FIXTURE).expect("fixture parses")
    }

    #[test]
    fn forward_spans_the_line_boxes_of_own_stamped_records() {
        let index = index();
        assert_eq!(index.page_count(), 2);

        let (top, bottom) = band(13156352);
        assert_eq!(
            index.forward(12),
            Some(SyncTarget {
                page: 0,
                top,
                bottom
            })
        );
        let (top, bottom) = band(26312704);
        assert_eq!(
            index.forward(14),
            Some(SyncTarget {
                page: 0,
                top,
                bottom
            })
        );
        assert_eq!(index.forward(30), None);
        assert_eq!(index.forward(20).map(|target| target.page), Some(1));
    }

    #[test]
    fn backward_picks_the_first_content_record_at_or_right_of_the_click() {
        let index = index();
        let all = |_| true;
        let baseline = pt(13156352);

        assert_eq!(
            index.backward(0, pt(9_000_000) - 1.0, baseline, all),
            Some(12)
        );
        assert_eq!(index.backward(0, pt(10_000_000), baseline, all), Some(13));
        assert_eq!(
            index.backward(0, pt(9_000_000), baseline, |line| line != 12),
            Some(13)
        );
        assert_eq!(index.backward(0, pt(6578176), pt(20000000), all), Some(14));
        assert_eq!(
            index.backward(0, pt(20000000), pt(52625408), |line| line != 40),
            None
        );
        assert_eq!(index.backward(5, 0.0, 0.0, all), None);
    }

    #[test]
    fn malformed_data_is_rejected() {
        assert!(matches!(
            SyncIndex::parse("{1\nq1,2:3,4\n}1\n"),
            Err(SyncError::Malformed { line: 2 })
        ));
        assert!(matches!(
            SyncIndex::parse("{1\n)\n"),
            Err(SyncError::Malformed { line: 2 })
        ));
    }

    #[test]
    fn gzip_data_parses_like_plain_text() {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(FIXTURE.as_bytes()).expect("gzip write");
        let bytes = encoder.finish().expect("gzip finish");

        assert_eq!(SyncIndex::from_gzip(&bytes).expect("gzip parses"), index());
    }
}
