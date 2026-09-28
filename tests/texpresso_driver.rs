use std::time::{Duration, Instant};

use mathnote::cache::{default_cache_dir, ensure_cache_warm};
use mathnote::document::Document;
use mathnote::driver::{DriverConfig, EngineState, TexDriver};
use mathnote::latex::{LatexDocument, emit_latex};
use mathnote::synctex::SyncIndex;

fn driver() -> TexDriver {
    let cache_dir = default_cache_dir();
    ensure_cache_warm(&cache_dir).expect("warm Tectonic cache");
    TexDriver::new(DriverConfig {
        engine_exe: env!("CARGO_BIN_EXE_mathnote").into(),
        cache_dir,
    })
}

fn latex(note: &str) -> LatexDocument {
    emit_latex(&Document::parse(note).expect("note parses"))
}

/// Step until the engine finishes the current document; returns the time it took.
fn run_to_end(driver: &mut TexDriver) -> Duration {
    let started = Instant::now();
    while driver.state() == EngineState::Running {
        assert!(
            started.elapsed() < Duration::from_secs(120),
            "engine did not finish"
        );
        driver.step(Duration::from_millis(20));
    }
    assert_eq!(driver.state(), EngineState::Finished);
    started.elapsed()
}

fn cold_xdv(source: &str) -> (Vec<u8>, Duration) {
    let mut driver = driver();
    driver.set_document(source.as_bytes());
    let elapsed = run_to_end(&mut driver);
    (driver.xdv().to_vec(), elapsed)
}

fn long_note(last: &str) -> String {
    let mut lines: Vec<String> = (0..39)
        .map(|i| format!("line{i} alpha beta gamma delta epsilon"))
        .collect();
    lines.push(last.to_owned());
    lines.join("\n")
}

fn assert_complete_xdv(xdv: &[u8]) {
    assert_eq!(xdv.first(), Some(&247), "XDV starts with pre");
    assert!(
        xdv.ends_with(&[223, 223, 223, 223]),
        "XDV ends with post_post padding"
    );
}

#[test]
fn cold_compile_produces_complete_xdv_and_synctex() {
    let mut driver = driver();
    assert_eq!(driver.state(), EngineState::Idle);
    driver.set_document(latex("alpha\nbeta $x squared$\n").source().as_bytes());
    assert_eq!(driver.state(), EngineState::Running);
    run_to_end(&mut driver);

    assert_complete_xdv(driver.xdv());
    let synctex = driver.synctex_gz().expect("synctex closed");
    SyncIndex::from_gzip(synctex).expect("synctex parses");
    let errors: Vec<_> = driver.messages().into_iter().filter(|m| m.error).collect();
    assert!(errors.is_empty(), "unexpected errors: {errors:?}");
    assert_eq!(driver.stats().roots_spawned, 1);
    assert_eq!(driver.stats().live_processes, 0);
}

#[test]
fn resumed_edits_match_a_cold_compile_without_respawning() {
    let mut driver = driver();
    driver.set_document(latex(&long_note("last line here")).source().as_bytes());
    let cold = run_to_end(&mut driver);

    // First edit: rolls back to the start (no snapshot yet) and places fences before the change.
    driver.set_document(latex(&long_note("last line hereX")).source().as_bytes());
    assert_eq!(driver.state(), EngineState::Running);
    let first = run_to_end(&mut driver);
    let roots = driver.stats().roots_spawned;
    assert!(driver.stats().forks > 0, "fences produced snapshots");

    // Second edit near the same place resumes from a snapshot.
    let final_source = latex(&long_note("last line hereXY")).source().to_owned();
    driver.set_document(final_source.as_bytes());
    assert_eq!(driver.state(), EngineState::Running);
    let resumed = run_to_end(&mut driver);
    assert_eq!(
        driver.stats().roots_spawned,
        roots,
        "resumed without a new root"
    );
    assert!(driver.stats().rollbacks >= 2);
    assert!(driver.take_xdv_truncation().is_some());

    let (fresh, fresh_elapsed) = cold_xdv(&final_source);
    assert_complete_xdv(driver.xdv());
    assert!(
        driver.xdv() == fresh.as_slice(),
        "resumed XDV equals a cold compile"
    );
    eprintln!(
        "timings: cold {cold:?}, first edit {first:?}, resumed edit {resumed:?}, fresh cold {fresh_elapsed:?}"
    );
}

