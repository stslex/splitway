# Splitway

Domain-based traffic splitting tool for Linux and macOS desktops. Routes traffic through VPN or direct connection based on configurable domain rules.

## Problem

Corporate VPNs like GlobalProtect capture all traffic by default. Splitting requires manually configuring split-DNS via shell scripts, editing NetworkManager dispatchers, and running `resolvectl` commands with sudo. Every new domain means editing a bash script and restarting.

## What it does

Splitway automates DNS-based traffic splitting: domains matching the rules are resolved through the VPN's DNS server; everything else goes direct. The daemon watches the VPN interface and applies/reverts rules automatically on up/down, and is controllable at runtime via the `splitway` CLI over a Unix socket.

## Current state

- Long-running daemon: auto-applies rules on VPN up, auto-reverts on down
- Auto-detects the VPN DNS server: NetworkManager D-Bus on Linux, SCDynamicStore + `scutil` on macOS
- Applies/reverts split-DNS rules through `resolvectl` (Linux) or `/etc/resolver` files (macOS)
- Runtime control over a Unix socket: `splitway status/enable/disable/add/remove/list/reload`
- Reverts DNS rules on `SIGTERM`/`SIGINT` so a stop never leaves the system half-configured
- Linux (GlobalProtect via openconnect, and OpenVPN — both NetworkManager-managed) and macOS (any `utun*` VPN) supported. The official GlobalProtect client (not NM-managed) is not covered

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
  "vpn_name": "tun0",
  "vpn_hosts": ["corp.example.com", "internal.example.com"]
}
```

`vpn_name` is the **network interface (device) name**, not the NetworkManager
connection name. Find it with `nmcli device status` / `ip link` (Linux) or
`scutil --nwi` / `ifconfig` (macOS) while the VPN is up:

- **OpenVPN via NetworkManager** creates a `tun*` device — usually `tun0`. Set
  `vpn_name` to that device (e.g. `tun0`), *not* the NM connection's name. NM
  models the VPN as a separate active connection bound to your base interface,
  but the pushed DNS and the up/down events live on the `tun*` device, which is
  what Splitway watches.
- **GlobalProtect** (openconnect) behaves the same way — a `tun*` device.
- **WireGuard** typically appears as the connection's own device name (e.g.
  `wg0`).
- **macOS** VPNs appear as `utun*` devices. The macOS backend writes one
  `/etc/resolver/<domain>` file per host and needs root; install it as a
  LaunchDaemon — see [packaging/](packaging/README.md) ("macOS (launchd)").

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

Rust, systemd-resolved, NetworkManager (Linux), SCDynamicStore + `/etc/resolver` (macOS), Cargo workspace
