//! The long-running daemon: builds a multi-thread tokio runtime, watches the
//! VPN, serves the IPC socket, and on `SIGTERM`/`SIGINT` reverts active rules
//! before exiting cleanly.

mod ipc;
mod state;

use std::path::{Path, PathBuf};
use std::process::exit;
use std::sync::Arc;

use notify::{RecursiveMode, Watcher};
use tokio::signal::unix::{signal, Signal, SignalKind};
use tokio::sync::{mpsc, oneshot};

use splitway_shared::config::{self, ConfigParseError, LocalConfig};
use splitway_shared::ipc::socket_path;
use splitway_shared::platform::DnsBackend;

use crate::backend::create_dns_backend;
use state::{
    run_state, ConfigStore, DetectorFactory, FileConfigStore, PlatformDetectorFactory,
    StateCommand, StateMachine,
};

/// Entry point for `splitway-daemon run`. `config_path` is the optional
/// `--config <PATH>` override; `None` uses the default config location. The
/// resolved path is the daemon's active config file for its whole lifetime —
/// the file `GetConfig`/`SetConfig` read and write.
///
/// `socket_group` is the optional `--socket-group <NAME>` (set by the systemd
/// unit for the unprivileged-GUI deployment): when present, the control socket
/// and its runtime dir are owned by that group (`0660`/`0750`) so an in-group
/// user can connect without `sudo`. It is a deployment concern, not routing
/// state, so it is a CLI flag rather than a field of the live-watched config.
pub fn run(config_path: Option<PathBuf>, socket_group: Option<String>) {
    let config_path = config_path.unwrap_or_else(config::config_file_path);
    let config = load_or_init_config(&config_path);
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => {
            log::error!("failed to build tokio runtime: {e}");
            exit(1);
        }
    };
    runtime.block_on(run_async(config, config_path, socket_group));
}

async fn run_async(config: LocalConfig, config_path: PathBuf, socket_group: Option<String>) {
    // Captured for the startup log only; the watch lifecycle (including the
    // empty-vpn_name case) is owned by the state machine's `arm_watch`.
    let interface = config.vpn_name.clone();

    let backend: Arc<dyn DnsBackend> = Arc::from(create_dns_backend());

    // Bind the IPC control socket before anything can apply DNS. A bind failure
    // is fatal (the socket is a core feature), and binding first guarantees that
    // fatal `exit(1)` happens before the watch/state pipeline can install any
    // rules — otherwise an already-up VPN could apply split-DNS on a worker
    // thread while this thread is still binding, and a bind failure would then
    // strand those rules with no daemon left to revert them. Serving the bound
    // socket is deferred until `state_tx` exists (below).
    let socket = socket_path();
    let listener = match ipc::bind_socket(&socket, socket_group.as_deref()) {
        Ok(listener) => listener,
        Err(e) => {
            log::error!("failed to bind IPC socket {}: {e}", socket.display());
            exit(1);
        }
    };
    log::info!("listening on IPC socket {}", socket.display());

    // Install the shutdown signal handlers before the pipeline can apply DNS,
    // for the same reason as the bind above: failing to install them is a fatal
    // startup error, and doing it now means that fatal exit cannot strand
    // applied rules. The streams also capture any signal that arrives during
    // startup, so an early shutdown still reverts gracefully.
    let (mut sigint, mut sigterm) = install_shutdown_signals();

    // Single state-owner task. Shutdown is delivered out-of-band via its own
    // channel so the revert preempts any queued commands. The command channel
    // is built *before* the state machine so the machine can hold a clone of
    // `state_tx`: it now owns the VPN-watch lifecycle (arming at startup,
    // re-arming on a config change), and each (re-)armed forwarding task feeds
    // `StateCommand::Vpn` back in through that clone.
    let (state_tx, state_rx) = mpsc::channel::<StateCommand>(64);
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<oneshot::Sender<bool>>();
    let detector_factory: Arc<dyn DetectorFactory> = Arc::new(PlatformDetectorFactory);
    // The config file is the single source of truth; the actor reads and writes
    // it only through this store (no inline `fs` in `StateMachine`).
    let config_store: Arc<dyn ConfigStore> = Arc::new(FileConfigStore::new(config_path.clone()));
    let machine = StateMachine::new(
        backend,
        detector_factory,
        config,
        config_store,
        state_tx.clone(),
        // A group-accessible socket (--socket-group) locks the file-reading
        // OpenVPN config fields against IPC mutation; see `StateMachine`.
        socket_group.is_some(),
    );
    // `run_state` arms the watch before entering its command loop. That happens
    // here — after the socket bind and signal handlers above (so a fatal startup
    // error can never strand applied rules) and before IPC is served below. An
    // empty `vpn_name` or a detector that fails to start leaves auto-apply off
    // while IPC stays up (see `arm_watch`).
    let state_handle = tokio::spawn(run_state(machine, state_rx, shutdown_rx));

    // Watch the config file for external hand-edits so they apply live, keeping
    // the file the single source of truth without requiring a manual reload. The
    // actor's equality check ignores the daemon's own writes. Best-effort: a
    // failure to start the watcher is logged and degrades to manual `ReloadConfig`.
    //
    // Held until `run_async` returns so the watch stays armed for the daemon's
    // lifetime; on shutdown its drop closes the watcher's event channel, which
    // lets the bridging blocking task exit so the runtime can shut down promptly
    // (otherwise that task's blocking `recv` would hang runtime shutdown).
    let _config_watcher = spawn_config_watcher(config_path, state_tx.clone());

    // Serve IPC requests on the socket bound above (before the state pipeline
    // started, so a bind failure could never have stranded any rules).
    tokio::spawn(ipc::serve(listener, state_tx.clone()));

    log::info!("splitway daemon started (interface: {interface})");

    wait_for_shutdown_signal(&mut sigint, &mut sigterm).await;
    log::info!("shutdown signal received; reverting active rules");

    // Ask the state task to revert (this preempts any queued commands), then
    // wait for it to report whether the system was left clean.
    let clean = {
        let (ack_tx, ack_rx) = oneshot::channel();
        if shutdown_tx.send(ack_tx).is_ok() {
            ack_rx.await.unwrap_or(false)
        } else {
            false
        }
    };
    let _ = state_handle.await;

    // Best-effort socket cleanup so the next start is clean.
    if socket.exists() {
        if let Err(e) = std::fs::remove_file(&socket) {
            log::debug!("could not remove socket {}: {e}", socket.display());
        }
    }

    if clean {
        log::info!("splitway daemon stopped");
    } else {
        log::error!(
            "splitway daemon stopped, but reverting DNS rules failed; \
             the system may be left half-configured"
        );
        exit(1);
    }
}

