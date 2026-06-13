# Splitway

Domain-based traffic splitting tool for Linux desktops (macOS support planned — see [ROADMAP.md](ROADMAP.md)). Routes traffic through VPN or direct connection based on configurable domain rules.

## Problem

Corporate VPNs like GlobalProtect capture all traffic by default. Splitting requires manually configuring split-DNS via shell scripts, editing NetworkManager dispatchers, and running `resolvectl` commands with sudo. Every new domain means editing a bash script and restarting.

## What it does

Splitway automates DNS-based traffic splitting: domains matching the rules are resolved through the VPN's DNS server; everything else goes direct. Today this is a one-shot command (`run`/`revert`); automatic apply/revert on VPN interface up/down is the headline feature of Phase 2 in the roadmap.

## Current state (MVP)

- Daemon reads config from `~/.config/splitway/config.json`
- Auto-detects VPN DNS server via NetworkManager
- Applies/reverts split-DNS rules through `resolvectl`
- CLI commands: `run`, `revert`, `status`
- Linux only, GlobalProtect as first supported VPN, one-shot (no interface monitoring yet)

## Workspace layout

```
splitway/
├── splitway-daemon/   # Core daemon — applies/reverts resolvectl rules
├── splitway-cli/      # CLI frontend (IPC client, in progress)
└── splitway-shared/   # Shared types and config parsing
```

## Config

Create `~/.config/splitway/config.json` (auto-created as empty on first run):

```json
{
  "vpn_name": "vpn0",
  "vpn_hosts": ["corp.example.com", "internal.example.com"]
}
```

## Usage

```sh
# Apply split-DNS rules for configured VPN interface
splitway-daemon run

# Revert all rules for the VPN interface
splitway-daemon revert

# Show current routing status
splitway-daemon status
```

## Build

```sh
cargo build --release
```

Binaries are placed in `target/release/`.

### Nix

With flakes enabled:

```sh
nix build      # build both binaries into ./result/bin/
nix develop    # dev shell with cargo, rustc, rustfmt, clippy, rust-analyzer
```

The flake also exposes `nixosModules.default`. On a NixOS host, import it and
set `services.splitway.enable = true;` to install the package system-wide. The
systemd service is a commented-out stub until the real daemon lands in Phase 2.

## Roadmap

See [ROADMAP.md](ROADMAP.md) — phased plan with done-criteria: testable foundation → abstraction split → real daemon + IPC → OpenVPN/macOS backends → primitive GUI. Near-term priorities: NixOS packaging, macOS, OpenVPN, minimal GUI.

## Development

Workflow rules live in [CLAUDE.md](CLAUDE.md): one phase = one branch = one PR into `dev`, English only. Per-phase implementation prompts are in `docs/prompts/`.

## Stack

Rust, systemd-resolved, NetworkManager, Cargo workspace
