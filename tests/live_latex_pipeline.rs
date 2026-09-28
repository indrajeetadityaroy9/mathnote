//! The live pipeline end to end: the snapshot engine typesets generated LaTeX, the XDV output
//! renders to pixels, and SyncTeX maps note lines to typeset lines and back.

use std::time::{Duration, Instant};

use mathnote::cache::{default_cache_dir, ensure_cache_warm};
use mathnote::document::Document;
use mathnote::driver::{DriverConfig, EngineState, TexDriver};
use mathnote::dvi::{BundleFontFiles, FontStore, XdvDocument, render_page};
use mathnote::latex::{LatexDocument, emit_latex};
use mathnote::synctex::SyncIndex;

const REPRESENTATIVE_NOTE: &str =
    "Proof: if $x squared equals 81$, then $root of 81$ equals 9.\n\n$$\nintegral of x\n$$\n";

struct Typeset {
    latex: LatexDocument,
    xdv: XdvDocument,
    sync: SyncIndex,
}

fn driver() -> TexDriver {
    let cache_dir = default_cache_dir();
    ensure_cache_warm(&cache_dir).expect("Tectonic cache is usable");
    TexDriver::new(DriverConfig {
        engine_exe: env!("CARGO_BIN_EXE_mathnote").into(),
        cache_dir,
    })
}

fn typeset(driver: &mut TexDriver, note: &str) -> Typeset {
    let latex = emit_latex(&Document::parse(note).expect("note parses"));
    driver.set_document(latex.source().as_bytes());
    let deadline = Instant::now() + Duration::from_secs(60);
    while driver.state() != EngineState::Finished {
        assert!(Instant::now() < deadline, "typesetting timed out");
        driver.step(Duration::from_millis(10));
    }
    let mut xdv = XdvDocument::new();
    let truncated = driver.take_xdv_truncation();
    xdv.update(driver.xdv(), truncated);
    let sync = SyncIndex::from_gzip(driver.synctex_gz().expect("the engine writes SyncTeX"))
        .expect("SyncTeX parses");
    Typeset { latex, xdv, sync }
}

#[test]
fn notes_typeset_to_rendered_pages() {
    let mut driver = driver();
    let blank = typeset(&mut driver, "");
    assert_eq!(blank.xdv.page_count(), 1);

    let note = typeset(&mut driver, REPRESENTATIVE_NOTE);
    assert!(note.xdv.is_complete());
    assert!(driver.messages().iter().all(|message| !message.error));
    let page = note.xdv.page(0).expect("first page");
    assert!(page.height > page.width, "A4 portrait");

    let mut fonts = FontStore::new(Box::new(
        BundleFontFiles::open(&default_cache_dir()).expect("bundle fonts"),
    ));
    let image = render_page(page, &mut fonts, 900.0 / page.width);
    assert_eq!(image.width(), 900);
    let ink = image
        .pixels()
        .filter(|pixel| pixel.0[0] < 128 && pixel.0[1] < 128 && pixel.0[2] < 128)
        .count();
    assert!(
        ink > 500,
        "text and mathematics are drawn ({ink} dark pixels)"
    );
}

#[test]
fn synctex_maps_single_word_lines_in_both_directions() {
    let mut driver = driver();
    let Typeset { latex, sync, .. } = typeset(&mut driver, "alpha\nbeta\n");

    let beta = latex
        .output_line_for_source_byte(6)
        .expect("beta is typeset");
    assert!(
        sync.forward(beta).is_some(),
        "the single-word last line is found through its anchor"
    );

    let alpha = latex
        .output_line_for_source_byte(0)
        .expect("alpha is typeset");
    let target = sync.forward(alpha).expect("alpha is found");
    let line = sync
        .backward(
            target.page,
            0.0,
            (target.top + target.bottom) / 2.0,
            |line| latex.is_content_line(line),
        )
        .expect("a click on the line resolves to a source line");
    assert_eq!(latex.source_byte_for_output_line(line), Some(0));
}

#[test]
fn display_math_and_wrapped_paragraphs_have_forward_targets() {
    let mut driver = driver();

    let Typeset { latex, sync, .. } = typeset(&mut driver, REPRESENTATIVE_NOTE);
    let display = REPRESENTATIVE_NOTE
        .find("integral")
        .expect("display source");
    let display_line = latex
        .output_line_for_source_byte(display)
        .expect("display content is typeset");
    assert!(sync.forward(display_line).is_some());

    let paragraph = (0..40)
        .map(|index| format!("line{index} alpha beta gamma delta"))
        .collect::<Vec<_>>()
        .join("\n");
    let Typeset { latex, sync, .. } = typeset(&mut driver, &paragraph);
    let first = latex
        .output_line_for_source_byte(0)
        .and_then(|line| sync.forward(line))
        .expect("first source line is typeset");
    let last = latex
        .output_line_for_source_byte(paragraph.rfind("line39").expect("last line"))
        .and_then(|line| sync.forward(line))
        .expect("last source line is typeset");
    assert!(last.page > first.page || last.top > first.top);
}
