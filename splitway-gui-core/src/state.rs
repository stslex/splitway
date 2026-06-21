//! `GuiCore` — the framework-agnostic state machine every Splitway GUI drives.
//!
//! This is where the **GUI mutation truth contract** (`docs/architecture.md` §2)
//! is implemented, once, for all frontends. The core renders the daemon's
//! *reported* state and never invents state of its own:
//!
//! - **No optimistic UI.** An intent ([`GuiCore::enable`], [`add_domain`],
//!   [`save_config`], …) only records the request to send; the displayed state
//!   changes solely in [`GuiCore::apply_reply`], from the daemon's reply. A save
//!   is not even marked "synced" until its reply confirms.
//! - **Pending → confirmed-from-refetch.** Every mutation enqueues a refresh
//!   (`Status` + `GetConfig`, plus `ListInterfaces` when the interface set may
//!   have changed); the refetch — not the mutation — updates what is shown.
//! - **Two error kinds, kept distinct.** A transport / version-skew failure goes
//!   to the connection banner ([`note_connection_from`]); an action-level
//!   `Response::Error` (persist-failed, or saved-but-apply-failed as reported by
//!   the daemon) goes to the per-action message.
//! - **Unsaved-edit preservation.** A reconnect/poll refresh never clobbers an
//!   in-progress config edit — the editor buffers are re-adopted only while they
//!   match the last synced snapshot.
//!
//! A frontend owns rendering and the socket round-trip only: it sends the
//! requests [`take_next_request`] hands it, feeds each reply to [`apply_reply`],
//! renders [`view`], and binds its config inputs to [`editor_mut`].
//!
//! [`add_domain`]: GuiCore::add_domain
//! [`save_config`]: GuiCore::save_config
//! [`note_connection_from`]: GuiCore::note_connection_from
//! [`take_next_request`]: GuiCore::take_next_request
//! [`apply_reply`]: GuiCore::apply_reply
//! [`view`]: GuiCore::view
//! [`editor_mut`]: GuiCore::editor_mut

use std::collections::VecDeque;

use splitway_shared::config::VpnBackend;
use splitway_shared::ipc::client::ClientError;
use splitway_shared::ipc::{ConfigView, InterfaceInfo, Request, Response, StatusInfo};

use crate::model::{
    classify_client_error, is_version_mismatch, reduce_action_result, reduce_status_result,
    refresh_requests, validate_config_fields, validate_domain, ConnectionState, Health,
};

/// Severity of a transient, dismissable message shown to the user. A frontend
/// maps this to its own styling (the egui harness picks a colour).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageKind {
    Info,
    Error,
}

/// The editable config buffers a frontend's widgets bind to directly (via
/// [`GuiCore::editor_mut`]). The core owns them so its unsaved-edit tracking can
/// compare them against the last synced [`ConfigSnapshot`]; that comparison is
/// what keeps a reconnect/poll refresh from clobbering an in-progress edit.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConfigEditor {
    /// The configured VPN interface (device) name.
    pub vpn_name: String,
    /// Which VPN detector to use.
    pub backend: VpnBackend,
    /// Standalone-OpenVPN management endpoint (`host:port` or a unix socket path).
    pub openvpn_management: String,
    /// Optional path to the management password file (empty = none).
    pub openvpn_password_file: String,
}

/// Snapshot of the editable config buffers as last synced with the daemon. Used
/// to detect unsaved edits so a reconnect/poll refresh does not clobber them.
#[derive(Clone, PartialEq, Eq)]
struct ConfigSnapshot {
    vpn_name: String,
    backend: VpnBackend,
    management: String,
    password_file: String,
}

/// The read-only projection a frontend renders each frame. Borrowed from the
/// core so rendering is zero-copy. The editable config buffers are *not* here —
/// a frontend mutates those through [`GuiCore::editor_mut`].
pub struct ViewModel<'a> {
    /// The connection banner: health plus an optional message (`None` only when
    /// healthy).
    pub connection: &'a ConnectionState,
    /// `connection.health == Connected` — gates the action buttons.
    pub connected: bool,
    /// A request is in flight — drives a "working…" indicator.
    pub working: bool,
    /// The last trustworthy live status, or `None` when it is not (dropped on a
    /// non-status reply so the toggle/applied state is never shown stale).
    pub status: Option<&'a StatusInfo>,
    /// The host interfaces for the picker.
    pub interfaces: &'a [InterfaceInfo],
    /// Whether a config has loaded — `false` gates a "loading config…"
    /// placeholder.
    pub config_loaded: bool,
    /// The daemon's effective config path (read-only).
    pub config_path: &'a str,
    /// A transient, dismissable message (severity + text).
    pub message: Option<(MessageKind, &'a str)>,
}

