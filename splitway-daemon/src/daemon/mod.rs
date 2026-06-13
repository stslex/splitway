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
            "vpn_name is empty in config; the daemon will idle until it is set and reloaded"
        );
    }

    let backend: Arc<dyn DnsBackend> = Arc::from(create_dns_backend());
    let detector = create_vpn_detector();

    // Single state-owner task.
    let (state_tx, state_rx) = mpsc::channel::<StateCommand>(64);
    let machine = StateMachine::new(backend, config, config::config_file_path());
    let state_handle = tokio::spawn(run_state(machine, state_rx));

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

    // Ask the state task to revert, then wait for it to finish.
    let (ack_tx, ack_rx) = oneshot::channel();
    if state_tx.send(StateCommand::Shutdown(ack_tx)).await.is_ok() {
        let _ = ack_rx.await;
    }
    let _ = state_handle.await;

    // Best-effort socket cleanup so the next start is clean.
    if socket.exists() {
        if let Err(e) = std::fs::remove_file(&socket) {
            log::debug!("could not remove socket {}: {e}", socket.display());
        }
    }
    log::info!("splitway daemon stopped");
}

/// Wait for either `SIGINT` (Ctrl-C) or `SIGTERM` (systemd stop). Both
/// trigger the graceful revert-then-exit path.
async fn wait_for_shutdown_signal() {
    let mut sigint = match signal(SignalKind::interrupt()) {
        Ok(stream) => stream,
        Err(e) => {
            log::error!("failed to install SIGINT handler: {e}");
            return;
        }
    };
    let mut sigterm = match signal(SignalKind::terminate()) {
        Ok(stream) => stream,
        Err(e) => {
            log::error!("failed to install SIGTERM handler: {e}");
            return;
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
