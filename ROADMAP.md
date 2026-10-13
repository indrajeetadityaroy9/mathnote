# mathnote roadmap

Prioritized work for the Rust live-typesetting pipeline: build speed and size, typing latency,
documentation, code structure, and feature gaps.

Priorities:

| Priority | Meaning |
|---|---|
| P0 | Cheap, measured, high impact: do first |
| P1 | Latency on the typing path |
| P2 | Correctness, documentation, hygiene |
| P3 | Deeper build cleanup and structure |
| P4 | Feature gaps that only matter if the LaTeX template grows |

Suggested order: P0.1–P0.3 in one commit (only `Cargo.toml` changes, already measured), then
P2.1–P2.5, then P1.1–P1.3, then P3, then P4 as needed. P0 to P3 are done.

## Baseline

Measured on an Apple Silicon Mac (11 cores, macOS), Rust 1.98.1, release profile with no
`[profile.release]` settings, warm Tectonic resource cache.

| Metric | Value | How it was measured |
|---|---|---|
| Clean release build, wall clock | 113 s | `cargo build --release --timings` in a fresh target directory |
| Clean release build, CPU time | 748 s | same build, `user` time |
| Binary size | 24.7 MB | `target/release/mathnote` |
| Typeset after one keystroke (steady state) | 3–9 ms median | driver-only probe typing a note one character at a time |
| First edit after launch | 160–300 ms | same probe, first keystroke after the startup run |
| Page render at 1200 px | ~5.9 ms per render, ~50 ms the first time | `cargo test --release --test dvi_render render_time -- --nocapture` |
| Preview image preparation on the UI thread | ~6 ms median at 1280×1760 px | standalone replica of the crop, tint, row split and row compare |
| Terminal output per keystroke (Kitty protocol) | ~4 KB, ~2.7 KB of it image data | pseudo-terminal harness answering like a Kitty-protocol terminal |
| Keystroke to redrawn screen | ~1.1 ms median | same harness |

Slowest build units in the baseline (CPU seconds, from the timings report):

| Unit | Seconds | Pulled in by |
|---|---|---|
| `aws-lc-sys` build script | 89.6 | reqwest → rustls (bundle download) |
| `ravif` | 39.2 | `image` default features, enabled by `ratatui-image`'s `image-defaults` |
| `tera` | 35.8 | `tectonic` umbrella → HTML output engine |
| `image` | 32.4 | direct and `ratatui-image` |
| `tectonic_bridge_harfbuzz` build script | 32.3 | XeTeX (required) |
| `read-fonts` | 29.3 | font outlines (required) |
| `watchexec` | 26.2 | `tectonic` umbrella → `-X watch` CLI support |
| `ratatui-image` | 26.2 | terminal graphics protocols |
| `tectonic_engine_xetex` build script | 25.3 | XeTeX (required) |
| `moxcms` | 24.7 | `image` (required) |
| `tectonic_pdf_io` build script | 22.9 | `tectonic` umbrella → xdvipdfmx |

---

## P0 — cheap, high impact

**Status: done.** All three changes are applied in `Cargo.toml`. Measured together against the
baseline (clean release build in a fresh target directory):

| | Baseline | After P0 |
|---|---|---|
| Build time (wall clock) | 113 s | 60 s |
| Build CPU time | 748 s | 412 s |
| Binary size | 24.7 MB | 11.4 MB |
| `Cargo.lock` packages | 663 | 544 |
| Page render at 1200 px | ~5.9 ms | ~5.1–5.6 ms |

`ravif`, `rav1e`, `aws-lc-sys`, `reqwest`, `rustls` and `hyper` are gone from the dependency
graph. `tokio` remains only through the `tectonic` umbrella's watchexec (removed by P3.1). A
first launch against an empty cache downloaded the bundle through the system libcurl, verified its
digest, built the format and wrote the ready marker; `cargo fmt --check`, `cargo clippy
--all-targets` and the full test suite pass.

### P0.1 Drop the image-format bundle from `ratatui-image`

**Why.** `ratatui-image`'s `image-defaults` feature enables every `image` codec, including an
AVIF encoder (`ravif`, `rav1e`) that costs about 50 s of build CPU. mathnote only needs PNG
(for iTerm2 inline images), and `ratatui-image` already enables `image/png` itself. Kitty and
Sixel output do not use `image` codecs.

