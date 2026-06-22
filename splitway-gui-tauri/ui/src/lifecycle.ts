// The request-lifecycle store: the ONLY state the frontend holds besides the
// cached view-model. It is deliberately, visibly separate from daemon truth.
//
// THE TRUTH CONTRACT (docs/design/tauri-mutations.md, architecture.md §2):
//   - Domain / interface / status truth is rendered SOLELY from
//     `view-model-changed` events (the cached `lastVm` in app.ts). It is never
//     composed, derived, or predicted here.
//   - This store holds only *request-lifecycle* facts that describe the in-flight
//     interaction, not the daemon's resulting state: which action is pending, its
//     last per-action error, the add-domain input (+ whether the inline add row is
//     open), the one-shot CheckDomain result, and the ephemeral undo snackbar.
//     None of these is a prediction of the daemon's truth — in particular the undo
//     snackbar is UX over a *completed* daemon delete; the displayed domain list
//     still comes from the VM after the delete (and after any undo re-add).
//
// Keeping it in one small, clearly-named module is what makes the contract
// greppable: search for writes to `lastVm` (only the VM-event path in app.ts) vs
// writes to `Lifecycle` (only user actions / command resolutions).

import type { CheckOutcome } from "./bindings/view-model";

/** A stable key per mutating control, for pending + per-action error tracking.
 *  `iface` is the interface-selector write (a `set_config` that round-trips the
 *  current backend/openvpn unchanged); `reload` is the apply-failed resync. */
export type ActionKey = "toggle" | "add" | "iface" | "reload" | `remove:${string}`;

/** The one-shot CheckDomain query state — ephemeral, never folded into the VM. */
export type CheckState = "idle" | "pending" | CheckOutcome;

/** The ephemeral undo snackbar over a *completed* domain delete. `domain` is what
 *  was removed; Undo re-adds it (another daemon write). The auto-commit timer
 *  handle lives in the controller closure, not here. */
export interface UndoState {
  domain: string;
}

export interface Lifecycle {
  /** Actions with an in-flight command (drives the disabled + "…" indicators). */
  pending: Set<ActionKey>;
  /** The last error per action (cleared when the action is retried / succeeds). */
  errors: Map<ActionKey, string>;
  /** Whether the inline add-domain row is revealed (the eyebrow "+ Add" toggles it). */
  addOpen: boolean;
  /** The add-domain text field (input hygiene only; the daemon validates). */
  addInput: string;
  /** The check-domain text field. */
  checkInput: string;
  /** The ephemeral CheckDomain result/area. */
  check: CheckState;
  /** The ephemeral undo snackbar, or null when hidden. */
  undo: UndoState | null;
}

export function newLifecycle(): Lifecycle {
  return {
    pending: new Set(),
    errors: new Map(),
    addOpen: false,
    addInput: "",
    checkInput: "",
    check: "idle",
    undo: null,
  };
}

export function isPending(lc: Lifecycle, key: ActionKey): boolean {
  return lc.pending.has(key);
}

export function errorFor(lc: Lifecycle, key: ActionKey): string | undefined {
  return lc.errors.get(key);
}

/** Mark an action in-flight and clear any stale error for it. */
export function beginAction(lc: Lifecycle, key: ActionKey): void {
  lc.pending.add(key);
  lc.errors.delete(key);
}

/** Resolve an action: clear pending, and set/clear its per-action error. */
export function endAction(lc: Lifecycle, key: ActionKey, error: string | null): void {
  lc.pending.delete(key);
  if (error === null) lc.errors.delete(key);
  else lc.errors.set(key, error);
}
