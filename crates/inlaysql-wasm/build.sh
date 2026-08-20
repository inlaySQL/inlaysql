#!/usr/bin/env bash
#
# Build the WASM module and report its size.
#
#   ./crates/inlaysql-wasm/build.sh          # build + size
#   ./crates/inlaysql-wasm/build.sh --serve  # …then serve the demo on :8000
#
# This produces `www/pkg`, which both demos load: the browser page here and the
# Cloudflare Worker in `edge/`. Run it before `npm run smoke` in either.
#
# Needs `wasm-bindgen-cli` (cargo install wasm-bindgen-cli) matching the
# wasm-bindgen version in Cargo.toml.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
OUT="$ROOT/crates/inlaysql-wasm/www/pkg"
WASM="$ROOT/target/wasm32-unknown-unknown/release-wasm/inlaysql_wasm.wasm"

cargo build --manifest-path "$ROOT/Cargo.toml" \
  -p inlaysql-wasm --target wasm32-unknown-unknown --profile release-wasm

if ! command -v wasm-bindgen >/dev/null; then
  echo "wasm-bindgen-cli is not installed; the module is at $WASM" >&2
  echo "install it with: cargo install wasm-bindgen-cli" >&2
  exit 1
fi

mkdir -p "$OUT"
wasm-bindgen --target web --no-typescript --out-dir "$OUT" "$WASM"

# The database the edge worker ships, written by the *native* build from the
# same corpus the browser page seeds itself with. Built here so that one
# command leaves both demos runnable.
cargo run --manifest-path "$ROOT/Cargo.toml" -q -p inlaysql-wasm --example edge_fixture

# Size is a first-class number for a module that ships over the network, so it
# is printed rather than left for someone to check.
raw=$(wc -c < "$OUT/inlaysql_wasm_bg.wasm")
gz=$(gzip -c "$OUT/inlaysql_wasm_bg.wasm" | wc -c)
printf '\nwasm: %s bytes (%s KiB), gzipped %s bytes (%s KiB)\n' \
  "$raw" "$((raw / 1024))" "$gz" "$((gz / 1024))"

if [ "${1:-}" = "--serve" ]; then
  echo
  echo "serving http://localhost:8000/ — ctrl-c to stop"
  cd "$ROOT/crates/inlaysql-wasm/www" && python3 -m http.server 8000
fi