/// The framework-agnostic GUI state machine. Holds the connection state, the
/// reconnect-refetch policy, the request queue, reply folding, unsaved-edit
/// preservation, and the view-model — everything a frontend needs apart from
/// rendering and the socket round-trip. No UI-framework references live here.
pub struct GuiCore {
    // --- request dispatch ---
    /// Requests waiting to be sent. The frontend pulls them via
    /// [`GuiCore::take_next_request`]; at most one is in flight at a time
    /// (`inflight`), so the queue serializes follow-up refreshes.
    pending: VecDeque<Request>,
    inflight: bool,

    // --- live status (from Status polls) ---
    status: Option<StatusInfo>,
    /// The host's interfaces, from `ListInterfaces`, populating the picker.
    interfaces: Vec<InterfaceInfo>,
    connection: ConnectionState,
    /// Connection health at the previous poll, to detect a (re)connection edge
    /// and re-fetch the config + interfaces then.
    last_health: Health,

    // --- config editor ---
    /// The editable buffers a frontend's widgets mutate directly.
    editor: ConfigEditor,
    /// The buffers as last synced with the daemon; `None` until the first
    /// successful `GetConfig`. Gates the "loading config…" placeholder and is the
    /// baseline for unsaved-edit detection.
    loaded: Option<ConfigSnapshot>,
    /// The daemon's effective config path (read-only, from `GetConfig`).
    cfg_path: String,

    // --- transient message ---
    message: Option<(MessageKind, String)>,
}

impl Default for GuiCore {
    fn default() -> Self {
        Self::new()
    }
}

impl GuiCore {
    /// Build a fresh core, primed with an initial `Status` request (the frontend
    /// sends it on the first pump). The editable config and the interface list
    /// are fetched on the first connection edge — see [`GuiCore::apply_reply`]'s
    /// `Status` arm — which also covers a client that started while the daemon
    /// was down.
    pub fn new() -> Self {
        let mut core = GuiCore {
            pending: VecDeque::new(),
            inflight: false,
            status: None,
            interfaces: Vec::new(),
            connection: ConnectionState::default(),
            last_health: Health::Unknown,
            editor: ConfigEditor::default(),
            loaded: None,
            cfg_path: String::new(),
            message: None,
        };
        core.enqueue(Request::Status);
        core
    }

    // --- request dispatch -------------------------------------------------

    /// Whether the core is idle — nothing queued and nothing in flight. A
    /// frontend gates its periodic poll on this so polls never stack behind a
    /// slow round-trip.
    pub fn is_idle(&self) -> bool {
        !self.inflight && self.pending.is_empty()
    }

    /// Hand the frontend the next request to send, marking it in flight. Returns
    /// `None` while a request is already in flight (at most one at a time) or the
    /// queue is empty. The frontend performs the actual socket round-trip and
    /// feeds the reply back via [`GuiCore::apply_reply`].
    ///
    /// The in-flight flag stays set until the *matching* [`apply_reply`] clears
    /// it — that is the core's only un-wedge path. A frontend whose send can fail
    /// while the app stays alive must therefore route that failure back through
    /// [`apply_reply`] (the same `request`, with the transport `Err`): that clears
    /// the flag and folds the error back into state — into the connection banner,
    /// and (for a mutation or `GetConfig`) the per-action message too. Dropping a
    /// failed send instead would strand the single in-flight slot and silently
    /// halt all further requests. The egui frontend may drop a failed send only
    /// because its worker channel closes solely on teardown (window closing), so a
    /// wedge can never outlive the process; a frontend without that guarantee (7b)
    /// must feed the error back.
    ///
    /// [`apply_reply`]: GuiCore::apply_reply
    pub fn take_next_request(&mut self) -> Option<Request> {
        if self.inflight {
            return None;
        }
        let request = self.pending.pop_front()?;
        self.inflight = true;
        Some(request)
    }

    /// Enqueue the periodic refresh a frontend's timer drives: `Status` +
    /// `ListInterfaces` always, plus `GetConfig` while the editor is clean so an
    /// external change over the *same* connection (`splitway reload` or another
    /// client's `SetConfig`) is picked up without a reconnect. `GetConfig` is
    /// skipped while there are unsaved edits so the user's in-progress changes
    /// are never clobbered. A frontend calls this only while [`is_idle`].
    ///
    /// [`is_idle`]: GuiCore::is_idle
    pub fn poll(&mut self) {
        // The plain `enqueue` (not `enqueue_unique`) below is correct only on the
        // documented precondition that the caller polls while idle, so a stacked
        // poll can never form a duplicate. That gate now lives in the frontend, a
        // crate away; assert it here so any future driver that polls non-idle
        // trips in debug/tests rather than silently double-queuing.
        debug_assert!(
            self.is_idle(),
            "poll() must be called only while the core is idle (see is_idle)"
        );
        self.enqueue(Request::Status);
        self.enqueue(Request::ListInterfaces);
        // The poll's GetConfig is the in-connection refresh; skip it while the
        // editor is dirty (the reconnect edge always refetches anyway, going
        // through the same dirty guard in `load_config_view`).
        if self.editor_clean() {
            self.enqueue(Request::GetConfig);
        }
    }

