#!/bin/sh
# get-inlaysql.sh — download the InlaySQL engine library for this machine and
# the one-file client for your language, into the current directory, verified.
#
#   curl -fsSL https://github.com/inlaySQL/inlaysql/releases/latest/download/get-inlaysql.sh | sh -s -- php
#
#   sh get-inlaysql.sh php              # inlaysql.php + the library
#   sh get-inlaysql.sh python ruby      # several clients, one library
#   sh get-inlaysql.sh --dir vendor/inlaysql php
#   VERSION=v0.0.5 sh get-inlaysql.sh php   # a specific release (default: latest)
#   sh get-inlaysql.sh --dry-run php    # print the URLs, download nothing
#   INLAYSQL_BASE_URL=https://mirror/x sh get-inlaysql.sh php   # take the files from elsewhere
#
# Languages: php, python, ruby, csharp, java, c (the header only). Every file
# is checked against the .sha256 published beside it. POSIX sh; needs curl or
# wget, and sha256sum or shasum.

set -eu

REPO="inlaySQL/inlaysql"
VERSION="${VERSION:-latest}"
DIR="."
DRY=0
LANGS=""

usage() {
  sed -n '2,18p' "$0" | sed 's/^# \{0,1\}//'
  exit "${1:-0}"
}

while [ $# -gt 0 ]; do
  case "$1" in
    --dir) DIR="$2"; shift 2 ;;
    --dir=*) DIR="${1#--dir=}"; shift ;;
    --dry-run) DRY=1; shift ;;
    -h|--help) usage 0 ;;
    -*) echo "unknown option: $1" >&2; usage 1 ;;
    *) LANGS="$LANGS $1"; shift ;;
  esac
done
[ -n "$LANGS" ] || { echo "which language? one or more of: php python ruby csharp java c" >&2; usage 1; }

case "$(uname -s)" in
  Darwin) os=apple-darwin; ext=dylib ;;
  Linux) os=unknown-linux-gnu; ext=so ;;
  *) echo "unsupported OS: $(uname -s) — the file layer is Unix-only today" >&2; exit 1 ;;
esac
case "$(uname -m)" in
  arm64|aarch64) arch=aarch64 ;;
  x86_64|amd64) arch=x86_64 ;;
  *) echo "unsupported architecture: $(uname -m)" >&2; exit 1 ;;
esac
target="${arch}-${os}"
case "$target" in
  aarch64-apple-darwin|x86_64-unknown-linux-gnu) ;;
  *) echo "no prebuilt library for $target yet — build it: cargo build --release -p inlaysql-ffi" >&2; exit 1 ;;
esac

if [ -n "${INLAYSQL_BASE_URL:-}" ]; then
  base="${INLAYSQL_BASE_URL%/}"
elif [ "$VERSION" = latest ]; then
  base="https://github.com/$REPO/releases/latest/download"
else
  base="https://github.com/$REPO/releases/download/$VERSION"
fi

fetch() {
  # GitHub is https-only here; an explicit INLAYSQL_BASE_URL may be a plain
  # http mirror on a private network, and the checksum still gates the file.
  proto='=https'
  case "$1" in http://*) proto='=http,https' ;; esac
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL --proto "$proto" --tlsv1.2 -o "$2" "$1"
  elif command -v wget >/dev/null 2>&1; then
    wget -q -O "$2" "$1"
  else
    echo "need curl or wget" >&2; exit 1
  fi
}

verify() {
  # $1 = file in $DIR; its .sha256 sits beside it
  if command -v sha256sum >/dev/null 2>&1; then
    ( cd "$DIR" && sha256sum -c --quiet "$1.sha256" )
  elif command -v shasum >/dev/null 2>&1; then
    ( cd "$DIR" && shasum -a 256 -c --quiet "$1.sha256" )
  else
    echo "warning: no sha256sum/shasum — $1 not verified" >&2
  fi
  rm -f "$DIR/$1.sha256"
}

get() {
  if [ "$DRY" = 1 ]; then
    echo "$base/$1"
    return
  fi
  mkdir -p "$DIR"
  fetch "$base/$1" "$DIR/$1"
  fetch "$base/$1.sha256" "$DIR/$1.sha256"
  verify "$1"
  echo "  $DIR/$1"
}

files="libinlaysql_ffi-${target}.${ext}"
for lang in $LANGS; do
  case "$lang" in
    php) files="$files inlaysql.php" ;;
    python|py) files="$files inlaysql.py" ;;
    ruby|rb) files="$files inlaysql.rb" ;;
    csharp|cs|dotnet) files="$files InlaySQL.cs" ;;
    java) files="$files InlaySQL.java" ;;
    c|header) files="$files inlaysql.h" ;;
    *) echo "unknown language: $lang (php, python, ruby, csharp, java, c)" >&2; exit 1 ;;
  esac
done

[ "$DRY" = 1 ] || echo "InlaySQL ($VERSION, $target) →"
for f in $files; do get "$f"; done

if [ "$DRY" = 0 ]; then
  # The clients look for the library beside themselves under its plain name.
  ( cd "$DIR" && ln -sf "libinlaysql_ffi-${target}.${ext}" "libinlaysql_ffi.${ext}" )
  echo "  $DIR/libinlaysql_ffi.${ext} -> libinlaysql_ffi-${target}.${ext}"
  echo "done. Open a database: see the header of the client file you downloaded."
fi
