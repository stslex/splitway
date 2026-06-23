# Design docs

Per-feature design decisions live here. Each file captures the **agreed shape** of
one feature — the decision a contributor would otherwise have to reverse-engineer
from the diff.

## When to write one

Design docs are **lightweight and written only where a feature carries a real
decision** — not mechanically, one per phase. Most changes need none: the plan in
[`../../ROADMAP.md`](../../ROADMAP.md), the cross-cutting invariants in
[`../architecture.md`](../architecture.md), and git history already cover them.
Add a doc here when a feature involves a genuine choice with tradeoffs or
rejected alternatives that future work should not have to relitigate.

## When it lands

A design doc lands in the **same PR as the feature it describes**, so it reflects
the *final agreed shape* rather than a pre-implementation guess. (This is the
distinction from the retired committed prompts: those were pre-implementation
scaffolding that went stale; these are post-decision records.)

## What it captures

- **The agreement** — the decision that was made.
- **Scope / out-of-scope** — what the feature does and explicitly does not do.
- **Notable tradeoffs / rejected alternatives** — and why.
- **Links** to the relevant [`architecture.md`](../architecture.md) invariants the
  feature must honor.

Keep it short. A design doc is a record of a decision, not a specification.