#[test]
fn tex_errors_are_reported_with_their_source_line_and_pages_still_ship() {
    let note = "alpha\n$x squared cubed$\n";
    let document = latex(note);
    let line = document
        .output_line_for_source_byte(note.find('$').unwrap())
        .expect("math line is content");
    let mut driver = driver();
    driver.set_document(document.source().as_bytes());
    run_to_end(&mut driver);

    let messages = driver.messages();
    assert!(
        messages.iter().any(|m| m.error && m.tex_line == Some(line)),
        "error on line {line}: {messages:?}"
    );
    assert_complete_xdv(driver.xdv());
}

#[test]
fn rapid_typing_converges_to_the_cold_output() {
    let base = long_note("typing: ");
    let mut driver = driver();
    driver.set_document(latex(&base).source().as_bytes());
    driver.step(Duration::from_millis(200));

    let mut note = base.clone();
    for character in "hello world".chars() {
        note.push(character);
        driver.set_document(latex(&note).source().as_bytes());
        assert_eq!(driver.state(), EngineState::Running);
        driver.step(Duration::from_millis(1));
    }
    run_to_end(&mut driver);

    let (fresh, _) = cold_xdv(latex(&note).source());
    assert_complete_xdv(driver.xdv());
    assert!(
        driver.xdv() == fresh.as_slice(),
        "converged XDV equals a cold compile"
    );
}

#[test]
fn a_new_document_after_finishing_runs_again_and_identical_bytes_are_a_no_op() {
    let mut driver = driver();
    let first = latex("alpha\n");
    driver.set_document(first.source().as_bytes());
    run_to_end(&mut driver);
    let first_xdv = driver.xdv().to_vec();

    driver.set_document(first.source().as_bytes());
    assert_eq!(driver.state(), EngineState::Finished);
    assert!(!driver.step(Duration::from_millis(5)));
    assert_eq!(driver.xdv(), first_xdv.as_slice());

    let second = latex("alpha\nbeta gamma\n");
    driver.set_document(second.source().as_bytes());
    assert_eq!(driver.state(), EngineState::Running);
    run_to_end(&mut driver);
    let (fresh, _) = cold_xdv(second.source());
    assert!(driver.xdv() == fresh.as_slice());
    assert!(driver.xdv() != first_xdv.as_slice());
}

/// Keep serving the engine for `duration` without waiting for it to finish.
fn step_for(driver: &mut TexDriver, duration: Duration) {
    let started = Instant::now();
    while let Some(left) = duration.checked_sub(started.elapsed()) {
        if left.is_zero() {
            break;
        }
        if driver.state() == EngineState::Running {
            driver.step(left);
        } else {
            std::thread::sleep(left.min(Duration::from_millis(1)));
        }
    }
}

#[test]
fn burst_typing_resumes_from_snapshots_instead_of_respawning() {
    let mut driver = driver();
    let mut note = String::from("Notes on typing\n");
    driver.set_document(latex(&note).source().as_bytes());
    run_to_end(&mut driver);
    let before = driver.stats();

    let typed = "the quick brown fox jumps over the lazy dog\nand then it keeps running far away";
    for character in typed.chars() {
        step_for(&mut driver, Duration::from_millis(25));
        note.push(character);
        driver.set_document(latex(&note).source().as_bytes());
    }
    let burst = driver.stats();
    let settle = run_to_end(&mut driver);
    let after = driver.stats();
    eprintln!(
        "burst: roots spawned {}, forks {}, rollbacks {}, finished {settle:?} after the last keystroke",
        after.roots_spawned - before.roots_spawned,
        burst.forks - before.forks,
        after.rollbacks - before.rollbacks,
    );

    assert!(
        after.roots_spawned - before.roots_spawned <= 2,
        "respawned {} roots",
        after.roots_spawned - before.roots_spawned
    );
    assert!(burst.forks > before.forks, "snapshots taken while typing");
    let (fresh, _) = cold_xdv(latex(&note).source());
    assert_complete_xdv(driver.xdv());
    assert!(
        driver.xdv() == fresh.as_slice(),
        "burst XDV equals a cold compile"
    );
}

#[test]
fn an_engine_stuck_in_a_loop_is_replaced_after_the_next_edit() {
    let fixed = latex("alpha beta\n").source().to_owned();
    let looping = fixed.replacen("alpha beta", r"alpha \def\loop{\loop}\loop beta", 1);
    assert_ne!(fixed, looping);

    let mut driver = driver();
    driver.set_document(looping.as_bytes());
    step_for(&mut driver, Duration::from_millis(1500));
    assert_eq!(driver.state(), EngineState::Running, "the loop never ends");

    driver.set_document(fixed.as_bytes());
    assert!(run_to_end(&mut driver) < Duration::from_secs(10));
    let (fresh, _) = cold_xdv(&fixed);
    assert_complete_xdv(driver.xdv());
    assert!(driver.xdv() == fresh.as_slice());
}
