# Phase 3b — macOS backend

Implement sub-phase 3b from `ROADMAP.md` (Phase 3 — Backend breadth). Read `CLAUDE.md` first and use the `platform-dns` skill (macOS section: SCDynamicStore event source, `/etc/resolver`, cache flush). This makes the daemon actually run on macOS: a real `VpnDetector` and a real `DnsBackend` replacing today's `todo!()` stubs.

This is the largest sub-phase and requires testing on real macOS hardware (available). Parallel-safe with Phase 3a: touches only `detector/macos.rs`, `backend/macos.rs`, their new submodules, macOS packaging, and docs. Run in a separate worktree.

## Branch

Branch `phase-3b-macos` from up-to-date `dev`.

## DnsBackend (macOS) — `/etc/resolver`

Replace the `backend/macos.rs` stub. Per the skill:

- `apply_rules`: for each domain, write `/etc/resolver/<domain>` containing `nameserver <ip>` lines (one per server in `vpn_info.dns_servers`). Create `/etc/resolver` if absent. Use the **atomic write** pattern (temp + rename within the same dir) so a crash can't leave a half-written resolver file. After writing, flush caches: `dscacheutil -flushcache` and `killall -HUP mDNSResponder`
- `revert_rules`: remove the `/etc/resolver/<domain>` files this daemon created, then flush caches. Track which files we own so revert never deletes a resolver file the user created by hand — record applied domains in daemon state (already tracked) and only remove those
- Apply must be transactional like Linux: if writing domain N fails after 1..N-1 succeeded, remove the ones already written and return the original error (no half-configured state)
- `status`: report resolver state for the interface's domains (read back the files / `scutil --dns`)
- Requires root — document in the PR and in the launchd plist (runs as root)

## VpnDetector (macOS) — SCDynamicStore

Replace the `detector/macos.rs` stub:

- `detect(interface)`: read current DNS servers for the VPN interface (`utun*`). Source: `scutil --dns` parsed, or the SCDynamicStore key for the interface. Parsing goes in a **pure function with unit tests** (mirror the Linux `parser.rs` + `state.rs` split — pure parsing/mapping tested, FFI plumbing thin and untested)
- `watch(interface)`: subscribe via SCDynamicStore (the `system-configuration` crate) to interface/DNS state keys; on change, diff to up/down and feed the same `tokio::sync::mpsc::Sender<VpnEvent>` contract the Linux detector uses. The Core Foundation run loop must run on its own dedicated thread (it blocks); bridge it to the async channel. Reuse a `Deduper`-equivalent so repeated notifications don't emit duplicate events
- Polling `scutil` on a timer is an acceptable **fallback only** — if you use it, justify why SCDynamicStore didn't work, and isolate it so it can be swapped later
- Add `system-configuration` (and `core-foundation` if needed) under `[target.'cfg(target_os = "macos")'.dependencies]` so Linux/Windows builds stay clean

## Packaging

- `packaging/launchd/com.splitway.daemon.plist` — a LaunchDaemon (root) running `splitway-daemon run`, `KeepAlive`, logging to a file under `/var/log` or the unified log. Document install steps (`launchctl load`) in the README
- README: add macOS to supported platforms; document `/etc/resolver` mechanism, root requirement, and launchd install

## Out of scope

- Windows (stub remains)
- GUI (Phase 4)
- OpenVPN-specific work (3a / 3c) — the macOS backend is VPN-agnostic; detection keys off `utun*` interface + DNS state regardless of which VPN client created it
- Routing-only semantics tuning

## Done criteria

- fmt, clippy, tests green on CI (ubuntu + macos)
- On real macOS hardware: connecting the VPN auto-applies `/etc/resolver` entries and DNS for the configured domains resolves through the VPN; disconnecting reverts and removes only daemon-owned files — manual verification log (with `scutil --dns` before/after, **redacted to placeholder IPs/domains**) in the PR
- `splitway-cli status/enable/disable/add/remove` drive the daemon on macOS over the same socket
- DNS/interface parsing and event mapping unit-tested as pure functions
- Revert never touches resolver files the daemon didn't create (verified)
- `SIGTERM` reverts before exit on macOS too

## Finish

PR into `dev` titled `Phase 3b: macOS backend`. Description: SCDynamicStore vs polling decision (with rationale), the owned-files revert safety design, atomic write, manual verification log from real hardware, done-criteria checklist.
