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

On NixOS, use the flake's `nixosModules.default` instead (`services.splitway.enable = true;`).

### Socket security model

The daemon is privileged; the CLI is not. The Unix control socket is the
privilege boundary:

- Path: `$XDG_RUNTIME_DIR/splitway.sock`, falling back to
  `/run/splitway/splitway.sock` for a system service.
- Permissions: **`0600`, owner-only**. The containing directory is `0700`
  (`$XDG_RUNTIME_DIR` already is; the `/run/splitway` fallback is created that
  way), so the brief window between `bind()` and `chmod` is still not reachable
  by other users.
- **Threat model:** any process that can write the socket can change DNS.
  `0600` restricts that to the user running the daemon — root, for the system
  service (so control commands run via `sudo`). For unprivileged multi-user
  control, an operator can widen this to `0660` owned by a dedicated
  `splitway` group; this is intentionally **not** the default, to avoid
  silently broadening who can change DNS.
- No secrets or user data are placed in the socket path.

`SIGTERM` (systemd stop / `kill`) makes the daemon revert active DNS rules
before exiting, so a stop never leaves the system half-configured.

## macOS (launchd)

`launchd/com.splitway.daemon.plist` runs `splitway-daemon run` as a
**LaunchDaemon (root)**. Root is required: the backend writes
`/etc/resolver/<domain>` files and flushes the DNS cache
(`dscacheutil -flushcache`, `killall -HUP mDNSResponder`). VPN up/down is
detected via SCDynamicStore (`scutil --dns` for the DNS servers).

```sh
sudo install -Dm755 target/release/splitway-daemon /usr/local/bin/splitway-daemon
sudo install -Dm755 target/release/splitway         /usr/local/bin/splitway
sudo install -Dm644 packaging/launchd/com.splitway.daemon.plist \
    /Library/LaunchDaemons/com.splitway.daemon.plist
sudo launchctl load -w /Library/LaunchDaemons/com.splitway.daemon.plist
# stop + revert:
sudo launchctl unload -w /Library/LaunchDaemons/com.splitway.daemon.plist
```

Configure the VPN interface in `~/.config/splitway/config.json` (`vpn_name` is
the `utun*` interface — find it with `scutil --nwi` or `ifconfig` while the VPN
is up); the daemon reads `$HOME/.config/splitway` of the user it runs as.

### Socket on macOS

There is no `$XDG_RUNTIME_DIR` for a LaunchDaemon, so the control socket falls
back to a system path: **`/var/run/splitway/splitway.sock`** on macOS (macOS has
no `/run`, and `/` is read-only). The daemon creates that `0700` directory on
start and binds a `0600` socket inside it. Drive the daemon with the `splitway`
CLI via `sudo` (the socket is root-owned), exactly as on the Linux system
service. `SIGTERM` (from `launchctl unload`) makes the daemon revert active
`/etc/resolver` rules before exiting.
