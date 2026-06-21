// Compile-time contract guard — the TypeScript half of the bindings type-drift
// guard (the Rust half is the serialize-vs-fixture test in src/bridge.rs). This
// file is type-checked by `tsc --noEmit` (it is in the tsconfig `include`), emits
// no runtime code, and is never imported by the entry point, so Vite does not
// bundle it. See docs/design/tauri-read-only.md.
//
// (A Vitest render smoke test was the original plan; jsdom's dependency tree
// could not be fetched through this environment's npm proxy, so the runtime
// render check is deferred — render() is exercised for real in the live e2e, and
// the contract is locked by this + the Rust-side guards.)

import type { ViewModel } from "./bindings/view-model";
import sample from "./bindings/view-model.sample.json";
import { render } from "./render";

// 1. The committed fixture must have every top-level key the mirror declares.
//    `resolveJsonModule` widens the JSON's enum strings to `string`, so a direct
//    `const x: ViewModel = sample` would falsely fail on the literal-union fields;
//    this top-level key check is widening-safe. The reverse direction (Rust emits
//    a field the mirror lacks) and deep/enum shape are locked by the Rust
//    serialize-vs-fixture guard + the gui-core serde-shape test.
type HasAllKeys<T> = { [K in keyof T]: unknown };
export const _fixtureHasAllViewModelKeys: HasAllKeys<ViewModel> = sample;

// 2. render() must accept a ViewModel (signature compatibility with the mirror).
//    Never executed — purely a type assertion.
export function _renderTypechecks(vm: ViewModel, root: HTMLElement): void {
  render(vm, root);
}
