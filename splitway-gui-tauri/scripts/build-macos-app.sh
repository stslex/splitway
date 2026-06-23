#!/usr/bin/env bash
#
# build-macos-app.sh — build the distributable, ad-hoc-signed (unsigned)
# Splitway.app on macOS. This is the macOS counterpart of the Linux Nix package
# (`nix build .#splitway-gui`): it is an ADDITIVE, machine-bound build path that
# does not touch the Linux/Nix build. The Tauri bundler is invoked ONLY here (via
# `cargo tauri build`); `cargo build` and the Nix package never read the bundle
# config, so this perturbs nothing else. See docs/design/macos-self-install.md.
#
# What it does:
#   1. builds the helper binaries (splitway-daemon, splitway) in release
#   2. builds the web frontend (tsc + esbuild) into ui/dist
#   3. stages the binaries + the GUI LaunchDaemon plist + bootstrap.sh + an icon
#      into the gitignored resources/ dir (embedded as app Resources)
#   4. runs `cargo tauri build --config tauri.bundle.macos.json --bundles app`
#
# Toolchain: this runs the whole build inside the flake dev shell (`nix
# develop`), which brings a consistent Rust toolchain AND the Nix clang stdenv
# (the latter sidesteps the Xcode-license gate on its own — no DEVELOPER_DIR
# override, which would break the Nix clang's SDK lookup; and the system rustc,
# which may be too old for the workspace's locked deps, is avoided). The frontend
# + bundler tools (cargo-tauri, node, tsc, esbuild, rsvg-convert) are layered on
# via `nix shell` since the darwin dev shell does not include them. So a plain
# `bash scripts/build-macos-app.sh` just works.
#
# Output: target/release/bundle/macos/Splitway.app

set -euo pipefail

# Run from the tauri crate dir (this script lives in <crate>/scripts).
CRATE_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$CRATE_DIR"
REPO_ROOT="$(cd "$CRATE_DIR/.." && pwd)"

# The build steps run inside the toolchain shell. They are written to a temp file
# (rather than an inline -c string) to avoid fragile multi-level shell quoting;
# the paths they need are passed through the environment.
BUILD_STEPS="$(mktemp -t splitway-build-steps.XXXXXX.sh)"
trap 'rm -f "$BUILD_STEPS"' EXIT

cat >"$BUILD_STEPS" <<'STEPS'
set -euo pipefail
# DEVELOPER_DIR pointed at the Command Line Tools would break the Nix clang's SDK
# lookup; the dev shell's own clang handles the Xcode license, so clear it.
unset DEVELOPER_DIR
cd "$CRATE_DIR"

echo "==> building helper binaries (release)"
cargo build --release -p splitway-daemon -p splitway-cli

echo "==> building the web frontend (tsc + esbuild)"
( cd ui && sh build.sh )

echo "==> staging app resources"
rm -rf "$STAGE"
mkdir -p "$STAGE"
cp "$REPO_ROOT/target/release/splitway-daemon" "$STAGE/splitway-daemon"
cp "$REPO_ROOT/target/release/splitway"        "$STAGE/splitway"
# Stage the GUI plist variant under the plain LABEL.plist name bootstrap.sh looks
# up (it copies "${SELF_DIR}/com.splitway.daemon.plist"); the ".gui" suffix only
# distinguishes it in the repo from the manual/sudo template.
cp "$GUI_PLIST"                                 "$STAGE/com.splitway.daemon.plist"
cp scripts/bootstrap.sh                         "$STAGE/bootstrap.sh"
chmod 0755 "$STAGE/bootstrap.sh"

# A crisp 1024px icon source (the committed icons/icon.png is only 128px):
# rasterize the high-res SVG, then let `cargo tauri icon` generate the full macOS
# icon set (incl. icon.icns) the bundler folds into Splitway.app. The bundler
# needs the named multi-size set, not a lone 1024px png.
rsvg-convert -w 1024 -h 1024 "$ICON_SVG" -o "$STAGE/icon-1024.png"
cargo tauri icon "$STAGE/icon-1024.png" --output "$STAGE/icons"

# Force the embedded-frontend codegen to re-run: `generate_context!` embeds
# ui/dist at compile time, so a frontend-only change (no Rust edit) would
# otherwise leave the previous dist baked into an up-to-date binary. Touching
# lib.rs (the macro site) guarantees a fresh embed every build.
touch src/lib.rs

echo "==> cargo tauri build (.app only, ad-hoc/unsigned)"
# The bundle overlay is named so Tauri does NOT auto-merge it on a plain
# `cargo build`/clippy (which would validate the staged resources before they
# exist) — it is applied ONLY here, via --config.
cargo tauri build --config tauri.bundle.macos.json --bundles app

echo "==> built: $REPO_ROOT/target/release/bundle/macos/Splitway.app"
STEPS

# Inside `nix develop` (Rust + Nix clang), layer the frontend/bundler tools via
# `nix shell`, then run the staged build steps. The paths are exported so the
# steps file needs no quoting gymnastics.
export CRATE_DIR REPO_ROOT BUILD_STEPS STAGE="${CRATE_DIR}/resources" \
  GUI_PLIST="${REPO_ROOT}/packaging/launchd/com.splitway.daemon.gui.plist" \
  ICON_SVG="${REPO_ROOT}/assets/icon/splitway-icon.svg"

exec nix develop "$REPO_ROOT" --command bash -euo pipefail -c '
  nix shell \
    nixpkgs#cargo-tauri \
    nixpkgs#nodejs \
    nixpkgs#typescript \
    nixpkgs#esbuild \
    nixpkgs#librsvg \
    --command bash "$BUILD_STEPS"
'
