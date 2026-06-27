# macOS self-install — one-click privileged daemon bootstrap from Splitway.app

The macOS daemon must run as **root** (it writes `/etc/resolver/<domain>` and
flushes the DNS cache). Before this, bringing it up meant a terminal ritual:
`sudo launchctl load …` plus a hand-written config. This feature makes the user
double-click `Splitway.app`, click one button, authenticate once via the native
macOS password dialog, and have the root daemon installed + running — no terminal.

## The agreement

`Splitway.app` is built locally from source and ships **unsigned** (ad-hoc
identity `-`): no Apple Developer account, no notarization, `.app` only (no
`.dmg`/`.pkg`). A locally-built `.app` carries no `com.apple.quarantine`, so it
launches from `/Applications` without Gatekeeper friction — which is exactly what
makes the no-signing path viable.

The app installs the daemon itself through two Tauri commands keyed to the
existing health states:

- **`install_service`** — offered when `Health == NotRunning` (no socket). It
  escalates via `osascript`'s `do shell script … with administrator privileges`
  (one native password prompt) to run the bundled `bootstrap.sh install` as root.
  The script, idempotently: installs `splitway-daemon` + `splitway` from the app
  bundle to `/usr/local/bin` (`755`, quarantine stripped); ensures a `splitway`
  group and adds the console user; installs the GUI LaunchDaemon plist (carrying
  `--socket-group splitway`) to `/Library/LaunchDaemons`; and
  `launchctl bootout` → `bootstrap` → `enable`s it.
- **`disable_service`** — a discreet footer link once connected. Runs
  `bootstrap.sh disable`: `launchctl bootout` (SIGTERM → the daemon reverts
  `/etc/resolver` before exit) and removes the plist so it will not relaunch.

Both keep the **truth contract** ([architecture.md](../architecture.md) §2): the
command does the privileged work, fires refresh-now, and returns a
`Result<(), String>` — it never touches the view-model. The real health
(`NotRunning` → `PermissionDenied`/`Connected`, or back to `NotRunning`) flows
back only through the next `view-model-changed`, exactly as for the mutation
commands. No optimistic flips.

A third command, **`host_platform`**, lets the frontend branch the remediation
copy: macOS gets the Install button / sign-out guidance; Linux keeps its
`systemctl` / `usermod` copy-paste commands. This is frontend presentation only —
no view-model field is added, so the bindings contract is untouched.

## Why this shape

- **`osascript` admin escalation, not `SMAppService`.** `SMAppService` needs code
  signing + notarization + a user-approved Login Item — all rejected here.
  `osascript … with administrator privileges` is the supported, signing-free way
  to get one password prompt and run a fixed root command.
- **Not the deprecated `AuthorizationExecuteWithPrivileges`**, and not a
  `brew services` wrapper (a Finder-launched `.app` has a minimal `PATH`, and the
  brew prefix differs Intel vs ARM — Homebrew is a later phase anyway).
- **The escalated command is inert.** It is a fixed `/bin/bash <script> install`
  where the only variable is the bundle-derived resource path — never GUI/user
  input. The path is injected as an AppleScript string variable and handed to the
  shell via `quoted form of`, so a path with spaces stays one token; an unsigned
  app running a root command must keep that surface closed
  (`build_admin_applescript` is pure + unit-tested for this).
- **All steps in one bundled `bootstrap.sh`** under one privilege prompt: one
  password dialog, `set -euo pipefail`, a pinned system `PATH`, every step
  idempotent — a failed install never half-configures the system (the same
  apply-or-rollback bar the daemon itself holds). The `bootout`→`bootstrap`
  relaunch settles (polls the service record to gone) and retries the transient
  launchd race so a re-install does not intermittently leave the daemon stopped.
- **The root daemon binary must live in a fully root-owned path.** It is installed
  to `/usr/local/bin` `root:wheel 0755`, and the installer **pins that directory to
  `root:wheel 0755` and verifies every parent component up to `/` is root-owned and
  not group/other-writable, aborting before it touches ownership if any link is
  not** — otherwise, on the Homebrew-on-Intel layout where `/usr/local` (or
  `/usr/local/bin`) is admin-writable, a non-root process could swap the binary —
  or, since renaming a directory entry needs write access to its *parent*, rename
  the pinned `bin` out from under us and drop in its own — and have the root
  LaunchDaemon exec it at the next boot (a persistent local privilege escalation —
  the launchd "unsafe binary location" anti-pattern).
