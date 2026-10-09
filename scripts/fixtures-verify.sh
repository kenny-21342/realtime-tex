#!/bin/sh
# rtex verify on four private fixture documents.
# Usage: scripts/fixtures-verify.sh [BINARY] [OUTDIR]
#   RTEX_FIXTURES  directory holding mcm-paper, math-a-level-notes, econ-notes, phy-hl-notes
#                  (default: ../rtex-fixtures/fixtures/ours)
#   RTEX_TEXLIVE_BIN  TeX Live bin directory (set by /etc/profile.d/rtex-texlive.sh in cloud VMs)
cd "$(dirname "$0")/.." || exit 1
BIN=${1:-target/release/rtex}; OUT=${2:-build/fixtures-verify/out}
FIX=${RTEX_FIXTURES:-../rtex-fixtures/fixtures/ours}
mkdir -p "$OUT"; rm -f "$OUT/DONE"
for spec in "mcm-paper main.tex" "math-a-level-notes main-noxy.tex" "econ-notes main-noxy.tex" "phy-hl-notes main-noxy.tex"; do
  set -- $spec
  echo "load1=$(uptime | sed 's/.*load average[s]*: //') start=$(date +%T)" > "$OUT/$1.verify.txt"
  /usr/bin/time -p "$BIN" verify --project "$FIX/$1" --main "$2" \
    --build "build/fixtures-verify/$1" --json-out "$OUT/$1.verify.json" >> "$OUT/$1.verify.txt" 2>&1
  echo "exit=$? end=$(date +%T)" >> "$OUT/$1.verify.txt"
done
echo done > "$OUT/DONE"
