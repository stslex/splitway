// Ambient types for the `window.__TAURI__` global that Tauri injects when
// `app.withGlobalTauri` is true (tauri.conf.json). We use the global rather than
// the @tauri-apps/api npm package so the frontend has no npm runtime dependency
// (see docs/design/tauri-read-only.md). Only the two members the read-only shell
// uses are declared: `core.invoke` (the get_view_model command) and
// `event.listen` (the view-model-changed event).

export {};

declare global {
  interface Window {
    __TAURI__: {
      core: {
        invoke<T>(cmd: string, args?: Record<string, unknown>): Promise<T>;
      };
      event: {
        listen<T>(
          event: string,
          handler: (event: { payload: T }) => void,
        ): Promise<() => void>;
      };
    };
  }
}