- **An independent LaunchDaemon, group-reachable socket.** The GUI runs as the
  desktop user, so it reaches the root daemon only through the opt-in socket group
  (`--socket-group splitway` → `/var/run/splitway` `0750 root:splitway`, socket
  `0660 root:splitway`) — the macOS analog of the NixOS `unprivilegedGui` option
  ([socket-group.md](socket-group.md)). The bundled GUI plist carries that flag;
  the sibling `com.splitway.daemon.plist` stays the manual/sudo (root-only socket)
  path.
- **Disable is conservative.** It stops the daemon and removes the plist, but
  leaves the binaries, group, membership, and config — so a re-install needs no
  re-prompt. Full uninstall is a separate, later step.

## Trust boundary of the unsigned self-installer

The `.app` is unsigned and built locally, so there is no cryptographic anchor over
the bundle. Authenticating the `osascript` prompt delegates root to whatever
currently sits in `Contents/Resources`: `osascript` runs
`/bin/bash <bundle>/Contents/Resources/bootstrap.sh install` **as root**, and
`splitway-daemon` is copied from that same directory. Codex flagged that this
copies a bundled binary into a root-run location without verifying it; the residual
risk is real, but it is a categorical limitation of the unsigned model, not a
fixable gap in the script — so it is documented here rather than papered over with
an in-bundle check.

