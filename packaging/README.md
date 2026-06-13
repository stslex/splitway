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

Not yet. The macOS DNS backend and VPN detector are still `todo!()` stubs, so
the daemon cannot run there. A launchd plist arrives with the macOS runtime in
**Phase 3** (see `ROADMAP.md`).
