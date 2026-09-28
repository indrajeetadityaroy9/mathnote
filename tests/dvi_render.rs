//! The XDV renderer against real Tectonic output: page structure, pixel agreement with the PDF
//! Tectonic makes from the same source (rendered by Hayro), incremental parsing and page-size
//! specials.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use hayro::hayro_syntax::Pdf;
use hayro::vello_cpu::color::palette::css::WHITE;
use hayro::{RenderCache, RenderSettings, hayro_interpret::InterpreterSettings, render};
use image::RgbaImage;
use mathnote::cache::{default_cache_dir, open_bundle};
use mathnote::document::Document;
use mathnote::dvi::{BundleFontFiles, FontStore, XdvDocument, render_page};
use mathnote::latex::emit_latex;
use tectonic::driver::{OutputFormat, ProcessingSessionBuilder};
use tectonic::status::NoopStatusBackend;
use tectonic_bundles::Bundle;

const A4_BP: (f32, f32) = (595.2756, 841.8898);
const WIDTH_PX: u32 = 1200;

/// Prose with inline math: powers, a root and a fraction, then a display integral.
const NOTE: &str = "Proof: if $x squared plus y squared equals r squared$, then $root of 81$ equals 9 and \
$a over b$ is a ratio.\n\n$$\nintegral of x\n$$\n";

/// Display mathematics the note grammar has no words for (sums, subscripts, limits). It is
/// inserted into the generated template as raw LaTeX, so the fonts and geometry stay mathnote's.
/// Script sizes only: the cache holds no scriptscript (6 pt) Type1 files.
const DISPLAY_MATH: &str = r"\[ \sum_{i=1}^{n} i^{2} = \frac{n(n+1)(2n+1)}{6} \]
\[ \int_{0}^{1} x_{k}\,dx + \sqrt{a_{1} + b^{2}} \]
";

fn template(note: &str, extra: &str) -> String {
    let latex = emit_latex(&Document::parse(note).expect("note parses"));
    latex
        .source()
        .replace(r"\end{document}", &format!("{extra}\\end{{document}}"))
}

fn two_page_source() -> String {
    let paragraph = "The quick brown fox jumps over the lazy dog while the committee reviews \
                     every clause of the proposal, and nobody objects to the revised schedule.";
    let prose = (0..28)
        .map(|index| format!("Paragraph {index}. {paragraph} {paragraph}\n"))
        .collect::<Vec<_>>()
        .join("\n");
    template(NOTE, &format!("{DISPLAY_MATH}\n{prose}"))
}

/// Mathematics only: if TFM glyphs were missing, only fraction bars and root rules would remain.
fn math_only_source() -> String {
    let body = r"\[ \sum_{i=1}^{n} \frac{a_{i}}{b_{i}} = \int_{0}^{1} f(x)\,dx \]
\[ \sqrt{x^{2} + y^{2}} \le \sum_{k=0}^{m} {m \choose k} \]
\[ \frac{\partial u}{\partial t} = \alpha \nabla u + \beta_{0} \]
";
    let latex = emit_latex(&Document::parse("").expect("empty note"));
    latex
        .source()
        .replace(r"\end{document}", &format!("{body}\\end{{document}}"))
}

fn open_cached_bundle() -> Box<dyn Bundle> {
    open_bundle(&default_cache_dir(), true).expect("verified cached bundle")
}

/// The reference: Hayro's rendering of the PDF Tectonic makes from the same source, flattened
/// over white. Returns the page count and page `index` at `width` pixels.
fn hayro_page(pdf: Vec<u8>, index: usize, width: u32) -> (usize, RgbaImage) {
    let pdf = Pdf::new(Arc::new(pdf)).expect("Hayro parses the PDF");
    let pages = pdf.pages();
    let page = &pages[index];
    let (native_width, native_height) = page.render_dimensions();
    let scale = width as f32 / native_width;
    let height = (native_height * scale).round() as u16;
    let pixmap = render(
        page,
        &RenderCache::new(),
        &InterpreterSettings::default(),
        &RenderSettings {
            x_scale: scale,
            y_scale: scale,
            width: Some(width as u16),
            height: Some(height),
            bg_color: WHITE,
        },
    );
    let rgba = pixmap
        .data_as_u8_slice()
        .as_chunks::<4>()
        .0
        .iter()
        .flat_map(|pixel| {
            let uncovered = 255 - u16::from(pixel[3]);
            let over_white = |channel: u8| (u16::from(channel) + uncovered).min(255) as u8;
            [
                over_white(pixel[0]),
                over_white(pixel[1]),
                over_white(pixel[2]),
                255,
            ]
        })
        .collect();
    let image = RgbaImage::from_raw(u32::from(pixmap.width()), u32::from(pixmap.height()), rgba)
        .expect("Hayro pixmap size");
    (pages.len(), image)
}