**Change.** In `Cargo.toml`:

```toml
ratatui-image = { version = "11.0.8", default-features = false, features = ["crossterm"] }
```

**Verify.** `cargo tree -i ravif` prints nothing; the iTerm2 path still builds (it uses
`image::ImageFormat::Png`).

### P0.2 Download the bundle with the system's libcurl instead of reqwest

**Why.** The network is used only to fill the resource cache on first launch (and for one
repair attempt). reqwest brings in rustls with `aws-lc-rs`, whose `aws-lc-sys` build script is the
slowest single unit of the build (90 s), plus tokio, hyper and h2.

**Change.** In `Cargo.toml`, on both `tectonic` and `tectonic_bundles`:

```toml
features = ["geturl-curl"]
```

**Trade-off.** On macOS this links `/usr/lib/libcurl.4.dylib`, which every macOS install has.
Linux builds need the libcurl development package (`libcurl4-openssl-dev` or equivalent); mention
it in the README build notes.

**Measured with P0.1 together** (clean release build; the snapshot-driver and page-renderer
tests pass):

| | Before | After |
|---|---|---|
| Build time (wall clock) | 113 s | 73 s |
| Build CPU time | 748 s | 520 s |
| Binary size | 24.7 MB | 14.2 MB |

**Verify.** `cargo tree -i aws-lc-sys` and `cargo tree -i tokio` print nothing for mathnote's
own code paths; `otool -L target/release/mathnote` lists `libcurl`; delete the cache directory
once and confirm first launch still downloads and verifies the bundle.

### P0.3 Add a release profile

**Change.** In `Cargo.toml`:

```toml
[profile.release]
strip = "symbols"   # or "debuginfo" to keep function names in panic backtraces
lto = "thin"
codegen-units = 4
```

**Measured on top of P0.1–P0.2:** binary 14.2 MB → 11.4 MB; rendering one page ~5.9 ms →
~5.2 ms (close to run-to-run noise); clean build time unchanged within noise (65–73 s).

**Trade-off.** `strip = "symbols"` removes function names from panic backtraces printed by
`color-eyre`. `strip = "debuginfo"` keeps them at a smaller size saving.

---

## P1 — latency on the typing path

**Status: done.** Measured in release builds:

| | Before | After P1 |
|---|---|---|
| UI-thread time per preview update (1280×1760 px) | 6.3–6.5 ms median | 0.003 ms |
| Page update until its strips are on screen | 6.8–7.0 ms median | 1.5–1.6 ms |
| First edit after launch (driver, release) | 100–300 ms | 4.2–5.0 ms |
| First keystroke after a fresh launch (real binary in a pseudo-terminal) | 160–300 ms | 4–5 ms reported, 5–7 ms keystroke to ready |
| First math page render after the startup page | 10–24 ms | 2.8–4.7 ms (math fonts preloaded in ~8–9 ms while idle) |

Deviation from the plan in P1.1: rows are compared exactly, byte for byte, on the encoder thread
against the shown rows (shared with it as `Arc<RgbaImage>`), instead of by hash. That costs no
UI-thread time either and cannot miss a change through a hash collision.

P1.2 was checked on macOS: the root forks at `BODY_SNAPSHOT_OFFSET` before any page is written,
the first edit resumes from that snapshot without spawning a new root, and its XDV is
byte-identical to a cold compile (`the_first_edit_after_the_blank_template_resumes_from_the_preamble_snapshot`
in `tests/snapshot_driver.rs`); the burst-typing and stuck-engine tests still pass.

### P1.1 Move preview image preparation off the UI thread

**Why.** On every rendered page and every highlight move, the UI thread copies and compares the
full pane image:

- `App::install_visible_preview` (`src/app.rs`): crops the visible rows of the page into a new
  image, creates a white canvas, copies the crop in, tints the highlighted rows, and possibly
  rescales (`fit_to_width`);
- `StripPreview::install` (`src/strips.rs`): crops the canvas into one image per cell row and
  compares each row with the one on screen.

