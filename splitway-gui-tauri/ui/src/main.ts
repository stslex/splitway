// Frontend entry point. The whole app lives in app.ts; this just mounts it.
//
// The truth contract (docs/design/tauri-mutations.md): the app renders the cached
// view-model (assigned only on the view-model-changed / initial-fetch path) and
// drives mutations daemon-first — a command never edits the displayed state; the
// daemon's resulting truth arrives via the next VM event. The only other state is
// the request-lifecycle store (pending / per-action error / ephemeral CheckDomain
// result / config-editor buffers) in lifecycle.ts.

import { start } from "./app";

const root = document.getElementById("app");
if (root) {
  start(root);
} else {
  // Should never happen (index.html ships the #app element), but fail loudly in
  // text rather than as a blank window.
  document.body.textContent = "Failed to start the Splitway UI: missing #app root element.";
}