    /// Queue a request for the frontend to send.
    fn enqueue(&mut self, request: Request) {
        self.pending.push_back(request);
    }

    /// Queue a request only if an identical one is not already pending, so a
    /// refresh (`Status` / `GetConfig` / `ListInterfaces`) triggered from several
    /// places at once is not double-sent.
    fn enqueue_unique(&mut self, request: Request) {
        if !self.pending.contains(&request) {
            self.pending.push_back(request);
        }
    }

    // --- user intents (no optimistic UI: only enqueue, never mutate the
    //     displayed state) ---------------------------------------------------

    /// Enable rule application. Records the request without flipping the toggle —
    /// the displayed state changes only when the confirming refetch lands.
    pub fn enable(&mut self) {
        self.enqueue(Request::Enable);
    }

    /// Disable rule application and revert. Records the request only.
    pub fn disable(&mut self) {
        self.enqueue(Request::Disable);
    }

    /// Remove a configured domain. The displayed domain list (from `Status`)
    /// updates only after the refetch the reply triggers.
    pub fn remove_domain(&mut self, domain: String) {
        self.enqueue(Request::RemoveDomain(domain));
    }

    /// Resync: ask the daemon to re-read its config and reconcile, then refresh
    /// everything (including the picker). Unsaved edits are kept — the refresh's
    /// `GetConfig` goes through the same dirty guard.
    pub fn reload_config(&mut self) {
        self.enqueue(Request::ReloadConfig);
    }

    /// Validate and submit a new domain. Returns `true` when accepted (the
    /// request was enqueued) so the frontend can clear its input field; on
    /// invalid input it records an error message and returns `false`. The daemon
    /// stays the source of truth (it rejects duplicates and persists); this only
    /// catches obvious garbage early.
    pub fn add_domain(&mut self, raw: &str) -> bool {
        match validate_domain(raw) {
            Ok(domain) => {
                self.enqueue(Request::AddDomain(domain));
                true
            }
            Err(why) => {
                self.message = Some((MessageKind::Error, why));
                false
            }
        }
    }

    /// Validate and submit the editable config buffers as a `SetConfig`. Builds
    /// the wire [`ConfigView`] from the current buffers (trimming, dropping an
    /// empty password file to `None`); on a validation failure it records an
    /// error message and enqueues nothing. No optimistic UI: the editor is not
    /// marked clean until the save's reply confirms (see [`GuiCore::apply_reply`]).
    pub fn save_config(&mut self) {
        let view = self.current_config_view();
        match validate_config_fields(&view) {
            Ok(()) => self.enqueue(Request::SetConfig(view)),
            Err(why) => self.message = Some((MessageKind::Error, why)),
        }
    }

    /// Clear the transient message (the user dismissed it).
    pub fn dismiss_message(&mut self) {
        self.message = None;
    }

    // --- reply folding (the truth contract) -------------------------------

