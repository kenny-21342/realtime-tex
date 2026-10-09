#!/usr/bin/env bash
# Install TeX Live (tlnet, via scripts/install-texlive.sh) into /opt/texlive on a cloud VM and write
# /etc/profile.d/rtex-texlive.sh. Run it from the repo root IN THE SESSION, in the background:
#   nohup bash scripts/cloud-texlive.sh > /tmp/texlive-install.log 2>&1 &
# then poll the log; it prints "TEXLIVE READY" at the end. Idempotent. Needs mirror.clarkson.edu in the
# environment's allowed domains (or set TL_REPO).
set -uo pipefail
cd "$(dirname "$0")/.." || exit 1
TEXDIR=${TEXDIR:-/opt/texlive}
BIN="$TEXDIR/bin/x86_64-linux"
t0=$(date +%s)
if [ ! -x "$BIN/lualatex" ]; then
  bash scripts/install-texlive.sh "$TEXDIR" || { echo "TEXLIVE FAILED after $(( $(date +%s) - t0 ))s"; exit 1; }
fi
cat > /etc/profile.d/rtex-texlive.sh <<EOP
export PATH="$BIN:\$PATH"
export RTEX_TEXLIVE_BIN="$BIN"
EOP
echo "TEXLIVE READY after $(( $(date +%s) - t0 ))s: $("$BIN/lualatex" --version | head -1)"
