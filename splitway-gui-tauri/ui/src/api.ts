// Typed wrappers over the Tauri command/event surface. The only place the
// frontend talks to the Rust backend. invoke/listen come from `window.__TAURI__`
// (withGlobalTauri) — see docs/design/tauri-read-only.md and tauri-global.d.ts.
//
// Read: `getViewModel` (once, on mount) + `listenViewModel` (every later update).
// Write: the mutation commands resolve on success and **reject with the daemon's
// error string** on failure (Rust `Result<(), String>` → JS resolve/reject), so
// callers `try/catch` to drive their per-action lifecycle state. `checkDomain` is
// the one-shot route-check; it always resolves with a `CheckOutcome` (its own
// `Error` variant carries a failed query — never a VM event).

import type { CheckOutcome, ViewModel, VpnBackend } from "./bindings/view-model";

const tauri = window.__TAURI__;

export function getViewModel(): Promise<ViewModel> {
  return tauri.core.invoke<ViewModel>("get_view_model");
}

export function listenViewModel(handler: (vm: ViewModel) => void): Promise<() => void> {
  return tauri.event.listen<ViewModel>("view-model-changed", (event) => handler(event.payload));
}

export function setEnabled(enabled: boolean): Promise<void> {
  return tauri.core.invoke<void>("set_enabled", { enabled });
}

export function addDomain(domain: string): Promise<void> {
  return tauri.core.invoke<void>("add_domain", { domain });
}

export function removeDomain(domain: string): Promise<void> {
  return tauri.core.invoke<void>("remove_domain", { domain });
}

// Mirrors the Rust `ConfigInput` command argument (the editable projection; the
// daemon owns and ignores `config_path`).
export interface ConfigInput {
  vpn_name: string;
  vpn_backend: VpnBackend;
  openvpn_management: string;
  openvpn_management_password_file: string | null;
}

export function setConfig(view: ConfigInput): Promise<void> {
  return tauri.core.invoke<void>("set_config", { view });
}

export function reload(): Promise<void> {
  return tauri.core.invoke<void>("reload");
}

export function checkDomain(domain: string): Promise<CheckOutcome> {
  return tauri.core.invoke<CheckOutcome>("check_domain", { domain });
}
