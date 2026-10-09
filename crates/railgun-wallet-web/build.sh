#!/usr/bin/env sh
# Railgun wallet web: builds the static site into crates/railgun-wallet-web/dist (index.html,
# shim.js, worker.js, waku-bundle.js, pkg/). Needs the wasm32-unknown-unknown target and
# wasm-bindgen-cli at the lockfile's version (0.2.108); wasm-opt is used when present.
#   CARGO=cargo WASM_BINDGEN=wasm-bindgen LTO=true sh crates/railgun-wallet-web/build.sh
set -eu
cd "$(dirname "$0")/../.."
CARGO=${CARGO:-cargo}
WASM_BINDGEN=${WASM_BINDGEN:-wasm-bindgen}
OUT=crates/railgun-wallet-web/dist
WASM=target/wasm32-unknown-unknown/release/railgun_wallet_web.wasm

CARGO_PROFILE_RELEASE_LTO=${LTO:-true} CARGO_PROFILE_RELEASE_CODEGEN_UNITS=${CODEGEN_UNITS:-1} \
  $CARGO build --release --target wasm32-unknown-unknown -p railgun-wallet-web ${CARGO_EXTRA:-}

rm -rf "$OUT"
mkdir -p "$OUT"
$WASM_BINDGEN --target web --no-typescript --out-dir "$OUT/pkg" "$WASM"
if command -v wasm-opt >/dev/null 2>&1; then
  wasm-opt -O2 --enable-bulk-memory --enable-nontrapping-float-to-int --enable-sign-ext \
    --enable-mutable-globals --enable-reference-types "$OUT/pkg/railgun_wallet_web_bg.wasm" \
    -o "$OUT/pkg/railgun_wallet_web_bg.wasm"
fi
cp crates/railgun-wallet/static/waku-bundle.js "$OUT/"
cp crates/railgun-wallet-web/web/shim.js crates/railgun-wallet-web/web/worker.js "$OUT/"
python3 crates/railgun-wallet-web/web/make_index.py crates/railgun-wallet/static/index.html "$OUT/index.html"
touch "$OUT/.nojekyll"
# Netlify: proxy /poi to the POI node server-side (its IPv6 is dead, IPv6-only clients cannot
# reach it directly — the wallet prefers this relay when it answers), and make browsers
# revalidate the unhashed assets so a redeploy is picked up immediately.
cat > "$OUT/_redirects" <<'EOF'
/poi   https://ppoi.fdi.network/   200
/poi/* https://ppoi.fdi.network/:splat 200
EOF
cat > "$OUT/_headers" <<'EOF'
/*
  Cache-Control: no-cache
EOF
ls -la "$OUT" "$OUT/pkg"