At 1280×1760 px (a retina preview pane, 9 MB of RGBA) this costs about 6 ms (median) per
update, during which keystrokes are not handled.

**Change.**

- Send the page image (`Arc<RgbaImage>`), the scroll offset, the highlight band and the pane
  size to the encoder thread in `src/strips.rs`, and compose the rows there.
- Compare rows by a hash of their pixels (computed while composing) instead of keeping a full
  copy of every shown row.
- Avoid the intermediate full-canvas copies: build each row directly from the page image,
  applying the tint and padding per row.
- Keep the existing guarantee that a new set of rows replaces the shown set only when all its
  changed rows are encoded.

**Verify.** The existing tests `a_change_within_one_cell_row_re_encodes_only_that_strip` and
`typing_on_the_highlighted_line_keeps_the_encoded_preview` still pass; UI-thread time for an
update drops below 1 ms (time `install_visible_preview` with a temporary probe).

### P1.2 Make the first edit after launch fast

**Why.** At launch, the pipeline typesets the blank template. That run is shorter than the 500 ms
snapshot interval, and on macOS `TexDriver::need_snapshot` (`src/driver.rs`) refuses any
snapshot before the first page is written (`xdv_output_started`), because a forked process cannot
load system fonts. So no snapshot exists, and the first keystroke re-runs the whole document
from scratch: 160–300 ms, against 6–19 ms for later edits.

**Change.** The template is fixed, so the byte offset of the note's body in the generated source
is known. On the first run, place a fence just before the body (the same mechanism
`compute_fences` uses after a rollback), so the engine forks a snapshot right after the preamble.

**Risk.** Forking before any page has been written is exactly what the macOS rule guards
against. mathnote's fonts all come from the Tectonic bundle and are loaded from memory, not
through system font lookup, so it should be safe; this must be checked on macOS (type into a fresh
launch, confirm the output is byte-identical to a cold compile, and run the burst-typing test).

**Verify.** A test in `tests/snapshot_driver.rs`: after the blank-template run, the first edit in
the body does not respawn the root (`stats().roots_spawned` unchanged) and produces output
byte-identical to a cold compile.

### P1.3 Pre-load fonts at startup

**Why.** The first page render takes about 50 ms, against about 5 ms afterwards: the font store
loads the bundle file index (~25 ms), reads and indexes `pdftex.map` (~8 ms), and loads each font
on first use.

