//! Latency and bandwidth of the typing path: `cargo bench --bench latency`.
//!
//! Types a note one character at a time through the snapshot driver, renders the typeset page,
//! and installs each new page into a Kitty-protocol strip preview, then reports:
//! - typeset time per one-character edit (median, p90, max);
//! - page render time (first render, then median);
//! - UI-thread time to install a new page into the preview, and the time until its changed
//!   strips are encoded;
//! - changed strips per edit and the Kitty bytes they send.
//!
//! Needs the verified resource cache (filled online on first use).

use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use image::RgbaImage;
use mathnote::cache::{default_cache_dir, ensure_cache_warm};
use mathnote::document::Document;
use mathnote::driver::{DriverConfig, EngineState, TexDriver};
use mathnote::dvi::{BundleFontFiles, FontStore, XdvDocument, render_page};
use mathnote::latex::{BODY_SNAPSHOT_OFFSET, TEMPLATE_MATH_FONTS, emit_latex};
use mathnote::strips::{Canvas, EncodedStrips, StripPreview};
use ratatui_image::picker::{Picker, ProtocolType};

const NOTE: &str = "Theorem: if $x squared equals 81$ then $root of 81$ is 9.\n\n\
Proof: $root of 81$ equals 9 because $9 times 9$ equals 81, and $x over 2$ is $x$ halved.\n";

/// A retina preview pane: 128 × 88 cells of 10 × 20 pixels (the half-block picker's cell size).
const CELL_HEIGHT: u32 = 20;
const PANE_ROWS: u32 = 88;
const PAGE_WIDTH_PX: u32 = 1280;

fn main() {
    let cache_dir = default_cache_dir();
    ensure_cache_warm(&cache_dir).expect("warm Tectonic resource cache");

    let pages = typeset_while_typing(&cache_dir);
    let images = render_pages(&cache_dir, &pages);
    preview_updates(&images);
}

/// Every revision's XDV, typing `NOTE` one character at a time; prints typeset times.
fn typeset_while_typing(cache_dir: &std::path::Path) -> Vec<Vec<u8>> {
    let mut driver = TexDriver::new(DriverConfig {
        engine_exe: env!("CARGO_BIN_EXE_mathnote").into(),
        cache_dir: cache_dir.to_path_buf(),
    });
    driver.set_root_snapshot(BODY_SNAPSHOT_OFFSET);
    driver.set_document(source("").as_bytes());
    run_to_end(&mut driver);

    let mut times = Vec::new();
    let mut revisions = Vec::new();
    let mut typed = String::new();
    for character in NOTE.chars() {
        typed.push(character);
        // Like the editor, skip revisions that do not parse (an unclosed `$`).
        let Ok(document) = Document::parse(&typed) else {
            continue;
        };
        let started = Instant::now();
        driver.set_document(emit_latex(&document).source().as_bytes());
        run_to_end(&mut driver);
        times.push(started.elapsed());
        revisions.push(driver.xdv().to_vec());
    }
    report("typeset per one-character edit", &mut times);
    println!(
        "  {} edits, {} engine roots, {} snapshots",
        times.len(),
        driver.stats().roots_spawned,
        driver.stats().forks
    );
    revisions
}

/// Page 0 of every revision at the preview width; prints render times.
fn render_pages(cache_dir: &std::path::Path, revisions: &[Vec<u8>]) -> Vec<Arc<RgbaImage>> {
    let mut fonts = FontStore::new(BundleFontFiles::open(cache_dir).expect("cached bundle opens"));
    let mut images = Vec::new();
    let mut times = Vec::new();
    for xdv in revisions {
        let mut document = XdvDocument::new();
        document.update(xdv, None);
        let page = document.page(0).expect("page 0");
        let started = Instant::now();
        images.push(Arc::new(render_page(
            page,
            &mut fonts,
            PAGE_WIDTH_PX as f32 / page.width,
        )));
        times.push(started.elapsed());
    }
    println!(
        "page render at {PAGE_WIDTH_PX} px: first {:.2} ms (loads fonts)",
        ms(times[0])
    );
    report("page render after the first", &mut times[1..]);
    fonts.preload(TEMPLATE_MATH_FONTS);
    let warnings = fonts.take_warnings();
    assert!(warnings.is_empty(), "renderer warnings: {warnings:?}");
    images
}

/// Install each revision's page into a Kitty strip preview, as the editor does after each edit.
fn preview_updates(images: &[Arc<RgbaImage>]) {
    let mut picker = Picker::halfblocks();
    picker.set_protocol_type(ProtocolType::Kitty);
    let (deliver, encoded) = mpsc::channel::<EncodedStrips>();
    let mut preview = StripPreview::new(picker, move |strips| deliver.send(strips).is_ok());

    let mut install = Vec::new();
    let mut to_screen = Vec::new();
    let mut changed = Vec::new();
    let mut bytes = Vec::new();
    for (key, image) in images.iter().enumerate() {
        let started = Instant::now();
        preview.install(
            key,
            Canvas {
                page: Arc::clone(image),
                top: 0,
                height: PANE_ROWS * CELL_HEIGHT,
                highlight: None,
                tint: None,
                max_width: PAGE_WIDTH_PX,
            },
        );
        install.push(started.elapsed());
        let strips = encoded
            .recv_timeout(Duration::from_secs(10))
            .expect("strips encoded");
        to_screen.push(started.elapsed());
        if key > 0 {
            changed.push(strips.changed_strips());
            bytes.push(strips.kitty_bytes());
        }
        preview.accept(strips).expect("strips encode");
        assert!(!preview.is_staging());
    }
    report("UI-thread preview install", &mut install[1..]);
    report("install until strips are encoded", &mut to_screen[1..]);

    let edits = changed.len().max(1);
    let total_rows: usize = changed.iter().sum();
    let total_bytes: usize = bytes.iter().sum();
    changed.sort_unstable();
    println!(
        "changed strips per edit: median {}, max {}; Kitty bytes per edit {:.0}, per changed strip {:.0}",
        changed.get(changed.len() / 2).copied().unwrap_or(0),
        changed.last().copied().unwrap_or(0),
        total_bytes as f64 / edits as f64,
        total_bytes as f64 / total_rows.max(1) as f64
    );
}

fn source(note: &str) -> String {
    emit_latex(&Document::parse(note).expect("note parses"))
        .source()
        .to_owned()
}

fn run_to_end(driver: &mut TexDriver) {
    let started = Instant::now();
    while driver.state() == EngineState::Running {
        assert!(started.elapsed() < Duration::from_secs(60), "engine stuck");
        driver.step(Duration::from_millis(5));
    }
    assert_eq!(driver.state(), EngineState::Finished);
}

fn report(label: &str, times: &mut [Duration]) {
    times.sort_unstable();
    let at = |fraction: f64| times[((times.len() - 1) as f64 * fraction).round() as usize];
    println!(
        "{label}: median {:.3} ms, p90 {:.3} ms, max {:.3} ms ({} samples)",
        ms(at(0.5)),
        ms(at(0.9)),
        ms(at(1.0)),
        times.len()
    );
}

fn ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}
