//! The long-running daemon: builds a multi-thread tokio runtime, watches the
//! VPN, serves the IPC socket, and on `SIGTERM`/`SIGINT` reverts active rules
//! before exiting cleanly.

mod ipc;
mod state;

use std::process::exit;
use std::sync::Arc;

use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::{mpsc, oneshot};

use splitway_shared::config::{self, ConfigParseError, LocalConfig};
use splitway_shared::ipc::socket_path;
use splitway_shared::platform::DnsBackend;

use crate::backend::create_dns_backend;
use crate::detector::create_vpn_detector;
use state::{run_state, StateCommand, StateMachine};

/// Entry point for `splitway-daemon run`.
pub fn run() {
    let config = load_or_init_config();
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
    runtime.block_on(run_async(config));
}

async fn run_async(config: LocalConfig) {
    let interface = config.vpn_name.clone();
    if interface.is_empty() {
        log::warn!(
            "vpn_name is empty in config; set it and restart the daemon to enable auto-apply \
             (config reload re-reads domains/enabled but does not restart the interface watch)"
        );
    }

    let backend: Arc<dyn DnsBackend> = Arc::from(create_dns_backend());
    let detector = create_vpn_detector(&config);

    // Single state-owner task. Shutdown is delivered out-of-band via its own
    // channel so the revert preempts any queued commands.
    let (state_tx, state_rx) = mpsc::channel::<StateCommand>(64);
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<oneshot::Sender<bool>>();
    let machine = StateMachine::new(backend, config, config::config_file_path());
    let state_handle = tokio::spawn(run_state(machine, state_rx, shutdown_rx));

    // VPN event stream -> state task.
    match detector.watch(&interface) {
        Ok(mut events) => {
            let tx = state_tx.clone();
            tokio::spawn(async move {
                while let Some(event) = events.recv().await {
                    if tx.send(StateCommand::Vpn(event)).await.is_err() {
                        break;
                    }
                }
                log::warn!("VPN event stream ended");
            });
        }
        Err(e) => log::error!(
            "failed to start VPN watch for {interface}: {e}; \
             IPC is still available, auto-apply is not"
        ),
    }

    // IPC server. A bind failure is fatal: the socket is a core feature.
    let socket = socket_path();
    let listener = match ipc::bind_socket(&socket) {
        Ok(listener) => listener,
        Err(e) => {
            log::error!("failed to bind IPC socket {}: {e}", socket.display());
            exit(1);
        }
    };
    log::info!("listening on IPC socket {}", socket.display());
    tokio::spawn(ipc::serve(listener, state_tx.clone()));

    log::info!("splitway daemon started (interface: {interface})");

    wait_for_shutdown_signal().await;
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

/// Wait for either `SIGINT` (Ctrl-C) or `SIGTERM` (systemd stop). Both
/// trigger the graceful revert-then-exit path.
async fn wait_for_shutdown_signal() {
    // A failure to install the handlers is a fatal startup error, not a
    // shutdown trigger — exit(1) (like the other fatal startup paths) so it
    // does not masquerade as a received signal and exit cleanly.
    let mut sigint = match signal(SignalKind::interrupt()) {
        Ok(stream) => stream,
        Err(e) => {
            log::error!("failed to install SIGINT handler: {e}");
            exit(1);
        }
    };
    let mut sigterm = match signal(SignalKind::terminate()) {
        Ok(stream) => stream,
        Err(e) => {
            log::error!("failed to install SIGTERM handler: {e}");
            exit(1);
        }
    };
    tokio::select! {
        _ = sigint.recv() => log::info!("received SIGINT"),
        _ = sigterm.recv() => log::info!("received SIGTERM"),
    }
}

fn load_or_init_config() -> LocalConfig {
    match config::get_config() {
        Ok(config) => config,
        Err(ConfigParseError::ConfigNotFound) => {
            log::warn!(
                "config not found; creating an empty one at {}",
                config::config_file_path().display()
            );
            if let Err(e) = config::create_empty_config() {
                log::error!("failed to create empty config: {e}");
                exit(1);
            }
            config::get_config().unwrap_or_else(|e| {
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
