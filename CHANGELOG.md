# Changelog

## Unreleased

## 0.0.10 (2026-10-13)

- Newly typed plain paragraphs can borrow outer heading/environment state when no standalone
  body paragraph is available. Paragraphs following a completed run-in heading recognize its
  inactive hook and become live-eligible after capture. Blank lines between an empty run-in
  heading and its first text no longer split that single TeX unit; live compilation keeps the
  title with the text, including before the first captured rows exist. Explicit titlesec
  display shapes and first-paragraph indentation are respected instead of treating every
  `\paragraph` as run-in.

## 0.0.9 (2026-10-13)

- New plain-text paragraphs and paragraph splits can use live-validated page previews without
  waiting for a full document pass. Shifted source line ranges no longer disable progress.
  Live boxes for new paragraphs that outgrow their guessed page position are retained with
  empty placements until a page supplies their real positions. Superseded runs deliver all
  changed rows and the next unchanged unit before yielding, keeping following drawings visible.
  Preamble/structural changes and removed units continue through ordinary full layouts.

## 0.0.8 (2026-10-12)

- **Preview rebuilt pages before the document finishes.** A warm background run can publish
  `PagePreview` as each affected page ships for validated plain-text paragraph edits. Hosts
  can display complete exact/native pages and their new placements while the suffix is still
  compiling. A superseded run yields after showing the affected page; its partial auxiliary
  files never seed the next run. Errors reset provisional pages. The completed layout and
  export retain the usual convergence and fidelity checks. Late pages still wait for TeX to
  process the preceding document; tables, floats, structural edits and unsupported drawing
  continue through full layouts.

- **Page updates while typing continuously.** Body reflow briefly coalesces edits (25 ms quiet
  time, capped at 100 ms) and starts full layouts at most once per debounce interval (300 ms by
  default), including when a paragraph gains or loses lines and moves later text onto another
  page. A pass that took longer than the interval catches up immediately when its sources are
  stale. Preamble requests retain their usual debounce, capped at three intervals. A superseded
  run delivers its completed capture as `Stale` and skips further auxiliary tools and convergence
  passes for that snapshot.
  Once typing pauses, the latest text still gets the usual clean convergence checks. Exports
  and held-picture runs continue to finish normally.

## 0.0.7 (2026-10-11)

- **Draw pictures in live label edits.** The live server shares capture's readable pgf/pict2e
  literal hooks. `ParagraphUpdate.native` carries complete native drawing operations and glyph
  transforms, restoring unchanged cached pictures alongside an edited one, so hosts keep
  axes, lines and rotated labels while a label is edited. Unsupported live pictures have
  no partial drawing; hosts keep their previous complete drawings until the full compile.

## 0.0.6 (2026-10-11)

- **Held pictures.** While an edit leaves a TikZ picture with errors (a half-typed coordinate
  reads the rest of the document into it), layouts keep the picture's last good drawing instead
  of losing it and the text after it. The errors are still reported; `LayoutUpdate.held` names
  the pictures, and a held layout is never `Converged`. When only the held picture changed, no
  extra pass runs. On by default: `hold_broken_pictures` (C ABI), `--no-hold-broken-pictures`
  (`rtex serve`). See docs/how-it-works.md, "Held pictures".

## 0.0.5 (2026-10-11)

### From upstream 0.0.3

Upstream 0.0.3 and this fork's 0.0.3 are different releases. This fork's 0.0.5
includes the upstream fixes below while preserving its own changes.

