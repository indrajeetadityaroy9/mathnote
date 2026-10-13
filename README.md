# mathnote

a simple terminal notebook for prose, proofs, and mathematical expressions. converts only explicit or unambiguous mathematics into canonical LaTeX, typesets a real document with an embedded XeTeX engine while you type, and shows the rendered page next to the note in the terminal. All typography comes from LaTeX: Latin Modern text and Computer Modern mathematics.

## Writing notes

Prose is written as is. Mathematics goes between `$…$` (inline) or on lines between `$$` markers (display), in words or symbols:

| Note | LaTeX |
|---|---|
| `if $x squared equals 81$ then $root of 81$ is 9.` | `if \(x^{2}=81\) then \(\sqrt{81}\) is 9.` |
| `$x is less than or equal to y$` | `\(x\le{}y\)` |
| `$α plus β$` | `\(\alpha+\beta\)` |
| `$$` / `integral of x over 2` / `$$` | `\[ \int \frac{x}{2}\,\mathrm{d}x \]` |

Mathematics that is not unambiguous is rejected with an error, marked at its position in the note, instead of being guessed.

## Run

```bash
cargo run --release
```

mathnote runs on macOS and Linux (the TeX engine relies on `fork`). The first launch downloads the files the fixed LaTeX template needs (about 40 MB) from one pinned Tectonic bundle; after that it works offline.

## Interface

Wide terminals show the note, the generated LaTeX, and the rendered page side by side. Narrow terminals stack the panes, and very small terminals show only the focused pane. The page preview follows the cursor: it shows and highlights the typeset line being edited, and TeX errors mark the note line they come from.

The page is drawn as terminal graphics: Kitty's graphics protocol (Kitty, Ghostty, WezTerm), iTerm2 inline images, or Sixel, with a half-block fallback elsewhere. tmux is supported through passthrough.

- `F1`: command reference (also lists preview warnings, such as a missing font)
- `F6` / `Shift-F6`: cycle panes
- Arrows, `Home`, `End`: move the cursor; the preview follows its line
- `PageUp` / `PageDown`: change the preview page (scroll the LaTeX pane when it is focused)
- `Ctrl-↑` / `Ctrl-↓`: scroll the preview
- `h` / `l`, `j` / `k`: move between and scroll the LaTeX and preview panes
- `Tab`: insert four spaces
- `Ctrl-U`: clear the note
- `Esc` / `Ctrl-C`: quit
- Mouse click: focus a pane, place the cursor, or click the page to jump to that line of the note
- Mouse wheel: scroll the pane under the pointer

## How it works

Nothing slow runs on the thread that handles keys and draws the screen:

1. **Note → LaTeX** (`note`, `document`, `latex`): a background thread parses the note and emits the LaTeX document, with a source map from every note line to its LaTeX.
2. **Incremental typesetting** (`driver`, `engine`, `protocol`): XeTeX runs in a separate process (`mathnote --tex-engine`) that asks the driver for every file it reads. While it typesets, the engine forks snapshots of itself. After an edit, the driver resumes the newest snapshot that has not yet read the changed text, so a keystroke re-typesets only from just before the edit, typically in a few milliseconds. A snapshot taken right after the preamble makes even the first edit fast.
3. **Page rendering** (`dvi`): the engine's XDV output is read page by page as it grows and drawn by mathnote's own renderer: TFM metrics and Type1 outlines (via `pdftex.map`) for the mathematics, OpenType outlines for the text, rasterized with vello_cpu.
4. **Terminal output** (`strips`): the page is sent as one image per row of terminal cells, and only changed rows are re-sent (zlib-compressed for Kitty-protocol terminals), so typing on one line costs a few kilobytes of output. Frames are drawn as synchronized updates with the caret hidden.
5. **SyncTeX** (`synctex`): maps between note lines and page positions in both directions, for the highlight, clicks on the page, and error locations.

## Build requirements

Tectonic's XeTeX engine is linked in as Rust crates; no external TeX installation, PDF viewer or image tool is used at runtime. It links to native text libraries, so the build host and the resulting executable need compatible ICU, FreeType, Graphite2, and libpng libraries, plus fontconfig on Linux. The first-launch download uses the system libcurl: macOS always ships it, while Linux builds need the libcurl and OpenSSL development packages. On macOS, install the libraries with Homebrew (`brew install icu4c freetype graphite2 libpng pkgconf`); on Ubuntu, `build-essential pkg-config libcurl4-openssl-dev libfontconfig1-dev libfreetype-dev libgraphite2-dev libicu-dev libpng-dev libssl-dev zlib1g-dev` is enough to build and pass the tests. On macOS, the repository Cargo configuration discovers an existing Homebrew ICU installation from either the Apple Silicon or Intel prefix. The produced macOS binary is host-local rather than a standalone relocatable bundle, so rebuild it on a destination with matching native libraries.

## Resource cache

The first launch retrieves only the files required by mathnote's fixed TeX template from one pinned format-33 Tectonic bundle: it builds the LaTeX format, typesets a sample note with the editor's own engine and template, and renders it with the page renderer, fetching the outline files of every math font size the template can use. Mathnote rejects any bundle whose cryptographic content digest differs from the compiled-in expected digest, then caches the verified resources under the platform cache directory (`~/Library/Caches/mathnote` on macOS, `~/.cache/mathnote` on Linux). Later runs lock Tectonic to cache-only operation and work offline. If that verified cache becomes unusable, mathnote invalidates it and makes one online repair attempt.

## Development

```bash
cargo test
cargo clippy --all-targets
cargo bench --bench latency
```

The tests typeset real documents, so the first run needs network access to fill the resource cache. The tests compare mathnote's page renderer with Tectonic's own PDF output, so they build Tectonic's full pipeline as a dev-dependency; the editor itself does not. The `latency` benchmark types a note one character at a time and reports typeset time per edit, page render time, preview update time, and the terminal bytes each edit sends. Planned work is tracked in [ROADMAP.md](ROADMAP.md).
