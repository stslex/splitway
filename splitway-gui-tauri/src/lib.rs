//! `splitway-gui-tauri`: the Tauri 2.x desktop shell for Splitway.
//!
//! Like the egui harness, this is a **thin, zero-privilege client** of the
//! daemon's control socket — it holds no truth-contract state of its own. All of
//! that lives in [`splitway_gui_core::GuiCore`], which this crate drives exactly
//! as the egui harness does: it sends the requests the core hands it, feeds each
//! reply back, and reads the core's [`ViewModelSnapshot`]. The only difference is
//! the *boundary*: instead of an in-process egui render loop, the view-model
//! crosses a serialization boundary to a web frontend.
//!
//! The read path (Phase 7b):
//!
//! 1. A dedicated **poll thread** ([`bridge::poll_loop`]) owns the `GuiCore` and
//!    the blocking IPC client. Each cycle it drives one whole poll round to
//!    completion (so `Status`/belief and `Verify`/reality in a snapshot are
//!    same-cycle), then builds [`GuiCore::snapshot`].
//! 2. It publishes that snapshot into shared state (so the [`bridge::get_view_model`]
//!    command can return the current one on mount) and, **only when the snapshot
//!    changed**, emits a `view-model-changed` event carrying the *whole* VM.
//! 3. The frontend renders whichever VM arrives last. It holds no authoritative
//!    state — see [`docs/design/tauri-read-only.md`](../../docs/design/tauri-read-only.md).
//!
//! Why the refresh loop lives in Rust and pushes full snapshots: the daemon
//! protocol is strictly request/response (no server push), so *someone* must
//! poll; doing it on the Rust side with full-VM events keeps the frontend a pure
//! renderer and makes update races benign (last-wins). See `bridge` for the
//! testable pieces.
//!
//! The write path (Phase 7c) keeps the same shape: mutations are command →
//! daemon → re-poll → event, never command → screen. A mutation command
//! (`set_enabled` / `add_domain` / `remove_domain` / `set_config` / `reload`)
//! round-trips the daemon on the blocking pool and returns a per-action `Result`,
//! then fires the [`bridge::RefreshSignal`] **refresh-now** wake so the poll
//! thread re-polls immediately — the poll thread stays the *sole* producer of
//! view-models, so no mutation can write displayed state (the truth contract).
//! `check_domain` is a one-shot route-check returning its own result; it is never
//! folded into the VM. See [`docs/design/tauri-mutations.md`](../../docs/design/tauri-mutations.md).
//!
//! On macOS the shell also owns the **privileged service bootstrap**
//! (`install_service` / `disable_service`): it escalates via `osascript ... with
//! administrator privileges` to run the bundled `bootstrap.sh` as root — one
//! native password prompt, no terminal. These keep the write-path shape exactly
//! (do the work → fire refresh-now → never touch the VM); the real health then
//! flows back through `view-model-changed`. `host_platform` lets the frontend
//! branch the macOS-vs-Linux remediation copy. See
//! [`docs/design/macos-self-install.md`](../../docs/design/macos-self-install.md).

pub mod bridge;

/// Build and run the Tauri application. Spawns the poll thread in `setup` and
/// blocks until the window closes. The webkit2gtk Wayland workaround is set by
/// the binary (`main.rs`) before this runs, because it must precede GTK init.
///
/// Excluded from the unit-test build (`cfg(not(test))`): it is the only user of
/// `tauri::generate_context!`, which embeds the pre-built `ui/dist`. Gating it
/// lets `cargo test --lib` exercise the bridge (and regenerate the bindings
/// fixture) without first building the frontend — breaking the otherwise-circular
/// "fixture needed to typecheck the frontend, frontend needed to build the test".
#[cfg(not(test))]
pub fn run() {
    use tauri::Manager;

    // env_logger reads RUST_LOG; default to a quiet-but-useful level if unset is
    // left to the caller's environment, mirroring the daemon/CLI/egui binaries.
    env_logger::init();

    // The refresh-now channel: mutation commands hold the sender (via the managed
    // `RefreshSignal`); the poll thread holds the receiver and waits on it between
    // cycles, so a mutation collapses the action→truth latency to ~one poll cycle.
    let (refresh_tx, refresh_rx) = std::sync::mpsc::channel::<()>();

    tauri::Builder::default()
        .manage(bridge::SharedVm::default())
        .manage(bridge::RefreshSignal::new(refresh_tx))
        .invoke_handler(tauri::generate_handler![
            bridge::get_view_model,
            bridge::set_enabled,
            bridge::add_domain,
            bridge::remove_domain,
            bridge::set_config,
            bridge::reload,
            bridge::check_domain,
            // macOS self-install: escalate via osascript to install/disable the
            // root LaunchDaemon; host_platform lets the frontend branch the
            // remediation copy. Custom commands are not ACL-gated, so the
            // capability file is unchanged.
            bridge::install_service,
            bridge::disable_service,
            bridge::host_platform,
        ])
        .setup(move |app| {
            // Clone the Arc to the shared VM and an AppHandle into the poll
            // thread. AppHandle::clone is cheap (Arc-based); the thread outlives
            // setup and pushes events for the app's lifetime. The refresh receiver
            // moves in too (the matching sender lives in the managed RefreshSignal).
            let shared = app.state::<bridge::SharedVm>().inner().clone();
            let handle = app.handle().clone();
            std::thread::Builder::new()
                .name("splitway-gui-poll".to_string())
                .spawn(move || bridge::poll_loop(handle, shared, refresh_rx))
                .expect("failed to spawn the splitway-gui poll thread");
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running the splitway tauri application");
}