    /// Fold one reply into state. This is the *only* place displayed state
    /// changes — never optimistically from an intent. Pending mutations are
    /// confirmed from the refetch they enqueue; transport/version errors land in
    /// the connection banner while action-level errors land in the per-action
    /// message; the live status is dropped whenever a `Status` round-trip is not
    /// a clean `Status` reply.
    pub fn apply_reply(&mut self, request: Request, result: Result<Response, ClientError>) {
        self.inflight = false;
        match request {
            Request::Status => {
                self.connection = reduce_status_result(&result);
                let now = self.connection.health;
                // (Re)connection edge → (re)fetch the editable config and the
                // interface list, so the editor, the read-only active-config
                // path, and the interface picker are never left stale across a
                // daemon restart (which may even re-point the daemon at a
                // different --config file). The `GetConfig` fold preserves any
                // unsaved edits; `enqueue_unique` guards against re-queuing.
                if now == Health::Connected && self.last_health != Health::Connected {
                    self.enqueue_unique(Request::GetConfig);
                    self.enqueue_unique(Request::ListInterfaces);
                }
                self.last_health = now;
                self.status = match result {
                    Ok(Response::Status(info)) => Some(info),
                    // Any non-status outcome means the live view is no longer
                    // trustworthy; drop it so the toggle/applied state is never
                    // shown stale (e.g. across a restart).
                    _ => None,
                };
            }
            Request::GetConfig => match result {
                Ok(Response::Config(view)) => self.load_config_view(view),
                other => {
                    self.note_connection_from(&other);
                    // A daemon-level error to GetConfig (e.g. the state task is
                    // gone) must not leave the editor silently stuck on "loading
                    // config…" — surface it as a dismissable note. Version skew
                    // is already shown in the connection banner.
                    if let Ok(Response::Error(msg)) = &other {
                        if !is_version_mismatch(msg) {
                            self.message =
                                Some((MessageKind::Error, format!("load config: {msg}")));
                        }
                    }
                }
            },
            // ListInterfaces never changes the interface set, so finishing the
            // domain/enable verbs never refreshes interfaces (`false`).
            Request::Enable => self.finish_action("enable", result, false),
            Request::Disable => self.finish_action("disable", result, false),
            Request::AddDomain(domain) => {
                self.finish_action(&format!("add {domain}"), result, false)
            }
            Request::RemoveDomain(domain) => {
                self.finish_action(&format!("remove {domain}"), result, false)
            }
            Request::SetConfig(_) => {
                let saved = matches!(result, Ok(Response::Ok));
                // A save may change `vpn_name`, so refresh the picker too.
                self.finish_action("save config", result, true);
                if saved {
                    // The save synced the buffers to the daemon; mark them clean
                    // so a later reconnect/poll refresh can adopt any daemon-side
                    // normalization without it being seen as an edit.
                    //
                    // TODO(7c): latent edge, faithfully preserved from the
                    // pre-refactor app.rs — this snapshots the *live* buffers at
                    // reply time, so edits made between clicking Save and the reply
                    // landing are silently marked "synced" and a later refresh
                    // won't restore the daemon's value over them. Out of scope for
                    // this pure refactor; when 7c reworks mutations, snapshot the
                    // `ConfigView` that was actually sent rather than re-reading the
                    // live buffers.
                    self.loaded = Some(self.current_snapshot());
                }
            }
            // Resync: the daemon re-read its config and reconciled; refresh
            // everything, including the picker.
            Request::ReloadConfig => self.finish_action("resync", result, true),
            Request::ListInterfaces => match result {
                Ok(Response::Interfaces(interfaces)) => self.interfaces = interfaces,
                // Keep the last list — the editor's free-text field is the
                // fallback. `note_connection_from` reflects a transport error or
                // version skew into the banner; a daemon-side enumeration failure
                // (`Ok(Response::Error(..))`) is deliberately tolerated in silence
                // rather than surfaced, because this verb is re-polled every
                // interval and a per-poll banner/message would flap.
                other => self.note_connection_from(&other),
            },
            // The GUI never issues these (they belong to the CLI and the
            // future 7b/7c render paths); fold as no-ops.
            Request::ListDomains => {}
            Request::CheckDomain(_) => {}
            Request::Verify => {}
        }
    }

    /// Finish a mutating action (or a resync): record the outcome message,
    /// reflect any connection-level error into the banner, and enqueue the
    /// refresh. `refresh_interfaces` re-fetches the interface list too (a save or
    /// a resync). No state changes optimistically — the refresh it enqueues is
    /// what updates the displayed state once it lands.
    fn finish_action(
        &mut self,
        action: &str,
        result: Result<Response, ClientError>,
        refresh_interfaces: bool,
    ) {
        match reduce_action_result(action, &result) {
            Ok(note) => self.message = Some((MessageKind::Info, note)),
            Err(note) => self.message = Some((MessageKind::Error, note)),
        }
        self.note_connection_from(&result);
        self.refresh_view(refresh_interfaces);
    }

    /// Enqueue the post-mutation / resync refresh: `Status` + `GetConfig`, plus
    /// `ListInterfaces` when the interface set or selection may have changed.
    /// `enqueue_unique` keeps a refresh triggered from several places at once
    /// from being double-sent.
    fn refresh_view(&mut self, include_interfaces: bool) {
        for request in refresh_requests(include_interfaces) {
            self.enqueue_unique(request);
        }
    }

