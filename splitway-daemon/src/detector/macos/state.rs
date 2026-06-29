//! Pure watcher state for the macOS DNS watcher, mirroring the Linux `state.rs`:
//! event dedup ([`Deduper`]) and the initial-sample retry policy
//! ([`sample_with_retry`]). Kept here, free of FFI/IO, so both are unit-tested.
//!
//! SCDynamicStore re-fires on every related change, so the [`Deduper`]
//! suppresses samples that carry no new information. It tracks the full detected
//! state ([`Detected`]) — both the corp DNS *and* the demote-target — so a
//! change to *either* (DNS rotation, or a new physical resolver after a Wi-Fi
//! switch) is a real change and is re-emitted, while an identical re-sample is
//! suppressed. Keying on the whole `Detected` (not just the servers) means a
//! demote-target change is not silently dropped.

use super::parser::Detected;

/// What the watcher should do for a freshly-sampled detection result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Emit {
    /// Emit `Up` (gained an override, or the corp DNS / demote-target changed).
    Up,
    /// Emit `Down` (the override went away).
    Down,
    /// No change since the last emission — emit nothing.
    Nothing,
}

/// Tracks the last emitted detection so identical samples are suppressed while a
/// changed state still propagates.
#[derive(Debug, Default)]
pub(super) struct Deduper {
    /// `Some(detected)` = last emitted `Up` carrying this state; `None` = last
    /// emitted `Down` (or nothing emitted yet).
    last: Option<Detected>,
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
    /// Decide what to emit for the just-sampled detection, recording it as the
    /// new last-emitted state. `Detected::Down` maps to a `Down` only if we were
    /// previously `Up`; an unchanged `Up` is suppressed; any change to the
    /// `Up` payload (corp DNS or demote-target) re-emits `Up`.
    pub(super) fn decide(&mut self, detected: &Detected) -> Emit {
        match detected {
            Detected::Down => {
                if self.last.is_none() {
                    Emit::Nothing
                } else {
                    self.last = None;
                    Emit::Down
                }
            }
            up @ Detected::Up { .. } => {
                if self.last.as_ref() == Some(up) {
                    Emit::Nothing
                } else {
                    self.last = Some(up.clone());
                    Emit::Up
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::parser::Detected;
    use super::{sample_with_retry, Deduper, Emit};
    use std::cell::Cell;

    fn s(values: &[&str]) -> Vec<String> {
        values.iter().map(|v| v.to_string()).collect()
    }

    /// An `Up` detection with the given corp DNS and a fixed demote-target.
    fn up(corp: &[&str]) -> Detected {
        Detected::Up {
            corp_dns: s(corp),
            demote_target: s(&["198.51.100.1"]),
        }
    }

    /// An `Up` detection with an explicit demote-target (for the demote-change
    /// test).
    fn up_with(corp: &[&str], demote: &[&str]) -> Detected {
        Detected::Up {
            corp_dns: s(corp),
            demote_target: s(demote),
        }
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
        assert_eq!(dedup.decide(&up(&["192.0.2.53"])), Emit::Up);
        assert_eq!(dedup.decide(&up(&["192.0.2.53"])), Emit::Nothing);
    }

    #[test]
    fn down_emitted_only_when_previously_up() {
        let mut dedup = Deduper::default();
        assert_eq!(dedup.decide(&Detected::Down), Emit::Nothing); // never up -> nothing
        assert_eq!(dedup.decide(&up(&["192.0.2.53"])), Emit::Up);
        assert_eq!(dedup.decide(&Detected::Down), Emit::Down);
        assert_eq!(dedup.decide(&Detected::Down), Emit::Nothing); // already down
        assert_eq!(dedup.decide(&up(&["192.0.2.53"])), Emit::Up); // up again (last reset to None)
    }

    #[test]
    fn corp_dns_rotation_reemits_up() {
        let mut dedup = Deduper::default();
        assert_eq!(dedup.decide(&up(&["192.0.2.53"])), Emit::Up);
        // Still up, but the corp DNS changed: must re-emit, not suppress.
        assert_eq!(dedup.decide(&up(&["192.0.2.54"])), Emit::Up);
        assert_eq!(dedup.decide(&up(&["192.0.2.54"])), Emit::Nothing);
        // A different set (added server) is also a change.
        assert_eq!(dedup.decide(&up(&["192.0.2.54", "192.0.2.55"])), Emit::Up);
    }

    #[test]
    fn demote_target_change_reemits_up() {
        // The demote-target (physical DHCP resolver) changing — e.g. after a
        // Wi-Fi switch — is a real change even when the corp DNS is unchanged,
        // so it must re-emit so the backend re-demotes to the new fallback.
        let mut dedup = Deduper::default();
        assert_eq!(
            dedup.decide(&up_with(&["192.0.2.53"], &["198.51.100.1"])),
            Emit::Up
        );
        assert_eq!(
            dedup.decide(&up_with(&["192.0.2.53"], &["198.51.100.9"])),
            Emit::Up
        );
        assert_eq!(
            dedup.decide(&up_with(&["192.0.2.53"], &["198.51.100.9"])),
            Emit::Nothing
        );
    }
}
