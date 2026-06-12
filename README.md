# Splitway

Domain-based traffic splitting tool for Linux desktops. Routes traffic through VPN or direct connection based on configurable domain rules.

## Problem

Corporate VPNs like GlobalProtect capture all traffic by default. Splitting requires manually configuring split-DNS via shell scripts, editing NetworkManager dispatchers, and running `resolvectl` commands with sudo. Every new domain means editing a bash script and restarting.

## What it does

Splitway automates DNS-based traffic splitting. A daemon applies routing rules to `systemd-resolved` when a VPN interface comes up, and reverts them when it goes down. Domains matching the rules are resolved through the VPN's DNS server; everything else goes direct.

## Current state (MVP)

- Daemon reads config from `~/.config/splitway/config.json`
- Auto-detects VPN DNS server via NetworkManager
- Applies/reverts split-DNS rules through `resolvectl`
- CLI commands: `run`, `revert`, `status`
- Linux only, GlobalProtect as first supported VPN

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
  "vpn_ip": "10.0.0.1",
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

## Roadmap

See [ROADMAP.md](ROADMAP.md) — phased plan with done-criteria, from testable foundation (Phase 0) to real daemon, IPC, and multi-backend support.

## Stack

Rust, systemd-resolved, NetworkManager, Cargo workspace
