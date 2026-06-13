//! macOS VPN detector: reads per-interface DNS from `scutil --dns` and watches
//! for changes via SCDynamicStore.
//!
//! Mirrors the Linux split: [`parser`] (pure `scutil` parsing) and [`state`]
//! (pure up/down mapping + dedup) are unit-tested; [`watch`] is the thin Core
//! Foundation plumbing, and [`detector`] wires them to the `VpnDetector` trait.

mod detector;
mod parser;
mod state;
mod watch;

pub struct MacosDetector;
