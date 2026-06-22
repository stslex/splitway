{
  description = "Splitway — domain-based split-DNS tool for Linux/macOS desktops";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  };

  outputs =
    { self, nixpkgs }:
    let
      # Linux hosts (x86_64 + aarch64): build targets and NixOS host
      # platforms — the NixOS module's default package resolves against
      # these. aarch64-darwin: Apple Silicon dev shells. Mirrors the
      # release matrix in .github/workflows/release.yml.
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "aarch64-darwin"
      ];
      # The nixosTest (socket-group) is a Linux-only VM build; darwin cannot run
      # it. Kept separate from `systems` so the test attrset is only defined where
      # it can evaluate/build.
      linuxSystems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAllSystems = nixpkgs.lib.genAttrs systems;
      pkgsFor = system: nixpkgs.legacyPackages.${system};

      # Native dependencies of the egui GUI (`splitway-gui`). Only Linux needs
      # them: eframe (glow renderer) links a windowing/GL stack (wayland/X11,
      # GL, libxkbcommon). rfd's file dialog uses the XDG desktop portal over
      # pure-Rust zbus (no GTK), so no GTK package is required — matching the CI
      # apt list. macOS uses system frameworks (Cocoa/Metal), so it needs none;
      # the daemon and CLI pull no native libraries. `pkg-config` (a build tool)
      # resolves them at compile time — enough for `nix build`/`nix flake
      # check`, which only compile the GUI and run the pure tests (no window is
      # opened in the sandbox).
      guiNativeBuildInputs = pkgs: pkgs.lib.optionals pkgs.stdenv.isLinux [ pkgs.pkg-config ];
      guiBuildInputs =
        pkgs:
        pkgs.lib.optionals pkgs.stdenv.isLinux (
          with pkgs;
          [
            libxkbcommon
            libGL
            wayland
            libx11
            libxcursor
            libxi
            libxrandr
          ]
        );
      # Native deps of the Tauri shell (`splitway-gui-tauri`), Linux-only: wry
      # links webkit2gtk-4.1 (which propagates gtk3/glib/soup3/cairo/pango/…) and
      # pkg-config (from guiNativeBuildInputs) resolves its `.pc` files. These are
      # used ONLY in the devShell — the Tauri crate is excluded from the default
      # package (see Cargo.toml `default-members`), so `nix build`/`nix flake
      # check` never pull the webkit toolchain. Packaging it (frontend vendoring +
      # wrapGAppsHook3 + bundling) is Phase 7d.
      tauriBuildInputs =
        pkgs:
        pkgs.lib.optionals pkgs.stdenv.isLinux (
          with pkgs;
          [
            webkitgtk_4_1
            gtk3
            glib
            cairo
            pango
            gdk-pixbuf
            harfbuzz
            openssl
            atk
          ]
        );

      # The packaged Tauri GUI — `packages.<system>.splitway-gui`, Linux-only (it
      # links webkit2gtk). A two-stage build: the vanilla-TS frontend (tsc +
      # esbuild, no npm) into ui/dist, then the Rust binary that embeds that dist
      # via Tauri's `generate_context!`. Wrapped by wrapGAppsHook3 (GTK / GIO /
      # gsettings env) with the niri/Wayland blank-window workaround baked into
      # the wrapper; ships distribution icons + a validated `.desktop` entry. It
      # is the only place that pulls the webkit toolchain into a `nix build` —
      # the default package and `nix develop`'s cargo path are untouched.
      guiPackage =
        pkgs:
        let
          desktopItem = pkgs.makeDesktopItem {
            # Filename = `${name}.desktop` = the app_id (freedesktop convention).
            name = "io.github.stslex.splitway";
            desktopName = "Splitway";
            genericName = "Split-DNS controller";
            comment = "Route selected domains through the VPN's DNS, everything else direct";
            # Bare command: $out/bin is on PATH once the package is installed
            # (systemPackages / home.packages / niri spawn), and the binary at
            # that path IS the wrapGAppsHook3 wrapper — so every launch path,
            # this launcher included, inherits the blank-window workaround env.
            # The `env …` prefix re-asserts it at the launcher level (defense in
            # depth + documents intent).
            exec = "env WEBKIT_DISABLE_DMABUF_RENDERER=1 splitway-gui-tauri";
            icon = "io.github.stslex.splitway";
            terminal = false;
            # One main category (a DNS/VPN routing tool lives under the Network
            # menu) keeps desktop-file-validate hint-free — multiple main
            # categories trigger a "may appear more than once" hint.
            categories = [ "Network" ];
            keywords = [
              "DNS"
              "VPN"
              "split-dns"
              "split"
            ];
            startupNotify = true;
            # MUST equal the GTK app_id (enableGTKAppId + identifier in
            # tauri.conf.json) so the compositor maps the window to this
            # launcher/icon and a niri window rule can target it.
            startupWMClass = "io.github.stslex.splitway";
          };
        in
        pkgs.rustPlatform.buildRustPackage {
          pname = "splitway-gui";
          version = "0.0.1";
          src = ./.;
          cargoLock.lockFile = ./Cargo.lock;

          # The Tauri crate is excluded from the workspace default-members, so
          # build (and test) it explicitly. Its bridge unit tests are pure (no
          # Tauri runtime), so the scoped checkPhase runs them headless — adding
          # coverage the default `cargo test` (default-members only) skips.
          cargoBuildFlags = [
            "--package"
            "splitway-gui-tauri"
          ];
          cargoTestFlags = [
            "--package"
            "splitway-gui-tauri"
          ];

          nativeBuildInputs = with pkgs; [
            pkg-config
            wrapGAppsHook3
            makeWrapper
            copyDesktopItems
            # Frontend toolchain — no npm registry access (see ui/build.sh).
            nodejs
            typescript
            esbuild
            # Build-time icon rasterization: provides `rsvg-convert`.
            librsvg
          ];

          buildInputs =
            tauriBuildInputs pkgs
            ++ (with pkgs; [
              librsvg # gdk-pixbuf SVG loader at runtime
              glib-networking # GIO TLS module (wrapGAppsHook wires GIO_EXTRA_MODULES)
              gsettings-desktop-schemas # default gsettings schemas the GTK/webkit stack reads
            ]);

          desktopItems = [ desktopItem ];

          # Build the web frontend into ui/dist BEFORE the Rust compile: Tauri's
          # `generate_context!` embeds frontendDist (ui/dist) at compile time, so
          # the bundled assets — including the local IBM Plex woff2 — must exist
          # first. tauri.conf.json's beforeBuildCommand is only run by the tauri
          # CLI (unused here — no AppImage/deb bundler), so run build.sh ourselves.
          preBuild = ''
            ( cd splitway-gui-tauri/ui && sh build.sh )
          '';

          # Compose the blank-window workaround INTO the wrapGAppsHook3 wrapper
          # rather than wrapping twice: appending to gappsWrapperArgs makes the
          # hook emit ONE wrapper that does both the GTK/GIO/gsettings setup and
          # our --set, so neither clobbers the other. WEBKIT_DISABLE_DMABUF_RENDERER=1
          # is the niri/webkit2gtk blank-window fix (docs/design/tauri-read-only.md
          # §4); the wrapper sets it earliest, the binary self-sets it as a fallback.
          preFixup = ''
            gappsWrapperArgs+=(--set WEBKIT_DISABLE_DMABUF_RENDERER 1)
          '';

          # Distribution icons: the tiled SVG to hicolor scalable + rasterized PNG
          # sizes from it. Basename = app_id, matching the .desktop Icon= field.
          postInstall = ''
            install -Dm644 assets/icon/splitway-icon.svg \
              "$out/share/icons/hicolor/scalable/apps/io.github.stslex.splitway.svg"
            for s in 16 24 32 48 64 128 256 512; do
              install -d "$out/share/icons/hicolor/''${s}x''${s}/apps"
              rsvg-convert -w "$s" -h "$s" assets/icon/splitway-icon.svg \
                -o "$out/share/icons/hicolor/''${s}x''${s}/apps/io.github.stslex.splitway.png"
            done

            # The binary embeds the bundled IBM Plex woff2 faces, so OFL-1.1
            # requires their license + copyright notice to be redistributed with
            # this output. Install it where it is easily found.
            install -Dm644 splitway-gui-tauri/ui/src/fonts/LICENSE-OFL.txt \
              "$out/share/licenses/splitway-gui/IBM-Plex-OFL-1.1.txt"
          '';

          meta = {
            description = "Splitway GUI — native Tauri desktop window for the split-DNS daemon";
            homepage = "https://github.com/stslex/splitway";
            mainProgram = "splitway-gui-tauri";
            platforms = pkgs.lib.platforms.linux;
          };
        };
    in
    {
      packages = forAllSystems (
        system:
        let
          pkgs = pkgsFor system;
        in
        {
          # Builds the whole workspace: the splitway-daemon and `splitway` CLI
          # binaries plus the `splitway-gui` front-end.
          default = pkgs.rustPlatform.buildRustPackage {
            pname = "splitway";
            version = "0.0.1";
            src = ./.;
            cargoLock.lockFile = ./Cargo.lock;

            # The GUI's native deps must be present to compile splitway-gui.
            nativeBuildInputs = guiNativeBuildInputs pkgs;
            buildInputs = guiBuildInputs pkgs;

            # buildRustPackage runs `cargo test` in its checkPhase by
            # default. The workspace unit tests are pure (no live system
            # commands), so they pass inside the Nix sandbox — keep it on.

            meta = {
              description = "Domain-based split-DNS tool for Linux/macOS desktops";
              homepage = "https://github.com/stslex/splitway";
              mainProgram = "splitway-daemon";
              platforms = pkgs.lib.platforms.linux ++ pkgs.lib.platforms.darwin;
            };
          };
        }
        # The native Tauri GUI is a separate, Linux-only package (it links
        # webkit2gtk; darwin would not evaluate). User-launched app, not a
        # service — so a `packages` output, optionally installed by the
        # nixosModule when the unprivileged-GUI path is enabled.
        // pkgs.lib.optionalAttrs pkgs.stdenv.isLinux {
          splitway-gui = guiPackage pkgs;
        }
      );

      devShells = forAllSystems (
        system:
        let
          pkgs = pkgsFor system;
        in
        {
          # Replaces the manual `nix shell nixpkgs#cargo nixpkgs#rustc ...`
          # invocation: the full toolchain for fmt / clippy / test.
          default = pkgs.mkShell {
            packages =
              (with pkgs; [
                cargo
                rustc
                rustfmt
                clippy
                rust-analyzer
              ])
              # Tooling to build the Tauri shell's vanilla-TS web frontend
              # (`splitway-gui-tauri/ui`) locally, WITHOUT npm: `tsc` type-checks
              # (the bindings drift guard) and `esbuild` bundles. `nodejs` is the
              # runtime tsc needs. No registry access required — these come from
              # Nix. See splitway-gui-tauri/ui/build.sh.
              ++ pkgs.lib.optionals pkgs.stdenv.isLinux (with pkgs; [
                nodejs
                typescript
                esbuild
              ]);
            # The egui GUI's + the Tauri shell's native deps, so
            # `cargo build`/`clippy`/`test` (and `cargo build -p
            # splitway-gui-tauri`) work in the dev shell. pkg-config
            # (guiNativeBuildInputs) resolves both stacks' `.pc` files.
            nativeBuildInputs = guiNativeBuildInputs pkgs;
            buildInputs = guiBuildInputs pkgs ++ tauriBuildInputs pkgs;
            # Make the dlopen'd egui (GL, libxkbcommon, wayland) and Tauri
            # (webkit2gtk, gtk3) libraries findable when running either GUI from
            # the dev shell.
            LD_LIBRARY_PATH = pkgs.lib.optionalString pkgs.stdenv.isLinux (
              pkgs.lib.makeLibraryPath (guiBuildInputs pkgs ++ tauriBuildInputs pkgs)
            );
            # Runtime env so the Tauri webview actually renders + loads when the
            # shell is launched from `nix develop` on NixOS (no global GIO /
            # gsettings paths; a packaged build would use wrapGAppsHook3 — Phase
            # 7d). WEBKIT_DISABLE_DMABUF_RENDERER is the niri/Wayland blank-window
            # workaround (also set defensively in the binary's `main`).
            shellHook = pkgs.lib.optionalString pkgs.stdenv.isLinux ''
              export WEBKIT_DISABLE_DMABUF_RENDERER=1
              export GIO_EXTRA_MODULES="${pkgs.glib-networking}/lib/gio/modules''${GIO_EXTRA_MODULES:+:$GIO_EXTRA_MODULES}"
              export XDG_DATA_DIRS="${pkgs.gsettings-desktop-schemas}/share/gsettings-schemas/${pkgs.gsettings-desktop-schemas.name}:${pkgs.gtk3}/share/gsettings-schemas/${pkgs.gtk3.name}''${XDG_DATA_DIRS:+:$XDG_DATA_DIRS}"
            '';
          };
        }
      );

      # `nix flake check` builds these, whose checkPhase runs the workspace
      # tests. The Tauri GUI package is added on Linux (it links webkit2gtk;
      # darwin has no such package) so CI exercises the whole packaging path —
      # the frontend build, font embedding, the wrapGAppsHook3 + workaround
      # compose, and the icon/.desktop install — on every PR, not just a manual
      # `nix build .#splitway-gui`. This pulls the webkit closure into the Linux
      # `nix` CI job (mostly cached binaries plus the tauri/wry compile); that is
      # the deliberate cost of keeping the GUI package green. (Contrast the
      # socket-group VM test below, kept out of `checks` only because it needs
      # KVM — not a constraint here.)
      checks = forAllSystems (
        system:
        let
          pkgs = pkgsFor system;
        in
        {
          package = self.packages.${system}.default;
        }
        // pkgs.lib.optionalAttrs pkgs.stdenv.isLinux {
          splitway-gui = self.packages.${system}.splitway-gui;
        }
      );

      # The socket-group nixosTest lives under `legacyPackages` (not `checks`) on
      # purpose: it boots a VM and so needs /dev/kvm, which GitHub's default CI
      # runners do not reliably expose. `nix flake check` does not build
      # `legacyPackages`, so keeping it here keeps CI green while leaving the test
      # runnable locally (the author daily-drives NixOS, where KVM is available):
      #   nix build .#legacyPackages.x86_64-linux.tests.socketGroup -L
      # See docs/design/socket-group.md for why this is the "real proof" of the
      # in-group-connect / out-of-group-denied contract.
      legacyPackages = nixpkgs.lib.genAttrs linuxSystems (
        system:
        let
          pkgs = pkgsFor system;
        in
        {
          tests.socketGroup = import ./nix/tests/socket-group.nix { inherit self pkgs; };
        }
      );

      nixosModules.default = import ./nix/module.nix self;
    };
}
