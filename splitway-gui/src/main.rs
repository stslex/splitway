//! `splitway-gui`: a primitive desktop GUI for the splitway daemon. Like
//! `splitway-cli`, it is a pure client of the daemon's IPC socket — it holds no
//! privileges, duplicates no daemon logic, and writes no config file itself.
//!
//! Unix-only: the IPC client uses a Unix domain socket, and the egui stack
//! targets Linux/macOS. On non-Unix the crate still builds via the stub `main`
//! below (and its egui/rfd deps are gated to `cfg(unix)` in `Cargo.toml`), so
//! the cross-platform release matrix stays green; the GUI is never built for
//! Windows.

#[cfg(unix)]
mod app;
#[cfg(unix)]
mod worker;

#[cfg(unix)]
fn main() -> eframe::Result<()> {
    env_logger::init();
    app::run()
}

#[cfg(not(unix))]
fn main() {
    eprintln!("splitway-gui is only supported on Unix platforms (Linux/macOS)");
    std::process::exit(1);
}
