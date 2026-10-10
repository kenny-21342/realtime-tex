#!/bin/sh
# Latency and correctness of live editing on the TeX stress test (rtex-fixtures latex_stress_test),
# measured with the replay example (crates/rtex-core/examples/replay.rs).
# Usage: scripts/stress-replay.sh [STRESS_ROOT] [OUT] [WHAT]
#   STRESS_ROOT  default ../rtex-fixtures/latex_stress_test
#   OUT          default build/stress-replay
#   WHAT         any of: edits main main-settle many-files (default: all)
# Writes OUT/<run>/report.json and OUT/<run>.txt per run, then OUT/summary.md
# (scripts/stress_summary.py). Run on an otherwise idle machine; log `uptime` with the results.
cd "$(dirname "$0")/.." || exit 1
ROOT=$(cd "${1:-../rtex-fixtures/latex_stress_test}" && pwd) || exit 1
OUT=${2:-build/stress-replay}
WHAT=${3:-edits main main-settle many-files}
cargo build --release -q -p rtex-core --example replay || exit 1
R=target/release/examples/replay
mkdir -p "$OUT"; rm -f "$OUT/DONE"
run() { # name, args...
  name=$1; shift
  echo "== $name start $(date +%T) load $(cut -d' ' -f1-3 /proc/loadavg)" | tee "$OUT/$name.txt"
  "$R" "$@" --out "$OUT/$name" >> "$OUT/$name.txt" 2>&1
  echo "== $name exit $? end $(date +%T) load $(cut -d' ' -f1-3 /proc/loadavg)" | tee -a "$OUT/$name.txt"
}
for w in $WHAT; do
  case $w in
    edits)
      for ops in "$ROOT"/suites/edits/snapshots/*/ops.jsonl; do
        s=$(basename "$(dirname "$ops")")
        run "edits-$s" ops --ops "$ops" --root "$ROOT" --cases "$ROOT/suites/edits/cases.json"
      done ;;
    main) run main-type type --project "$ROOT" --main main.tex --units 24 --cadence-ms 120 ;;
    main-settle) run main-type-settle type --project "$ROOT" --main main.tex --units 8 --cadence-ms 120 --settle-each ;;
    many-files) run many-files-type type --project "$ROOT/suites/scale/many-files" --main main.tex --units 24 --cadence-ms 120 ;;
  esac
done
python3 scripts/stress_summary.py "$OUT" > "$OUT/summary.md"
echo done > "$OUT/DONE"
