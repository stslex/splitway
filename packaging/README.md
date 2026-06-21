# Packaging & deployment

## Linux (systemd)

`systemd/splitway.service` runs `splitway-daemon run` as a **system service
(root)**. The daemon makes privileged DNS changes via `resolvectl`, which
polkit otherwise gates behind interactive authentication — impractical for an
unattended daemon, so it runs as root. A user service would instead need
polkit rules granting the `resolvectl` DNS actions.

The CLI is installed as `splitway` (the name it advertises: `splitway status`,
`splitway enable`, …); the daemon as `splitway-daemon`.

```sh
sudo install -Dm644 packaging/systemd/splitway.service /etc/systemd/system/splitway.service
sudo install -Dm755 target/release/splitway-daemon /usr/bin/splitway-daemon
sudo install -Dm755 target/release/splitway         /usr/bin/splitway
sudo systemctl enable --now splitway
```

On NixOS, use the flake's `nixosModules.default` instead
(`services.splitway.enable = true;`) — this is also the author's daily-driver and
iteration channel. The module runs the daemon with
`--config /var/lib/splitway/config.json` and provisions that path via systemd's
`StateDirectory` (a `0700` dir owned by the service). The config is **writable
and owned by the daemon** (the GUI/CLI mutate it at runtime, and external
hand-edits are picked up live); the module deliberately does not generate a
read-only `/etc` config. The daemon creates an empty config there on first run.
See [docs/architecture.md](../docs/architecture.md) ("Config is the single
source of truth").

### Socket security model

The daemon is privileged; the CLI is not. The Unix control socket is the
privilege boundary:

- Path: `$XDG_RUNTIME_DIR/splitway.sock`, falling back to
  `/run/splitway/splitway.sock` for a system service.
- Permissions: **`0600`, owner-only** by default. The containing directory is
  `0700` (`$XDG_RUNTIME_DIR` already is; the `/run/splitway` fallback is created
  that way), so the brief window between `bind()` and `chmod` is still not
  reachable by other users.
- **Threat model:** any process that can write the socket can change DNS.
  `0600` restricts that to the user running the daemon — root, for the system
  service (so control commands run via `sudo`).
- No secrets or user data are placed in the socket path.

#### Unprivileged access via a socket group (opt-in)

For unprivileged control — e.g. a GUI run as your desktop user under a
no-system-tray compositor like niri — the daemon accepts `--socket-group <name>`.
With it, the socket is `0660` and the runtime dir `0750`, both owned by
`root:<group>`: a member of `<group>` can connect without `sudo`; a non-member
cannot even traverse the dir to reach the socket. Without the flag, behavior is
unchanged (`0600`, root-only).

- **On NixOS**, do not pass the flag by hand — enable
  `services.splitway.unprivilegedGui` (see the README's niri section), which
  creates the group, adds the listed users, sets `RuntimeDirectoryMode=0750`,
  and passes `--socket-group` for you.
- **On other init systems**, add `--socket-group splitway` to the daemon's
  `ExecStart`, set `RuntimeDirectoryMode=0750`, and `groupadd splitway` +
  `usermod -aG splitway <user>`. The daemon `chgrp`s the dir and socket on
  start; a missing group makes it exit non-zero with an actionable message.

> **Security:** membership in this group grants the ability to drive the daemon's
> privileged split-DNS operations (`resolvectl`/`nmcli`) — adding a user to the
> group ≈ granting them control of system split-DNS routing. That is why it is
> **not** the default and the group is empty until you opt in. (Stronger per-peer
> authentication via `SO_PEERCRED` is a later phase.) See
> [docs/design/socket-group.md](../docs/design/socket-group.md).

`SIGTERM` (systemd stop / `kill`) makes the daemon revert active DNS rules
before exiting, so a stop never leaves the system half-configured.

## macOS (launchd)

`launchd/com.splitway.daemon.plist` runs `splitway-daemon run` as a
**LaunchDaemon (root)**. Root is required: the backend writes
`/etc/resolver/<domain>` files and flushes the DNS cache
(`dscacheutil -flushcache`, `killall -HUP mDNSResponder`). VPN up/down is
detected via SCDynamicStore (`scutil --dns` for the DNS servers).

macOS ships BSD `install`, which has no `-D` flag, so create the target dir
first (`/Library/LaunchDaemons` already exists):

```sh
sudo mkdir -p /usr/local/bin
sudo install -m 755 target/release/splitway-daemon /usr/local/bin/splitway-daemon
sudo install -m 755 target/release/splitway         /usr/local/bin/splitway
sudo install -m 644 packaging/launchd/com.splitway.daemon.plist \
    /Library/LaunchDaemons/com.splitway.daemon.plist
```

Configure the VPN interface **before** starting the daemon. The plist runs the
daemon as root and pins `HOME=/var/root`, so the config lives at
`/var/root/.config/splitway/config.json` (`vpn_name` is the `utun*` interface —
find it with `scutil --nwi` or `ifconfig` while the VPN is up):

```sh
sudo mkdir -p /var/root/.config/splitway
sudo tee /var/root/.config/splitway/config.json >/dev/null <<'EOF'
{
  "vpn_name": "utun0",
  "vpn_hosts": [],
  "enabled": true
}
EOF
```

Then start the daemon:

```sh
sudo launchctl load -w /Library/LaunchDaemons/com.splitway.daemon.plist
# stop + revert:
sudo launchctl unload -w /Library/LaunchDaemons/com.splitway.daemon.plist
```

The daemon fixes the interface it watches at startup, so `vpn_name` must be set
before the first `launchctl load`. Changing `vpn_name` afterwards needs a
service restart to take effect — a config reload re-reads `vpn_hosts` and
`enabled`, but not the watched interface:

```sh
sudo launchctl unload -w /Library/LaunchDaemons/com.splitway.daemon.plist
sudo launchctl load   -w /Library/LaunchDaemons/com.splitway.daemon.plist
```

### Socket on macOS

There is no `$XDG_RUNTIME_DIR` for a LaunchDaemon, so the control socket falls
back to a system path: **`/var/run/splitway/splitway.sock`** on macOS (macOS has
no `/run`, and `/` is read-only). The daemon creates that `0700` directory on
start and binds a `0600` socket inside it. Drive the daemon with the `splitway`
CLI via `sudo` (the socket is root-owned), exactly as on the Linux system
service. `SIGTERM` (from `launchctl unload`) makes the daemon revert active
`/etc/resolver` rules before exiting.
