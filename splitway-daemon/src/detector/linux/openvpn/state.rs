//! Pure OpenVPN state-token -> VPN transition mapping.
//!
//! Reuses the NetworkManager detector's [`Transition`] and [`Deduper`]
//! (`detector::linux::state`) rather than duplicating them, so both Linux
//! detectors share one dedup definition and its tests. Only the token mapping
//! differs and lives here.

use crate::detector::linux::state::Transition;

/// Map an OpenVPN management state token to a VPN transition.
///
/// `CONNECTED` means the tunnel is up; `EXITING` (clean shutdown) and
/// `RECONNECTING` (link lost, OpenVPN is restarting the session) both mean it
/// is down. Every intermediate token (`CONNECTING`, `WAIT`, `AUTH`,
/// `GET_CONFIG`, `ASSIGN_IP`, `ADD_ROUTES`, `RESOLVE`, ...) and any unknown
/// token is ignored — only a definitive up/down drives a rule change.
pub(super) fn transition_for_state(state: &str) -> Option<Transition> {
    match state {
        "CONNECTED" => Some(Transition::Up),
        "EXITING" | "RECONNECTING" => Some(Transition::Down),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::transition_for_state;
    use crate::detector::linux::state::Transition;

    #[test]
    fn connected_maps_to_up() {
        assert_eq!(transition_for_state("CONNECTED"), Some(Transition::Up));
    }

    #[test]
    fn exiting_and_reconnecting_map_to_down() {
        assert_eq!(transition_for_state("EXITING"), Some(Transition::Down));
        assert_eq!(transition_for_state("RECONNECTING"), Some(Transition::Down));
    }

    #[test]
    fn intermediate_and_unknown_tokens_are_ignored() {
        for token in [
            "CONNECTING",
            "WAIT",
            "AUTH",
            "GET_CONFIG",
            "ASSIGN_IP",
            "ADD_ROUTES",
            "RESOLVE",
            "TCP_CONNECT",
            "",
            "BOGUS",
        ] {
            assert_eq!(transition_for_state(token), None, "token {token:?}");
        }
    }
}
