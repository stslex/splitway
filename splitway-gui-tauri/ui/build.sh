#!/usr/bin/env sh
# Build the read-only web frontend into `dist/` — WITHOUT npm. Uses `tsc` (type
# check, incl. the bindings drift guard in contract-check.ts) and `esbuild`
# (bundle), both provided by the Nix dev shell (`nix develop`). No npm registry
# access is needed. `tauri.conf.json` points `frontendDist` at `ui/dist`, so run
# this before `cargo build -p splitway-gui-tauri`.
#
# A future, network-reliable setup could restore Vite + @tauri-apps/api here
# (Phase 7d packaging); the TypeScript source is unchanged either way.
set -eu

cd "$(dirname "$0")"

echo "==> tsc --noEmit (type check + bindings drift guard)"
tsc --noEmit

echo "==> esbuild bundle"
rm -rf dist
mkdir -p dist/assets
esbuild src/main.ts \
  --bundle \
  --format=esm \
  --target=safari13 \
  --outfile=dist/assets/main.js

echo "==> copy static assets"
cp index.html dist/index.html
cp src/styles.css dist/styles.css

echo "==> built ui/dist"
