//! Binary entry point for the Splitway Tauri shell.
//!
//! Its only job before handing off to [`splitway_gui_tauri::run`] is the
//! webkit2gtk Wayland workaround: on Linux, webkit2gtk 4.1's DMA-BUF renderer can
//! fail to initialise under Wayland compositors and then **silently render a
//! blank window** (Tauri's own Linux-graphics guide documents this). Disabling it
//! forces the reliable rendering path. It must be set before any GTK/WebKit init
//! — i.e. before the Tauri builder runs — so it lives here, ahead of `run()`.
//!
//! We respect an already-set value so a user can override (e.g. to re-enable the
//! faster path on hardware that works, or to add the last-resort
//! `WEBKIT_DISABLE_COMPOSITING_MODE`). No NVIDIA-specific vars are set: the target
//! environment (niri, non-NVIDIA) needs only the DMA-BUF disable — see
//! `docs/design/tauri-read-only.md`.

fn main() {
    #[cfg(target_os = "linux")]
    {
        // Safe on the 2021 edition (set_var is unsafe only from edition 2024).
        if std::env::var_os("WEBKIT_DISABLE_DMABUF_RENDERER").is_none() {
            std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1");
        }
    }

    splitway_gui_tauri::run();
}
