# Socket group — unprivileged GUI/CLI access to the root daemon

Status: implemented. Lands with the `--socket-group` daemon flag and the
`services.splitway.unprivilegedGui` NixOS option.

## The decision

Let a non-root user in a dedicated group connect to the daemon's control socket,
so the GUI can run unprivileged under niri (a compositor with **no system tray**,
so the GUI is an ordinary window run as the desktop user — see ROADMAP Phase 7).
This is the prerequisite for the Tauri shell (7b): without it, an unprivileged
GUI cannot reach a root daemon and the only path is the CLI via `sudo`.

The mechanism is **opt-in** and **secure-by-default**:

- **Default (no flag):** the socket is `0600`, root-owned, inside a `0700`
  runtime dir — byte-identical to before this change.
- **Opt-in (`--socket-group <name>`):** the runtime dir is `0750 root:<group>`
  and the socket is `0660 root:<group>`. An in-group user can connect; everyone
  else is blocked — they cannot even traverse the dir to probe the socket path.

## Why a CLI flag, not config

The socket group is a **deployment** concern, not routing state, so it is **not**
in the live-watched routing config (the single source of truth for routing — see
[`../architecture.md`](../architecture.md), "Config is the single source of
truth"). Reasons:

- Socket setup runs once at startup, outside the config-watch loop. Re-`chmod`ing
  a live socket on a config edit is meaningless.
- Keeping it out of the SSOT file preserves a clean boundary: the config file
  describes *what to route*, the unit/flag describes *how the daemon is deployed*.

So the systemd unit passes `--socket-group`; on NixOS the module sets it from
`services.splitway.unprivilegedGui`.

## Security model

Membership in the socket group grants the ability to drive the daemon's
privileged split-DNS operations (`resolvectl` / `nmcli`). **Adding a user to the
group ≈ granting them control of system split-DNS routing.** That is why it is
opt-in and the NixOS module's `users` list is empty by default — the module never
silently widens who can change DNS. (Stronger per-peer authentication via
`SO_PEERCRED` is deferred to a later phase; this change is about *who may
connect*, not *how the peer is authenticated*.)

The socket remains the privilege boundary (the daemon is privileged; the
CLI/GUI are not). This change widens that boundary from "one user (root)" to
"root plus a named group", explicitly and opt-in.

### Group access must not become arbitrary root-file reads

Widening *who may connect* also widens who may issue the existing `SetConfig`
verb — and two config fields make the **root** daemon read a file and send its
first line to a config-named endpoint: the standalone-OpenVPN
`openvpn.management` (a `host:port` or unix socket the daemon connects to) and
`openvpn.management_password_file` (a path whose first line the daemon sends as
the management password). Left IPC-mutable over a group socket, a non-root
in-group caller could point `password_file` at a root-only secret (e.g.
`/etc/shadow`) and `management` at a listener they control — exfiltrating the
file's first line with root's read privilege. That exceeds the intended grant
("control of split-DNS routing").

So when the socket is group-accessible, the `SetConfig` handler **refuses to
change** those two fields (only *changes* are rejected, so a client that
round-trips the current values while editing `vpn_name`/backend still works).
They stay settable by editing the root-owned config file
(`/var/lib/splitway/config.json`, `0700` root) — which an in-group user cannot
write. This is a deliberately blunt instrument: without per-peer identity the
daemon cannot tell a root caller from an in-group one, so it locks the fields for
*all* IPC callers while a group is configured. It is removable once per-peer
`SO_PEERCRED` auth (Phase 8) can authorize the dangerous fields for a root peer
specifically.

### Defense in depth

Both gates are applied, not just one:

- **Runtime dir `0750 root:<group>`** — a non-member cannot traverse into the dir,
  so they cannot even reach (let alone connect to) the socket path.
- **Socket `0660 root:<group>`** — members can `connect()` (which needs write);
  non-members never get the chance.

### The `bind()`→`chmod` window

A freshly `bind()`ed Unix socket briefly has `umask`-default perms before we
`chmod` it. `umask` is **not** used to pre-narrow it: `umask` is process-global
and would race file creation in other tasks of the multi-threaded daemon. Instead
the window is closed by **ordering**:

1. The parent dir is tightened to `0750 root:<group>` **before** the socket is
   bound — so only members (and root) can traverse to it at all.
2. The socket is `chmod`ed to its final mode (`0660`) **while still owned by
   `root:root`** — so only root can connect during the window.
3. Only then is it `chgrp`ed to `<group>` — at which instant it is already
   `0660`, so exactly the target group gains access and never a wider set.

At no point is the socket reachable by a non-member.

### Fail-fast

If `--socket-group` is set but cannot be honored — the group name does not
resolve, or the daemon is not privileged to `chown` (not root and not in the
group) — the daemon logs an actionable error and **exits non-zero**. Silent
degradation into ambiguous permissions is the worse outcome; an operator who
asked for a group gets a clear failure instead of a quietly-wrong socket.

One related case is **warned, not fatal**: if the socket resolves under a
user-private `$XDG_RUNTIME_DIR` (a login-session daemon) while a group is
requested, the group socket is *inert* — the `0700` session dir, which we must
not widen, blocks group traversal regardless of the socket's own mode. This is a
deployment mismatch (the group socket is a system-service feature), not a security
hole (it fails closed), so the daemon logs a loud `warn` rather than aborting. No
supported deployment hits it: the systemd unit / NixOS module run as root with no
`XDG_RUNTIME_DIR`, so the socket lands in the system runtime dir and the group is
applied to both dir and socket.

## NixOS module shape

`services.splitway.unprivilegedGui`:

- `.enable` (bool, default `false`) — the high-level toggle.
- `.group` (str, default `"splitway"`) — escape hatch for the group name.
- `.users` (list of str, default `[]`) — existing users to add to the group.
  Empty by default so the module never silently grants DNS-control rights; the
  operator opts in by listing users (or adds the group in their own user config).

When enabled the module creates the group, adds the listed users, sets
`RuntimeDirectory=splitway` with `RuntimeDirectoryMode=0750`, passes
`--socket-group <group>` to `ExecStart`, and adds the group to the daemon's
`SupplementaryGroups`. The daemon stays `User=root` (privileged DNS changes); root
can `chgrp` to any group, so `SupplementaryGroups` is future-proofing for a later
drop to a non-root user, not a present requirement. When disabled, none of this
is emitted — no group, no flag, socket stays root-only.

## Scope

- **In:** Linux + systemd / NixOS. The daemon flag, the dir/socket perms, the
  module option, unit-level tests, and a nixosTest.
- **Out:** any GUI code (7b); macOS socket access (a different model — separate
  phase); `SO_PEERCRED` / per-peer auth (later phase); multi-user / multi-profile
  semantics. **No protocol version bump** — this changes who may connect, not the
  wire format (still v6); the framing and handshake are untouched.

## Notable choices / rejected alternatives

- **`libc` over the `nix` crate.** The original plan favored `nix`
  (`Group::from_name`, `chown`). The build environment has no crates.io network
  access, so adding a new dependency tree is not possible without breaking the
  reproducible build; `libc` is already in the lockfile and `getgrnam_r` +
  `chown` are a small, self-contained amount of `unsafe`. The flag plumbing and
  perms logic are identical either way.
- **Fail-fast over warn-and-continue** when the group can't be honored (Open
  decision #1) — see "Fail-fast" above.
- **Toggle + optional `group` + optional empty `users`** over a single toggle
  (Open decision #2). The empty `users` default is the key safety property.
- **nixosTest under `legacyPackages`, not `checks`.** It boots a VM and needs
  `/dev/kvm`, which GitHub's default CI runners do not reliably expose. `checks`
  is built by CI's `nix flake check`; `legacyPackages` is not. The test is run
  locally (the author daily-drives NixOS, where KVM is available):
  `nix build .#legacyPackages.x86_64-linux.tests.socketGroup -L`.
- **Cross-user *denial* is proven by the nixosTest, not the daemon unit tests.**
  Faking a second uid/gid unprivileged is not portable, so the unit tests cover
  the resulting mode + ownership + same-user connect (injecting the caller's own
  gid, which `chgrp` always permits unprivileged), and the nixosTest covers the
  in-group-connects / out-of-group-denied contract end-to-end with `splitway-cli`.

## Links

- [`../architecture.md`](../architecture.md) — "Config is the single source of
  truth" (why the group is a flag, not config).
- [`../../packaging/README.md`](../../packaging/README.md) — socket security model
  / threat model.
- `splitway-daemon/src/daemon/ipc.rs` — `bind_socket` and the perms helpers.
- `splitway-daemon/src/daemon/state.rs` — the `SetConfig` group-lock for the
  file-reading OpenVPN fields.
- `nix/module.nix`, `nix/tests/socket-group.nix` — module option + nixosTest.
