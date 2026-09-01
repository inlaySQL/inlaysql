#!/usr/bin/env bash
#
# Build the WASM module and report its size.
#
#   ./crates/inlaysql-wasm/build.sh          # build + size
#   ./crates/inlaysql-wasm/build.sh --serve  # …then serve the demo on :8000
#
# This produces `www/pkg`, which every front-end consumer loads: the main
# browser page, the demos staged into `www/demo/`, and the Cloudflare Worker
# in `edge/`. Run it before `npm run smoke` in any of them.
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

# The static-site demo stages into www/demo/site-search/ beside the module,
# which is what Pages publishes verbatim: the source of truth is demos/,
# and the database is built natively by its own fixture, exactly like the
# edge one.
DEMO_OUT="$ROOT/crates/inlaysql-wasm/www/demo/site-search"
mkdir -p "$DEMO_OUT"
cargo run --manifest-path "$ROOT/Cargo.toml" -q -p inlaysql-wasm --example site_search_fixture
cp "$ROOT/crates/inlaysql-wasm/demos/site-search/index.html" "$DEMO_OUT/index.html"
cp "$ROOT/crates/inlaysql-wasm/demos/site-search/site.inlay" "$DEMO_OUT/site.inlay"

# The playground has no fixture to build — it starts from an empty database
# and the lessons create everything — so staging is a copy.
DEMO_PG="$ROOT/crates/inlaysql-wasm/www/demo/playground"
mkdir -p "$DEMO_PG"
cp "$ROOT/crates/inlaysql-wasm/demos/playground/index.html" "$DEMO_PG/index.html"

# The JS SDK demo. The HTML is committed here; the SDK sources and the engine
# module are copied from where they live. With a sibling inlaysql-js checkout
# (SDK_DIR, default ../inlaysql-js) this leaves the demo fully runnable from
# `--serve`; the org-site workflow supplies the SDK sources in CI, and the
# engine module is the same www/pkg this script just built — no version skew.
DEMO_JS="$ROOT/crates/inlaysql-wasm/www/demo/js-sdk"
SDK_DIR="${SDK_DIR:-$ROOT/../inlaysql-js}"
mkdir -p "$DEMO_JS/engine"
cp "$ROOT/crates/inlaysql-wasm/demos/js-sdk/index.html" "$DEMO_JS/index.html"
cp "$ROOT/crates/inlaysql-wasm/demos/js-sdk/simple.html" "$DEMO_JS/simple.html"
cp "$OUT/inlaysql_wasm.js" "$DEMO_JS/engine/inlaysql_wasm.js"
cp "$OUT/inlaysql_wasm_bg.wasm" "$DEMO_JS/engine/inlaysql_wasm_bg.wasm"
if [ -d "$SDK_DIR/packages" ]; then
  for pkg in wasm core orm storage simple; do
    mkdir -p "$DEMO_JS/packages/$pkg"
    cp -R "$SDK_DIR/packages/$pkg/src" "$DEMO_JS/packages/$pkg/src"
  done
  echo "js-sdk demo: SDK sources from $SDK_DIR"
else
  echo "js-sdk demo: no SDK checkout at $SDK_DIR — sources must be staged by the deploy workflow" >&2
fi

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
