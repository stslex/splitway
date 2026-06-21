// The request-lifecycle store: the ONLY state the frontend holds besides the
// cached view-model. It is deliberately, visibly separate from daemon truth.
//
// THE TRUTH CONTRACT (docs/design/tauri-mutations.md, architecture.md §2):
//   - Domain / config / status truth is rendered SOLELY from `view-model-changed`
//     events (the cached `lastVm` in app.ts). It is never composed, derived, or
//     predicted here.
//   - This store holds only *request-lifecycle* facts that describe the in-flight
//     interaction, not the daemon's resulting state: which action is pending, its
//     last per-action error, the one-shot CheckDomain result, and the config
//     editor's input buffers. None of these is a prediction of the daemon's truth.
//
// Keeping it in one small, clearly-named module is what makes the contract
// greppable: search for writes to `lastVm` (only the VM-event path) vs writes to
// `Lifecycle` (only user actions / command resolutions).

import type { CheckOutcome, VpnBackend } from "./bindings/view-model";

/** A stable key per mutating control, for pending + per-action error tracking. */
export type ActionKey = "toggle" | "add" | "config" | "reload" | `remove:${string}`;

/** The config editor's input buffers. Pre-filled from the VM while *clean*; once
 *  the user edits, `dirty` latches so a VM refresh can no longer clobber the
 *  in-progress edit (the same guard gui-core's egui editor uses). Cleared back to
 *  clean after a successful save, so the daemon-normalized values are re-adopted. */
export interface ConfigForm {
  vpn_name: string;
  vpn_backend: VpnBackend;
  openvpn_management: string;
  openvpn_management_password_file: string;
  dirty: boolean;
}

/** The one-shot CheckDomain query state — ephemeral, never folded into the VM. */
export type CheckState = "idle" | "pending" | CheckOutcome;

export interface Lifecycle {
  /** Actions with an in-flight command (drives the disabled + "…" indicators). */
  pending: Set<ActionKey>;
  /** The last error per action (cleared when the action is retried / succeeds). */
  errors: Map<ActionKey, string>;
  /** The add-domain text field (input hygiene only; the daemon validates). */
  addInput: string;
  /** The check-domain text field. */
  checkInput: string;
  /** The ephemeral CheckDomain result/area. */
  check: CheckState;
  /** The config editor buffers + dirty flag. */
  config: ConfigForm;
}

export function newLifecycle(): Lifecycle {
  return {
    pending: new Set(),
    errors: new Map(),
    addInput: "",
    checkInput: "",
    check: "idle",
    config: {
      vpn_name: "",
      vpn_backend: "network-manager",
      openvpn_management: "",
      openvpn_management_password_file: "",
      dirty: false,
    },
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
