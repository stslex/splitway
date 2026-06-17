# Splitway

Domain-based traffic splitting tool for Linux and macOS desktops. Routes traffic through VPN or direct connection based on configurable domain rules.

## Problem

Corporate VPNs like GlobalProtect capture all traffic by default. Splitting requires manually configuring split-DNS via shell scripts, editing NetworkManager dispatchers, and running `resolvectl` commands with sudo. Every new domain means editing a bash script and restarting.

## What it does

Splitway automates DNS-based traffic splitting: domains matching the rules are resolved through the VPN's DNS server; everything else goes direct. The daemon watches the VPN interface and applies/reverts rules automatically on up/down, and is controllable at runtime via the `splitway` CLI over a Unix socket.

## Current state

- Long-running daemon: auto-applies rules on VPN up, auto-reverts on down
- Auto-detects the VPN DNS server: NetworkManager D-Bus on Linux, a standalone OpenVPN's management interface, or SCDynamicStore + `scutil` on macOS
- Applies/reverts split-DNS rules through `resolvectl` (Linux) or `/etc/resolver` files (macOS)
- Runtime control over a Unix socket: `splitway status/enable/disable/add/remove/list/reload`, or a primitive GUI (`splitway-gui`) over the same socket
- Reverts DNS rules on `SIGTERM`/`SIGINT` so a stop never leaves the system half-configured
- Linux (GlobalProtect via openconnect, and OpenVPN — both NetworkManager-managed; plus standalone OpenVPN via its management interface, no NM) and macOS (any `utun*` VPN) supported. The official GlobalProtect client (not NM-managed) is not covered

## Workspace layout

```
splitway/
├── splitway-daemon/   # Core daemon — applies/reverts resolvectl rules
├── splitway-cli/      # CLI frontend (IPC client over the daemon socket)
├── splitway-gui/      # Primitive GUI (egui; IPC client, no privileges)
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

### Standalone OpenVPN (no NetworkManager)

For an OpenVPN connection started directly by `openvpn` (or
`openvpn-client@.service`) — *not* imported into NetworkManager — set
`vpn_backend` to `openvpn` and point Splitway at OpenVPN's **management
interface**. Unlike the NetworkManager case, nothing applies the pushed DNS onto
the `tun*` link for Splitway to read back, so it learns the DNS from OpenVPN
itself (the management `log` channel surfaces the `PUSH_REPLY`).

Enable the management interface in your `openvpn.conf`, bound to localhost (TCP)
or a unix socket:

```ini
# TCP on localhost:
management 127.0.0.1 7505

# ...or a unix socket (preferred — filesystem permissions gate access):
management /run/openvpn/mgmt.sock unix
```

Then configure Splitway:

```json
{
  "vpn_name": "tun0",
  "vpn_hosts": ["corp.example.com"],
  "vpn_backend": "openvpn",
  "openvpn": {
    "management": "127.0.0.1:7505",
    "management_password_file": "/etc/openvpn/mgmt.pass"
  }
}
```

- `vpn_backend` defaults to `network-manager`; set it to `openvpn` for this mode.
  Configs without the field keep selecting NetworkManager, so existing setups are
  unaffected.
- `openvpn.management` is either `host:port` (TCP) or a unix socket path — a value
  containing `/` is treated as a socket path, otherwise as `host:port`.
- `vpn_name` is still the `tun*` device the DNS rules are applied to (find it with
  `ip link` while the VPN is up); the management interface only supplies VPN
  state and the pushed DNS, not the device.
- `openvpn.management_password_file` is optional — set it (to a file whose first
  line is the password) only when the management interface is password-protected.
- If OpenVPN pushes **no DNS servers** (a `PUSH_REPLY` with no `dhcp-option DNS`),
  there is nowhere to route the selected domains, so Splitway leaves DNS unchanged
  and applies nothing; any rules from a previous session are reverted.

Splitway sends only **read-only** management commands (`state`, `log`); it never
sends `signal`/`hold` or otherwise controls the tunnel. A management-socket drop
is never itself treated as VPN-down: Splitway reconnects with backoff, then
re-samples the tunnel and reconciles — keeping the rules unchanged when the
pushed DNS is the same, re-applying when it changed, and reverting when the
reconnected session pushes no DNS (as well as on a genuine OpenVPN
`EXITING`/`RECONNECTING` state).

> **Known limitation:** if OpenVPN pushes *different* DNS servers mid-session
> (a TLS renegotiation that changes `dhcp-option DNS` without a reconnect),
> Splitway does not re-apply them until the next down/up cycle. This is rare —
> renegotiation normally re-pushes the same servers — and is a noted follow-up.

**Security.** The management interface is OpenVPN's control channel: anything
that can reach it can drive the VPN. Bind it to `127.0.0.1` or a unix socket with
tight permissions (socket directory `0700`, owned by the OpenVPN user); **never**
expose it over TCP to other hosts or on `0.0.0.0`. Prefer a unix socket so
filesystem permissions gate access, and password-protect any TCP endpoint.

No extra deployment artifact is needed for this mode: OpenVPN runs as its own
service, and the existing `splitway-daemon` unit (see
[packaging/](packaging/README.md)) drives it once `vpn_backend = openvpn`.

## Usage

`splitway-daemon run` is a long-running daemon: it watches the configured VPN
interface and automatically applies split-DNS rules when it comes up and
reverts them when it goes down. It also serves a Unix control socket. Run it
as a service — see [packaging/](packaging/README.md) (systemd) or the flake's
`nixosModules.default` (NixOS).

```sh
# Start the daemon (normally via systemd, not by hand)
splitway-daemon run

# Use a config file other than the default location:
splitway-daemon run --config /etc/splitway/config.json

# Daemon's own subcommands:
splitway-daemon status   # query the running daemon over IPC
splitway-daemon revert   # emergency direct revert; works even with no daemon
```

`--config <PATH>` overrides the config file the daemon reads and writes for its
whole lifetime (it also applies to `revert`, which reads `vpn_name` from the
same file). Without it, the default `~/.config/splitway/config.json` is used.
The chosen file is fixed at launch — there is no runtime switching.

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

### GUI

`splitway-gui` is a small desktop window (egui) that drives the daemon over the
**same IPC socket as the CLI**. It is a pure client: it holds **no privileges**,
duplicates no daemon logic, and never touches `resolvectl`/`/etc/resolver` or
writes the config file itself — every action is an IPC request, every config
change goes through the daemon's single-writer state actor.

It shows live status (`vpn_up`, `applied`, interface, domain count), an
enable/disable toggle, the domain list with add/remove, and an editor for the
remaining config fields (`vpn_name`, `vpn_backend`, `openvpn.management`,
`openvpn.management_password_file`). Changing `vpn_name`/`vpn_backend` reverts
the old interface but does not re-arm the VPN watch, so the GUI flags that a
**daemon restart** is needed for auto-apply on the new interface.

```sh
splitway-gui
```

Reachability matches the CLI: it tries the per-user socket
(`$XDG_RUNTIME_DIR/splitway.sock`) then the system socket (`/run/splitway` on
Linux, `/var/run/splitway` on macOS), so a login-session GUI can reach a system
daemon. If the daemon runs as root with its default `0600` socket, an
unprivileged GUI sees "permission denied" and shows the daemon's own guidance
(run as the daemon's user/group) — it never escalates. A daemon that is not
running shows a non-fatal banner and the GUI recovers on the next poll once it
is back.

The config-file path is shown read-only; the "Choose a file…" picker produces a
`splitway-daemon run --config <PATH>` launch hint rather than switching the
daemon's active file at runtime (runtime switching is a planned follow-up).

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
