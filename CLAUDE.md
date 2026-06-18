# CLAUDE.md

## Project

Splitway — domain-based split-DNS tool for Linux/macOS desktops. Routes selected domains through the VPN's DNS, everything else direct. See `README.md` and `ROADMAP.md`.

Cargo workspace:

- `splitway-daemon` — core: detects VPN, applies/reverts DNS rules
- `splitway-cli` — IPC client over the daemon socket
- `splitway-gui` — interim egui GUI; a pure IPC client with no privileges (native Tauri GUI planned — see `ROADMAP.md`)
- `splitway-shared` — config parsing, platform traits (`DnsBackend`, `VpnDetector`)

## Commands

```sh
cargo build
cargo test
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
```

All four must pass before opening a PR.

## Workflow

Development follows `ROADMAP.md` phases strictly: one phase = one branch = one PR.

- Branch from `dev`, named `phase-<n>-<slug>` (e.g. `phase-0-foundation`)
- Phase prompts live in `docs/prompts/`; implement exactly the prompt's scope, nothing more
- PR targets `dev`. Never push directly to `dev` or the default branch
- A new phase starts only after the previous PR is: CI green, all review comments resolved, merged into `dev`
- Address review comments only when there is a real need; push back with reasoning otherwise

## Language

English only — everywhere: code, comments, docs, branch names, commit messages, PR titles and descriptions, prompts.

## Quality bar

- No features beyond the current phase scope
- Pure logic gets unit tests; parsing must be testable without live system commands
- A failed apply must never leave the system in a half-configured state
- `log` crate for all output in library/backend code, never `println!`
