#!/usr/bin/env bash
# Builds the browser demo of waku-light into examples/web/pkg. Needs the wasm32 target and
# wasm-bindgen-cli at the version the workspace pins (0.2.108).
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../../../.." && pwd)"
cd "$root"
want="0.2.108"
have="$(wasm-bindgen --version 2>/dev/null | awk '{print $2}' || true)"
if [ "$have" != "$want" ]; then
  echo "wasm-bindgen-cli $want required (found: ${have:-none}):"
  echo "  cargo install wasm-bindgen-cli --version $want --locked"
  exit 1
fi
cargo build --release -p waku-light --example web_fleet --target wasm32-unknown-unknown
wasm-bindgen --target web --no-typescript --out-dir "$here/pkg" \
  target/wasm32-unknown-unknown/release/examples/web_fleet.wasm
ls -la "$here/pkg"
echo "serve with: python3 -m http.server 8088 -d $here   then open http://localhost:8088 (page) or http://localhost:8088/worker.html (Web Worker)"