fn run(source: &str, format: OutputFormat) -> HashMap<String, Vec<u8>> {
    let cache = default_cache_dir();
    let bundle = open_cached_bundle();
    let mut status = NoopStatusBackend::default();
    let mut builder = ProcessingSessionBuilder::default();
    builder
        .bundle(bundle)
        .primary_input_buffer(source.as_bytes())
        .tex_input_name("doc.tex")
        .filesystem_root(cache.join("sandbox"))
        .format_name("latex")
        .format_cache_path(cache.join("formats"))
        .keep_intermediates(true)
        .keep_logs(false)
        .print_stdout(false)
        .output_format(format)
        .do_not_write_output_files();
    let mut session = builder.create(&mut status).expect("session");
    session
        .run(&mut status)
        .expect("Tectonic compiles the document");
    session
        .into_file_data()
        .into_iter()
        .map(|(name, file)| (name, file.data))
        .collect()
}

fn xdv(source: &str) -> Vec<u8> {
    run(source, OutputFormat::Xdv)
        .remove("doc.xdv")
        .expect("Tectonic keeps the XDV")
}

fn pdf(source: &str) -> Vec<u8> {
    run(source, OutputFormat::Pdf)
        .remove("doc.pdf")
        .expect("Tectonic writes the PDF")
}

fn parse(data: &[u8]) -> XdvDocument {
    let mut document = XdvDocument::new();
    document.update(data, None);
    document
}

fn fonts() -> FontStore {
    FontStore::new(Box::new(
        BundleFontFiles::open(&default_cache_dir()).expect("bundle opens"),
    ))
}

fn fingerprints(document: &XdvDocument) -> Vec<u64> {
    (0..document.page_count())
        .map(|index| document.page(index).unwrap().fingerprint)
        .collect()
}

/// Byte offset one past each page's `eop`, recovered independently of the parser from the DVI
/// back-pointers: `post` points at the last `bop`, and every `bop` at the previous one.
fn page_ends(data: &[u8]) -> Vec<usize> {
    let be = |at: usize| u32::from_be_bytes(data[at..at + 4].try_into().unwrap()) as usize;
    let mut tail = data.len() - 1;
    while data[tail] == 223 {
        tail -= 1;
    }
    // post_post: 249, q[4], id; q is the offset of post.
    let post = be(tail - 4);
    assert_eq!(data[post], 248);
    let mut bops = Vec::new();
    let mut bop = be(post + 1);
    while bop != u32::MAX as usize {
        assert_eq!(data[bop], 139);
        bops.push(bop);
        bop = be(bop + 41);
    }
    bops.reverse();
    let mut ends: Vec<usize> = bops[1..].to_vec();
    ends.push(post);
    ends
}

fn gray(image: &RgbaImage, x: u32, y: u32) -> f64 {
    let [r, g, b, _] = image.get_pixel(x, y).0;
    (f64::from(r) + f64::from(g) + f64::from(b)) / 3.0
}

/// Smallest rectangle `(left, top, right, bottom)` holding every pixel darker than 240.
fn ink_box(image: &RgbaImage) -> (u32, u32, u32, u32) {
    let mut bounds = (u32::MAX, u32::MAX, 0, 0);
    for (x, y, _) in image.enumerate_pixels() {
        if gray(image, x, y) < 240.0 {
            bounds = (
                bounds.0.min(x),
                bounds.1.min(y),
                bounds.2.max(x),
                bounds.3.max(y),
            );
        }
    }
    bounds
}

/// Ink (255 − gray) per row.
fn row_profile(image: &RgbaImage) -> Vec<f64> {
    (0..image.height())
        .map(|y| (0..image.width()).map(|x| 255.0 - gray(image, x, y)).sum())
        .collect()
}

fn correlation(a: &[f64], b: &[f64]) -> f64 {
    let n = a.len().min(b.len());
    let (a, b) = (&a[..n], &b[..n]);
    let mean = |v: &[f64]| v.iter().sum::<f64>() / n as f64;
    let (ma, mb) = (mean(a), mean(b));
    let cov: f64 = a.iter().zip(b).map(|(x, y)| (x - ma) * (y - mb)).sum();
    let var = |v: &[f64], m: f64| v.iter().map(|x| (x - m).powi(2)).sum::<f64>();
    cov / (var(a, ma) * var(b, mb)).sqrt()
}

/// Render page 0 of `source` both ways and compare. Agreement means:
/// * ink bounding boxes within 2 px on every side (positions and page geometry);
/// * row ink profiles correlate above 0.98 (every line is where the PDF has it);
/// * total ink within 12% (no glyph family is missing or drawn at the wrong size).
fn assert_page_zero_matches_pdf(label: &str, source: &str) {
    let (_, reference) = hayro_page(pdf(source), 0, WIDTH_PX);
    let document = parse(&xdv(source));
    let page = document.page(0).expect("page 0");
    let mut fonts = fonts();
    let scale = WIDTH_PX as f32 / page.width;
    let ours = render_page(page, &mut fonts, scale);
    assert_eq!(ours.width(), WIDTH_PX, "{label}: width");
    assert!(
        ours.height().abs_diff(reference.height()) <= 1,
        "{label}: height {} vs {}",
        ours.height(),
        reference.height()
    );

    let (a, b) = (ink_box(&ours), ink_box(&reference));
    let sides = [
        a.0.abs_diff(b.0),
        a.1.abs_diff(b.1),
        a.2.abs_diff(b.2),
        a.3.abs_diff(b.3),
    ];
    let rows = correlation(&row_profile(&ours), &row_profile(&reference));
    let ink = |image: &RgbaImage| row_profile(image).iter().sum::<f64>();
    let ratio = ink(&ours) / ink(&reference);
    println!("{label}: ink box {a:?} vs {b:?}, row correlation {rows:.4}, ink ratio {ratio:.4}");
    assert!(
        sides.iter().all(|&side| side <= 2),
        "{label}: ink boxes differ: {a:?} vs {b:?}"
    );
    assert!(rows > 0.98, "{label}: row profiles correlate {rows}");
    assert!(
        (0.88..=1.12).contains(&ratio),
        "{label}: ink ratio {ratio} (missing or mis-sized glyphs)"
    );
}

