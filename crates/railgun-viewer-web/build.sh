#!/usr/bin/env sh
# Dr Rail web: builds the static site into crates/railgun-viewer-web/dist (index.html, shim.js,
# worker.js, waku-bundle.js, pkg/). Needs the wasm32-unknown-unknown target and wasm-bindgen-cli
# at the version of the lockfile (0.2.108); wasm-opt is used when present.
#   CARGO=cargo WASM_BINDGEN=wasm-bindgen LTO=true sh crates/railgun-viewer-web/build.sh
set -eu
cd "$(dirname "$0")/../.."
CARGO=${CARGO:-cargo}
WASM_BINDGEN=${WASM_BINDGEN:-wasm-bindgen}
OUT=crates/railgun-viewer-web/dist
WASM=target/wasm32-unknown-unknown/release/railgun_viewer_web.wasm

CARGO_PROFILE_RELEASE_LTO=${LTO:-true} CARGO_PROFILE_RELEASE_CODEGEN_UNITS=${CODEGEN_UNITS:-1} \
  $CARGO build --release --target wasm32-unknown-unknown -p railgun-viewer-web ${CARGO_EXTRA:-}

rm -rf "$OUT"
mkdir -p "$OUT"
$WASM_BINDGEN --target web --no-typescript --out-dir "$OUT/pkg" "$WASM"
if command -v wasm-opt >/dev/null 2>&1; then
  wasm-opt -O2 --enable-bulk-memory --enable-nontrapping-float-to-int --enable-sign-ext \
    --enable-mutable-globals "$OUT/pkg/railgun_viewer_web_bg.wasm" -o "$OUT/pkg/railgun_viewer_web_bg.wasm"
fi
cp crates/railgun-wallet/static/waku-bundle.js "$OUT/"
cp crates/railgun-viewer-web/web/shim.js crates/railgun-viewer-web/web/worker.js "$OUT/"
python3 crates/railgun-viewer-web/web/make_index.py crates/railgun-viewer/static/index.html "$OUT/index.html"
touch "$OUT/.nojekyll"
ls -la "$OUT" "$OUT/pkg"
