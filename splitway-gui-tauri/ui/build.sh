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

# Bundle the local IBM Plex woff2 (OFL-1.1) referenced by @font-face in
# styles.css. The sandboxed webview has a self-only CSP and cannot reach a CDN,
# so the fonts MUST travel inside dist/ — both for the packaged build and for a
# `nix develop` run. See src/fonts/README.md.
echo "==> copy bundled fonts"
mkdir -p dist/assets/fonts
cp src/fonts/*.woff2 dist/assets/fonts/
# OFL-1.1 requires the license + copyright notice to travel with the
# redistributed faces; ship it inside the bundle so it rides along with the
# embedded fonts (the packaged build also installs it under share/licenses).
cp src/fonts/LICENSE-OFL.txt dist/assets/fonts/

echo "==> built ui/dist"