- **No in-bundle integrity check is attempted, by design — the load-bearing
  reason is co-location.** `bootstrap.sh` is co-located with, and exactly as
  writable as, the binaries it copies (and the GUI binary in `Contents/MacOS`).
  Any principal who can replace `splitway-daemon` can equally rewrite
  `bootstrap.sh`, so a compiled-in hash or an in-script source check is defeated
  by the same write access: the attacker edits out the check, or replaces the
  whole script with a root payload that runs before any check. This holds even for
  a *narrower, non-breaking* source check (e.g. "refuse if the source is
  group/other-writable, or owned by a principal other than the console user/root"):
  in the only cross-user state where it would fire, the less-privileged writer
  authored `bootstrap.sh` too and just deletes the check; and in the same-user
  TOCTOU case it is a no-op (owner == console user, path not world-writable → it
  passes). The verifier cannot sit outside the writable bundle, so it offers no
  protection against the only attacker who can trigger the bug.

- **A root-owned-source check would additionally break the feature.** A
  drag-installed bundle's contents are owned by the *installing user*, not root, in
  `/Applications` and `~/Applications` alike (the property that lets apps
  self-update without admin; Finder copy preserves source perms). So a
  root-owned / not-group-or-other-writable source check would reject every
  supported install location while still being bypassable via the co-located
  script — hence it is not adopted.

- **Location is part of the boundary, but does not close the same-user case.**
  `/Applications` is `drwxrwxr-x root:admin` (0775): a non-admin "other" cannot
  create/replace entries there, and a drag-installed bundle's contents are not
  group/other-writable, so a *different* non-admin user cannot tamper. But bundle
  contents are owned by the installing user **regardless of `/Applications` vs
  `~/Applications`**, so same-user TOCTOU — code running as the user who clicks
  Install, riding their single legitimate authentication — is location-independent.

- **The destination side IS hardened.** `bootstrap.sh` pins `/usr/local/bin` to
  `root:wheel 0755` and re-verifies its ownership, and `assert_root_only_path`
  verifies every ancestor (`/usr/local` up to `/`) is root-owned and not
  group/other-writable before install. The asymmetry with the source is
  fundamental: the destination is a fixed system path that root can soundly check
  (a non-root principal cannot touch it); the source is, by the feature's premise,
  a freshly built/copied user-writable bundle with no root-only path to verify
  against.

- **The residual risk and its only sound fix.** A genuine local privilege
  escalation remains where a *less-privileged* principal can write the bundle and a
  *more-privileged* one authenticates: a non-admin user's writable `~/Applications`
  bundle (or a world-writable shared dir such as `/tmp` or `/Users/Shared`, 1777)
  that an admin later authorizes, or non-root malware running as the admin user
  riding the install prompt. This is closed only by an OS-enforced root of trust —
  Developer-ID signing + notarization + `SMAppService`, where the kernel verifies a
  signature the attacker cannot forge and binds the privileged helper to the signed
  app — listed under [Scope / out of scope](#scope--out-of-scope). Until then,
  operational guidance: an admin should not authenticate an install of a
  `Splitway.app` that lives where a less-privileged principal can write it; a
  read-only distribution medium (a signed DMG/PKG) would also close in-place source
  tampering but is itself out of current scope.

## The re-login gotcha — observed behavior

macOS materializes a process's supplementary group set into its kernel credential
at **login**. `dseditgroup -o edit -a <user> … splitway` updates the Directory
Services record, but an already-running login session — including the GUI and
anything it spawns — keeps its original group set until the user logs out and back
in.

**Observed on the live machine (macOS 26, Apple Silicon): the gotcha did NOT
manifest.** A GUI launched _after_ the `dseditgroup` add connected to the
group-owned socket immediately — `id -Gn` already listed `splitway`, the
unprivileged CLI got a clean `status`, and the freshly-launched `.app` showed
`Connected`, not `PermissionDenied`. So on this macOS, a process spawned after the
membership change picks up the new group from Directory Services without a
re-login. (The classic gotcha bites a process that was _already running_ across
the change — e.g. a GUI left open during install. Launching or relaunching the
app after install is enough here; a full logout was not needed.)

Because the failure mode is still reachable (an app open across the change),
`Health::PermissionDenied` on macOS is kept honest: its blocker reports that
membership was added and points at the sign-out / relaunch remedy, and does
**not** tell the user to run `usermod` (wrong OS, and membership is already
granted). After a relaunch the GUI connects and shows `Connected`.

### A WKWebView confirmation gotcha (found in live test)

The in-app "Stop the Splitway service" link does **not** use `window.confirm()`:
under WKWebView (the macOS webview Tauri uses), native `confirm()` is suppressed
and returns `false`, which would silently no-op the disable. The confirmation is a
**two-click arm** instead (first click → inline "Confirm stop / Cancel"; second
click runs the privileged disable). This keeps the confirmation working without a
native dialog or any new plugin/capability.

## Scope / out of scope

- **In:** the `.app` bundle (ad-hoc, additive to the Linux/Nix build), the
  install/disable/platform Tauri commands, the bundled `bootstrap.sh` + GUI plist,
  and the health-keyed GUI affordances.
- **Out:** Homebrew tap (next phase — it installs the same `.app` + binaries and
  must **not** add a `service` block, or two owners would fight over the socket
  and `/etc/resolver`); code signing / notarization / Developer ID; `.dmg` /
  `.pkg` / `SMAppService`; any macOS DNS/VPN backend change; full uninstall.

## How the bundle stays additive

The Tauri bundler (`cargo tauri build`) is the only thing that reads
`tauri.conf.json`'s `bundle` section; `cargo build` and the Linux `nix build`
(`buildRustPackage` + `wrapGAppsHook3`) never invoke it. So the macOS bundle is a
separate path that cannot perturb the Linux build. To keep it explicit, the bundle
settings live in a `tauri.bundle.macos.json` overlay applied **only** by the build
wrapper via `--config` — named so Tauri does **not** auto-merge it into a plain
`cargo build`/clippy (which would validate the staged resources before they
exist). `scripts/build-macos-app.sh` builds the helper binaries + frontend, stages
the resources, and runs the bundler; it pulls the off-PATH toolchain
(`cargo-tauri`, `node`, `typescript`, `esbuild`, `librsvg`) from nixpkgs via
`nix shell`, so a plain `bash scripts/build-macos-app.sh` just works.

## Links

- [socket-group.md](socket-group.md) — the `--socket-group` model this reuses.
- [architecture.md](../architecture.md) — the truth contract the commands honor.
- [tauri-mutations.md](tauri-mutations.md) — the daemon-first / refresh-now command
  shape the install/disable commands follow.
- `packaging/README.md` — the manual/sudo macOS install (the non-GUI path).