    /// Reflect a non-`Status` reply into the connection banner when it signals a
    /// connection-level problem (transport error or version skew). Action-level
    /// `Response::Error`s (e.g. "domain already present") are left to the
    /// per-action message instead.
    fn note_connection_from(&mut self, result: &Result<Response, ClientError>) {
        let degraded = match result {
            Err(err) => Some(ConnectionState {
                health: classify_client_error(err),
                message: Some(err.to_string()),
            }),
            Ok(Response::Error(msg)) if is_version_mismatch(msg) => Some(ConnectionState {
                health: Health::VersionMismatch,
                message: Some(msg.clone()),
            }),
            _ => None,
        };
        if let Some(state) = degraded {
            // Also lower `last_health` so the reconnect-edge check in the Status
            // arm fires on the next successful poll. Otherwise a daemon that goes
            // down and recovers entirely within one poll interval — its outage
            // seen only by a mutation/GetConfig, never by a Status poll — would
            // leave `last_health == Connected`, skip the config re-fetch, and
            // strand the editor on stale config / a stale active-file path.
            self.last_health = state.health;
            self.connection = state;
        }
    }

    // --- config editor / unsaved-edit tracking ----------------------------

    /// Populate the editor from a freshly fetched config projection. The
    /// read-only active-config path is always refreshed (authoritative, never
    /// user-edited). The editable buffers are repopulated only when they have not
    /// been edited since the last sync, so a reconnect/poll refresh never
    /// clobbers an in-progress edit.
    fn load_config_view(&mut self, view: ConfigView) {
        let path_changed = !self.cfg_path.is_empty() && self.cfg_path != view.config_path;
        self.cfg_path = view.config_path;
        let dirty = self
            .loaded
            .as_ref()
            .is_some_and(|snap| *snap != self.current_snapshot());
        if dirty {
            // Kept the unsaved edits — but if the daemon's active file changed
            // underneath them (a restart against a different --config), warn:
            // Save writes to the daemon's *current* file, so editing values from
            // the old file and saving them onto the new one would be silent and
            // misleading.
            if path_changed {
                self.message = Some((
                    MessageKind::Error,
                    format!(
                        "the daemon's active config file changed to {} while you have unsaved \
                         edits — re-check before saving (Save writes to the daemon's current file)",
                        self.cfg_path
                    ),
                ));
            }
        } else {
            self.editor.vpn_name = view.vpn_name;
            self.editor.backend = view.vpn_backend;
            self.editor.openvpn_management = view.openvpn_management;
            self.editor.openvpn_password_file =
                view.openvpn_management_password_file.unwrap_or_default();
            self.loaded = Some(self.current_snapshot());
        }
    }

    /// Snapshot the current editable buffers, for unsaved-edit detection.
    fn current_snapshot(&self) -> ConfigSnapshot {
        ConfigSnapshot {
            vpn_name: self.editor.vpn_name.clone(),
            backend: self.editor.backend,
            management: self.editor.openvpn_management.clone(),
            password_file: self.editor.openvpn_password_file.clone(),
        }
    }

    /// Build a [`ConfigView`] from the current editor buffers. `config_path` is
    /// left empty: the daemon ignores it (the active path is fixed at launch).
    fn current_config_view(&self) -> ConfigView {
        let password_file = {
            let trimmed = self.editor.openvpn_password_file.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        };
        ConfigView {
            vpn_name: self.editor.vpn_name.trim().to_string(),
            vpn_backend: self.editor.backend,
            openvpn_management: self.editor.openvpn_management.trim().to_string(),
            openvpn_management_password_file: password_file,
            config_path: String::new(),
        }
    }

    /// Whether the config editor has loaded and has no unsaved edits — i.e. it is
    /// safe to adopt a fresh `GetConfig` without clobbering the user.
    pub fn editor_clean(&self) -> bool {
        self.loaded
            .as_ref()
            .is_some_and(|snap| *snap == self.current_snapshot())
    }

    // --- read surface -----------------------------------------------------

