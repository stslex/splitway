//! Pure up/down mapping + event dedup for the macOS DNS watcher, mirroring the
//! Linux `state.rs`. SCDynamicStore re-fires on every related change, so the
//! [`Deduper`] suppresses repeats of the same transition.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Transition {
    Up,
    Down,
}

/// A VPN interface is "up" exactly when it currently has DNS servers.
pub(super) fn transition(has_dns: bool) -> Transition {
    if has_dns {
        Transition::Up
    } else {
        Transition::Down
    }
}

/// Suppresses consecutive duplicate transitions.
#[derive(Debug, Default)]
pub(super) struct Deduper {
    last: Option<Transition>,
}

impl Deduper {
    /// Record `transition`; return `true` only if it differs from the last one
    /// recorded (i.e. the caller should emit an event).
    pub(super) fn changed(&mut self, transition: Transition) -> bool {
        if self.last == Some(transition) {
            false
        } else {
            self.last = Some(transition);
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{transition, Deduper, Transition};

    #[test]
    fn has_dns_maps_to_up_else_down() {
        assert_eq!(transition(true), Transition::Up);
        assert_eq!(transition(false), Transition::Down);
    }

    #[test]
    fn deduper_emits_only_on_change() {
        let mut dedup = Deduper::default();
        assert!(dedup.changed(Transition::Up)); // first ever -> emit
        assert!(!dedup.changed(Transition::Up)); // repeat -> suppress
        assert!(dedup.changed(Transition::Down)); // change -> emit
        assert!(!dedup.changed(Transition::Down)); // repeat -> suppress
        assert!(dedup.changed(Transition::Up)); // change again -> emit
    }
}
