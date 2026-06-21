# Splitway

Domain-based traffic splitting tool for Linux and macOS desktops. Routes traffic through VPN or direct connection based on configurable domain rules.

## Problem

Corporate VPNs like GlobalProtect capture all traffic by default. Splitting requires manually configuring split-DNS via shell scripts, editing NetworkManager dispatchers, and running `resolvectl` commands with sudo. Every new domain means editing a bash script and restarting.

## What it does

Splitway automates DNS-based traffic splitting: domains matching the rules are resolved through the VPN's DNS server; everything else goes direct. The daemon watches the VPN interface and applies/reverts rules automatically on up/down, and is controllable at runtime via the `splitway` CLI over a Unix socket.

## Current state

- Long-running daemon: auto-applies rules on VPN up, auto-reverts on down, and re-points its watch live when the configured interface/backend changes (no restart)
- Reports its own belief over IPC for verification: a self-explaining routing state, the applied DNS mapping (interface → domains → DNS servers), and detector health
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
splitway status            # enabled / vpn_up / routing state / applied mapping / detector / domains
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

It shows live status — the routing state, the applied DNS mapping (interface →
domains → DNS servers), `vpn_up`, detector health, and the domain count — an
enable/disable toggle, the domain list with add/remove, and an editor for the
remaining config fields (`vpn_name`, `vpn_backend`, `openvpn.management`,
`openvpn.management_password_file`).

`vpn_name` is an **interface picker** populated from the daemon's live interface
list (up interfaces and VPN-like devices flagged), with a free-text fallback
that always preserves the configured value even when that interface is down.
Config changes take effect **live**: saving a new
`vpn_name`/`vpn_backend`/`openvpn` re-arms the daemon's VPN watch with no
restart — the old interface is reverted and the new one is watched immediately,
so `vpn_up` and the applied mapping track the configured interface right away. A
**Resync** button re-reads the config, reconciles, and refreshes the view; every
change refreshes the status immediately.

```sh
splitway-gui
```

