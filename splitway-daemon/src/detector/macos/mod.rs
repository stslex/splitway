//! macOS VPN detector: reads the SystemConfiguration DNS model from the dynamic
//! store (via `scutil`) and decides VPN up/down **structurally** — is the system
//! default resolver overridden by a non-physical (VPN) service? — rather than by
//! an interface name. See [`parser`] for the rationale (a global-DNS-hijack VPN
//! client scopes no resolver to any `utun`, so an interface-keyed read is blind).
//!
//! Mirrors the Linux split: [`parser`] (pure dynamic-store parsing + the up/down
//! decision) and [`state`] (pure dedup + the initial-sample retry policy) are
//! unit-tested; [`watch`] is the thin Core Foundation plumbing, and [`detector`]
//! wires them to the `VpnDetector` trait and owns the `scutil` I/O.

mod detector;
mod parser;
mod state;
mod watch;

/// Re-export the dynamic-store dump parsers for the macOS DNS backend's demote,
/// which reads the same `scutil` dump shape (see [`crate::detector`]).
pub(crate) use parser::{parse_array_field, parse_scalar_field};

pub struct MacosDetector;