- **Live typing on Windows with MiKTeX.** The live engine exited at startup with
  "exit code: 1" on every MiKTeX installation (realtime-tex-vsc-plugin#1): MiKTeX refuses to
  let LuaTeX write outside the output directory, which includes the engine's response pipe.
  rtex now sets `MIKTEX_ALLOWUNSAFEOUTPUTFILES=1` for the LuaLaTeX processes it starts, the
  MiKTeX equivalent of the `openout_any=a` it already sets for TeX Live.
- When the live engine cannot start, the error quotes the first TeX or Lua error from its log
  instead of only the exit code.
- The live driver path uses forward slashes on Windows.
- A job runs a live-editing session on Windows with MiKTeX, in paths with spaces and non-ASCII
  characters and on a CRLF document.
  MiKTeX downloads use a fixed server and retries.

## 0.0.4 (2026-10-11)

- **Ignored errors are not reported.** LuaTeX logs an error the document told it to ignore
  (`\ignoreprimitiveerror`) as `file:line: ignored: ...`, in the log only. The LaTeX kernel does
  that around the `\vsplit` that extracts marks, so with some TeX Live versions every page a
  `longtable` breaks logged "ignored: Infinite glue shrinkage found in box being split". Each was
  reported as a compile error and the run ended in `PassLimitReached`; they are now skipped.

## 0.0.3 (2026-10-11)

The first release of kenny-21342/realtime-tex: upstream 0.0.2 (`e35f869`) plus the fork's work:

- **Native drawing of TikZ/pgf pages.** Pages whose only degradation is pgf literals and
  shadings carry their drawing operations (`pages_changed[].native`): paths, fills, strokes,
  dashes, clips, opacity, axial and radial shadings, transformed node text, and cached pictures
  through their stored drawing. Hosts draw them without the PDF. Checked against MuPDF
  (`scripts/gfx_compare.py`, `scripts/gfx_shading_check.py`).
- **Tiling patterns drawn natively.** Fills with pgf's `patterns` / `patterns.meta` patterns
  (coloured and uncoloured) name their cell (`Paint.pattern`, `NativePage.patterns`); the page
  list carries the declarations (`patterns`, binary PATTERN record), cached pictures included.
- **pict2e and clipped images drawn natively.** pict2e's literals (picture mode, diagbox) are
  readable like pgf's; a box resource used at its saved size with plain contents (graphicx's
  `trim`/`clip`, `viewport`) is written as its contents, clipped to its box.
