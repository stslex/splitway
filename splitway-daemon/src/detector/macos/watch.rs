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
//! is built on the watcher thread rather than moved into it; the `Rc<RefCell>`
//! dedup likewise never leaves this thread.
//!
//! Shutdown is lazy, unlike the Linux watch (which selects on `tx.closed()`):
//! the dropped receiver is only noticed inside the callback, which fires on the
//! next network/DNS change. If the daemon exits while the network is quiet, the
//! thread stays parked in `CFRunLoop::run_current()` until the next event, and
//! is otherwise reaped at process exit. That is acceptable for a daemon whose
//! lifetime is the process's; it is not a leak that accumulates.
//!
//! The same laziness applies to a live watch **re-arm** (Phase 5): when the
//! configured interface changes, the state machine drops this watch's receiver
//! and arms a new one. This thread then stops only on the next network/DNS
//! change, when its callback observes the dropped receiver (`blocking_send`
//! fails) and stops the run loop. A re-arm triggered purely by a config edit
//! (e.g. changing `vpn_name` in the GUI) need not coincide with a network
//! change, so on a quiet network each such re-arm leaves the previous thread
//! parked until the next network/DNS event or process exit; several can be
//! parked transiently. No stale event can reach the state machine (the receiver
//! is gone), the parked threads hold no live resources, and all are reaped at
//! process exit — a bounded, self-healing backlog, not a growing leak. NM /
//! standalone-OpenVPN release promptly via `tx.closed()`; macOS trades that
//! promptness for staying purely event-driven (no idle wakeups). Deterministic
//! teardown (stopping this run loop from the actor on re-arm, rather than
//! waiting for the next event) is a possible macOS follow-up.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use core_foundation::array::CFArray;
use core_foundation::runloop::{kCFRunLoopCommonModes, CFRunLoop};
use core_foundation::string::CFString;
use system_configuration::dynamic_store::{
    SCDynamicStore, SCDynamicStoreBuilder, SCDynamicStoreCallBackContext,
};
use tokio::sync::mpsc::{self, Receiver, Sender};

use splitway_shared::platform::{PlatformError, VpnEvent, VpnInfo};

use super::detector::current_vpn_state;
use super::parser::Detected;
use super::state::{sample_with_retry, Deduper, Emit, INITIAL_SAMPLE_ATTEMPTS};

/// Delay between retries of the post-arm initial sample (see [`emit_initial`]).
const INITIAL_SAMPLE_RETRY_DELAY: Duration = Duration::from_millis(300);

/// State carried through the SCDynamicStore callback (a bare `fn`, so it cannot
/// capture — everything it needs lives here). The dedup is shared (`Rc`) with
/// the post-arm initial sample so the two never double-emit.
struct WatchContext {
    interface: String,
    tx: Sender<VpnEvent>,
    dedup: Rc<RefCell<Deduper>>,
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
    let dedup = Rc::new(RefCell::new(Deduper::default()));

    let context = SCDynamicStoreCallBackContext {
        callout: on_change,
        info: WatchContext {
            interface: interface.clone(),
            tx: tx.clone(),
            dedup: dedup.clone(),
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

    // Watch the per-service DNS and per-interface IPv4 keys (what detection now
    // reads — see `parser`), plus the two global keys. `State:/Network/Global/IPv4`
    // is essential, not just an extra trigger: detection reads `PrimaryService`/
    // `PrimaryInterface` from it to anchor the physical service and demote target,
    // and macOS can switch the primary route between already-configured services by
    // updating only this key, without touching any watched DNS key — so without it
    // the daemon would sleep with a stale primary-service decision. `Global/DNS` is
    // the extra trigger. The callback re-reads the full model and dedups, so
    // over-broad keys (and the callback our own demote of a service's DNS triggers)
    // only cost a redundant, suppressed read — never a spurious state change.
    let keys: CFArray<CFString> = CFArray::from_CFTypes(&[
        CFString::from_static_string("State:/Network/Global/IPv4"),
        CFString::from_static_string("State:/Network/Global/DNS"),
    ]);
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

    // Sample the current state only AFTER the source is armed: a transition
    // racing between the sample and arming would otherwise be lost (it happened
    // before we were listening, and we'd already have a stale sample). With the
    // source live, any such change is queued and delivered once the run loop
    // starts; the shared dedup keeps this sample and that delivery from
    // double-emitting.
    if !emit_initial(&interface, &tx, &mut dedup.borrow_mut()) {
        return; // receiver already gone
    }

    log::debug!("starting macOS SCDynamicStore watch");
    CFRunLoop::run_current();
    log::debug!("macOS SCDynamicStore watch stopped");
}

/// SCDynamicStore change callback. Must match `SCDynamicStoreCallBackT`:
/// `fn(SCDynamicStore, CFArray<CFString>, &mut T)`.
///
/// This runs via an `extern "C"` trampoline, so a panic must not unwind across
/// it (that aborts the process). Catch any unexpected panic, log it, and keep
/// the watch alive — a one-off failed sample is recoverable on the next event.
fn on_change(_store: SCDynamicStore, _changed_keys: CFArray<CFString>, ctx: &mut WatchContext) {
    let alive = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut dedup = ctx.dedup.borrow_mut();
        emit_current(&ctx.interface, &ctx.tx, &mut dedup)
    }))
    .unwrap_or_else(|_| {
        log::error!("macOS DNS watch callback panicked; ignoring this notification");
        true // stay alive rather than tear the loop down on a transient panic
    });
    if !alive {
        // Receiver dropped: stop the run loop so the thread can exit.
        CFRunLoop::get_current().stop();
    }
}