/// Install the `SIGINT`/`SIGTERM` handlers. A failure to install them is a fatal
/// startup error (`exit(1)`), done before the pipeline applies any rules so that
/// fatal exit cannot strand them — and so it does not masquerade as a received
/// signal that would exit "cleanly" without reverting.
fn install_shutdown_signals() -> (Signal, Signal) {
    let sigint = match signal(SignalKind::interrupt()) {
        Ok(stream) => stream,
        Err(e) => {
            log::error!("failed to install SIGINT handler: {e}");
            exit(1);
        }
    };
    let sigterm = match signal(SignalKind::terminate()) {
        Ok(stream) => stream,
        Err(e) => {
            log::error!("failed to install SIGTERM handler: {e}");
            exit(1);
        }
    };
    (sigint, sigterm)
}

/// Wait for either `SIGINT` (Ctrl-C) or `SIGTERM` (systemd stop). Both trigger
/// the graceful revert-then-exit path.
async fn wait_for_shutdown_signal(sigint: &mut Signal, sigterm: &mut Signal) {
    tokio::select! {
        _ = sigint.recv() => log::info!("received SIGINT"),
        _ = sigterm.recv() => log::info!("received SIGTERM"),
    }
}

/// Spawn a best-effort watcher that turns an external hand-edit of the config
/// file into a [`StateCommand::ConfigChanged`] for the state actor — so an edit
/// applies live without an IPC call. It watches the file's *parent directory*
/// (not the inode) so the atomic temp-file-plus-rename write that `save`
/// performs — which replaces the file rather than editing it in place — is still
/// observed. Events are filtered to the config file's name; the actor's equality
/// check then debounces the daemon's own writes and coalesces a burst of events
/// for one save into a single reload.
///
/// A failure to set the watcher up is logged and degrades to manual
/// `ReloadConfig` only; it is never fatal.
///
/// Returns the [`notify::RecommendedWatcher`] so the caller keeps it alive for
/// the daemon's lifetime. Crucially, the bridging blocking task does **not** own
/// the watcher — it holds only the receiving end of the channel. Dropping the
/// returned watcher (on shutdown) drops the callback that holds the sender, which
/// closes the channel, ends the blocking task's `recv` loop, and lets it exit.
/// If the blocking task owned the watcher instead, its `recv` would block a
/// runtime thread forever with no further event, and dropping the tokio runtime
/// would hang waiting for that thread until systemd's stop timeout (a SIGKILL).
/// The directory to watch for a given config path, or `None` when it cannot be
/// determined. A bare relative path like `config.json` has an *empty* parent, but
/// load/save still operate on it in the current directory — so watch `.` there
/// rather than disabling the live watch; only a truly parent-less path (e.g. a
/// bare root) returns `None`.
fn watch_dir(config_path: &Path) -> Option<PathBuf> {
    match config_path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => Some(dir.to_path_buf()),
        Some(_) => Some(PathBuf::from(".")),
        None => None,
    }
}