Reachability matches the CLI: it tries the per-user socket
(`$XDG_RUNTIME_DIR/splitway.sock`) then the system socket (`/run/splitway` on
Linux, `/var/run/splitway` on macOS), so a login-session GUI can reach a system
daemon. If the daemon runs as root with its default `0600` socket, an
unprivileged GUI sees "permission denied" and shows the daemon's own guidance
(run as the daemon's user/group) — it never escalates. To let it connect as your
normal user, enable the opt-in socket group (see
[Using it under niri](#using-it-under-niri-wayland)). A daemon that is not
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

The flake also exposes `nixosModules.default` for installing Splitway as a
systemd service on a NixOS host — see [Install (NixOS)](#install-nixos) below.

## Install (NixOS)

On NixOS the flake's `nixosModules.default` takes you from zero to a running
daemon: it installs the package and runs `splitway-daemon run` as a systemd
service, with no manual `install`/`systemctl enable` steps (contrast the by-hand
systemd setup in [packaging/](packaging/README.md)).

### Add the flake input

Add Splitway as a flake input and import its NixOS module into the host. The
input's **default branch is the stable channel**; append `/dev` for the latest
development channel:

```nix
# flake.nix
{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    splitway.url = "github:stslex/splitway";      # latest dev channel: github:stslex/splitway/dev
  };

  outputs = { nixpkgs, splitway, ... }: {
    nixosConfigurations.myhost = nixpkgs.lib.nixosSystem {
      modules = [
        splitway.nixosModules.default
        {
          services.splitway.enable = true;

          # Prerequisites — the daemon shells out to nmcli + resolvectl,
          # so the host must provide both:
          networking.networkmanager.enable = true;
          services.resolved.enable = true;
        }
      ];
    };
  };
}
```

The module deliberately does **not** pull in NetworkManager or systemd-resolved
itself — the daemon resolves `nmcli` and `resolvectl` by bare name from the
host's PATH, so you enable those services yourself (above). Then rebuild:

```sh
sudo nixos-rebuild switch --flake .#myhost
```

The service runs as **root** (privileged `resolvectl` changes), gets a `0700`
`RuntimeDirectory` for its `0600` control socket, restarts on failure, and
reverts DNS rules on `SIGTERM` so a stop never leaves the system half-configured.

### Where the config lives on NixOS

The NixOS service runs as root and owns a **writable** config at
**`/var/lib/splitway/config.json`**, provisioned by systemd's `StateDirectory`
(a `0700` directory owned by the service). This is **not**
`~/.config/splitway/config.json` — that default applies only to a by-hand
`splitway-daemon run`. The daemon creates the file empty on first start; on
upgrade from an older module that ran without `--config`, the module's systemd
`preStart` seeds it once from a pre-existing `/root/.config/splitway/config.json`
so an existing `vpn_name`/domains are not silently dropped.

Prefer changing it through the CLI or GUI, which mutate it through the daemon's
single-writer state actor; a direct `sudo`-edit works too, and external edits are
picked up live. See [Config](#config) for the field reference (`vpn_name`,
`vpn_hosts`, `vpn_backend`, `openvpn`).

### Using it under niri (Wayland)

niri is a tiling Wayland compositor with **no system tray**, so Splitway is a
normal CLI plus an ordinary GUI window.

**CLI** — talks to the root daemon over its root-owned socket, so it needs root:

```sh
sudo splitway status
sudo splitway add corp.example.com
sudo splitway check https://corp.example.com
sudo splitway verify
```

**GUI** — with no tray, run `splitway-gui` as a plain window, bound to a niri
keybind (or launched with `spawn-at-startup`):

```kdl
# ~/.config/niri/config.kdl
binds {
    Mod+Shift+S { spawn "splitway-gui"; }
}
```

**Unprivileged access (opt-in).** By default the control socket is `0600` and
root-owned, so a CLI or GUI launched as your normal desktop user gets "permission
denied" — it surfaces the daemon's own guidance and never escalates (see
[GUI](#gui)) — and the working path is the CLI via `sudo` above. Running a Wayland
GUI as root is not a good answer, so the daemon supports an **opt-in
group-accessible socket**: a `0660` socket owned by a dedicated group, inside a
`0750 root:<group>` runtime dir, that you join to connect without `sudo`. On NixOS
enable it via the module:

```nix
services.splitway = {
  enable = true;
  unprivilegedGui = {
    enable = true;
    users = [ "your-username" ];   # added to the "splitway" group
  };
};
```

After a rebuild, `splitway status` and `splitway-gui` work as your normal user —
no `sudo`. (Other init systems: add `--socket-group splitway` to the daemon's
`ExecStart`, set the runtime dir to `0750`, and create + join the group; see
[packaging/README.md](packaging/README.md#socket-security-model).)

> **Security note.** Membership in this group grants the ability to drive the
> daemon's privileged split-DNS operations — **adding a user to the group ≈
> granting them control of system split-DNS routing.** That is why it is opt-in
> and the group is empty by default. For why `0600` is the default, and the full
> threat model, see [packaging/README.md](packaging/README.md#socket-security-model).

## Roadmap

See [ROADMAP.md](ROADMAP.md) for the phased plan and done-criteria. Shipped so
far: testable foundation → abstraction split (`VpnDetector`/`DnsBackend`) → real
daemon + IPC → OpenVPN and macOS backends → an interim egui GUI. Next: finish
the verification / business-logic work, then Linux + macOS packaging, a native
Tauri GUI, and a hardening pass.

## Development

Workflow rules live in [CLAUDE.md](CLAUDE.md): one phase = one branch = one PR into `dev`, English only. Implementation prompts are ephemeral and not committed; durable design lives in [ROADMAP.md](ROADMAP.md), [docs/architecture.md](docs/architecture.md), and [docs/design/](docs/design/).

## Stack

Rust, systemd-resolved, NetworkManager (Linux), SCDynamicStore + `/etc/resolver` (macOS), Cargo workspace
