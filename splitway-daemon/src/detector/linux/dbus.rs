//! Thin NetworkManager D-Bus plumbing for the VPN event stream.
//!
//! Signatures verified against a live NetworkManager via `busctl introspect`:
//! `GetDeviceByIpIface(s) -> o`, `DeviceAdded(o)`, `DeviceRemoved(o)`,
//! `Device.StateChanged(u new, u old, u reason)`, device properties
//! `Interface (s)` and `State (u)`.

use splitway_shared::platform::{PlatformError, VpnDetector, VpnEvent};
use tokio::sync::mpsc::Sender;
use zbus::export::ordered_stream::OrderedStreamExt;
use zbus::zvariant::OwnedObjectPath;
use zbus::{proxy, Connection};

use crate::detector::linux::state::{transition, Deduper, Transition};
use crate::detector::linux::LinuxDetector;

#[proxy(
    interface = "org.freedesktop.NetworkManager",
    default_service = "org.freedesktop.NetworkManager",
    default_path = "/org/freedesktop/NetworkManager"
)]
trait NetworkManager {
    fn get_device_by_ip_iface(&self, iface: &str) -> zbus::Result<OwnedObjectPath>;

    #[zbus(signal)]
    fn device_added(&self, device_path: OwnedObjectPath) -> zbus::Result<()>;

    #[zbus(signal)]
    fn device_removed(&self, device_path: OwnedObjectPath) -> zbus::Result<()>;
}

#[proxy(
    interface = "org.freedesktop.NetworkManager.Device",
    default_service = "org.freedesktop.NetworkManager"
)]
trait Device {
    #[zbus(property)]
    fn interface(&self) -> zbus::Result<String>;

    #[zbus(property)]
    fn state(&self) -> zbus::Result<u32>;

    // Renamed on the Rust side: the generated `receive_state_changed`
    // would collide with the `State` property's change stream.
    #[zbus(signal, name = "StateChanged")]
    fn device_state_changed(&self, new_state: u32, old_state: u32, reason: u32)
        -> zbus::Result<()>;
}

fn to_platform_error(e: zbus::Error) -> PlatformError {
    PlatformError::DbusError(e.to_string())
}

/// A device currently subscribed to: its object path plus the
/// `StateChanged` signal stream.
struct WatchedDevice {
    path: OwnedObjectPath,
    states: StateChangedStream,
}

/// Feed `VpnEvent`s for `interface` into `tx` until the receiver is
/// dropped or the D-Bus connection breaks.
pub(crate) async fn watch_loop(
    interface: String,
    tx: Sender<VpnEvent>,
) -> Result<(), PlatformError> {
    let connection = Connection::system().await.map_err(to_platform_error)?;
    let nm = NetworkManagerProxy::new(&connection)
        .await
        .map_err(to_platform_error)?;

    let mut added_stream = nm.receive_device_added().await.map_err(to_platform_error)?;
    let mut removed_stream = nm
        .receive_device_removed()
        .await
        .map_err(to_platform_error)?;

    let mut dedup = Deduper::default();

    // The device may not exist yet: tun/wireguard devices are created
    // when the VPN client starts. DeviceAdded picks it up later.
    let mut device = match nm.get_device_by_ip_iface(&interface).await {
        Ok(path) => Some(watch_device(&connection, path, &interface, &mut dedup, &tx).await?),
        Err(e) => {
            log::debug!("device {interface} not present yet: {e}");
            None
        }
    };

    loop {
        tokio::select! {
            // Receiver dropped: the subscriber is gone, stop watching.
            _ = tx.closed() => return Ok(()),

            added = added_stream.next() => {
                let Some(signal) = added else { return Ok(()) };
                let args = signal.args().map_err(to_platform_error)?;
                let path = args.device_path;
                match device_interface_name(&connection, &path).await {
                    Ok(name) if name == interface => {
                        log::debug!("device {interface} appeared at {path}");
                        device = Some(
                            watch_device(&connection, path, &interface, &mut dedup, &tx).await?,
                        );
                    }
                    Ok(_) => {}
                    Err(e) => {
                        log::debug!("cannot read Interface of added device {path}: {e}");
                    }
                }
            }

            removed = removed_stream.next() => {
                let Some(signal) = removed else { return Ok(()) };
                let args = signal.args().map_err(to_platform_error)?;
                if device.as_ref().is_some_and(|d| d.path == args.device_path) {
                    log::debug!("device {interface} removed");
                    device = None;
                    emit_down(&interface, &mut dedup, &tx).await;
                }
            }

            // The async block keeps the disabled branch from evaluating
            // the unwrap when no device is being watched.
            state = async { device.as_mut().unwrap().states.next().await },
                if device.is_some() =>
            {
                match state {
                    Some(signal) => {
                        let args = signal.args().map_err(to_platform_error)?;
                        handle_state(args.new_state, &interface, &mut dedup, &tx).await?;
                    }
                    None => device = None,
                }
            }
        }
    }
}