**Change.** In `preview_thread` (`src/preview.rs`), render the blank-template page once at
startup (or explicitly load `pdftex.map` and the template's fonts into the `FontStore`) while the
user has not typed yet.

**Verify.** The first `PreviewEvent::Page` after the first keystroke arrives without the ~50 ms
font-loading delay (time `render_view` with a temporary probe).

---

## P2 — correctness, documentation, hygiene

**Status: done.**

- **P2.1:** `README.md` describes the note syntax, the snapshot engine, the XDV renderer, terminal
  output, SyncTeX, supported terminals, key bindings, build requirements and the resource cache.
- **P2.2:** the reference-source clone was moved next to the repository (out of the working
  tree), and the `.git/info/exclude` entry for it was removed.
- **P2.3:** the `MTIM` query and answer are gone from the protocol, driver and engine; no
  `#[allow(dead_code)]` remains in `src/`.
- **P2.4:** `latex::TEMPLATE_MATH_FONTS` lists the ten classic math fonts the template can use
  (cmr/cmmi/cmsy at 10, 8 and 6 pt, and cmex10). The warm-up loads all of them through the
  renderer's own lookups with the bundle online, and fails if any is missing;
  `CACHE_SCHEMA_VERSION` 3 makes existing caches refill (about 1 s online). An empty cache now
  bootstraps with all ten `.pfb` files (42 MB, about 30 s). The page-renderer test includes
  script-script material and fails on any renderer warning, and
  `every_template_math_font_loads_from_the_offline_cache` failed against the old cache (cmsy8,
  cmr6, cmmi6, cmsy6 missing) and passes after the refill.
- **P2.5:** `FontStore` and `XdvDocument` collect warnings (`take_warnings`) instead of printing
  them; the pipeline forwards them as `PreviewEvent::Warnings`, the status line adds
  "N preview warning(s), see F1", and the F1 overlay lists them (at most 20, oldest dropped).

### P2.1 Rewrite the README

**Why.** `README.md` still describes the old pipeline (compile to PDF with Tectonic, rasterize with
Hayro, debounced compiles). The current architecture is different:

- the editor (terminal UI) turns the note into LaTeX on a background thread;
- a driver thread runs XeTeX in forked snapshot processes (`mathnote --tex-engine`) and resumes
  the newest snapshot that has not read the changed text;
- pages are read incrementally from XeTeX's XDV output and rendered by mathnote's own renderer
  (TFM metrics, `pdftex.map` Type1 fonts, OpenType native fonts, vello_cpu);
- the preview is sent to the terminal one cell row at a time, re-sending only changed rows
  (zlib-compressed for Kitty-protocol terminals), with the caret hidden during each frame;
- SyncTeX in both directions: the preview follows and highlights the cursor's line, clicks move
  the cursor, TeX errors mark their note line.

**Change.** Describe the architecture, supported terminals (Kitty, Ghostty, WezTerm via the Kitty
protocol; iTerm2; Sixel; half-block fallback), the key bindings (F1, F6/Shift-F6, h/l, j/k,
PageUp/PageDown, Ctrl-↑/↓, Tab, Ctrl-U, Esc/Ctrl-C, mouse), Unix-only support (the engine forks),
and build requirements (ICU via Homebrew on macOS; libcurl if P0.2 is adopted). Keep the
remote edits made on GitHub (palette attribution removed).

### P2.2 Remove or relocate the reference-source clone

**Why.** An untracked clone of the reference implementation's C sources (5.8 MB) sits in the
working tree. It is kept out of commits only by an entry in the local, uncommitted
`.git/info/exclude`; a fresh clone or another machine would not have that entry.

**Change.** Delete it, or move it outside the repository directory.

### P2.3 Remove the dead modification-time query

**Why.** `Client::mtime` in `src/engine.rs` is never called and is marked
`#[allow(dead_code)]`; the engine's `InputFeatures::get_unix_mtime` returns `Ok(None)`. The driver
still answers the query (`QueryBody::Mtime` in `src/driver.rs`, always `0`), and the wire protocol
still decodes it (`Q_MTIM`, `QueryBody::Mtime`, `Answer::Mtime` in `src/protocol.rs`).

**Change.** Remove `Client::mtime` and the `Q_MTIM` query/answer pair from the protocol, driver
and engine, or wire `get_unix_mtime` to it if a real modification time is ever needed (it is not:
inputs come from the immutable bundle or from the driver).

**Verify.** `cargo clippy --all-targets` clean with no `#[allow(dead_code)]` left in `src/`.

### P2.4 Cache the Type1 files of every math size the template uses

**Why.** Found while measuring P1.3. The first-launch warm-up typesets `CACHE_WARMUP_NOTE`
(`src/cache.rs`), which only uses math at text and script size, so only `cmr10`, `cmmi10`,
`cmsy10`, `cmex10`, `cmr8` and `cmmi8` have their `.pfb` outline files cached. A note with
script-script material (for example `$x squared over 2$` in display mathematics) uses `cmr6`,
whose `.tfm` is cached (the engine typesets it) but whose `.pfb` is not. The renderer opens the
bundle cache-only, so those glyphs are missing from the preview.

**Change.** Extend the warm-up note with script-script and other sizes (`cmr6`, `cmmi6`,
`cmsy6`, `cmsy8`, and the 5/7 pt sizes if reachable), or have the warm-up load the `.pfb` of every
cached math `.tfm` named in `pdftex.map`. Bump the cache ready marker so existing caches refill.

**Verify.** After the warm-up, every math `.tfm` in the cache has its `.pfb`; a note using
script-script digits renders them (compare with the Tectonic PDF in `tests/dvi_render.rs`).

### P2.5 Keep renderer warnings off the terminal

**Why.** `FontStore::warn` (`src/dvi/fonts.rs`) and `XdvDocument::fail` (`src/dvi/xdv.rs`) write
with `eprintln!`. While the TUI owns the terminal, stderr is the same terminal, so a warning (such
as the missing `cmr6.pfb` above) is printed over the interface.

**Change.** Collect these warnings in the pipeline and surface them in the status line or the
help overlay instead of writing to stderr.

---

## P3 — deeper build cleanup and structure

**Status: done.** Clean release build in a fresh target directory, nothing else running:

| | After P0 | After P3 |
|---|---|---|
| Build time (wall clock) | 60 s | 44 s |
| Build CPU time | 412 s | 238 s |
| Binary size | 11.4 MB | 5.6 MB |
| Packages in the normal dependency graph | 376 | 234 |

- **P3.1:** mathnote depends on `tectonic_engine_xetex`, `tectonic_io_base`,
  `tectonic_status_base` and `tectonic_errors` directly; `tectonic` is a dev-dependency only, for
  the reference PDFs in `tests/dvi_render.rs`. watchexec, tera, clap, the gix family, tokio and
  the xdvipdfmx, bibtex and HTML engines are gone from the editor's build (`tectonic_pdf_io`
  stays: XeTeX itself links it). `cache::FormatCache` replaces Tectonic's format cache with the same
  file names, so existing caches stay valid. The warm-up runs XeTeX in-process with the bundle
  online: initex on `tectonic-format-latex.tex` when the format is missing, then the canonical
  note with the editor's engine settings, then renders its pages and preloads
  `TEMPLATE_MATH_FONTS` through the page renderer. Warm-ups are serialized per process: concurrent
  test threads warming an empty cache made the bundle cache report files another fetch was still
  writing as unavailable. Verified from an empty cache on macOS (tests, and a real first launch
  that reached "ready" after about 30 s with no TeX output on the screen) and on Linux, built and
  tested for the first time in an Ubuntu 24.04 container (aarch64) with no source changes; it
  needs `libssl-dev`, which the libcurl bindings pull in on Linux.
