# Security scrub — remove leaked real infrastructure data

Real infrastructure data (a real resolver IP and real internal domain names, from
pasted `resolvectl status`-style verification output) leaked into the repo's
surface. The author has already removed the known **PR comment**. This task
audits and scrubs **everywhere else**: tracked files on every branch, full git
history, and the `docs/prompts/*.md` files.

## CRITICAL meta-rule — do not re-leak while cleaning

Do **not** reproduce any real value you find in a commit message, the PR
description, this file, or any other file. Re-pasting the secret to "document"
the fix repeats the exact mistake. Describe findings by **category + count and
location** only (e.g. "redacted 11 real internal domains and 1 resolver IP in
`X`"), never the values themselves. Find sensitive data by **pattern**, below —
this prompt intentionally contains **no** real values.

## Branch

Branch `security-scrub` from up-to-date `dev`. Also inspect every other local
branch and the open Phase 5 PR branch (the leak originated there). PR into `dev`.
English only.

## What counts as sensitive (identify by pattern, not a hardcoded list)

- **Real resolver / DNS IPs.** Concrete IPv4 that is *not* a documentation
  placeholder. Placeholders are the RFC 5737 ranges (`192.0.2.0/24`,
  `198.51.100.0/24`, `203.0.113.0/24`) and obvious stand-ins like `10.0.0.1`. A
  specific private address tied to a real network (a real `10.x` / `192.168.x`
  resolver) is suspect.
- **Real domains.** Anything that is *not* an obvious placeholder
  (`example.com` / `.org` / `.net`, `*.example`, `*.test`, `*.invalid`,
  `localhost`). Real-looking internal domains — custom or internal TLDs, real
  organization or product names — are suspect.
- **Command-output blocks with real values.** Signatures like `Current DNS
  Server:`, `DNS Servers:`, `DNS Domain:`, `Link N (...)`, or pasted `resolvectl
  status` / `nmcli … DNS` / `scutil --dns` dumps.
- **Other identifying data.** Real hostnames, usernames, home paths
  (`/home/<user>`), MAC addresses, or interface names tied to a specific machine
  (beyond generic `tun0` / `eth0` / `wlan0` / `utun4`).

## Sweep scope

1. **Working tree, every branch.** `git grep` the patterns across all local
   branches and the open Phase 5 branch — source, docs, `README`, tests and
   fixtures, CI config, and `docs/prompts/*.md`.
2. **Full git history.** Search both file content and commit messages across all
   history (`git log -p`, `git grep` over refs) — a value may have been committed
   and "removed" in a later commit yet still live in history.
3. **PR/issue comments (optional, report-only).** The known PR comment is already
   removed; double-check others via `gh` and report, do not assume.

## Remediation

- **Tracked files (working tree).** Replace each real value with the placeholder
  convention (`example.com`, `10.0.0.1`, generic interface names); commit on the
  branch where it lives. Commit messages describe category + count, never the
  value.
- **Git history.** If a sensitive value exists in *historical* commits (not only
  the current tree), a new commit does **not** remove it. **Report** each
  occurrence (commit ref + file + category, no value) and provide an exact
  `git filter-repo` plan to purge it. **Do not auto-force-push** — history rewrite
  is destructive, affects the open PR branch(es), and cached SHAs persist on
  GitHub; leave the force-push for the author to run after review, and note that
  GitHub Support can purge cached SHAs afterward.
- **Instructions that solicit real output (root-cause fix).** Wherever a prompt or
  doc tells the implementer to paste `resolvectl status` / `nmcli` / `scutil`
  output, real screenshots, or any live values into a PR / commit / docs, change
  it to require **redacted / synthetic** values only (`example.com`, `10.0.0.1`).
  Apply this to every `docs/prompts/*.md` and any README/verification guidance.

## Optional (prevention, only if cheap)

A lightweight CI guard that fails when a non-placeholder domain or resolver IP
appears in tracked files (a `gitleaks`-style or a targeted `grep` step). Note it
will need an allowlist for the placeholder ranges; skip if it produces noise.

## Done criteria

- `git grep` across all branches for the patterns returns only placeholders.
- A **redacted report** lists: occurrences found, working-tree fixes done, and
  history occurrences with the exact rewrite plan (gated on the author's
  confirmation).
- Every prompt/doc instruction that solicited real output now requires synthetic
  values.
- This prompt, all commit messages, and the PR description contain **zero** real
  values.
- fmt / clippy / tests green (docs/text only, but run them).

## Finish

PR into `dev` titled
`security: scrub leaked infrastructure data; require synthetic values in verification`.
Description = the redacted report + the history-rewrite plan (commands, gated on
author confirmation). No real values anywhere in the PR.
