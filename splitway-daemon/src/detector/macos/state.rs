//! Pure event-dedup for the macOS DNS watcher, mirroring the Linux `state.rs`.
//!
//! SCDynamicStore re-fires on every related change, so the [`Deduper`]
//! suppresses samples that carry no new information. Unlike a plain up/down
//! flag, it tracks the actual DNS server list: a VPN DNS rotation (still "up"
//! but with different servers) is a real change and must be re-emitted, or the
//! `/etc/resolver` files would keep pointing at stale servers.

/// What the watcher should do for a freshly-sampled DNS server list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Emit {
    /// Emit `Up` with the sampled servers (gained DNS, or the server set changed).
    Up,
    /// Emit `Down` (lost its DNS).
    Down,
    /// No change since the last emission — emit nothing.
    Nothing,
}

/// Tracks the last emitted DNS state so identical samples are suppressed while a
/// changed server set still propagates.
#[derive(Debug, Default)]
pub(super) struct Deduper {
    /// `Some(servers)` = last emitted `Up` with these servers; `None` = last
    /// emitted `Down` (or nothing emitted yet).
    last: Option<Vec<String>>,
}

impl Deduper {
    /// Decide what to emit for the just-sampled `servers` (empty = the interface
    /// is down), recording it as the new last-emitted state.
    pub(super) fn decide(&mut self, servers: &[String]) -> Emit {
        if servers.is_empty() {
            if self.last.is_none() {
                Emit::Nothing
            } else {
                self.last = None;
                Emit::Down
            }
        } else if self.last.as_deref() == Some(servers) {
            Emit::Nothing
        } else {
            self.last = Some(servers.to_vec());
            Emit::Up
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Deduper, Emit};

    fn s(values: &[&str]) -> Vec<String> {
        values.iter().map(|v| v.to_string()).collect()
    }

    #[test]
    fn first_up_then_identical_is_suppressed() {
        let mut dedup = Deduper::default();
        assert_eq!(dedup.decide(&s(&["1.1.1.1"])), Emit::Up);
        assert_eq!(dedup.decide(&s(&["1.1.1.1"])), Emit::Nothing);
    }

    #[test]
    fn down_emitted_only_when_previously_up() {
        let mut dedup = Deduper::default();
        assert_eq!(dedup.decide(&s(&[])), Emit::Nothing); // never up -> nothing
        assert_eq!(dedup.decide(&s(&["1.1.1.1"])), Emit::Up);
        assert_eq!(dedup.decide(&s(&[])), Emit::Down);
        assert_eq!(dedup.decide(&s(&[])), Emit::Nothing); // already down
    }

    #[test]
    fn dns_server_rotation_reemits_up() {
        let mut dedup = Deduper::default();
        assert_eq!(dedup.decide(&s(&["1.1.1.1"])), Emit::Up);
        // Still up, but the server changed: must re-emit, not suppress.
        assert_eq!(dedup.decide(&s(&["2.2.2.2"])), Emit::Up);
        assert_eq!(dedup.decide(&s(&["2.2.2.2"])), Emit::Nothing);
        // A different set (added server) is also a change.
        assert_eq!(dedup.decide(&s(&["2.2.2.2", "3.3.3.3"])), Emit::Up);
    }
}
