# Phase 0.5 — NixOS packaging

Implement Phase 0.5 from `ROADMAP.md`. Read `CLAUDE.md` for workflow rules first.

Scope: Nix flake with package and dev shell, plus a NixOS module skeleton. **No Rust code changes at all** — this phase touches only new Nix files, so it can run in parallel with Phase 1 (`phase-1-abstraction-split`): zero file overlap. Whichever PR merges second rebases trivially.

## Branch

Branch `phase-0.5-nixos-packaging` from up-to-date `dev`.

## Tasks

### 1. `flake.nix`

- Inputs: `nixpkgs` (and `flake-utils` or plain per-system attrs — keep it simple, no extra frameworks)
- `packages.<system>.default`: `rustPlatform.buildRustPackage` over the workspace, `cargoLock.lockFile = ./Cargo.lock` (ensure `Cargo.lock` is committed). Build all workspace binaries (`splitway-daemon`, `splitway-cli`)
- `devShells.<system>.default`: `cargo`, `rustc`, `rustfmt`, `clippy`, `rust-analyzer` — replaces the manual `nix shell nixpkgs#cargo nixpkgs#rustc ...` invocation currently needed for development
- `checks`: at minimum the package build; add `cargo test` via `checkPhase` if it doesn't already run in `buildRustPackage` (it does by default — keep it enabled)
- Systems: `x86_64-linux` and `aarch64-darwin` at minimum

### 2. NixOS module skeleton — `nix/module.nix`

- Exposed as `nixosModules.default` from the flake
- Options: `services.splitway.enable`, `services.splitway.package` (defaults to the flake package)
- When enabled: install the package into `environment.systemPackages`. Define the systemd service as a **commented-out or minimal stub** with a note that the real long-running daemon arrives in Phase 2 — do not invent service behavior the binary doesn't have yet (it is one-shot today)
- Keep runtime deps explicit where the module can help (the binary shells out to `nmcli` and `resolvectl` — document this in the module comments; NixOS systems with NetworkManager + systemd-resolved have both in PATH)

### 3. CI for the flake

Extend `.github/workflows/ci.yml` with a `nix` job on `ubuntu-latest`: install Nix (e.g. `DeterminateSystems/nix-installer-action` or `cachix/install-nix-action`), run `nix flake check` and `nix build`. Without CI the flake will rot silently.

### 4. Docs

- README: add a "Nix" subsection under Build — `nix build`, `nix develop`, one line about `nixosModules.default`

## Out of scope

- Any Rust code change
- Real systemd service definition (Phase 2)
- Home-manager module, cachix/binary cache, overlays

## Done criteria

- `nix build` produces working binaries
- `nix develop` provides the full dev toolchain (fmt/clippy/test runnable inside)
- `nix flake check` green locally and in CI
- `cargo`-only workflow keeps working untouched for non-Nix users

## Finish

Open a PR into `dev` titled `Phase 0.5: NixOS packaging` with the done-criteria checklist. Note in the description that it is parallel-safe with Phase 1.
