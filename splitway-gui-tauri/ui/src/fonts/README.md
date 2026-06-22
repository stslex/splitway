# Bundled fonts — IBM Plex glyphs, renamed (OFL-1.1)

The Tauri webview is sandboxed with a self-only CSP (`default-src 'self'`, no
`connect-src` to any CDN), so webfonts **cannot** be fetched at runtime. These
`woff2` files are therefore vendored and shipped inside the frontend bundle and
referenced by local `@font-face` rules in `../styles.css`. Without them the
shipped app would silently fall back to the platform sans/mono — the
machine→human typographic split the design relies on would degrade.

## What is here

Latin + symbols subsets of IBM Plex, **renamed** to the `Splitway Sans` /
`Splitway Mono` families (see "Naming & license" below), one file per weight the
UI actually uses:

| File                          | Family         | `font-weight` |
| ----------------------------- | -------------- | ------------- |
| `SplitwaySans-Regular.woff2`  | Splitway Sans  | 400           |
| `SplitwaySans-Medium.woff2`   | Splitway Sans  | 500           |
| `SplitwaySans-SemiBold.woff2` | Splitway Sans  | 600           |
| `SplitwaySans-Bold.woff2`     | Splitway Sans  | 700           |
| `SplitwayMono-Regular.woff2`  | Splitway Mono  | 400           |

The glyphs are unmodified IBM Plex; only the name table (and the matching
`@font-face` family) is changed. `Splitway Mono` is used only at weight 400
(machine data); every 500/600/700 in the stylesheet is `Splitway Sans` — hence
five files, not eight.

Glyphs the UI uses that IBM Plex does not contain (`✕` U+2715, `⚠` U+26A0)
are *not* in these subsets either; the browser falls back to a platform font for
those individual glyphs — exactly as it would with the full upstream font.

## Naming & license

IBM Plex is licensed under the SIL Open Font License 1.1 — see
[`LICENSE-OFL.txt`](LICENSE-OFL.txt). The OFL reserves the font name **"Plex"**:
clause 3 forbids a *Modified Version* from using a Reserved Font Name as the name
presented to users. A glyph subset is a Modified Version, so these faces are
renamed off the reserved name — the presented names (family / full / PostScript /
typographic) are `Splitway Sans` / `Splitway Mono`, with **no "Plex"**. IBM's
copyright (name ID 0) and the OFL license entry (name ID 13) are kept for
attribution, the upstream design is otherwise untouched, and IBM's name is not
used to promote this Modified Version (clause 4).

The OFL also requires the copyright notice and license to travel with the files
on redistribution. That is why [`LICENSE-OFL.txt`](LICENSE-OFL.txt) lives here,
why `build.sh` copies it into the bundle alongside the fonts, and why the
packaged `nix build .#splitway-gui` installs it under `share/licenses/`.

## Provenance / how to regenerate

Subset from the upstream TrueType masters that ship in nixpkgs `ibm-plex`, then
rename the name table — `fontTools`, no network access needed (the exact woff2
bytes still track the fontTools/brotli versions in nixpkgs). Run from this
directory:

```sh
export TTF="$(nix build --no-link --print-out-paths nixpkgs#ibm-plex)/share/fonts/truetype"
export UNI="U+0000-00FF,U+0131,U+0152-0153,U+0160-0161,U+0178,U+017D-017E,U+0192,U+02C6,U+02DA,U+02DC,U+2000-206F,U+2070,U+2074,U+20AC,U+2122,U+2190-2193,U+2202,U+2206,U+220F,U+2211,U+2212,U+2215,U+2219,U+221A,U+221E,U+222B,U+2248,U+2260,U+2264,U+2265,U+25A0-25FF,U+2605-2606,U+2713-2717,U+26A0"

nix shell --impure --expr \
  'with (builtins.getFlake "github:NixOS/nixpkgs/nixos-unstable").legacyPackages.x86_64-linux;
   [ (python3.withPackages (ps: with ps; [ fonttools brotli ])) ]' \
  -c python3 - <<'PY'
import os
from fontTools.ttLib import TTFont
from fontTools.subset import Subsetter, Options, parse_unicodes
TTF, UNI = os.environ["TTF"], os.environ["UNI"]
faces = [("IBMPlexSans-Regular",  "SplitwaySans-Regular"),
         ("IBMPlexSans-Medium",   "SplitwaySans-Medium"),
         ("IBMPlexSans-SemiBold", "SplitwaySans-SemiBold"),
         ("IBMPlexSans-Bold",     "SplitwaySans-Bold"),
         ("IBMPlexMono-Regular",  "SplitwayMono-Regular")]
RENAME = {1, 3, 4, 6, 16, 17, 18, 20, 21}  # presented-name IDs; keep 0/7/13 (copyright/trademark/license)
for src, dst in faces:
    opts = Options(); opts.flavor = "woff2"; opts.layout_features = ["*"]; opts.name_IDs = ["*"]
    f = TTFont(os.path.join(TTF, src + ".ttf"))
    s = Subsetter(options=opts); s.populate(unicodes=parse_unicodes(UNI)); s.subset(f)
    name = f["name"]
    for rec in list(name.names):
        if rec.nameID in RENAME:
            new = rec.toUnicode().replace("IBM Plex", "Splitway").replace("IBMPlex", "Splitway")
            name.setName(new, rec.nameID, rec.platformID, rec.platEncID, rec.langID)
    f.flavor = "woff2"; f.save(dst + ".woff2")
PY
```

The unicode set covers Basic Latin, Latin-1, general punctuation, currency,
arrows, common math operators and the dingbats/symbols the UI renders.
