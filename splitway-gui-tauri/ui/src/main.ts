// Frontend entry point. Wires the read-only contract:
//   1. subscribe to view-model-changed (full VM per event),
//   2. fetch the current VM once for first paint,
//   3. render whichever VM is newest.
//
// The frontend holds NO authoritative state. `lastVm` is a cache of the most
// recent push — it is only ever assigned on the VM-event path (or the initial
// fetch) and only read by render(). The frontend never composes, derives, or
// mutates state. See docs/design/tauri-read-only.md.
//
// invoke/listen come from `window.__TAURI__` (tauri.conf.json `withGlobalTauri`),
// not the @tauri-apps/api npm package — see docs/design/tauri-read-only.md for
// why (the build deliberately has no npm runtime dependency). The global's types
// are declared in tauri-global.d.ts.

import type { ViewModel } from "./bindings/view-model";
import { render } from "./render";

const tauri = window.__TAURI__;

let lastVm: ViewModel | null = null;

function apply(vm: ViewModel): void {
  lastVm = vm;
  const root = document.getElementById("app");
  if (root) render(vm, root);
}

async function main(): Promise<void> {
  // Subscribe BEFORE the initial fetch so no push is missed in the gap between
  // mount and the listener being ready. Each event carries the whole VM
  // (last-wins), so an event landing during the fetch is benign.
  await tauri.event.listen<ViewModel>("view-model-changed", (event) => apply(event.payload));

  const initial = await tauri.core.invoke<ViewModel>("get_view_model");
  // If an event already arrived during the await, it is at least as fresh as this
  // initial fetch — don't clobber it.
  if (lastVm === null) apply(initial);
}

main().catch((err) => {
  // A hard bootstrap failure should surface as text, never a blank window.
  const root = document.getElementById("app");
  if (root) root.textContent = `Failed to start the Splitway UI: ${String(err)}`;
});
