# mathnote

a simple terminal notebook for prose, proofs, and mathematical expressions. converts only explicit or unambiguous mathematics into canonical LaTeX, typesets it while you type, and shows the rendered page next to the note in the terminal.

> **Work in progress.** Expect rough edges and breaking changes.

## Writing notes

Prose is written as is. Mathematics goes between `$…$` (inline) or on lines between `$$` markers (display), in words or symbols:

| Note | LaTeX |
|---|---|
| `if $x squared equals 81$ then $root of 81$ is 9.` | `if \(x^{2}=81\) then \(\sqrt{81}\) is 9.` |
| `$x is less than or equal to y$` | `\(x\le{}y\)` |
| `$$` / `integral of x over 2` / `$$` | `\[ \int \frac{x}{2}\,\mathrm{d}x \]` |

Ambiguous mathematics is rejected with an error instead of being guessed.

## Run

```bash
cargo run --release
```

Runs on macOS and Linux. The first launch downloads the LaTeX resources it needs (about 40 MB); after that it works offline.

Building needs ICU, FreeType, Graphite2 and libpng (on macOS: `brew install icu4c freetype graphite2 libpng pkgconf`). Linux also needs fontconfig and the libcurl and OpenSSL development packages.

The page preview uses terminal graphics (Kitty, Ghostty, WezTerm, iTerm2, or Sixel), with a half-block fallback elsewhere.

## Keys

- `F1`: command reference
- `F6` / `Shift-F6`: cycle panes
- `PageUp` / `PageDown`: change the preview page
- `Ctrl-U`: clear the note
- `Esc` / `Ctrl-C`: quit
- Click the page to jump to that line of the note