- **P3.2:** `src/preview_pane.rs` holds the preview pane (`PreviewPane`: page, scroll, sync
  highlight, warnings, strips); `App` passes it the sync target and documents and sends its view
  requests. `src/app.rs` went from 1,624 to 1,334 lines.
- **P3.3:** `cargo bench --bench latency` (`benches/latency.rs`) types a note one character at a
  time and reports, on this machine: typeset per edit median 2.8 ms (p90 3.7 ms, 1 engine root
  for 86 edits); page render at 1280 px 34 ms first, then median 1.3–1.5 ms; UI-thread preview
  install about 0.001 ms; strips encoded 1.2–1.5 ms after install; 1 changed strip per edit
  (max 2), about 4.6 KB of Kitty output per edit. `mathnote::strips` is public for it.

### P3.1 Replace the `tectonic` umbrella crate with its component crates

**Why.** After P0.1–P0.2, crates reachable only through the `tectonic` umbrella still account for
about 263 of 715 CPU seconds of a clean build, and about 120 crates: watchexec, tera, clap, the
gix family, regex, jiff, notify, zip, and the xdvipdfmx, bibtex and HTML-output engines with their
C sources.

mathnote uses from the umbrella only:

| Use | Where | Replacement |
|---|---|---|
| I/O traits and types (`IoProvider`, `InputHandle`, `OutputHandle`, `OpenResult`, `InputFeatures`) | `src/engine.rs`, `src/dvi/fonts.rs` | `tectonic_io_base` directly |
| Status traits (`StatusBackend`, `MessageKind`, `NoopStatusBackend`) | `src/engine.rs`, `src/cache.rs`, `src/dvi/fonts.rs` | `tectonic_status_base` directly |
| `TexEngine`, `TexOutcome` | `src/engine.rs` | `tectonic_engine_xetex` directly |
| `io::format_cache::FormatCache` | `src/engine.rs` | copy (about 100 lines) into `src/cache.rs` |
| `driver::ProcessingSessionBuilder` for the first-launch warm-up | `src/cache.rs` (`warm_cache`) | see below |
| `driver::ProcessingSessionBuilder` to make reference PDFs | `tests/dvi_render.rs` | keep `tectonic` as a dev-dependency |

**Change.**

- Depend on `tectonic_engine_xetex`, `tectonic_io_base`, `tectonic_status_base` (versions
  already in `Cargo.lock`) next to `tectonic_bridge_core` and `tectonic_bundles`.