#[test]
fn page_structure_matches_the_pdf_of_the_same_source() {
    let source = two_page_source();
    let document = parse(&xdv(&source));
    let (pdf_pages, _) = hayro_page(pdf(&source), 0, 16);

    assert!(pdf_pages >= 2, "the document spans {pdf_pages} page(s)");
    assert_eq!(document.page_count(), pdf_pages);
    assert!(document.is_complete());
    for index in 0..document.page_count() {
        let page = document.page(index).unwrap();
        assert!((page.width - A4_BP.0).abs() < 0.01, "width {}", page.width);
        assert!(
            (page.height - A4_BP.1).abs() < 0.01,
            "height {}",
            page.height
        );
    }
    let prints = fingerprints(&document);
    assert_ne!(prints[0], prints[1], "different pages differ");
}

#[test]
fn prose_and_math_page_agrees_with_the_pdf() {
    assert_page_zero_matches_pdf("prose+math", &two_page_source());
}

#[test]
fn math_only_page_agrees_with_the_pdf() {
    assert_page_zero_matches_pdf("math only", &math_only_source());
}

#[test]
fn growing_prefixes_yield_only_complete_pages_and_truncation_rewinds() {
    let data = xdv(&two_page_source());
    let ends = page_ends(&data);
    assert!(ends.len() >= 2);
    let complete = parse(&data);
    let expected = fingerprints(&complete);
    assert_eq!(expected.len(), ends.len());

    let mut document = XdvDocument::new();
    let mut length = 0;
    while length < data.len() {
        length = (length + 97).min(data.len());
        document.update(&data[..length], None);
        let finished = ends.iter().filter(|&&end| end <= length).count();
        assert_eq!(document.page_count(), finished, "prefix of {length} bytes");
        assert_eq!(fingerprints(&document), expected[..finished]);
        // Complete once the whole 29-byte `post` instruction has arrived.
        let post = *ends.last().unwrap();
        assert_eq!(document.is_complete(), length >= post + 29);
    }
    assert_eq!(fingerprints(&document), expected);

    // Cut inside the second page: only the first survives, and the postamble is gone.
    let cut = ends[0] + (ends[1] - ends[0]) / 2;
    document.update(&data[..cut], Some(cut));
    assert_eq!(fingerprints(&document), expected[..1]);
    assert!(!document.is_complete());
    document.update(&data, None);
    assert_eq!(fingerprints(&document), expected);
    assert!(document.is_complete());

    // Cut inside the first page, then regrow in one step.
    document.update(&data, Some(ends[0] - 1));
    assert_eq!(fingerprints(&document), expected);
    document.update(&data[..ends[0] - 1], Some(ends[0] - 1));
    assert_eq!(document.page_count(), 0);

    // A rewrite that shortens the output without reporting it is still noticed.
    document.update(&data, None);
    document.update(&data[..ends[0]], None);
    assert_eq!(fingerprints(&document), expected[..1]);
}

#[test]
fn papersize_special_sets_the_page_size_in_big_points() {
    let source = template("Small page.", "\\special{papersize=100pt,200pt}\n");
    let document = parse(&xdv(&source));
    let page = document.page(0).expect("page 0");
    assert!(
        (page.width - 100.0 * 72.0 / 72.27).abs() < 1e-3,
        "{}",
        page.width
    );
    assert!(
        (page.height - 200.0 * 72.0 / 72.27).abs() < 1e-3,
        "{}",
        page.height
    );

    let image = render_page(page, &mut fonts(), 2.0);
    assert_eq!(
        (image.width(), image.height()),
        (
            (page.width * 2.0).ceil() as u32,
            (page.height * 2.0).ceil() as u32
        )
    );
}

#[test]
fn render_time_of_a_text_and_math_page() {
    let document = parse(&xdv(&two_page_source()));
    let page = document.page(0).unwrap();
    let mut fonts = fonts();
    let scale = WIDTH_PX as f32 / page.width;
    let cold = Instant::now();
    render_page(page, &mut fonts, scale);
    let cold = cold.elapsed();
    let runs = 10;
    let warm = Instant::now();
    for _ in 0..runs {
        render_page(page, &mut fonts, scale);
    }
    let warm = warm.elapsed() / runs;
    println!("page 0 at {WIDTH_PX} px: first render {cold:?}, then {warm:?} per render");
}
