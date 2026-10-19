#!/usr/bin/env bash
# The version strings a release changes, in one place.
#
#   scripts/version.sh check                 every version string agrees with Cargo.toml, and
#                                            CHANGELOG.md has a section for that version
#   scripts/version.sh bump X.Y.Z [DATE]     set X.Y.Z everywhere (Cargo.lock through cargo) and
#                                            move CHANGELOG.md's Unreleased entries under
#                                            "## X.Y.Z (DATE)"; DATE defaults to today (UTC)
#   scripts/version.sh notes [X.Y.Z]         print CHANGELOG.md's section for X.Y.Z (default:
#                                            Cargo.toml's version), for release notes
set -euo pipefail
cd "$(dirname "$0")/.."

current() { sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -n1; }

# file|perl regex with one capture group: the version the file states
LOCATIONS=(
  'README.md|^Version (\S+)\. It works'
  'README.zh-CN.md|^当前版本 (\S+)。'
  'docs/display-list.md|^Format revision: \*\*\d+\*\* \(rtex (\S+)\)'
  'include/rtex.h|C ABI, rtex (\S+) \(unstable'
  'tex/latex/rtex-capture.sty|\\ProvidesPackage\{rtex-capture\}\[\S+ v(\S+) rtex layout capture\]'
)

stated() { # FILE REGEX: the versions the file states, one per line
  perl -CSD -Mutf8 -ne "print \"\$1\n\" if /$2/" "$1"
}

check() {
  local want status=0
  want=$(current)
  for loc in "${LOCATIONS[@]}"; do
    local file=${loc%%|*} re=${loc#*|} got
    got=$(stated "$file" "$re")
    if [ -z "$got" ]; then
      echo "$file: no version line found" >&2
      status=1
    elif [ "$got" != "$want" ]; then
      echo "$file: states $got, Cargo.toml says $want" >&2
      status=1
    fi
  done
  local lock
  lock=$(awk '/^name = "rtex/ {getline; print}' Cargo.lock | sed 's/version = "\(.*\)"/\1/' | sort -u)
  if [ "$lock" != "$want" ]; then
    echo "Cargo.lock: rtex crates at $(echo "$lock" | tr '\n' ' ')- run cargo update -w" >&2
    status=1
  fi
  if ! grep -q "^## $want (" CHANGELOG.md; then
    echo "CHANGELOG.md: no \"## $want (date)\" section" >&2
    status=1
  fi
  [ $status -eq 0 ] && echo "version $want: all version strings agree"
  return $status
}

bump() {
  local new=$1 date=${2:-$(date -u +%F)} old
  old=$(current)
  [[ $new =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || { echo "not a version: $new" >&2; exit 2; }
  [ "$new" != "$old" ] || { echo "already at $new" >&2; exit 2; }
  ! grep -q "^## $new (" CHANGELOG.md || { echo "CHANGELOG.md already has $new" >&2; exit 2; }
  export OLD=$old NEW=$new DATE=$date
  perl -pi -e 's/^version = "\Q$ENV{OLD}\E"$/version = "$ENV{NEW}"/ if $. < 20' Cargo.toml
  perl -CSD -Mutf8 -pi -e 's/^Version \Q$ENV{OLD}\E\./Version $ENV{NEW}./' README.md
  perl -CSD -Mutf8 -pi -e 's/^当前版本 \Q$ENV{OLD}\E。/当前版本 $ENV{NEW}。/' README.zh-CN.md
  perl -pi -e 's/\(rtex \Q$ENV{OLD}\E\)/(rtex $ENV{NEW})/ if /^Format revision/' docs/display-list.md
  perl -pi -e 's/C ABI, rtex \Q$ENV{OLD}\E /C ABI, rtex $ENV{NEW} /' include/rtex.h
  perl -pi -e '($d = $ENV{DATE}) =~ tr{-}{/}; s/\[\S+ v\Q$ENV{OLD}\E rtex layout capture\]/[$d v$ENV{NEW} rtex layout capture]/' \
    tex/latex/rtex-capture.sty
  perl -0pi -e 's/^## Unreleased\n/## Unreleased\n\n## $ENV{NEW} ($ENV{DATE})\n/m or die "no ## Unreleased heading\n"' CHANGELOG.md
  cargo update -w --quiet
  check
}

notes() {
  local v=${1:-$(current)}
  awk -v h="## $v (" 'index($0, h) == 1 {on = 1; next} on && /^## / {exit} on' CHANGELOG.md |
    sed -e '/./,$!d' | sed -e ':a' -e '/^\n*$/{$d;N;ba' -e '}'
}

case ${1:-} in
  check) check ;;
  bump) [ $# -ge 2 ] || { echo "usage: $0 bump X.Y.Z [DATE]" >&2; exit 2; }; bump "$2" "${3:-}" ;;
  notes) notes "${2:-}" ;;
  *) sed -n '2,10p' "$0" >&2; exit 2 ;;
esac
