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
   FIFO write; without it the 5 s watchdog kills the server). Whether Linux needs it is an open question
   the first cloud run answers (see below).
2. `crates/rtex-core/tests/mutation.rs`: random-edit correctness test (env vars documented in its header,
   plus `RTEX_MUTATION_NO_PICCACHE`).
3. `scripts/fixtures-verify.sh`, `scripts/fixtures-mutation.sh`, `scripts/serve_convergence.py`, `scripts/cloud-setup.sh` (light), `scripts/cloud-texlive.sh`.

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

## First tasks (Linux control)
1. `cargo build --release -p rtex-cli`; `cargo test -p rtex-core` with the poll fix as committed, record every failure verbatim.
2. Same with the fix reverted (`git stash`/revert locally, do not commit): do `boundary_change_and_preamble_change`
   and `every_unit_kind_takes_the_fast_path` pass on Linux without it? (macOS: fail 3/3 without, pass 3/3 with.)
3. `scripts/fixtures-verify.sh` and compare with the expected table in the plan (all four: 0 differing units).
4. `scripts/fixtures-mutation.sh 30 21`: math and phy ended with "the clean pass did not converge" on macOS; find out whether Linux does too.
