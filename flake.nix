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

      # `nix flake check` builds the package, whose checkPhase runs the
      # workspace tests.
      checks = forAllSystems (system: {
        package = self.packages.${system}.default;
      });

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
