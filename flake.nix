{
  description = "Splitway — domain-based split-DNS tool for Linux/macOS desktops";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  };

  outputs =
    { self, nixpkgs }:
    let
      # x86_64-linux: primary target and NixOS host platform.
      # aarch64-darwin: Apple Silicon dev shells.
      systems = [
        "x86_64-linux"
        "aarch64-darwin"
      ];
      forAllSystems = nixpkgs.lib.genAttrs systems;
      pkgsFor = system: nixpkgs.legacyPackages.${system};
    in
    {
      packages = forAllSystems (
        system:
        let
          pkgs = pkgsFor system;
        in
        {
          # Builds the whole workspace, i.e. both binaries
          # (splitway-daemon, splitway-cli).
          default = pkgs.rustPlatform.buildRustPackage {
            pname = "splitway";
            version = "0.0.1";
            src = ./.;
            cargoLock.lockFile = ./Cargo.lock;

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
