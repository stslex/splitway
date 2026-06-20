//! The long-running daemon: builds a multi-thread tokio runtime, watches the
//! VPN, serves the IPC socket, and on `SIGTERM`/`SIGINT` reverts active rules
//! before exiting cleanly.

mod ipc;
mod state;

use std::path::{Path, PathBuf};
use std::process::exit;
use std::sync::Arc;

use tokio::signal::unix::{signal, Signal, SignalKind};
use tokio::sync::{mpsc, oneshot};

use splitway_shared::config::{self, ConfigParseError, LocalConfig};
use splitway_shared::ipc::socket_path;
use splitway_shared::platform::DnsBackend;

use crate::backend::create_dns_backend;
use state::{run_state, DetectorFactory, PlatformDetectorFactory, StateCommand, StateMachine};

/// Entry point for `splitway-daemon run`. `config_path` is the optional
/// `--config <PATH>` override; `None` uses the default config location. The
/// resolved path is the daemon's active config file for its whole lifetime —
/// the file `GetConfig`/`SetConfig` read and write.
pub fn run(config_path: Option<PathBuf>) {
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
    runtime.block_on(run_async(config, config_path));
}

async fn run_async(config: LocalConfig, config_path: PathBuf) {
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
    let listener = match ipc::bind_socket(&socket) {
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
    let machine = StateMachine::new(
        backend,
        detector_factory,
        config,
        config_path,
        state_tx.clone(),
    );
    // `run_state` arms the watch before entering its command loop. That happens
    // here — after the socket bind and signal handlers above (so a fatal startup
    // error can never strand applied rules) and before IPC is served below. An
    // empty `vpn_name` or a detector that fails to start leaves auto-apply off
    // while IPC stays up (see `arm_watch`).
    let state_handle = tokio::spawn(run_state(machine, state_rx, shutdown_rx));

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
