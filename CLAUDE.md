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

- Branch from `dev`, named `phase-<n>-<slug>` (e.g. `phase-0-foundation`) or a
  descriptive slug; one branch = one PR
- Implementation prompts are **ephemeral** — used to drive a change, **not
  committed** to the repo. Durable design knowledge lives in `ROADMAP.md` (the
  plan), `docs/architecture.md` (cross-cutting invariants), and `docs/design/`
  (per-feature decisions, landing with the feature's PR)
- PR targets `dev`. Never push directly to `dev` or the default branch
- A new phase starts only after the previous PR is: CI green, all review comments resolved, merged into `dev`
- Address review comments only when there is a real need; push back with reasoning otherwise

## Language

English only — everywhere: code, comments, docs, branch names, commit messages, PR titles and descriptions, prompts.

## Redaction — never commit real infrastructure data

Test fixtures, docs, prompts, commit messages, PR descriptions, screenshots, and verification logs must contain **synthetic placeholder values only** — never real infrastructure data captured from a live machine.

- IPs: use the RFC 5737 ranges (`192.0.2.0/24`, `198.51.100.0/24`, `203.0.113.0/24`) or `10.0.0.1`; IPv6 uses `2001:db8::/32` (RFC 3849).
- Domains: use `example.com`/`.org`/`.net`, `*.example`, or `corp.example.com` — never a real internal domain or a custom/internal TLD.
- Redact any captured `resolvectl status` / `nmcli … DNS` / `scutil --dns` output (and any GUI screenshot) before committing it or pasting it into a PR: replace every real resolver IP, internal domain, hostname, username, and MAC with a placeholder.

## Quality bar

- No features beyond the current phase scope
- Pure logic gets unit tests; parsing must be testable without live system commands
- A failed apply must never leave the system in a half-configured state
- `log` crate for all output in library/backend code, never `println!`
