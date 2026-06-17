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
            packages = with pkgs; [
              cargo
              rustc
              rustfmt
              clippy
              rust-analyzer
            ];
            # The GUI's native deps, so `cargo build`/`clippy`/`test` work in
            # the dev shell too.
            nativeBuildInputs = guiNativeBuildInputs pkgs;
            buildInputs = guiBuildInputs pkgs;
            # Make the dlopen'd GUI libraries (GL, libxkbcommon, wayland)
            # findable when running the GUI from the dev shell.
            LD_LIBRARY_PATH = pkgs.lib.optionalString pkgs.stdenv.isLinux (
              pkgs.lib.makeLibraryPath (guiBuildInputs pkgs)
            );
          };
        }
      );

      # `nix flake check` builds the package, whose checkPhase runs the
      # workspace tests.
      checks = forAllSystems (system: {
        package = self.packages.${system}.default;
      });

      nixosModules.default = import ./nix/module.nix self;
    };
}
