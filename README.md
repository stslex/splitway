# Splitway

Domain-based traffic splitting tool for Linux desktops (macOS support planned — see [ROADMAP.md](ROADMAP.md)). Routes traffic through VPN or direct connection based on configurable domain rules.

## Problem

Corporate VPNs like GlobalProtect capture all traffic by default. Splitting requires manually configuring split-DNS via shell scripts, editing NetworkManager dispatchers, and running `resolvectl` commands with sudo. Every new domain means editing a bash script and restarting.

## What it does

Splitway automates DNS-based traffic splitting: domains matching the rules are resolved through the VPN's DNS server; everything else goes direct. The daemon watches the VPN interface and applies/reverts rules automatically on up/down, and is controllable at runtime via the `splitway` CLI over a Unix socket.

## Current state

- Long-running daemon: auto-applies rules on VPN up, auto-reverts on down
- Auto-detects VPN DNS server via NetworkManager (D-Bus event stream)
- Applies/reverts split-DNS rules through `resolvectl`
- Runtime control over a Unix socket: `splitway status/enable/disable/add/remove/list/reload`
- Reverts DNS rules on `SIGTERM`/`SIGINT` so a stop never leaves the system half-configured
- Linux only, GlobalProtect as first supported VPN

## Workspace layout

```
splitway/
├── splitway-daemon/   # Core daemon — applies/reverts resolvectl rules
├── splitway-cli/      # CLI frontend (IPC client over the daemon socket)
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

`splitway-daemon run` is a long-running daemon: it watches the configured VPN
interface and automatically applies split-DNS rules when it comes up and
reverts them when it goes down. It also serves a Unix control socket. Run it
as a service — see [packaging/](packaging/README.md) (systemd) or the flake's
`nixosModules.default` (NixOS).

```sh
# Start the daemon (normally via systemd, not by hand)
splitway-daemon run

# Daemon's own subcommands:
splitway-daemon status   # query the running daemon over IPC
splitway-daemon revert   # emergency direct revert; works even with no daemon
```

Control a running daemon with the `splitway` CLI over the socket:

```sh
splitway status            # show enabled / vpn_up / applied / domains
splitway enable            # start applying rules (persisted)
splitway disable           # stop applying and revert (persisted)
splitway add corp.example  # route a domain through the VPN (persisted)
splitway remove corp.example
splitway list              # list configured domains
splitway reload            # re-read config.json from disk
```

`disable` tells the running daemon to stop applying and persists that choice;
`splitway-daemon revert` is a one-shot escape hatch that talks straight to the
DNS backend and works even when no daemon is running.

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
