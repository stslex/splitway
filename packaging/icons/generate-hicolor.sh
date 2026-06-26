#!/usr/bin/env bash
# Rasterize the Splitway app icon into a freedesktop hicolor theme tree, named
# by the app_id (io.github.stslex.splitway) so the .desktop `Icon=` key and the
# GUI's Wayland app_id both resolve to it.
#
# The generated tree (packaging/icons/hicolor/) is committed so the deb/rpm/
# pacman packages and the tarballs can ship icons without a rasterizer in their
# build jobs. Re-run this after editing assets/icon/splitway-icon.svg:
#
#     packaging/icons/generate-hicolor.sh            # writes ./hicolor
#     packaging/icons/generate-hicolor.sh /some/dir  # writes /some/dir/hicolor
#
# Requires rsvg-convert (librsvg); falls back to ImageMagick magick/convert.
# The sizes mirror the Nix flake's icon derivation (see flake.nix).
set -euo pipefail

APP_ID="io.github.stslex.splitway"
SIZES=(16 24 32 48 64 128 256 512)

here="$(cd "$(dirname "$0")" && pwd)"
repo_root="$(cd "$here/../.." && pwd)"
src="$repo_root/assets/icon/splitway-icon.svg"
dest="${1:-$here}"
hic="$dest/hicolor"

[ -f "$src" ] || { echo "error: source icon not found: $src" >&2; exit 1; }

raster() { # <size> <out-png>
    if command -v rsvg-convert >/dev/null 2>&1; then
        rsvg-convert -w "$1" -h "$1" "$src" -o "$2"
    elif command -v magick >/dev/null 2>&1; then
        magick -background none "$src" -resize "${1}x${1}" "$2"
    elif command -v convert >/dev/null 2>&1; then
        convert -background none "$src" -resize "${1}x${1}" "$2"
    else
        echo "error: need rsvg-convert (librsvg) or ImageMagick to rasterize" >&2
        exit 1
    fi
}

install -d "$hic/scalable/apps"
cp "$src" "$hic/scalable/apps/$APP_ID.svg"
for s in "${SIZES[@]}"; do
    install -d "$hic/${s}x${s}/apps"
    raster "$s" "$hic/${s}x${s}/apps/$APP_ID.png"
done

echo "icons -> $hic"
