#!/usr/bin/env bash
# Builds the ratty wasm bundle into site/pkg/.
#
# Requirements:
#   rustup target add wasm32-unknown-unknown
#   cargo install wasm-bindgen-cli --version 0.2.118   # must match Cargo.toml
#   (optional) wasm-opt from binaryen, for a much smaller bundle
set -euo pipefail

cd "$(dirname "$0")/.."

# getrandom 0.3+ selects its web backend via this cfg; 0.2 uses the "js"
# feature already declared in Cargo.toml.
export RUSTFLAGS='--cfg getrandom_backend="wasm_js"'

cargo build --lib --target wasm32-unknown-unknown --profile wasm-release

wasm-bindgen \
    --target web \
    --out-dir site/pkg \
    --no-typescript \
    target/wasm32-unknown-unknown/wasm-release/ratty.wasm

if command -v wasm-opt >/dev/null 2>&1; then
    wasm-opt -Oz --output site/pkg/ratty_bg.wasm.opt site/pkg/ratty_bg.wasm
    mv site/pkg/ratty_bg.wasm.opt site/pkg/ratty_bg.wasm
    echo "wasm-opt: optimized"
else
    echo "wasm-opt not found; shipping unoptimized bundle (fine for local dev)"
fi

ls -lh site/pkg/
echo "done — serve the site with: python3 -m http.server -d site"
echo "(copy or symlink ../transmissions into site/ first, or use the CI layout)"
