# Working notes for Claude Code (kenny-21342/realtime-tex)

This is Kenny's fork of `HenryXiaoYang/realtime-tex` (rtex, a Rust real-time LuaLaTeX engine, MIT).
Branch `kenny/main` is the working branch (the fork's default); `main` mirrors upstream.

## Rules
- Remote `upstream` is Henry's repo, read-only. Never push there, never open PRs against it unless Kenny asks.
- Push only to `origin` (the fork) and only branches; ask before force-pushing or changing the default branch.
- Fixtures are Kenny's private documents and live in the private repo `kenny-21342/rtex-fixtures`,
  never in this public fork. Never copy their text, images or run output excerpts into this repo.
- Report status checked against the disk, not remembered. Log `uptime` next to any timing.
- Commit messages end with `Co-Authored-By: Claude Sonnet 5.5 <noreply@anthropic.com>`.

## Local changes on top of upstream 25b3f8f
1. `crates/rtex-core/src/engine.rs`: poll the response FIFO in 2 ms slices (macOS `poll()` is not woken by a
   FIFO write; without it the 5 s watchdog kills the server). **Linux does not need it** (2026-10-09 cloud run:
   all tests pass 3/3 without it; strace shows the blocking poll woken by the write in 0.3-137 ms). Kept
   unconditionally: harmless on Linux.
2. `crates/rtex-core/tests/mutation.rs`: random-edit correctness test (env vars documented in its header,
   plus `RTEX_MUTATION_NO_PICCACHE`, `RTEX_MUTATION_WAIT`). It edits text only (no option lists, no picture code),
   prints the session state and last events when a clean pass does not converge, and fails (instead of a
   silent `ok`) when `RTEX_MUTATION_PROJECT` is set but TeX Live is missing.
3. Fixes for the math/phy "clean pass did not converge" hangs (`tests/regressions.rs` reproduces both):
   - `session.rs`: a final pass with a stable aux family but compile errors ends as `PassLimitReached`
     (it said `Converging`, promising a pass that never ran; docs/CONVERGENCE.md).
   - `piccache.rs`: pictures linked by a pgf node name (one names a node, another uses it) are not cached.
4. `scripts/fixtures-verify.sh`, `scripts/fixtures-mutation.sh`, `scripts/serve_convergence.py`, `scripts/cloud-setup.sh` (light), `scripts/cloud-texlive.sh`.

5. Native TikZ drawing (merged, PR #2): pgf literals, shadings and cached pictures drawn from the display list
   (`crates/rtex-dl/src/gfx.rs`, `LayoutUpdate.pages_changed[].native`, docs/DISPLAY_LIST.md "Native drawing"),
   checked against MuPDF by `scripts/gfx_compare.py` / `scripts/gfx_shading_check.py` (inputs from the
   `gfx_dump` and `native_session` examples). Tiling patterns still fall back to the PDF.

## Running in a Claude Code cloud session
Environment: Ubuntu 24.04 x86_64, 4 vCPU, 16 GB RAM, 30 GB disk; Rust is preinstalled; TeX Live is installed by
`scripts/cloud-texlive.sh` into `/opt/texlive`. The environment's setup script is only the light `scripts/cloud-setup.sh`
(a TeX Live install inside the setup script exceeded its ~5 minute budget and the session failed to start). At the start of a
session run, from this repo's root: `nohup bash scripts/cloud-texlive.sh > /tmp/texlive-install.log 2>&1 &` and poll the log until it
prints `TEXLIVE READY`. Then `. /etc/profile.d/rtex-texlive.sh` in each shell.
Foreground commands are capped at 10 minutes: run `cargo test`, verify and mutation runs with
`run_in_background` and poll the output files.

Attach the private repo `kenny-21342/rtex-fixtures` to the session; it is cloned next to this repo.
`export RTEX_FIXTURES=<path to rtex-fixtures>/fixtures/ours`. Read `<rtex-fixtures>/notes/PLAN-next-session.md` first.

## Linux control (done 2026-10-09; results in rtex-fixtures `notes/linux-cloud/`)
Verify matches the macOS table on all four fixtures; econ-notes needs `makecell` (now in `install-texlive.sh`) and
the DengXian font: run `sh scripts/install-fonts.sh` in the rtex-fixtures clone after TeX Live is ready. After the fixes above, `scripts/fixtures-mutation.sh 30 21` gives
30/30 served and equal on all four. `over_budget_units_fall_back_to_background` is load sensitive: run the suite
on an otherwise idle machine.
