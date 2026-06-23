// Compile-time contract guard — the TypeScript half of the bindings type-drift
// guard (the Rust half is the serialize-vs-fixture test in src/bridge.rs and the
// gui-core serde-shape tests). Type-checked by `tsc --noEmit` (it is in the
// tsconfig `include`), emits no runtime code, and is never imported by the entry
// point, so esbuild does not bundle it. See docs/design/tauri-read-only.md.
//
// (A Vitest render smoke test was the original plan; jsdom's dependency tree
// could not be fetched through this environment's npm proxy, so the runtime
// render check is deferred — the controls are exercised for real in the live
// e2e, and the contract is locked by this + the Rust-side guards.)

import type { CheckOutcome, ViewModel } from "./bindings/view-model";
import sample from "./bindings/view-model.sample.json";
import { renderApp, type Actions } from "./app";
import { newLifecycle } from "./lifecycle";

// 1. The committed fixture must have every top-level key the mirror declares.
//    `resolveJsonModule` widens the JSON's enum strings to `string`, so a direct
//    `const x: ViewModel = sample` would falsely fail on the literal-union fields;
//    this top-level key check is widening-safe. The reverse direction (Rust emits
//    a field the mirror lacks) and deep/enum shape are locked by the Rust
//    serialize-vs-fixture guard + the gui-core serde-shape tests.
type HasAllKeys<T> = { [K in keyof T]: unknown };
export const _fixtureHasAllViewModelKeys: HasAllKeys<ViewModel> = sample;

// 2. renderApp must accept a ViewModel (signature compatibility with the mirror).
//    Never executed — purely a type assertion.
export function _renderTypechecks(vm: ViewModel, actions: Actions): Node[] {
  return renderApp(vm, "macos", newLifecycle(), actions);
}

// 3. CheckOutcome must stay an exhaustively-handled discriminated union — locks
//    the TS mirror's `state` discriminant against the Rust `CheckOutcome` shape
//    (the gui-core `check_outcome_serializes_internally_tagged_on_state` test
//    locks the Rust → JSON direction). A new variant would break the `never`.
export function _checkOutcomeIsExhaustive(outcome: CheckOutcome): string {
  switch (outcome.state) {
    case "Checked":
      return outcome.result.host;
    case "Error":
      return outcome.message;
    default: {
      const unreachable: never = outcome;
      return unreachable;
    }
  }
}
