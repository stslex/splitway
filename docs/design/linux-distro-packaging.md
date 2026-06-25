# Linux distribution packaging — apt / dnf / pacman on GitHub Pages

Status: implemented (Linux). Lands with the deb/rpm packages
(`splitway-daemon/Cargo.toml`, `splitway-gui/Cargo.toml`), the Arch PKGBUILDs
(`packaging/aur/`), the signed GitHub Pages repos, and the Phase-6 packaging CI
(`.github/workflows/packaging.yml` + `packaging/ci/`). The macOS Homebrew track
is separate and deferred.

This is the durable record for getting Splitway onto non-Nix Linux machines.
The NixOS path (the flake's `nixosModules.default`) is unchanged and remains the
author's iteration channel.

## The decision

1. **Two packages, lockstep-versioned — not one.** This supersedes the
   ROADMAP's original "one package" sketch.
   - `splitway` — `splitway-daemon` + the `splitway` CLI + the systemd unit.
     Built **musl-static** (`*-unknown-linux-musl`), so it has near-zero
     shared-library dependencies and runs on any glibc/musl baseline. It is the
     security-critical root daemon; musl sidesteps the glibc-baseline trap on
     old Debian/RHEL.
   - `splitway-gui` — the egui binary. Built **glibc** against a low floor.
     `Depends`/`Requires: splitway (>= <version>)`.
   - Why split: the GUI drags in a GL/X11/wayland stack and a glibc baseline
     that a headless / CLI-only install should not have to carry, and the
     security-critical core stays minimal and statically linked. The cost — a
     GUI↔daemon version axis — is handled by decision 3.
2. **Version source of truth = `splitway-daemon/Cargo.toml` `version`** (same as
   `release.yml`). Both packages and all tarballs are stamped with it at the
   packaging layer (`cargo deb --deb-version`, `cargo generate-rpm
   --set-metadata version=…`), so the dev suffix never has to live in a
   `Cargo.toml` (it is not valid semver). Per-crate `version` fields drift and
   are ignored for packaging.
3. **GUI `Depends >=`, not `=`.** The real compatibility contract is the
   (currently unversioned) GUI↔daemon IPC; `>=` survives Pages-mirror skew and
   partial upgrades, and CI rewrites the floor to the exact built version for
   the dev channel so a `~dev` core still satisfies it.
4. **Arch = AUR shape, but a self-hosted signed pacman repo now.** AUR account
   registration is disabled, so the automated AUR push is deferred; the in-repo
   PKGBUILDs (`splitway`, `splitway-bin`, `splitway-gui`) are usable today via
   `makepkg -si`, and the hosted pacman repo (x86_64) ships prebuilt copies.
5. **Pages hosts apt + dnf + pacman, all GPG-signed**, with separate `dev` and
   `release` channel subtrees.
6. **Pages deploys MERGE, never wipe.** `gh-pages` is persistent state;
   metadata is regenerated incrementally per channel, so old versions and the
   other channel always survive (the "full-site replace" trap).
7. **Socket-group is opt-in only.** The GUI package creates an **empty**
   `splitway` group and installs a service drop-in enabling `--socket-group`.
   Empty group ⇒ posture identical to `0600 root` (a no-op). The only grant is
   a human running `usermod -aG splitway $USER` + re-login. Maintainer scripts
   **never** add a user. Mirrors `nix/tests/socket-group.nix`.

## Channel & version topology

| Trigger            | Channel | Version              | Publishes to |
|--------------------|---------|----------------------|--------------|
| push → `master`    | release | `<X.Y.Z>`            | `deb/release`, `rpm/release`, `arch/release/x86_64`, GitHub Release (tarballs) |
| push → `dev`       | dev     | `<X.Y.Z>~dev.<utc>.<sha>` | `deb/dev`, `rpm/dev` |
| pull_request       | —       | `<X.Y.Z>~dev.<utc>.<sha>` | nothing (build + test only) |

`~dev` sorts **below** the clean release in dpkg and rpm (≥4.10), so a tester
with both repos enabled upgrades dev → release cleanly.

**pacman has no dev channel.** `vercmp` does not treat `~` as a pre-release
marker the way dpkg/rpm do, so a dev pacman channel (if ever added) must use the
Arch VCS convention `pkgver=<lasttag>.r<N>.g<shortsha>`, not `~dev`. The hosted
pacman repo is therefore **release-only and x86_64-only** (mainline Arch is
x86_64; aarch64/ALARM users use the `splitway-bin` PKGBUILD).

## Dependencies

The core package is musl-static ⇒ **no** shared-lib deps; `network-manager` /
`systemd-resolved` are `Recommends` (runtime prerequisites for actually applying
rules) not `Depends`, so it still installs for inspection or on other-resolver
hosts.

The GUI's eframe/glow stack is the subtle part: **winit/glow `dlopen` the
windowing libraries at runtime**, so they are absent from the binary's ELF
`DT_NEEDED`. Neither cargo-deb's `$auto` (dpkg-shlibdeps) nor
cargo-generate-rpm's ELF-based `auto-req` can see them — they **must be
hardcoded**:

- Debian: `libgl1, libx11-6, libxcursor1, libxi6, libxrandr2,
  libwayland-client0, libxkbcommon0`, plus `libc6 (>= 2.31)` to pin the floor.
- Fedora: `mesa-libGL, libX11, libXcursor, libXi, libXrandr, libwayland-client,
  libxkbcommon` (auto-req still derives the glibc/libgcc floor from the
  linked-against sonames).
- Arch: `libglvnd libxkbcommon wayland libx11 libxcursor libxi libxrandr`.

`xdg-desktop-portal` + a backend (`-gtk`/`-wlr`/`-kde`) is `Recommends`: rfd's
file dialog uses the portal here (no GTK linked) and **silently no-ops** without
a running portal.

**glibc floor: 2.31** (`debian:bullseye` / `ubuntu:20.04`). The GUI is built
inside a bullseye container so the declared floor is true. RHEL 8 (glibc 2.28)
is intentionally uncovered **for the GUI**; the musl-static core still runs
there.

## Signing & merge mechanics

One RSA GPG key signs all three formats. **RSA, not EdDSA:** `rpm --addsign`
only produces a verifiable header signature with an RSA key (an EdDSA key
silently yields no `RPMTAG_RSAHEADER`).

- **apt** (`packaging/ci/build-apt-repo.sh`): `apt-ftparchive` builds per-arch
  `Packages` (a combined index split by `Architecture`) and the suite
  `Release`; gpg writes `InRelease` (clearsigned) + `Release.gpg`. Clients use
  `[signed-by=/usr/share/keyrings/splitway.gpg]`.
- **dnf** (`packaging/ci/build-dnf-repo.sh`): `rpm --addsign` header-signs every
  rpm, `createrepo_c` regenerates `repodata/`, gpg writes
  `repomd.xml.asc`. Clients use `gpgcheck=1` + `repo_gpgcheck=1`.
- **pacman**: each `.pkg.tar.zst` gets a detached `.sig`; `repo-add` rebuilds
  `splitway.db.tar.gz` incrementally (old packages preserved). `repo-add` runs
  in an `archlinux` container (it is an Arch tool); all gpg signing happens on
  the host. GitHub Pages does not serve symlinks, so `repo-add`'s `*.db`/`*.files`
  symlinks are replaced with real copies. Clients use
  `SigLevel = Required DatabaseOptional`.

All three regenerate **only the current channel's** metadata against whatever is
already in `gh-pages`, then commit — merge, never wipe. The publish job is
serialized by a single `pages-deploy` concurrency group
(`cancel-in-progress: false`) so two deploys queue instead of clobbering.

The passphrase-protected real key is fed via loopback + a 0600 passphrase file
(`SPLITWAY_GPG_PASSFILE`), never on a command line.

### cargo-deb vs cargo-generate-rpm asset paths

Verified against cargo-deb 3.7.0 / cargo-generate-rpm 0.21.0, the two tools
resolve non-`target/` asset paths against **different** bases — an intentional
skew in the two metadata blocks:

- **cargo-deb**: relative to the crate's manifest dir ⇒ workspace-root files
  (`LICENSE`, `README.md`, `packaging/…`) need `../`.
- **cargo-generate-rpm**: relative to the invocation dir (workspace root) ⇒ bare
  paths.

cargo-deb's systemd-units integration only **generates** the enable/start/stop
maintainer scripts when a `maintainer-scripts` directory is also set (even an
empty one — `packaging/deb-maintainer-scripts/`); without it the unit is
installed but never enabled.

## Two-layer test design (gates every PR, no secrets)

1. **Local-artifact install**: install the built `.deb`/`.rpm`/`.pkg.tar.zst`
   directly in `debian:bookworm`, `ubuntu:22.04`, `fedora:latest`, `archlinux`;
   assert binaries run, the unit validates (`systemd-analyze verify`), the GUI
   pulls `splitway` + GL deps, the empty `splitway` group exists, the `.desktop`
   validates.
2. **Ephemeral-key signed-repo round trip**: generate a throwaway RSA key, build
   + sign local apt/dnf/pacman repos from the artifacts (the *same* scripts the
   real publish uses), serve over localhost, install with signature
   verification ON. Proves metadata + signing + verify end-to-end with no
   production secret.

On every push the publish job additionally runs a **post-deploy smoke** against
the just-published live channel with the real key + verification ON.

arm64: the core is cross-built (musl); the GUI is built on a native arm64
runner (bullseye container, no QEMU). arm64 install is smoke-tested under QEMU
(best-effort).

## Arch: pacman now, AUR deferred

`packaging/aur/` holds `splitway` (source), `splitway-bin` (prebuilt from the
release tarball, x86_64 + aarch64), and `splitway-gui` (source), each with an
`.install` mirroring the deb/rpm scriptlets (incl. the empty-group invariant).
CI builds the source PKGBUILDs from the checkout (the release tag may not exist
on a dev/PR run) and validates `.SRCINFO` + `namcap`.

The committed `pkgver=` pins a **released tag**, not the in-tree daemon version.
The two differ on purpose: release.yml's post-release auto-bump moves
`splitway-daemon/Cargo.toml` to the next (unreleased) version, so at rest on
`master` the daemon is one patch ahead of the newest `v*` tag. Each PKGBUILD's
`source=` fetches `v$pkgver`, so the pin must name a tag that exists. The
bump-version job runs `sync-pkgver.sh` **before** the daemon bump, stamping the
version just tagged — so on `master` at rest the pin is the *latest* release.
The gate `check-pkgver-sync.sh` (packaging.yml `meta`, with tags fetched)
enforces only the weaker, sufficient condition that the pinned tag **exists**
(not that it is the latest): that keeps `makepkg -si` working while avoiding
spurious failures on dev/PR branches or in the release-push window, where the
PKGBUILDs legitimately still point at the previous release until the bump commit
lands.

**Deferred:** the automated `ssh://aur@aur.archlinux.org/<pkg>.git` push, blocked
on AUR registration reopening. Design preserved for then: per package
`makepkg --printsrcinfo > .SRCINFO`, commit + push idempotently, release-only.
That same automation must also **pin real `sha256sums`** for the `splitway-bin`
prebuilt tarballs (currently `SKIP`): the digest is only knowable after the
release is built, and the publish job already holds the artifacts, so it should
stamp the per-tag hashes — `SKIP` on a prebuilt privileged daemon trusts the
Releases CDN for integrity, which transport TLS alone does not guarantee.

## The signing key

RSA. The workflow derives the armored public key from `GPG_PRIVATE_KEY` and
publishes it to `gh-pages/splitway.gpg`
(`https://stslex.github.io/splitway/splitway.gpg`). The maintainer holds
`GPG_PRIVATE_KEY` + `GPG_PASSPHRASE` as repository secrets.

Fingerprint: confirm with `gpg --show-keys splitway.gpg` against the published
key. (Record the provisioned key's fingerprint here once the maintainer sets up
the secret; it is public project infrastructure, not sensitive.)
