//! Thin SCDynamicStore plumbing for the macOS VPN event stream.
//!
//! The Core Foundation run loop blocks its thread, so the watch runs on a
//! dedicated `std` thread and feeds the same `tokio::sync::mpsc::Sender`
//! contract the Linux detector uses (`blocking_send`, since this thread is not
//! a tokio worker). All real logic lives in the pure `parser`/`state` modules;
//! this file is the FFI glue and is intentionally not unit-tested.
//!
//! API verified against `system-configuration` 0.7 + `core-foundation` 0.9 and
//! the crate's own `watch_dns` example. `SCDynamicStore` is not `Send`, so it
//! is built on the watcher thread rather than moved into it.
//!
//! Shutdown is lazy, unlike the Linux watch (which selects on `tx.closed()`):
//! the dropped receiver is only noticed inside the callback, which fires on the
//! next network/DNS change. If the daemon exits while the network is quiet, the
//! thread stays parked in `CFRunLoop::run_current()` until the next event, and
//! is otherwise reaped at process exit. That is acceptable for a daemon whose
//! lifetime is the process's; it is not a leak that accumulates.

use core_foundation::array::CFArray;
use core_foundation::runloop::{kCFRunLoopCommonModes, CFRunLoop};
use core_foundation::string::CFString;
use system_configuration::dynamic_store::{
    SCDynamicStore, SCDynamicStoreBuilder, SCDynamicStoreCallBackContext,
};
use tokio::sync::mpsc::{self, Receiver, Sender};

use splitway_shared::platform::{PlatformError, VpnEvent, VpnInfo};

use super::detector::current_dns;
use super::state::{transition, Deduper, Transition};

/// State carried through the SCDynamicStore callback (a bare `fn`, so it cannot
/// capture — everything it needs lives here).
struct WatchContext {
    interface: String,
    tx: Sender<VpnEvent>,
    dedup: Deduper,
}

/// Spawn the macOS watch thread and return the event receiver.
pub(super) fn watch(interface: &str) -> Result<Receiver<VpnEvent>, PlatformError> {
    let (tx, rx) = mpsc::channel(16);
    let interface = interface.to_string();
    std::thread::Builder::new()
        .name("splitway-scdynamicstore".to_string())
        .spawn(move || run_watch(interface, tx))
        .map_err(|e| {
            PlatformError::CommandFailed(format!("failed to spawn macOS watch thread: {e}"))
        })?;
    Ok(rx)
}

fn run_watch(interface: String, tx: Sender<VpnEvent>) {
    // Emit the current state once up-front: the VPN may already be up before we
    // subscribe (mirrors the Linux watch reading the device's current state).
    let mut dedup = Deduper::default();
    if !emit_current(&interface, &tx, &mut dedup) {
        return; // receiver already gone
    }

    let context = SCDynamicStoreCallBackContext {
        callout: on_change,
        info: WatchContext {
            interface,
            tx,
            dedup,
        },
    };

    let store = match SCDynamicStoreBuilder::new("splitway-dns-watch")
        .callback_context(context)
        .build()
    {
        Some(store) => store,
        None => {
            log::error!("failed to create SCDynamicStore; macOS VPN watch disabled");
            return;
        }
    };

    // Watch global DNS plus per-service DNS and per-interface IPv4 changes; any
    // of these fires when a VPN comes up or goes down. The callback re-reads the
    // full state, so over-broad keys only cost a redundant (deduped) read.
    let keys: CFArray<CFString> =
        CFArray::from_CFTypes(&[CFString::from_static_string("State:/Network/Global/DNS")]);
    let patterns: CFArray<CFString> = CFArray::from_CFTypes(&[
        CFString::from_static_string("(State|Setup):/Network/Service/.*/DNS"),
        CFString::from_static_string("State:/Network/Interface/.*/IPv4"),
    ]);
    if !store.set_notification_keys(&keys, &patterns) {
        log::error!("SCDynamicStore::set_notification_keys failed; macOS VPN watch disabled");
        return;
    }

    let source = match store.create_run_loop_source() {
        Some(source) => source,
        None => {
            log::error!(
                "failed to create SCDynamicStore run loop source; macOS VPN watch disabled"
            );
            return;
        }
    };

    let run_loop = CFRunLoop::get_current();
    // `kCFRunLoopCommonModes` is an extern static; reading it is unsafe. It is
    // already a `CFRunLoopMode`, so it is passed without a cast.
    run_loop.add_source(&source, unsafe { kCFRunLoopCommonModes });
    log::debug!("starting macOS SCDynamicStore watch");
    CFRunLoop::run_current();
    log::debug!("macOS SCDynamicStore watch stopped");
}

/// SCDynamicStore change callback. Must match `SCDynamicStoreCallBackT`:
/// `fn(SCDynamicStore, CFArray<CFString>, &mut T)`.
fn on_change(_store: SCDynamicStore, _changed_keys: CFArray<CFString>, ctx: &mut WatchContext) {
    if !emit_current(&ctx.interface, &ctx.tx, &mut ctx.dedup) {
        // Receiver dropped: stop the run loop so the thread can exit.
        CFRunLoop::get_current().stop();
    }
}

/// Read the interface's current DNS, map it to up/down, and send an event if
/// the state changed. Returns `false` if the receiver has been dropped.
fn emit_current(interface: &str, tx: &Sender<VpnEvent>, dedup: &mut Deduper) -> bool {
    let servers = match current_dns(interface) {
        Ok(servers) => servers,
        // A transient `scutil` failure is not "VPN down": keep the last known
        // state instead of emitting a spurious Down that would revert rules.
        Err(e) => {
            log::warn!("reading DNS for {interface} failed: {e}; keeping last state");
            return true;
        }
    };
    let transition = transition(!servers.is_empty());
    if !dedup.changed(transition) {
        return true;
    }
    let event = match transition {
        Transition::Up => VpnEvent::Up(VpnInfo {
            interface_name: interface.to_string(),
            dns_servers: servers,
        }),
        Transition::Down => VpnEvent::Down {
            interface_name: interface.to_string(),
        },
    };
    tx.blocking_send(event).is_ok()
}
