# Phase 0 — Foundation

Implement Phase 0 from `ROADMAP.md`. Read `CLAUDE.md` for workflow rules first. Scope is exactly the tasks below — no extra features, no trait refactoring (that is Phase 1), keep the current one-shot behavior intact.

## Branch

- If a `dev` branch does not exist yet, create it from the default branch and push it
- Branch `phase-0-foundation` from `dev`

## Tasks

### 1. Testable DNS parser

In `splitway-daemon/src/backend/linux/`, extract the parsing logic from `detect_vpn` into a pure function in a new `parser.rs` module:

```rust
fn parse_dns_from_nmcli(output: &str) -> Result<Vec<String>, PlatformError>
```

- Collect all `IP4.DNS[n]` entries (and `IP6.DNS[n]` if present) instead of the current first-line-containing-"DNS" heuristic, which is fragile and silently drops secondary servers
- `detect_vpn` keeps running `nmcli` and delegates parsing to this function
- Unit tests: realistic `nmcli device show` output with one DNS entry, multiple entries, IPv6 entries, no DNS entries (must return `ParseError`), empty input

### 2. Rollback in apply_rules

In `splitway-daemon/src/backend/linux/backend.rs`: `apply_rules` sets DNS first, then domains. If the domain step fails, the system is left half-configured. Fix: on domain-step failure, run the revert path for the interface, log the rollback outcome, and return the original error. Document the manual verification steps in the PR description (unit-testing this requires mocking command execution, which is out of scope until Phase 1 abstractions).

### 3. Binary resolution

Replace the hardcoded `/usr/bin/resolvectl` with `Command::new("resolvectl")` (PATH lookup, same as `nmcli` already does). This unblocks NixOS, where binaries do not live in `/usr/bin`.

### 4. Logging

Replace all `println!` in backend code with appropriate `log::debug!` / `log::info!` / `log::error!`. `env_logger` is already initialized in `main`.

### 5. Cleanup

- Remove unused `ConfigParseError::Unresolve` variant
- Fix `CommandParser::parse_command(self)` in `splitway-daemon/src/command/parser.rs`: it ignores `self` and re-reads `std::env::args()`. Make it consume `self`. Do not redesign CLI argument handling beyond this
- Add a serde round-trip unit test for `LocalConfig` in `splitway-shared` (the stale `vpn_ip` README example is already fixed)

### 6. CI

Add `.github/workflows/ci.yml`:

- Trigger: PRs and pushes to `dev`
- Matrix: `ubuntu-latest`, `macos-latest`
- Steps: `cargo fmt --all -- --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`
- The macOS backend is a `todo!()` stub — it must still compile; tests must not invoke it

## Done criteria

- `cargo test`, fmt, clippy green locally and on CI for both runners
- A failed apply leaves the system in its pre-apply state
- No `println!` left in backend code, no dead code listed above

## Finish

Open a PR into `dev` titled `Phase 0: Foundation`, with a description listing the changes and a checklist mirroring the done criteria.