/// Subscribe to a device's `StateChanged`, then feed its *current* state
/// through the same path: the device may have activated between
/// `DeviceAdded`/startup and our subscription.
async fn watch_device(
    connection: &Connection,
    path: OwnedObjectPath,
    interface: &str,
    dedup: &mut Deduper,
    tx: &Sender<VpnEvent>,
) -> Result<WatchedDevice, PlatformError> {
    let proxy = DeviceProxy::builder(connection)
        .path(path.clone())
        .map_err(to_platform_error)?
        .build()
        .await
        .map_err(to_platform_error)?;

    let states = proxy
        .receive_device_state_changed()
        .await
        .map_err(to_platform_error)?;
    let current = proxy.state().await.map_err(to_platform_error)?;
    handle_state(current, interface, dedup, tx).await?;

    Ok(WatchedDevice { path, states })
}

async fn device_interface_name(
    connection: &Connection,
    path: &OwnedObjectPath,
) -> Result<String, PlatformError> {
    let proxy = DeviceProxy::builder(connection)
        .path(path.clone())
        .map_err(to_platform_error)?
        .build()
        .await
        .map_err(to_platform_error)?;
    proxy.interface().await.map_err(to_platform_error)
}

/// Map an NM device state to a deduplicated `VpnEvent` and send it.
async fn handle_state(
    new_state: u32,
    interface: &str,
    dedup: &mut Deduper,
    tx: &Sender<VpnEvent>,
) -> Result<(), PlatformError> {
    let Some(t) = transition(new_state) else {
        log::debug!("ignoring state {new_state} for {interface}");
        return Ok(());
    };
    if dedup.is_dup(t) {
        return Ok(());
    }

    match t {
        Transition::Up => {
            let iface = interface.to_string();
            // detect() runs nmcli synchronously (and may briefly block to let
            // pushed DNS settle); keep it off the async thread.
            let detected = tokio::task::spawn_blocking(move || LinuxDetector.detect(&iface))
                .await
                .map_err(|e| PlatformError::CommandFailed(format!("detect task panicked: {e}")))?;
            match detected {
                // Up-ness is NOT gated on finding pushed DNS: detect() returns
                // Ok with possibly-empty dns_servers (e.g. a VPN that pushed no
                // DNS), and we emit Up either way. An Up with empty DNS is a
                // safe no-op downstream — StateMachine::desired() returns None
                // for it, so nothing is applied or reverted.
                Ok(info) => {
                    dedup.record(t);
                    send_event(tx, VpnEvent::Up(info)).await;
                }
                // detect() now errors only on a genuine nmcli failure / absent
                // device, never on empty DNS. Not recorded in dedup: a later
                // re-emitted ACTIVATED state gets another chance to detect.
                Err(e) => log::warn!("{interface} reported up but detect failed: {e}"),
            }
        }
        Transition::Down => emit_down(interface, dedup, tx).await,
    }
    Ok(())
}

async fn emit_down(interface: &str, dedup: &mut Deduper, tx: &Sender<VpnEvent>) {
    if dedup.push(Transition::Down).is_none() {
        return;
    }
    send_event(
        tx,
        VpnEvent::Down {
            interface_name: interface.to_string(),
        },
    )
    .await;
}

async fn send_event(tx: &Sender<VpnEvent>, event: VpnEvent) {
    // A send error means the receiver was dropped; the `tx.closed()`
    // branch of the watch loop terminates the task right after.
    if let Err(e) = tx.send(event).await {
        log::debug!("VPN event receiver dropped: {e}");
    }
}
