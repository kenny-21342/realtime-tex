#!/usr/bin/env bash
# Provision a Claude Code cloud VM (Ubuntu 24.04, x86_64, root) for rtex.
# Installs TeX Live from tlnet into /opt/texlive via scripts/install-texlive.sh, plus the Python tools the
# verification scripts use, and writes /etc/profile.d/rtex-texlive.sh (PATH and RTEX_TEXLIVE_BIN).
# Idempotent. Needs network access to the tlnet mirror (default mirror.clarkson.edu: add it to the
# environment's allowed domains) and to archive.ubuntu.com, pypi.org, github.com.
# Environment: TEXDIR (default /opt/texlive), RTEX_REPO, RTEX_BRANCH, TL_REPO (see install-texlive.sh).
set -uo pipefail
TEXDIR=${TEXDIR:-/opt/texlive}
REPO=${RTEX_REPO:-https://github.com/kenny-21342/realtime-tex.git}
BRANCH=${RTEX_BRANCH:-kenny/main}
BIN="$TEXDIR/bin/x86_64-linux"
export DEBIAN_FRONTEND=noninteractive
t0=$(date +%s)
say() { echo "[rtex-setup +$(( $(date +%s) - t0 ))s] $*"; }

say "apt packages"
(apt-get update -qq && apt-get install -y -qq perl curl xz-utils fontconfig python3-pip) || say "apt failed (continuing)"

say "python tools"
pip install --quiet --break-system-packages pymupdf numpy pillow scipy || say "pip failed (continuing)"

if [ ! -x "$BIN/lualatex" ]; then
  say "TeX Live into $TEXDIR"
  tmp=$(mktemp -d)
  git clone --quiet --depth 1 --branch "$BRANCH" "$REPO" "$tmp/src" || { say "clone failed"; exit 1; }
  (cd "$tmp/src" && bash scripts/install-texlive.sh "$TEXDIR") || { say "TeX Live install failed"; exit 1; }
fi

cat > /etc/profile.d/rtex-texlive.sh <<EOP
export PATH="$BIN:\$PATH"
export RTEX_TEXLIVE_BIN="$BIN"
EOP
grep -q rtex-texlive /root/.bashrc 2>/dev/null || echo '. /etc/profile.d/rtex-texlive.sh' >> /root/.bashrc
say "done: $("$BIN/lualatex" --version | head -1)"
