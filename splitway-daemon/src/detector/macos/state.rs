//! Pure watcher state for the macOS DNS watcher, mirroring the Linux `state.rs`:
//! event dedup ([`Deduper`]) and the initial-sample retry policy
//! ([`sample_with_retry`]). Kept here, free of FFI/IO, so both are unit-tested.
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

/// How many times the post-arm initial sample reads `scutil` before giving up.
/// The steady-state callback does not retry (SCDynamicStore re-fires it), but
/// the initial sample is the daemon's only startup detection path, so a one-off
/// transient failure there must not strand an already-up VPN — see
/// [`sample_with_retry`] and `watch::emit_initial`.
pub(super) const INITIAL_SAMPLE_ATTEMPTS: u32 = 5;

/// Read a DNS sample, retrying a transient failure up to `attempts` times,
/// `pause`-ing between tries. Returns the first successful sample, or the last
/// error if every attempt failed. Pure with respect to I/O — the caller injects
/// the `scutil` read and the sleep — so the retry behavior is unit-tested
/// without a live system. `attempts` is always read at least once (a `0` budget
/// still performs one read).
pub(super) fn sample_with_retry<T, E>(
    attempts: u32,
    mut read: impl FnMut() -> Result<T, E>,
    mut pause: impl FnMut(),
) -> Result<T, E> {
    let mut result = read();
    let mut tried = 1;
    while result.is_err() && tried < attempts {
        pause();
        result = read();
        tried += 1;
    }
    result
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
    use super::{sample_with_retry, Deduper, Emit};
    use std::cell::Cell;

    fn s(values: &[&str]) -> Vec<String> {
        values.iter().map(|v| v.to_string()).collect()
    }

    #[test]
    fn retry_succeeds_on_first_try_without_pausing() {
        let pauses = Cell::new(0);
        let reads = Cell::new(0);
        let r: Result<Vec<String>, &str> = sample_with_retry(
            5,
            || {
                reads.set(reads.get() + 1);
                Ok(s(&["9.9.9.9"]))
            },
            || pauses.set(pauses.get() + 1),
        );
        assert_eq!(r, Ok(s(&["9.9.9.9"])));
        assert_eq!(reads.get(), 1, "a first success reads exactly once");
        assert_eq!(pauses.get(), 0, "no pause when the first read succeeds");
    }

    #[test]
    fn retry_recovers_from_transient_failures() {
        // Fails twice, then succeeds: the initial-sample hiccup is absorbed.
        let attempt = Cell::new(0);
        let pauses = Cell::new(0);
        let r = sample_with_retry(
            5,
            || {
                attempt.set(attempt.get() + 1);
                if attempt.get() < 3 {
                    Err("scutil hiccup")
                } else {
                    Ok(s(&["1.1.1.1"]))
                }
            },
            || pauses.set(pauses.get() + 1),
        );
        assert_eq!(r, Ok(s(&["1.1.1.1"])));
        assert_eq!(attempt.get(), 3);
        assert_eq!(pauses.get(), 2, "paused before each of the two retries");
    }

    #[test]
    fn retry_gives_up_after_attempts_returning_last_error() {
        let reads = Cell::new(0);
        let pauses = Cell::new(0);
        let r: Result<Vec<String>, &str> = sample_with_retry(
            3,
            || {
                reads.set(reads.get() + 1);
                Err("persistent failure")
            },
            || pauses.set(pauses.get() + 1),
        );
        assert_eq!(r, Err("persistent failure"));
        assert_eq!(reads.get(), 3, "reads exactly `attempts` times");
        assert_eq!(pauses.get(), 2, "pauses between tries, not after the last");
    }

    #[test]
    fn retry_reads_once_even_with_zero_budget() {
        let reads = Cell::new(0);
        let r: Result<Vec<String>, &str> = sample_with_retry(
            0,
            || {
                reads.set(reads.get() + 1);
                Ok(s(&["8.8.8.8"]))
            },
            || panic!("must not pause with a zero budget"),
        );
        assert_eq!(r, Ok(s(&["8.8.8.8"])));
        assert_eq!(reads.get(), 1);
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
        assert_eq!(dedup.decide(&s(&["1.1.1.1"])), Emit::Up); // up again (last reset to None)
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
