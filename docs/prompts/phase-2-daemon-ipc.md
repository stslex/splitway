# Phase 2 — Real daemon + IPC

Implement Phase 2 from `ROADMAP.md`. Read `CLAUDE.md` first. This is the headline feature: the daemon stops being one-shot and becomes a long-running process that auto-applies rules on VPN up and reverts on down, controllable at runtime via a Unix socket from `splitway-cli`.

This is a large phase. If during implementation it proves cleaner to land it as two stacked PRs (2a: daemon event loop + graceful shutdown; 2b: IPC protocol + live CLI), that is acceptable — both target `dev`, 2b branches from 2a. Default to a single PR if it stays reviewable.

## Branch

Branch `phase-2-daemon-ipc` from up-to-date `dev`.

## Architecture

### Daemon = long-running async process

`splitway-daemon run` becomes the persistent daemon (this changes the old one-shot `run` semantics — intended). It:

1. Builds a multi-thread tokio runtime
2. Starts `VpnDetector::watch` for the configured interface
3. Runs an event loop: on `VpnEvent::Up(info)` → apply rules; on `VpnEvent::Down` → revert
4. Concurrently serves an IPC socket for CLI commands
5. On `SIGTERM`/`SIGINT` → revert active rules, then exit cleanly (never leave the system half-configured — this is in the quality bar)

`DnsBackend` methods are blocking (they shell out). Call them via `tokio::task::spawn_blocking` so a slow `resolvectl` never stalls the event loop or the IPC accept loop.

### Single-owner state, no shared locks

The event loop and IPC handlers both mutate "currently applied state" (interface, applied domains, enabled/disabled). Do **not** wrap state in a shared `Mutex` accessed from many tasks. Use a single state-owner task that receives commands over an `mpsc` channel (both VPN events and IPC requests funnel into it); IPC handlers send a command + a `oneshot` reply channel. This serializes all state transitions and removes ordering/lock-poisoning bugs by construction. Unit-test the state machine (enabled+up→applied, disabled→reverted, domain add while up→re-apply, etc.) without any real backend by injecting a mock `DnsBackend`.

### IPC

- Unix domain socket. Path: `$XDG_RUNTIME_DIR/splitway.sock`, fallback `/run/splitway/splitway.sock`. Remove a stale socket file on startup; recreate
- Protocol: newline-delimited JSON (one JSON object per line), request → single response. Define `Request`/`Response` enums in a **new `splitway-shared/src/ipc.rs` module** so daemon and CLI share one source of truth. Version the protocol with a constant
- Requests (minimum): `Status`, `Enable`, `Disable`, `AddDomain(String)`, `RemoveDomain(String)`, `ListDomains`, `ReloadConfig`
- Responses carry either data or a structured error; never panic on a malformed request — log and reply with an error

### Security (must address explicitly in the PR description)

The daemon performs privileged DNS changes; the CLI must not require root. So the socket is the privilege boundary:

- Create the socket with restrictive permissions: owner-only (`0600`) by default, or `0660` owned by a dedicated group if multi-user control is wanted. Set the mode atomically (umask around bind, or bind then `set_permissions`)
- Any local process that can write the socket can change DNS — document the chosen perms and threat model in the PR
- Do not put secrets or user data in the socket path

## Config mutation

`AddDomain`/`RemoveDomain`/`Enable`-state must persist to `config.json`. Current `create_empty_config` writes in place — replace the write path with an **atomic write** (write to a temp file in the same dir, `fsync`, then `rename`) so a crash mid-write can't corrupt the config. Add this as a function in `splitway-shared/src/config.rs` and unit-test it. Reject domain duplicates; `RemoveDomain` of an absent domain is a no-op success.

## CLI (`splitway-cli`)

Currently `println!("init cli!")`. Implement a real client:

- Arg parsing for subcommands mirroring the IPC requests: `status`, `enable`, `disable`, `add <domain>`, `remove <domain>`, `list`, `reload`. Use `clap` (derive)
- Connects to the socket, sends one request, prints the response, exits. No daemon logic in the CLI
- `tokio` (or sync `UnixStream` — CLI is single-shot, sync is simpler and fewer deps; prefer sync unless there's a reason). Add deps to `splitway-cli/Cargo.toml`
- Clear error if the daemon socket is absent ("is splitway-daemon running?")

## Deployment artifacts

- `packaging/systemd/splitway.service` — runs `splitway-daemon run`. Decide and document: system service (root, `resolvectl` works directly) vs user service. Given `resolvectl` needs privilege, a system service is the realistic default; note the polkit/root rationale in comments
- Update the NixOS module from Phase 0.5: replace the stub service with the real `systemd.services.splitway` running the daemon
- launchd plist for macOS: **not now** — the macOS detector is still a `todo!()` stub, so the daemon can't run there yet. Leave a one-line note pointing to Phase 3

## Keep / retire commands

- `run` → the daemon (changed semantics)
- `status` → keep as a quick one-shot that talks to the running daemon over IPC (not the old direct-backend call); if no daemon, say so
- `revert` → keep as an emergency one-shot direct-backend revert (works even with no daemon running) — useful escape hatch; document the distinction from `disable`
- `watch` → remove; the daemon's event loop supersedes the debug subcommand (or keep it as a hidden debug aid — your call, justify in PR)

## Out of scope

- GUI (Phase 4)
- New VPN/DNS backends, macOS runtime (Phase 3)
- Routing-only `~domain` semantics (still deferred; noted in the platform-dns skill)
- Multi-interface / multiple simultaneous VPNs

## Done criteria

- fmt, clippy, tests green on CI (ubuntu + macos compile; macOS daemon need not run)
- Connecting the VPN auto-applies rules with no manual command; disconnecting auto-reverts — manual verification log in the PR
- `splitway-cli status/enable/disable/add/remove/list/reload` drive a running daemon over the socket
- `SIGTERM` reverts rules before exit (verified)
- State machine unit-tested with a mock backend; atomic config write unit-tested
- Socket permission model implemented and documented

## Finish

PR into `dev` titled `Phase 2: Real daemon + IPC`. Description: architecture (state-owner task, spawn_blocking rationale), IPC protocol + socket security model, shutdown/revert behavior, manual verification log, done-criteria checklist.
