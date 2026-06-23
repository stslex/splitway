//! Pure NMDeviceState -> VPN transition mapping and event deduplication.
//!
//! State values follow the NetworkManager D-Bus API (verified via
//! `busctl introspect`): 30 disconnected, 40-90 activation stages,
//! 100 activated, 110 deactivating, 120 failed.

const NM_DEVICE_STATE_DISCONNECTED: u32 = 30;
const NM_DEVICE_STATE_ACTIVATED: u32 = 100;
const NM_DEVICE_STATE_DEACTIVATING: u32 = 110;
const NM_DEVICE_STATE_FAILED: u32 = 120;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Transition {
    Up,
    Down,
}

/// Map a new NMDeviceState to a VPN transition.
/// Intermediate activation stages and unknown states are ignored.
pub(crate) fn transition(new_state: u32) -> Option<Transition> {
    match new_state {
        NM_DEVICE_STATE_ACTIVATED => Some(Transition::Up),
        NM_DEVICE_STATE_DISCONNECTED | NM_DEVICE_STATE_DEACTIVATING | NM_DEVICE_STATE_FAILED => {
            Some(Transition::Down)
        }
        _ => None,
    }
}

/// Suppresses consecutive duplicate transitions: NetworkManager can
/// re-emit the same state, and removal of an already-down device must
/// not produce a second `Down` event.
#[derive(Debug, Default)]
pub(crate) struct Deduper {
    last: Option<Transition>,
}

impl Deduper {
    /// Would this transition be a consecutive duplicate?
    /// Split from [`Deduper::record`] so callers can skip expensive event
    /// construction (detect) without committing the transition.
    pub(crate) fn is_dup(&self, transition: Transition) -> bool {
        self.last == Some(transition)
    }

    /// Commit a transition as the last emitted one.
    pub(crate) fn record(&mut self, transition: Transition) {
        self.last = Some(transition);
    }

    /// Forget the last transition, returning to the initial neutral state in
    /// which neither an `Up` nor a `Down` is a duplicate.
    ///
    /// Used after emitting a *soft* `Up` — one carrying no DNS — which must pin
    /// nothing: recording it as `Up` would foreclose re-detection on a
    /// re-emitted ACTIVATED, while leaving the prior transition in place (e.g. a
    /// startup/teardown `Down`) would make the genuine following `Down` look
    /// like a duplicate and get dropped, stranding the daemon's `vpn_up` flag.
    pub(crate) fn reset(&mut self) {
        self.last = None;
    }

    pub(crate) fn push(&mut self, transition: Transition) -> Option<Transition> {
        if self.is_dup(transition) {
            return None;
        }
        self.record(transition);
        Some(transition)
    }
}

#[cfg(test)]
mod tests {
    use super::{transition, Deduper, Transition};

    #[test]
    fn activated_maps_to_up() {
        assert_eq!(transition(100), Some(Transition::Up));
    }

    #[test]
    fn disconnected_deactivating_failed_map_to_down() {
        assert_eq!(transition(30), Some(Transition::Down));
        assert_eq!(transition(110), Some(Transition::Down));
        assert_eq!(transition(120), Some(Transition::Down));
    }

    #[test]
    fn intermediate_and_unknown_states_are_ignored() {
        for state in [0, 10, 20, 40, 50, 60, 70, 80, 90, 999] {
            assert_eq!(transition(state), None, "state {state} must be ignored");
        }
    }

    #[test]
    fn deduper_passes_first_event() {
        let mut dedup = Deduper::default();
        assert_eq!(dedup.push(Transition::Up), Some(Transition::Up));
    }

    #[test]
    fn deduper_reset_returns_to_neutral_state() {
        // After a soft (empty-DNS) Up, `reset()` must leave neither transition a
        // duplicate: the genuine following Down still emits (so `vpn_up` clears),
        // and a re-emitted ACTIVATED still re-detects.
        let mut dedup = Deduper::default();
        dedup.record(Transition::Down);
        assert!(dedup.is_dup(Transition::Down));

        dedup.reset();
        assert!(!dedup.is_dup(Transition::Down));
        assert!(!dedup.is_dup(Transition::Up));
        // The following Down passes through rather than being swallowed.
        assert_eq!(dedup.push(Transition::Down), Some(Transition::Down));
    }

    #[test]
    fn deduper_suppresses_consecutive_duplicates() {
        let mut dedup = Deduper::default();
        assert_eq!(dedup.push(Transition::Up), Some(Transition::Up));
        assert_eq!(dedup.push(Transition::Up), None);
        assert_eq!(dedup.push(Transition::Down), Some(Transition::Down));
        assert_eq!(dedup.push(Transition::Down), None);
        assert_eq!(dedup.push(Transition::Down), None);
        assert_eq!(dedup.push(Transition::Up), Some(Transition::Up));
    }
}
