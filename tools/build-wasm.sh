#!/usr/bin/env bash
# Build the @quosh/* browser packages.
#
# Produces, under web/pkg/<name>/:
#   * ESM glue + .wasm + .d.ts  (wasm-bindgen --target web)   for the PWA
#   * CommonJS glue             (wasm-bindgen --target nodejs) for smoke tests
#
# Requires: rustup target add wasm32-unknown-unknown
#           wasm-bindgen (matching the pinned 0.2.100) on PATH.
set -euo pipefail
cd "$(dirname "$0")/.."

TARGET=wasm32-unknown-unknown
OUT=web/pkg
VER=0.1.0

cargo build --release --target "$TARGET" \
  -p quosh-proto-wasm -p quosh-predict-wasm -p quosh-client-wasm

for spec in "proto:quosh-proto-wasm:@quosh/proto" \
            "predict:quosh-predict-wasm:@quosh/predict" \
            "client:quosh-client-wasm:@quosh/client"; do
  name="${spec%%:*}"
  rest="${spec#*:}"
  crate="${rest%%:*}"
  pkg="${rest#*:}"
  wasm="target/$TARGET/release/${crate//-/_}.wasm"
  glue="${crate//-/_}.js"

  rm -rf "$OUT/$name"
  mkdir -p "$OUT/$name/node"
  wasm-bindgen --target web --out-dir "$OUT/$name" "$wasm"
  wasm-bindgen --target nodejs --out-dir "$OUT/$name/node" "$wasm"

  cat > "$OUT/$name/package.json" <<EOF
{
  "name": "$pkg",
  "version": "$VER",
  "type": "module",
  "module": "$glue",
  "types": "${crate//-/_}.d.ts",
  "files": ["$glue", "${crate//-/_}_bg.wasm", "${crate//-/_}.d.ts"]
}
EOF
  echo '{"type":"commonjs"}' > "$OUT/$name/node/package.json"
  echo "packaged $pkg -> $OUT/$name"
done