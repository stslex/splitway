//! The owned, **serializable** view-model a non-egui frontend consumes.
//!
//! The borrowed [`ViewModel`](crate::ViewModel) is zero-copy for egui, which
//! renders it each frame from the same thread that owns the [`GuiCore`]. A
//! Tauri-style frontend lives across a serialization boundary instead: the Rust
//! backend serializes one whole snapshot and pushes it to the web layer, which
//! holds only that last snapshot and renders it. So this module mirrors the
//! borrowed view as an owned, `Serialize` value — [`ViewModelSnapshot`], built
//! by [`GuiCore::snapshot`](crate::GuiCore::snapshot).
//!
//! Design rules this snapshot encodes (the read path the Tauri shell relies on):
//!
//! - **Whole-VM, never deltas.** The frontend renders whichever snapshot arrives
//!   last; there is no partial assembly. `PartialEq` lets the driver emit only
//!   when the snapshot actually changed.
//! - **Drift is computed at snapshot time, against this snapshot's own belief.**
//!   The [`verify`](ViewModelSnapshot::verify) section pairs the live read-back
//!   with a drift verdict derived from the *same* [`status`](ViewModelSnapshot::status)
//!   `applied` carried in this snapshot (see [`GuiCore::snapshot`]). A frontend
//!   never sees a drift verdict computed against a different poll cycle's config.
//! - **Verify degrades in isolation.** A failed `Verify` round-trip only turns
//!   [`VerifyView`] into [`VerifyView::Unavailable`]; the connection banner,
//!   status, config and interfaces in the same snapshot stay valid.
//!
//! Serde shapes (the TypeScript mirror in the Tauri crate must match these):
//! the shared wire enums ([`DriftVerdict`], `RoutingState`, `DetectorHealth`,
//! [`Health`](crate::model::Health), `VpnBackend`) keep their existing
//! externally-tagged / `rename_all` forms; only [`VerifyView`] is new here and is
//! internally tagged on `state` for an ergonomic discriminated union.

use serde::Serialize;

use splitway_shared::config::VpnBackend;
use splitway_shared::ipc::{DriftVerdict, InterfaceInfo, LinkDnsState, StatusInfo};

use crate::model::ConnectionState;
use crate::state::MessageKind;

/// The whole read-only view-model, owned and serializable. The field set mirrors
/// the borrowed [`ViewModel`](crate::ViewModel), plus [`config`] (the loaded
/// config projection, surfaced for read-only display) and [`verify`] (the live
/// DNS read-back + drift).
///
/// [`config`]: ViewModelSnapshot::config
/// [`verify`]: ViewModelSnapshot::verify
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ViewModelSnapshot {
    /// The connection banner: health plus an optional message (`None` only when
    /// healthy).
    pub connection: ConnectionState,
    /// `connection.health == Connected` — the read path is trustworthy.
    pub connected: bool,
    /// A request is in flight — drives a "working…" indicator.
    pub working: bool,
    /// The last trustworthy live status, or `None` when it is not (e.g. dropped on
    /// a non-status reply, so the toggle/applied state is never shown stale).
    pub status: Option<StatusInfo>,
    /// The host interfaces for the (read-only) picker display.
    pub interfaces: Vec<InterfaceInfo>,
    /// Whether a config has loaded — `false` gates a "loading config…" placeholder
    /// and leaves [`config`](ViewModelSnapshot::config) `None`.
    pub config_loaded: bool,
    /// The loaded editable-config projection, for read-only display (the selected
    /// interface, backend, OpenVPN endpoint). `None` until the first `GetConfig`.
    pub config: Option<ConfigFields>,
    /// The daemon's effective config path (read-only).
    pub config_path: String,
    /// The live per-link DNS read-back and drift verdict — see [`VerifyView`].
    pub verify: VerifyView,
    /// A transient, dismissable message (severity + text).
    pub message: Option<MessageView>,
}

/// The read-only projection of the editable config, surfaced for display. A
/// dedicated owned struct rather than the wire `ConfigView` so the snapshot does
/// not carry a second, redundant `config_path` (that lives once at the top level).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ConfigFields {
    /// The configured VPN interface (device) name — the "selected interface".
    pub vpn_name: String,
    /// Which VPN detector is configured.
    pub vpn_backend: VpnBackend,
    /// Standalone-OpenVPN management endpoint (`host:port` or a unix socket path).
    pub openvpn_management: String,
    /// Optional path to the management password file (`None` = none).
    pub openvpn_management_password_file: Option<String>,
}

/// A transient, dismissable message: severity plus text.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MessageView {
    pub kind: MessageKind,
    pub text: String,
}

/// The `Verify` section of the snapshot: the live per-link DNS state read back
/// from the system and how it compares to the daemon's `applied` belief.
///
/// Internally tagged on `state` so the frontend renders a clean discriminated
/// union (`{ state: "Available", live, drift }`, etc.). [`Unavailable`] is a
/// *transport/daemon* failure of the read-back and is deliberately distinct from
/// [`DriftVerdict::NotApplicable`] (a perfectly healthy "nothing is applied, so
/// there is nothing to compare"), which rides inside [`Available`].
///
/// [`Unavailable`]: VerifyView::Unavailable
/// [`Available`]: VerifyView::Available
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "state")]
pub enum VerifyView {
    /// No `Verify` round-trip has completed yet (e.g. just after connecting,
    /// before the first poll cycle that includes it).
    Unknown,
    /// The last `Verify` succeeded: the live read-back, plus a drift verdict
    /// computed against the `applied` belief in this same snapshot.
    Available {
        live: LinkDnsState,
        drift: DriftVerdict,
    },
    /// The last `Verify` round-trip failed (daemon down, version skew, or an
    /// unexpected reply). Only this section is degraded; the rest of the snapshot
    /// stays valid.
    Unavailable { message: String },
}