fn spawn_config_watcher(
    config_path: PathBuf,
    state_tx: mpsc::Sender<StateCommand>,
) -> Option<notify::RecommendedWatcher> {
    let dir = match watch_dir(&config_path) {
        Some(dir) => dir,
        None => {
            log::warn!(
                "config path {} has no parent directory; live config watch disabled",
                config_path.display()
            );
            return None;
        }
    };
    let target = match config_path.file_name() {
        Some(name) => name.to_os_string(),
        None => {
            log::warn!(
                "config path {} has no file name; live config watch disabled",
                config_path.display()
            );
            return None;
        }
    };

    // notify delivers events on its own thread through this std channel; a
    // blocking task bridges matching events onto the actor's async channel.
    let (raw_tx, raw_rx) = std::sync::mpsc::channel();
    let mut watcher = match notify::recommended_watcher(move |res| {
        let _ = raw_tx.send(res);
    }) {
        Ok(watcher) => watcher,
        Err(e) => {
            log::warn!("failed to create config watcher: {e}; live config watch disabled");
            return None;
        }
    };
    if let Err(e) = watcher.watch(&dir, RecursiveMode::NonRecursive) {
        log::warn!(
            "failed to watch config directory {}: {e}; live config watch disabled",
            dir.display()
        );
        return None;
    }
    log::info!("watching {} for external config edits", dir.display());

    // Holds only `raw_rx` (not the watcher): the loop ends when the watcher is
    // dropped (channel closed, on shutdown) or the actor is gone.
    tokio::task::spawn_blocking(move || {
        for event in raw_rx {
            match event {
                Ok(event)
                    if event
                        .paths
                        .iter()
                        .any(|p| p.file_name() == Some(target.as_os_str())) =>
                {
                    // One notification per observed event for the config file; the
                    // actor's equality check makes redundant reloads no-ops.
                    if state_tx.blocking_send(StateCommand::ConfigChanged).is_err() {
                        return; // actor gone; stop watching
                    }
                }
                Ok(_) => {}
                Err(e) => log::warn!("config watcher error: {e}"),
            }
        }
    });

    Some(watcher)
}

fn load_or_init_config(path: &Path) -> LocalConfig {
    match config::load_config_from(path) {
        Ok(config) => config,
        Err(ConfigParseError::ConfigNotFound) => {
            log::warn!(
                "config not found; creating an empty one at {}",
                path.display()
            );
            if let Err(e) = config::create_empty_config_at(path) {
                log::error!("failed to create empty config: {e}");
                exit(1);
            }
            config::load_config_from(path).unwrap_or_else(|e| {
                log::error!("failed to read freshly created config: {e}");
                exit(1);
            })
        }
        Err(e) => {
            log::error!("failed to read config: {e}");
            exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::watch_dir;
    use std::path::{Path, PathBuf};

    #[test]
    fn watch_dir_resolves_parent_or_current_directory() {
        // An absolute path watches its parent directory.
        assert_eq!(
            watch_dir(Path::new("/var/lib/splitway/config.json")),
            Some(PathBuf::from("/var/lib/splitway"))
        );
        // A bare relative path watches the current directory (where load/save
        // operate), rather than disabling the live watch.
        assert_eq!(
            watch_dir(Path::new("config.json")),
            Some(PathBuf::from("."))
        );
        // A relative path with a directory component watches that directory.
        assert_eq!(
            watch_dir(Path::new("sub/config.json")),
            Some(PathBuf::from("sub"))
        );
        // A parent-less path (a bare root) cannot be watched.
        assert_eq!(watch_dir(Path::new("/")), None);
    }
}