    /// The read-only view-model a frontend renders this frame.
    pub fn view(&self) -> ViewModel<'_> {
        ViewModel {
            connection: &self.connection,
            connected: self.connection.health == Health::Connected,
            working: self.inflight,
            status: self.status.as_ref(),
            interfaces: &self.interfaces,
            config_loaded: self.loaded.is_some(),
            config_path: &self.cfg_path,
            message: self
                .message
                .as_ref()
                .map(|(kind, text)| (*kind, text.as_str())),
        }
    }

    /// The editable config buffers, for reading (e.g. to build the interface
    /// picker from the current `vpn_name`).
    pub fn editor(&self) -> &ConfigEditor {
        &self.editor
    }

    /// The editable config buffers, for a frontend to bind its inputs to. The
    /// core detects edits by comparing these against the last synced snapshot
    /// (see [`GuiCore::editor_clean`]), so a frontend mutates them freely.
    pub fn editor_mut(&mut self) -> &mut ConfigEditor {
        &mut self.editor
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use splitway_shared::ipc::VERSION_MISMATCH_PREFIX;
    use splitway_shared::ipc::{DetectorHealth, RoutingState};
    use std::io;

    // --- fixtures ---------------------------------------------------------

    fn status(enabled: bool, domains: &[&str]) -> StatusInfo {
        StatusInfo {
            enabled,
            interface: "tun0".to_string(),
            vpn_up: true,
            applied: None,
            routing_state: RoutingState::VpnDown,
            detector_health: DetectorHealth::Active,
            domains: domains.iter().map(|d| d.to_string()).collect(),
        }
    }

    fn status_reply(enabled: bool, domains: &[&str]) -> Result<Response, ClientError> {
        Ok(Response::Status(status(enabled, domains)))
    }

    fn config_view(vpn_name: &str) -> ConfigView {
        ConfigView {
            vpn_name: vpn_name.to_string(),
            vpn_backend: VpnBackend::NetworkManager,
            openvpn_management: String::new(),
            openvpn_management_password_file: None,
            config_path: "/etc/splitway/config.json".to_string(),
        }
    }

    fn config_reply(vpn_name: &str) -> Result<Response, ClientError> {
        Ok(Response::Config(config_view(vpn_name)))
    }

    fn not_running() -> Result<Response, ClientError> {
        Err(ClientError::NotRunning(io::Error::new(
            io::ErrorKind::NotFound,
            "test",
        )))
    }

    fn version_mismatch_msg() -> String {
        format!("{VERSION_MISMATCH_PREFIX}: daemon speaks 7, client speaks 6 — update splitway")
    }

    /// Drive the core to a clean `Connected` state with config + interfaces
    /// loaded, draining the reconnect-edge refetch it enqueues. Leaves the queue
    /// empty and `inflight` false. Returns the core ready for a scenario.
    fn connected_core(vpn_name: &str) -> GuiCore {
        let mut core = GuiCore::new();
        // The primed Status request is the first thing the frontend would send.
        assert_eq!(core.take_next_request(), Some(Request::Status));
        core.apply_reply(Request::Status, status_reply(true, &["a.com"]));
        // Reconnect edge from Unknown → Connected enqueued the config + iface
        // refetch; satisfy them so the core is idle and the editor is loaded.
        assert_eq!(core.take_next_request(), Some(Request::GetConfig));
        core.apply_reply(Request::GetConfig, config_reply(vpn_name));
        assert_eq!(core.take_next_request(), Some(Request::ListInterfaces));
        core.apply_reply(
            Request::ListInterfaces,
            Ok(Response::Interfaces(vec![InterfaceInfo {
                name: "tun0".to_string(),
                up: true,
                vpn_like: true,
            }])),
        );
        assert!(core.is_idle());
        assert!(core.editor_clean());
        core
    }

    // --- dispatch ---------------------------------------------------------

    #[test]
    fn primes_an_initial_status_request() {
        let mut core = GuiCore::new();
        assert_eq!(core.take_next_request(), Some(Request::Status));
    }

    #[test]
    fn serializes_at_most_one_request_in_flight() {
        let mut core = GuiCore::new();
        // Status is primed; first take hands it out and marks in flight.
        assert_eq!(core.take_next_request(), Some(Request::Status));
        // A second take returns nothing while a request is in flight, even if
        // more were queued.
        core.enqueue(Request::ListInterfaces);
        assert_eq!(core.take_next_request(), None);
        assert!(!core.is_idle());
        // The reply clears in flight, so the next queued request can go.
        core.apply_reply(Request::Status, status_reply(true, &[]));
        // The Unknown→Connected edge also queued GetConfig + ListInterfaces; the
        // exact next is GetConfig (enqueued before the pre-queued ListInterfaces
        // is deduped away by enqueue_unique). What matters: a request is handed
        // out and only one is in flight at a time.
        assert!(core.take_next_request().is_some());
        assert_eq!(core.take_next_request(), None);
    }

    // --- reconnect edge ---------------------------------------------------

    #[test]
    fn reconnect_edge_refetches_once_per_edge() {
        let mut core = GuiCore::new();
        let _ = core.take_next_request();

        // First Connected (from Unknown) is an edge → refetch config + ifaces.
        core.apply_reply(Request::Status, status_reply(true, &[]));
        assert!(core.pending.contains(&Request::GetConfig));
        assert!(core.pending.contains(&Request::ListInterfaces));
        core.pending.clear();

        // A second Connected status is NOT an edge → no refetch enqueued.
        core.apply_reply(Request::Status, status_reply(true, &[]));
        assert!(core.pending.is_empty());

        // Drop to NotRunning, then back to Connected: that *is* a fresh edge.
        core.apply_reply(Request::Status, not_running());
        assert_eq!(core.connection.health, Health::NotRunning);
        assert!(core.pending.is_empty());
        core.apply_reply(Request::Status, status_reply(true, &[]));
        assert!(core.pending.contains(&Request::GetConfig));
        assert!(core.pending.contains(&Request::ListInterfaces));
    }

    #[test]
    fn within_interval_outage_seen_only_by_a_mutation_still_refetches_on_recovery() {
        let mut core = connected_core("tun0");
        // An outage observed only by a mutation/GetConfig (never by a Status
        // poll) lowers last_health, so the next successful Status is an edge.
        core.apply_reply(Request::GetConfig, not_running());
        assert_eq!(core.connection.health, Health::NotRunning);
        core.pending.clear();
        core.apply_reply(Request::Status, status_reply(true, &[]));
        assert!(core.pending.contains(&Request::GetConfig));
        assert!(core.pending.contains(&Request::ListInterfaces));
    }

    // --- unsaved-edit preservation ---------------------------------------

    #[test]
    fn poll_skips_getconfig_while_the_editor_is_dirty() {
        let mut core = connected_core("tun0");
        // User edits the interface name → editor is dirty.
        core.editor_mut().vpn_name = "tun9".to_string();
        assert!(!core.editor_clean());

        core.poll();
        assert!(core.pending.contains(&Request::Status));
        assert!(core.pending.contains(&Request::ListInterfaces));
        // The in-connection GetConfig refresh is skipped so it cannot clobber.
        assert!(!core.pending.contains(&Request::GetConfig));

        // When clean again, the poll does include GetConfig.
        core.pending.clear();
        core.editor_mut().vpn_name = "tun0".to_string();
        assert!(core.editor_clean());
        core.poll();
        assert!(core.pending.contains(&Request::GetConfig));
    }

    #[test]
    fn a_refresh_does_not_clobber_an_unsaved_edit() {
        let mut core = connected_core("tun0");
        core.editor_mut().vpn_name = "tun9".to_string();

        // A GetConfig refresh arrives anyway (e.g. from a reconnect edge, which
        // always refetches). It must NOT overwrite the in-progress edit.
        core.apply_reply(Request::GetConfig, config_reply("tun0"));
        assert_eq!(core.editor().vpn_name, "tun9");
        // Still dirty, so a later save still sends the user's value.
        assert!(!core.editor_clean());
    }

    #[test]
    fn a_refresh_is_adopted_while_the_editor_is_clean() {
        let mut core = connected_core("tun0");
        assert!(core.editor_clean());
        // The daemon's value changed (another client saved); a clean editor
        // adopts it.
        core.apply_reply(Request::GetConfig, config_reply("wg1"));
        assert_eq!(core.editor().vpn_name, "wg1");
        assert!(core.editor_clean());
    }

    #[test]
    fn active_path_change_under_an_unsaved_edit_warns() {
        let mut core = connected_core("tun0");
        core.editor_mut().vpn_name = "tun9".to_string();
        // The daemon restarted against a different --config file.
        let mut view = config_view("tun0");
        view.config_path = "/etc/splitway/other.json".to_string();
        core.apply_reply(Request::GetConfig, Ok(Response::Config(view)));
        // The path is always refreshed (authoritative)…
        assert_eq!(core.view().config_path, "/etc/splitway/other.json");
        // …the edit is kept…
        assert_eq!(core.editor().vpn_name, "tun9");
        // …and the user is warned that Save targets the new file.
        let (kind, text) = core.view().message.unwrap();
        assert_eq!(kind, MessageKind::Error);
        assert!(text.contains("active config file changed"));
    }

    // --- finish_action folding -------------------------------------------

    #[test]
    fn finish_action_folds_success_into_an_info_message_and_a_refresh() {
        let mut core = connected_core("tun0");
        core.pending.clear();
        core.apply_reply(Request::Enable, Ok(Response::Ok));
        let (kind, text) = core.view().message.unwrap();
        assert_eq!(kind, MessageKind::Info);
        assert_eq!(text, "enable: done");
        // Enable refreshes Status + GetConfig but not interfaces.
        assert!(core.pending.contains(&Request::Status));
        assert!(core.pending.contains(&Request::GetConfig));
        assert!(!core.pending.contains(&Request::ListInterfaces));
    }

    #[test]
    fn finish_action_folds_an_action_error_into_an_error_message_not_the_banner() {
        let mut core = connected_core("tun0");
        core.apply_reply(
            Request::AddDomain("a.com".to_string()),
            Ok(Response::Error("domain already present: a.com".to_string())),
        );
        let (kind, text) = core.view().message.unwrap();
        assert_eq!(kind, MessageKind::Error);
        assert_eq!(text, "add a.com: domain already present: a.com");
        // An action-level error is NOT a connection problem: the banner stays
        // Connected.
        assert_eq!(core.view().connection.health, Health::Connected);
    }

    #[test]
    fn save_config_refreshes_interfaces_and_marks_clean_only_on_success() {
        let mut core = connected_core("tun0");
        core.editor_mut().vpn_name = "tun9".to_string();
        assert!(!core.editor_clean());

        // A failed save keeps the editor dirty (no optimistic "synced").
        core.apply_reply(
            Request::SetConfig(config_view("tun9")),
            Ok(Response::Error("persist failed".to_string())),
        );
        assert!(!core.editor_clean());
        assert_eq!(core.view().message.unwrap().0, MessageKind::Error);

        // A successful save marks the buffers clean and refreshes the picker.
        core.pending.clear();
        core.apply_reply(Request::SetConfig(config_view("tun9")), Ok(Response::Ok));
        assert!(core.editor_clean());
        assert!(core.pending.contains(&Request::ListInterfaces));
    }

    // --- version mismatch -------------------------------------------------

    #[test]
    fn version_mismatch_status_reply_sets_the_banner_and_drops_status() {
        let mut core = connected_core("tun0");
        core.apply_reply(Request::Status, Ok(Response::Error(version_mismatch_msg())));
        assert_eq!(core.view().connection.health, Health::VersionMismatch);
        assert!(core.view().status.is_none());
    }

    #[test]
    fn version_mismatch_on_getconfig_sets_the_banner_without_a_load_error_message() {
        let mut core = connected_core("tun0");
        core.apply_reply(
            Request::GetConfig,
            Ok(Response::Error(version_mismatch_msg())),
        );
        assert_eq!(core.view().connection.health, Health::VersionMismatch);
        // Version skew is shown by the banner only — not duplicated as a
        // "load config: …" message.
        assert!(core.view().message.is_none());
    }

    #[test]
    fn getconfig_daemon_error_surfaces_as_a_message() {
        let mut core = connected_core("tun0");
        core.apply_reply(
            Request::GetConfig,
            Ok(Response::Error("state task is gone".to_string())),
        );
        let (kind, text) = core.view().message.unwrap();
        assert_eq!(kind, MessageKind::Error);
        assert_eq!(text, "load config: state task is gone");
        // A daemon-side error is not a transport/version problem, so the banner
        // is untouched.
        assert_eq!(core.view().connection.health, Health::Connected);
    }

    #[test]
    fn listinterfaces_daemon_error_is_silent() {
        let mut core = connected_core("tun0");
        let before = core.view().interfaces.len();
        core.apply_reply(
            Request::ListInterfaces,
            Ok(Response::Error("enumeration failed".to_string())),
        );
        // The last list is kept, nothing is surfaced, the banner is untouched.
        assert_eq!(core.view().interfaces.len(), before);
        assert!(core.view().message.is_none());
        assert_eq!(core.view().connection.health, Health::Connected);
    }

    // --- no optimistic UI -------------------------------------------------

    #[test]
    fn intents_never_mutate_displayed_state_before_a_reply() {
        let mut core = connected_core("tun0");
        let domains_before = core.view().status.unwrap().domains.clone();
        let enabled_before = core.view().status.unwrap().enabled;

        core.disable();
        core.remove_domain("a.com".to_string());
        core.add_domain("new.example.com");

        // The displayed status is unchanged — only requests were queued.
        assert_eq!(core.view().status.unwrap().enabled, enabled_before);
        assert_eq!(core.view().status.unwrap().domains, domains_before);
        assert!(core.pending.contains(&Request::Disable));
        assert!(core
            .pending
            .contains(&Request::RemoveDomain("a.com".to_string())));
        assert!(core
            .pending
            .contains(&Request::AddDomain("new.example.com".to_string())));
    }

    #[test]
    fn add_domain_validates_and_reports_without_enqueuing_garbage() {
        let mut core = connected_core("tun0");
        core.pending.clear();

        assert!(core.add_domain("  ok.example.com "));
        assert!(core
            .pending
            .contains(&Request::AddDomain("ok.example.com".to_string())));

        core.pending.clear();
        assert!(!core.add_domain("bad domain"));
        assert!(core.pending.is_empty());
        assert_eq!(core.view().message.unwrap().0, MessageKind::Error);
    }

    #[test]
    fn save_config_rejects_openvpn_without_a_management_endpoint() {
        let mut core = connected_core("tun0");
        core.editor_mut().backend = VpnBackend::OpenVpn;
        core.editor_mut().openvpn_management = String::new();
        core.pending.clear();

        core.save_config();
        // Nothing sent; the validation error is shown.
        assert!(core.pending.is_empty());
        assert_eq!(core.view().message.unwrap().0, MessageKind::Error);
    }
}
