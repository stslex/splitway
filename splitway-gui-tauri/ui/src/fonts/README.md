# Bundled fonts — IBM Plex (OFL-1.1)

The Tauri webview is sandboxed with a self-only CSP (`default-src 'self'`, no
`connect-src` to any CDN), so webfonts **cannot** be fetched at runtime. These
`woff2` files are therefore vendored and shipped inside the frontend bundle and
referenced by local `@font-face` rules in `../styles.css`. Without them the
shipped app would silently fall back to the platform sans/mono — the
machine→human typographic split the design relies on would degrade.

## What is here

Latin + symbols subsets of IBM Plex, one file per weight the UI actually uses:

| File                          | Family         | `font-weight` |
| ----------------------------- | -------------- | ------------- |
| `IBMPlexSans-Regular.woff2`   | IBM Plex Sans  | 400           |
| `IBMPlexSans-Medium.woff2`    | IBM Plex Sans  | 500           |
| `IBMPlexSans-SemiBold.woff2`  | IBM Plex Sans  | 600           |
| `IBMPlexSans-Bold.woff2`      | IBM Plex Sans  | 700           |
| `IBMPlexMono-Regular.woff2`   | IBM Plex Mono  | 400           |

IBM Plex Mono is used only at weight 400 (machine data); every 500/600/700 in
the stylesheet is IBM Plex Sans — hence five files, not eight.

Glyphs the UI uses that IBM Plex does not contain (`✕` U+2715, `⚠` U+26A0)
are *not* in these subsets either; the browser falls back to a platform font for
those individual glyphs — exactly as it would with the full upstream font.

## License

IBM Plex is licensed under the SIL Open Font License 1.1 — see
[`LICENSE-OFL.txt`](LICENSE-OFL.txt). The OFL permits bundling/embedding and
redistribution (including glyph subsets) provided the copyright notice and the
license travel with the files. That is why both live here, why `build.sh` copies
`LICENSE-OFL.txt` into the bundle alongside the fonts, and why the packaged
`nix build .#splitway-gui` installs it under `share/licenses/`.

These files are **glyph-reduced subsets**, not the Original Version: glyphs are
dropped, but nothing is redesigned and nothing is renamed — they retain IBM
Plex's own internal family names (`IBM Plex Sans` / `IBM Plex Mono`) and the
upstream copyright/license name-table entries.

## Provenance / how to regenerate

Subset from the upstream TrueType masters that ship in nixpkgs `ibm-plex`, using
`pyftsubset` (fontTools) — no network access needed (the exact woff2 bytes still
track the fontTools/brotli versions in nixpkgs):

```sh
TTF=$(nix build --no-link --print-out-paths nixpkgs#ibm-plex)/share/fonts/truetype
UNI="U+0000-00FF,U+0131,U+0152-0153,U+0160-0161,U+0178,U+017D-017E,U+0192,\
U+02C6,U+02DA,U+02DC,U+2000-206F,U+2070,U+2074,U+20AC,U+2122,U+2190-2193,\
U+2202,U+2206,U+220F,U+2211,U+2212,U+2215,U+2219,U+221A,U+221E,U+222B,U+2248,\
U+2260,U+2264,U+2265,U+25A0-25FF,U+2605-2606,U+2713-2717,U+26A0"

nix shell --impure --expr \
  'with (builtins.getFlake "github:NixOS/nixpkgs/nixos-unstable").legacyPackages.x86_64-linux;
   [ (python3.withPackages (ps: with ps; [ fonttools brotli ])) ]' \
  -c sh -c '
    for w in Sans-Regular Sans-Medium Sans-SemiBold Sans-Bold Mono-Regular; do
      python3 -m fontTools.subset "$TTF/IBMPlex$w.ttf" \
        --unicodes="'"$UNI"'" --layout-features="*" --flavor=woff2 \
        --output-file="IBMPlex$w.woff2"
    done'
```

The unicode set covers Basic Latin, Latin-1, general punctuation, currency,
arrows, common math operators and the dingbats/symbols the UI renders.