- Generate the `latex` format when it is missing by running the engine in initex mode
  (`TexEngine::initex_mode(true)`) on the bundle's format source, writing it through the copied
  format cache.
- Replace the warm-up's PDF compile with mathnote's own pipeline: typeset the canonical warm-up
  note through the engine and render its page with the XDV renderer, with the bundle opened
  online (`open_bundle(cache_dir, false)`) so every file the template, the engine and the renderer
  need is fetched and cached.
- Keep `tectonic` only under `[dev-dependencies]` for the pixel-agreement test (or move that test
  behind a cargo feature so plain `cargo test` does not build xdvipdfmx).

**Verify.** `cargo tree -e normal -i watchexec` and `-i tera` print nothing; deleting the cache
directory and launching rebuilds the format and fetches resources; the full test suite passes;
re-run `cargo build --release --timings` from a clean target directory and record the new numbers
here.

### P3.2 Split `src/app.rs`

**Why.** `src/app.rs` is 1,624 lines and mixes editor state with the preview pane.

**Change.** Move the preview-pane state and logic into its own module (for example
`src/preview_pane.rs`): `PreviewViewport`, page index and count, scroll offset, sync target and
highlight, preview warnings, `present_preview`, `install_visible_preview`, `scroll_preview`,
`change_page`, `select_page`, `request_view` and `sync_from_preview_click` (`fit_to_width`
already moved to `src/strips.rs` in P1.1). `App` keeps the note buffer, LaTeX pane, focus,
status line and event loop.

### P3.3 Permanent latency and bandwidth benchmark

**Why.** The baseline numbers above came from throwaway probes and a pseudo-terminal harness that
were deleted after use. Nothing catches a regression.

**Change.** Add a small benchmark (an example binary or `cargo bench`) that reports: typeset time
per one-character edit (median, p90), page render time, UI-thread preview update time, and encoded
bytes per changed row.

---

## P4 — only if the LaTeX template grows

None of these affect today's output: the fixed template produces only text, mathematics, rules and
page numbers. Port them in this order when the template needs them.

### P4.1 Reruns until the `.aux` file is stable

**Needed for.** `\ref`/`\label`, a table of contents, citations.

**What.** After a run finishes, if the `.aux` (or `.toc`) output differs from what the run read,
start another run with the new file, until the contents stop changing. The driver already serves
earlier outputs to the engine (it answers open requests for `mathnote.aux`); what is missing is
detecting the change and scheduling the rerun without losing snapshots for unaffected edits.

### P4.2 Colour stacks and raw PDF drawing and transform specials

**Needed for.** `xcolor` through the LaTeX3 backend (colour stacks) and TikZ (raw PDF path
operators, coordinate transforms).

**What.** In `src/dvi/xdv.rs`, handle `pdfcolorstack` specials, `pdf:literal`/`pdf:code` path
construction and painting, `pdf:btrans`/`pdf:etrans` and `x:` transforms, and add path items
(stroke/fill with line width, dashes, joins) to the display list rendered by `src/dvi/render.rs`.
Today only `color push`/`pop`/set (rgb, gray, cmyk) and page-size specials are handled.

### P4.3 SyncTeX during a run

**Needed for.** Moving the highlight before a long run finishes. Low value while runs take a few
milliseconds.

**What.** Stock Tectonic XeTeX writes SyncTeX gzip-compressed, so `src/synctex.rs` parses it only
when the output is closed. Either decode the growing gzip stream incrementally (unverified: the
compressor may hold data until the stream ends) or use an engine build that writes it
uncompressed, then parse records as pages are shipped and roll back with the output log.

### P4.4 Images, links, virtual fonts

**Needed for.** `\includegraphics` (PDF, PNG, JPEG), `hyperref` links and destinations, and font
packages built on virtual fonts.

**What.** Image specials decoded and drawn into the display list; link areas recorded per page
(and made clickable in the preview); a VF parser that expands virtual characters into their base
font glyphs and rules.

### P4.5 Preview zoom and themes

**Needed for.** Reading small text or comparing with a printout; dark terminals.

**What.** Zoom levels and fit-page in the preview pane (the pipeline already renders at any
width: `PreviewCommand::View`), and an inverted or theme-coloured page rendering.
