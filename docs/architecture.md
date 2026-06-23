# Architecture — cross-cutting invariants

This document records the **invariants** every phase and implementation must
honor. Unlike `ROADMAP.md` (which sequences *what* gets built) these are the
contracts that hold *across* phases — change one only with a deliberate decision,
not as a side effect of a feature. Per-feature decisions live in
[`docs/design/`](design/README.md); the plan lives in [`ROADMAP.md`](../ROADMAP.md).

## 1. Config is the single source of truth, read fresh

The config file is authoritative. The daemon **reads it fresh on every
operation** — there is **no in-memory config cache**. Reconciliation is
event-driven (a VPN event, an IPC request, a config-file change), never a hot
polling loop.

The only state held in memory is runtime state the file *cannot* hold:

- the **applied snapshot** — what the daemon last pushed to the system (interface
  + domains + DNS servers), and
- the **armed-watch parameters** — which interface/backend the live watch is
  currently armed on.

Writes are **atomic** (write a temp file, then rename) and **read-modify-write**
on every mutation, so a concurrent external edit is never silently clobbered and
a crash never leaves a half-written file. A **file watcher** (inotify / FSEvents,
or the `notify` crate) makes external hand-edits take effect live: watch the
*directory* (not the inode) to survive atomic-rename-replace, and debounce the
daemon's own writes so a self-write does not loop. A **malformed config freezes**
the current state — the daemon keeps the last-applied rules, surfaces the file as
invalid, and recovers automatically once it parses again; it never reverts to a
blank or default state on a parse error. **Hand-edit the config atomically**
(write a temp file and rename over it, as the daemon does): an in-place
truncate-then-write can be observed mid-write and briefly read as invalid until
the completing write fires another event. Because the file is the single source
of truth, a malformed file also blocks every IPC mutation (a write derived from a
config the daemon could not read would clobber the fields it preserves from
disk), so a corrupt config is repaired *on disk*, not through the GUI/CLI.

Config access sits behind a **testable abstraction** — no inline `fs::read` /
`fs::write` inside the `StateMachine` — so reconciliation logic is unit-testable
without touching the filesystem.

On **NixOS** the writable config lives in `/var/lib/splitway/` (provisioned via
the service's `StateDirectory`), **not** a module-generated read-only `/etc`
file. The model is **imperative**: the daemon owns the writable file and the GUI
mutates it at runtime. Module options may *seed* an initial config but must not
*lock* it read-only, because a read-only config breaks runtime mutation.

## 2. GUI mutation truth contract

Every client (today egui, later Tauri, and the CLI) renders the daemon's
*reported* state — it never invents state of its own.

- **No optimistic UI.** A mutation is shown as *pending* until a refetch confirms
  it; the UI renders from the daemon's reported state, not from what the user
  just typed.
- **Two distinct meanings of "applied", shown separately.** *Saved to config* and
  *routing in effect* are different facts: saving a domain never implies it is
  routing. The UI must not collapse them into one "done" indicator.
- **Two error types, kept distinct.** The daemon returns two failures —
  *persist-failed* (the write did not land) and *saved-but-apply-failed* (written
  to config, but the system rules could not be applied). The UI shows them
  differently because they call for different user action.
- **"Not applied" is graded.** *Waiting* (VPN down, feature disabled, no DNS from
  the VPN) is a **neutral** state; *failed* is an **error**. Never alarm on
  waiting, never reassure on failed.
- **Per-domain truth.** Per-domain status comes from comparing *configured*
  domains against the *applied* snapshot — not from assuming a save took effect.

## 3. DNS vs IP-routing boundary

Splitway governs **DNS** — which resolver answers a given name — **not IP
routing** — whether the traffic to the resolved address actually traverses the
tunnel. This boundary bounds the domain route-check (coverage + live resolution
are in scope; reachability is not) and sets user expectations: a domain can be
correctly DNS-routed through the VPN and still be unreachable for reasons
Splitway does not, and should not, manage.

## 4. One package, one version

The daemon, CLI (and later the GUI) ship as **one package at a single version**.
There is therefore no GUI↔daemon version-compatibility matrix to reason about.
The protocol's strict-equality **version-peek** (`VERSION_MISMATCH_PREFIX`)
covers only the brief upgrade window where a new binary meets a still-running old
daemon; `postinst` restarts the service to close it. On NixOS the module is the
single-version unit. Revisiting strict-equality (toward additive / negotiated
compatibility) is deferred to Phase 8.

## 5. Watch-as-unit + multi-VPN migration-awareness

The VPN-watch lifecycle (arm / re-arm / disarm on an interface) is modeled as a
**self-contained unit**. This is deliberate: the stated north-star is multiple
*simultaneous* VPN routings (see `ROADMAP.md` → Later), where the watch becomes a
**collection** of N independent units. v1 stays single-VPN, but the single→plural
migration is de-risked up front — read-fresh config plus atomic writes make the
config-shape migration (single object → list) safe, and modeling the watch as a
unit now means N watches later is a collection rather than a rewrite. v1 paints
no corner it would have to tear out.
