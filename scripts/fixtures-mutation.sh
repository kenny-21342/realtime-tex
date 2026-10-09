#!/bin/sh
# Edit-mutation test (crates/rtex-core/tests/mutation.rs) on the four private fixtures, one after the other.
# Usage: scripts/fixtures-mutation.sh [EDITS] [SEED] [OUTDIR]
#   RTEX_FIXTURES, RTEX_TEXLIVE_BIN as in fixtures-verify.sh
#   RTEX_MUTATION_NO_PICCACHE=1  switch the picture cache off; RTEX_MUTATION_BATCH=N  edits per pass
cd "$(dirname "$0")/.." || exit 1
EDITS=${1:-30}; SEED=${2:-21}; OUT=${3:-build/fixtures-mutation/out}
FIX=${RTEX_FIXTURES:-../rtex-fixtures/fixtures/ours}
mkdir -p "$OUT"; rm -f "$OUT/DONE"
for spec in mcm-paper:main.tex math-a-level-notes:main-noxy.tex econ-notes:main-noxy.tex phy-hl-notes:main-noxy.tex; do
  name=${spec%%:*}; main=${spec#*:}
  RTEX_MUTATION_PROJECT="$FIX/$name" RTEX_MUTATION_MAIN="$main" RTEX_MUTATION_EDITS=$EDITS RTEX_MUTATION_SEED=$SEED \
  RTEX_MUTATION_REPORT="$PWD/$OUT/$name.json" cargo test -p rtex-core --test mutation -- --nocapture > "$OUT/$name.txt" 2>&1
  echo "$name exit=$?" >> "$OUT/DONE"
done
echo ALL >> "$OUT/DONE"