- **Fadings drawn natively.** pgf's soft masks (`path fading`, `scope fading`) carry what their
  form draws (`NativePage.masks`; the page list's `masks`, binary MASK record); masked paints and
  shadings name their mask.
- **Glossaries built between passes.** `\makeglossaries` lists (glossary, acronyms, custom
  glossaries) are built as makeglossaries builds them (makeindex with glossaries' style;
  makeglossaries-lite for xindy styles), and a changed list brings another pass.
- **`\input` files that do not exist yet** (or are not UTF-8) are no longer compiled as empty
  files: TeX reports a missing file, and one created later is read. A main file that is not
  UTF-8 is refused with a message that says so.
- **Landscape pages are reported turned** (`rotate`, binary ROTATE record) instead of being
  degraded; rows inside a rotated box are flagged `transformed_rows`.
- **Colour stacks across pages.** A page starting inside a colour group records the stacks it
  starts with (`color_base`) and each colour item the stack's top after it (`after`); the
  binary format carries both.
- **The live budget follows the document**: at least 4× the session's median live compile
  (`fast_budget_factor`), so a document whose fonts make every paragraph slow stays live.
- **A reopened session shows its last layout at once** when the sources are unchanged
  (`rtex-sources` next to the capture): 0.2–0.3 s instead of a full pass.
- **Errors at the lines lualatex reports**: fatal errors list the log's errors with
  `file:line`; an error just after the preamble is placed at `\begin{document}`.
- **Passes that never end are stopped** (`pass_timeout`, 120 s, or sooner once the sources
  change). A loop in a body line no longer reaches the live engine's preamble, and a server whose
  startup fails is killed.
- **Second probe compile**: state the leak check cannot see (expl3 sequences) shows as a second
  result that differs, and the unit is demoted.
- A run whose aux files are stable but whose log has errors ends (`PassLimitReached`); it said
  `Converging`, promising a pass that never ran.
- The standby engine is used when the preamble `\input`s files (its hash never matched).
- A fatal error is a failed pass (its partial PDF is not used); a picture with an error, or one
  that names a node another picture uses, is not cached; repeated images keep their files.
- `\endinput` ends a file's units and never reaches the live engine.
- verify: included PDF figures are measured by their BBox.
- `examples/replay.rs`: live-editing replays (edit scripts, typing, error checks) with verdicts
  against the full run; `tests/mutation.rs`: random-edit correctness test.

- **Prebuilt binaries** for Linux (x86_64, arm64), macOS (Apple silicon, Intel) and Windows
  (x86_64), attached to each release by CI. Each holds `rtex`, the shared C library, `rtex.h`
  and the support files.
- `rtex` finds its support files next to its executable (`share/rtex/tex`), so an unpacked
  archive works from anywhere. `rtex doctor` checks the installation.

- GitHub counts the project as Rust: fixtures, benchmarks and the TeX/Lua files are marked as
  vendored for language statistics.

## 0.0.2 (2026-10-10)

### New

- **Windows support.** rtex runs on Linux, macOS and Windows. CI tests all three against a real
  TeX Live.
- **Much more goes live.** Not just plain paragraphs any more, but:
  - paragraphs with display math, footnote marks, references and citations (natbib and
    biblatex);
  - lists, quotes, theorems and proofs, figures and tables with images and captions, verbatim
    and listings;
  - headings, the title block, manual bibliographies;
  - your own macros and environments, and text in `\input`/`\include`d files.

  Counters, `\the<counter>` formats and macros the body redefines are restored per unit, so
  numbers come out right.
- **Probe mode** (the default). A paragraph that uses commands rtex does not know is no longer
  sent to the background automatically. rtex compiles it once as the last pass saw it and
  compares the result with the page. If it matches exactly, edits go live. Definitions that
  leak out of a paragraph are detected.
- **New paragraphs and splits stay live.** A paragraph you split or type fresh borrows its
  neighbour's settings until the next pass.
- **Faster background passes.**
  - A standby LuaLaTeX with the preamble already loaded typesets only the body (3–4× faster for
    short documents).
  - Unchanged TikZ/pgfplots pictures are reused from the previous pass (on a 111-page document
    with 120 pictures, 45 s → 18 s per pass).
  - Slow multi-pass runs show each finished pass right away.
- **Bibliographies and indexes run automatically.** biber, bibtex, makeindex, and imakeidx's
  own program and options.
- **Configurable time budget** per live compile (`fast_budget_ms`, default 50 ms).
- **Diagnostics for engine failures.** Set `debug_dir` and every failure leaves a bundle: what
  was sent, the TeX log, and a stage-by-stage trace.
- **New `rtex` subcommands.** `rtex probe` shows where a document's live latency goes, and
  `rtex verify --permissive` / `--pic-cache` check probe mode and the picture cache.

### Faster

- **About 0.5 ms per keystroke for a one-line paragraph, 1.1 ms for four lines**, independent of
  document length. That is down from 0.8 and 1.5 ms in 0.0.1. The engine is driven by raw
  frames, the editor's thread writes straight to an idle engine, and LaTeX work that cannot be
  seen in a paragraph (running-head marks, repeated math font setup, file probes) is skipped in
  the live engine.

### Fixed

Most of these were found by running real documents (a 111-page TikZ-heavy set of notes and a
58-page document with luatexja, biblatex, imakeidx, glossaries and tcolorbox):

- The response channel is read by a dedicated thread. On macOS a large result could block the
  engine until the watchdog killed it.
- A Lua error inside the live engine is reported at once instead of hanging until the watchdog.
- Projects with subfolders, and figures that are PDFs, now work in background passes.
- Layouts stopped converging when bookmarks recorded the pass directory, or the index was
  missing. Both are fixed.
- `\begin{document}` inside comments or verbatim no longer confuses rtex.
- biblatex citations are live in a session opened without an earlier build.
- glossaries' first-use forms are right in live updates.
- Running heads no longer count as part of the paragraph at the top of a page, and appendix
  numbering is right.
- Paragraphs that start inside a group (`{\em …}`) now have their lines.
- Fixed a deadlock when an edit arrived while a layout was being installed.
- A paragraph that crashes the engine is kept off the live path instead of crashing it on
  every keystroke.
- PDFs of earlier layouts stay valid while the host reads them. Two engines could write the
  same file, which produced blank pages.

### Changed

- The library, C ABI, display-list format and engine protocol are versioned together as 0.0.x.
  Nothing is frozen before 0.1.
- The C ABI takes more configuration keys: `fast_budget_ms`, `eligibility`, `picture_cache`,
  `debug_dir`, `unit_envs` and `warm_background`.

## 0.0.1

First version:
- a persistent LuaTeX engine that re-typesets single paragraphs;
- the capture package;
- the display-list format (binary and JSON);
- the Rust library with a C ABI;
- background passes with honest convergence reporting;
- PDF export;
- the fidelity checks;
- fixtures and benchmarks.