/// Steady-state callback path: read the current DNS model, decide whether it
/// represents a new state, and send the corresponding event. Returns `false`
/// if the receiver has been dropped.
fn emit_current(interface: &str, tx: &Sender<VpnEvent>, dedup: &mut Deduper) -> bool {
    match current_vpn_state() {
        Ok(detected) => emit_detected(interface, tx, dedup, detected),
        // A transient `scutil` failure is not "VPN down": keep the last known
        // state instead of emitting a spurious Down that would revert rules.
        // SCDynamicStore re-fires the callback on the next change, so a one-off
        // hiccup here recovers on its own.
        Err(e) => {
            log::warn!("reading the DNS model failed: {e}; keeping last state");
            true
        }
    }
}

/// Post-arm initial sample — the daemon's *only* startup detection path
/// (`daemon/mod.rs` wires `watch()` as the sole VPN-state source; there is no
/// startup `detect()`). The steady-state callback can keep the last state on a
/// transient `scutil` error because the next notification will re-fire it, but
/// at startup `last` is `None` and an already-up VPN may sit on a quiescent link
/// for a long time before any notification arrives — so a single `scutil` hiccup
/// here would silently leave split-DNS off until something perturbs the network.
/// Retry the read a few times before giving up. The source is already armed, so
/// notifications that fire during the retries are queued and delivered once the
/// run loop starts, and the shared dedup prevents a double-emit.
fn emit_initial(interface: &str, tx: &Sender<VpnEvent>, dedup: &mut Deduper) -> bool {
    let sample = sample_with_retry(INITIAL_SAMPLE_ATTEMPTS, current_vpn_state, || {
        std::thread::sleep(INITIAL_SAMPLE_RETRY_DELAY)
    });
    match sample {
        Ok(detected) => emit_detected(interface, tx, dedup, detected),
        Err(e) => {
            log::error!(
                "initial DNS-model read failed after {INITIAL_SAMPLE_ATTEMPTS} \
                 attempts: {e}; auto-apply will start on the next DNS/network change"
            );
            true // stay alive; a later notification can still recover
        }
    }
}

/// Dedup a freshly-read detection and send the resulting event. Returns `false`
/// if the receiver has been dropped. The `interface_name` carried in `Up` is
/// advisory on macOS (nothing keys on it); the demote-target rides along so the
/// backend can demote the hijacked default to it.
fn emit_detected(
    interface: &str,
    tx: &Sender<VpnEvent>,
    dedup: &mut Deduper,
    detected: Detected,
) -> bool {
    let event = match dedup.decide(&detected) {
        Emit::Up => match detected {
            Detected::Up {
                corp_dns,
                demote_target,
            } => VpnEvent::Up(VpnInfo {
                interface_name: interface.to_string(),
                dns_servers: corp_dns,
                demote_target: Some(demote_target),
            }),
            // `decide` only returns `Up` for a `Detected::Up`.
            Detected::Down => unreachable!("decide returned Up for a Down detection"),
        },
        Emit::Down => VpnEvent::Down {
            interface_name: interface.to_string(),
        },
        Emit::Nothing => return true,
    };
    tx.blocking_send(event).is_ok()
}
