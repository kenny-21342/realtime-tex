#!/usr/bin/env bash
# LIGHT provisioning for a Claude Code cloud VM (Ubuntu 24.04, root). Must finish well inside the setup-script
# budget (about 5 minutes): apt tools and the Python packages the verification scripts use, in parallel.
# TeX Live is NOT installed here (it exceeded the budget: "Setup script failed with exit code -1"); the session
# starts scripts/cloud-texlive.sh in the background instead.
set -uo pipefail
export DEBIAN_FRONTEND=noninteractive
( timeout 200 apt-get update -qq && timeout 200 apt-get install -y -qq perl curl xz-utils fontconfig python3-pip ) \
  > /var/log/rtex-setup-apt.log 2>&1 &
( timeout 200 pip install --quiet --break-system-packages pymupdf numpy pillow scipy ) \
  > /var/log/rtex-setup-pip.log 2>&1 &
wait
echo "rtex light setup finished"
exit 0
