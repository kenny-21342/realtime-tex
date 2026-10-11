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

## Local changes on top of upstream e35f869
Upstream (Henry's refactor) was merged in on 2026-10-10. The merge PR describes, fix by fix, which side's
version was kept where both changed the same thing. The old 2 ms FIFO poll slices are gone: upstream's
response reader thread (`crates/rtex-core/src/transport.rs`) replaces them on every platform.
1. `crates/rtex-core/tests/mutation.rs`: random-edit correctness test (env vars documented in its header,
   plus `RTEX_MUTATION_NO_PICCACHE`, `RTEX_MUTATION_WAIT`). It edits text only (no option lists, no picture code),
   prints the session state and last events when a clean pass does not converge, and fails (instead of a
   silent `ok`) when `RTEX_MUTATION_PROJECT` is set but TeX Live is missing.
2. Fixes for the math/phy "clean pass did not converge" hangs (`tests/regressions.rs` reproduces both):
   - `session/passes.rs`: a final pass with a stable aux family but compile errors ends as `PassLimitReached`
     (it said `Converging`, promising a pass that never ran; docs/how-it-works.md).
   - `piccache.rs`: pictures linked by a pgf node name (one names a node, another uses it) are not cached.
3. `scripts/fixtures-verify.sh`, `scripts/fixtures-mutation.sh`, `scripts/serve_convergence.py`, `scripts/cloud-setup.sh` (light), `scripts/cloud-texlive.sh`.
4. Native TikZ drawing: pgf literals, shadings, tiling patterns and cached pictures drawn from the display list
   (`crates/rtex-dl/src/gfx.rs`, `LayoutUpdate.pages_changed[].native`, docs/display-list.md "Native drawing"),
   checked against MuPDF by `scripts/gfx_compare.py` / `scripts/gfx_shading_check.py` (inputs from the
   `gfx_dump` and `native_session` examples); pattern fills by the plugin's pixel render check (`test/render`).
6. Glossaries (`\makeglossaries`) built between passes (`background.rs` `run_makeglossaries`).
5. Session: pass timeout (`pass_timeout_ms`), relative fast-path budget (`fast_budget_factor` on top of
   upstream's 50 ms floor), warm start of the background build, diagnostics for fatal passes, page `rotate`
   and `color_base` in the display list.

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
