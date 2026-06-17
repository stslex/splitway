//! The egui front-end: a pure IPC client over the daemon's control socket.
//! It builds every action as a [`Request`] and renders every [`Response`],
//! exactly like `splitway-cli` — it holds no privileges, writes no config file
//! itself, and knows no daemon types beyond `splitway-shared::ipc`.
//!
//! This module is thin plumbing (rendering + request dispatch). All decisions
//! it relies on — error classification, validation, the connection reducer —
//! live in `model.rs` and are unit-tested there.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender};
use std::time::{Duration, Instant};

use eframe::egui;

use splitway_shared::config::VpnBackend;
use splitway_shared::ipc::{ConfigView, Request, Response, StatusInfo};

use crate::model::{
    self, classify_client_error, interface_change_needs_restart, reduce_action_result,
    reduce_status_result, ConnectionState, Health,
};
use crate::worker::{self, Job, Reply};

/// How often the UI re-polls `Status` so the toggle/applied/vpn_up display
/// stays live without a push channel (the protocol has none).
const POLL_INTERVAL: Duration = Duration::from_millis(1500);

/// Severity of a transient, dismissable message shown to the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MessageKind {
    Info,
    Error,
}

/// Snapshot of the editable config buffers as last synced with the daemon. Used
/// to detect unsaved edits so a reconnect refresh does not clobber them.
#[derive(Clone, PartialEq, Eq)]
struct ConfigSnapshot {
    vpn_name: String,
    backend: VpnBackend,
    management: String,
    password_file: String,
}

/// Run the GUI event loop. Blocks until the window is closed.
pub fn run() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([540.0, 720.0])
            .with_min_inner_size([420.0, 480.0])
            .with_title("Splitway"),
        ..Default::default()
    };
    eframe::run_native(
        "Splitway",
        options,
        Box::new(|cc| Ok(Box::new(SplitwayApp::new(cc)))),
    )
}

struct SplitwayApp {
    // --- IPC plumbing ---
    job_tx: Sender<Job>,
    reply_rx: Receiver<Reply>,
    /// Requests waiting for a free slot. At most one request is in flight at a
    /// time (`inflight`), so the queue serializes follow-up refreshes.
    pending: VecDeque<Request>,
    inflight: bool,
    last_poll: Instant,

    // --- live status (from Status polls) ---
    status: Option<StatusInfo>,
    connection: ConnectionState,
    /// Connection health at the previous poll, to detect a (re)connection edge
    /// and re-fetch the config then.
    last_health: Health,

    // --- domain editing ---
    new_domain: String,

    // --- config editor (buffers populated from GetConfig) ---
    /// The editable buffers as last synced with the daemon; `None` until the
    /// first successful GetConfig. Also gates the "loading config…" placeholder.
    loaded: Option<ConfigSnapshot>,
    cfg_vpn_name: String,
    cfg_backend: VpnBackend,
    cfg_openvpn_management: String,
    cfg_openvpn_password_file: String,
    /// The daemon's effective config path (read-only, from GetConfig).
    cfg_path: String,
    /// A file the user picked to *launch* a daemon against (runtime switching
    /// is deferred), shown as a launch hint.
    picked_path: Option<PathBuf>,

    // --- transient message ---
    message: Option<(MessageKind, String)>,
}

impl SplitwayApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let (job_tx, reply_rx) = worker::spawn(cc.egui_ctx.clone());
        let mut app = SplitwayApp {
            job_tx,
            reply_rx,
            pending: VecDeque::new(),
            inflight: false,
            last_poll: Instant::now(),
            status: None,
            connection: ConnectionState::default(),
            last_health: Health::Unknown,
            new_domain: String::new(),
            loaded: None,
            cfg_vpn_name: String::new(),
            cfg_backend: VpnBackend::NetworkManager,
            cfg_openvpn_management: String::new(),
            cfg_openvpn_password_file: String::new(),
            cfg_path: String::new(),
            picked_path: None,
            message: None,
        };
        // Prime the view with a status poll. The editable config is fetched on
        // the first (and every later) connection edge — see `drain_replies`.
        app.enqueue(Request::Status);
        app
    }

    /// Queue a request for the worker. The actual send happens in `update`,
    /// gated on a free in-flight slot.
    fn enqueue(&mut self, request: Request) {
        self.pending.push_back(request);
    }

    /// Send the next queued request if no request is in flight.
    fn pump(&mut self) {
        if self.inflight {
            return;
        }
        if let Some(request) = self.pending.pop_front() {
            // If the worker is gone the window is closing; nothing to do.
            if self.job_tx.send(Job { request }).is_ok() {
                self.inflight = true;
            }
        }
    }

    /// Drain all replies that have arrived since the last frame.
    fn drain_replies(&mut self) {
        while let Ok(reply) = self.reply_rx.try_recv() {
            self.inflight = false;
            match reply.request {
                Request::Status => {
                    self.connection = reduce_status_result(&reply.result);
                    let now = self.connection.health;
                    // (Re)connection edge → (re)fetch the editable config, so the
                    // editor and the read-only active-config path are never left
                    // stale across a daemon restart (which may even re-point the
                    // daemon at a different --config file). This also covers a GUI
                    // that started while the daemon was down. `load_config_view`
                    // preserves any unsaved edits. Guarded against re-queuing.
                    if now == Health::Connected
                        && self.last_health != Health::Connected
                        && !self.pending.iter().any(|r| matches!(r, Request::GetConfig))
                    {
                        self.enqueue(Request::GetConfig);
                    }
                    self.last_health = now;
                    self.status = match reply.result {
                        Ok(Response::Status(info)) => Some(info),
                        // Any non-status outcome means the live view is no
                        // longer trustworthy; drop it so the toggle/applied
                        // state is never shown stale (e.g. across a restart).
                        _ => None,
                    };
                }
                Request::GetConfig => match reply.result {
                    Ok(Response::Config(view)) => self.load_config_view(view),
                    other => {
                        self.note_connection_from(&other);
                        // A daemon-level error to GetConfig (e.g. the state task
                        // is gone) must not leave the editor silently stuck on
                        // "loading config…" — surface it as a dismissable note.
                        // Version skew is already shown in the connection banner.
                        if let Ok(Response::Error(msg)) = &other {
                            if !model::is_version_mismatch(msg) {
                                self.message =
                                    Some((MessageKind::Error, format!("load config: {msg}")));
                            }
                        }
                    }
                },
                Request::Enable => self.finish_action("enable", reply.result),
                Request::Disable => self.finish_action("disable", reply.result),
                Request::AddDomain(domain) => {
                    self.finish_action(&format!("add {domain}"), reply.result)
                }
                Request::RemoveDomain(domain) => {
                    self.finish_action(&format!("remove {domain}"), reply.result)
                }
                Request::SetConfig(_) => {
                    let saved = matches!(reply.result, Ok(Response::Ok));
                    self.finish_action("save config", reply.result);
                    if saved {
                        // The save synced the buffers to the daemon; mark them
                        // clean so a later reconnect refresh can adopt any
                        // daemon-side normalization without being seen as edits.
                        self.loaded = Some(self.current_snapshot());
                    }
                }
                // The GUI never issues these.
                Request::ListDomains | Request::ReloadConfig => {}
            }
        }
    }

    /// Finish a mutating action: record the outcome message, reflect any
    /// connection-level error into the banner, and refresh `Status`.
    fn finish_action(&mut self, action: &str, result: Result<Response, ClientResult>) {
        match reduce_action_result(action, &result) {
            Ok(note) => self.message = Some((MessageKind::Info, note)),
            Err(note) => self.message = Some((MessageKind::Error, note)),
        }
        self.note_connection_from(&result);
        self.enqueue(Request::Status);
    }

    /// Reflect a non-`Status` reply into the connection banner when it signals a
    /// connection-level problem (transport error or version skew). Action-level
    /// `Response::Error`s (e.g. "domain already present") are left to the
    /// per-action message instead.
    fn note_connection_from(&mut self, result: &Result<Response, ClientResult>) {
        let degraded = match result {
            Err(err) => Some(ConnectionState {
                health: classify_client_error(err),
                message: Some(err.to_string()),
            }),
            Ok(Response::Error(msg)) if model::is_version_mismatch(msg) => Some(ConnectionState {
                health: Health::VersionMismatch,
                message: Some(msg.clone()),
            }),
            _ => None,
        };
        if let Some(state) = degraded {
            // Also lower `last_health` so the reconnect-edge check in the Status
            // arm fires on the next successful poll. Otherwise a daemon that
            // goes down and recovers entirely within one poll interval — its
            // outage seen only by a mutation/GetConfig, never by a Status poll —
            // would leave `last_health == Connected`, skip the config re-fetch,
            // and strand the editor on stale config / a stale active-file path.
            self.last_health = state.health;
            self.connection = state;
        }
    }

    /// Populate the editor from a freshly fetched config projection. The
    /// read-only active-config path is always refreshed (it is authoritative and
    /// never user-edited). The editable buffers are repopulated only when they
    /// have not been edited since the last sync, so a reconnect refresh never
    /// clobbers an in-progress edit.
    fn load_config_view(&mut self, view: ConfigView) {
        self.cfg_path = view.config_path;
        let dirty = self
            .loaded
            .as_ref()
            .is_some_and(|snap| *snap != self.current_snapshot());
        if !dirty {
            self.cfg_vpn_name = view.vpn_name;
            self.cfg_backend = view.vpn_backend;
            self.cfg_openvpn_management = view.openvpn_management;
            self.cfg_openvpn_password_file =
                view.openvpn_management_password_file.unwrap_or_default();
            self.loaded = Some(self.current_snapshot());
        }
    }

    /// Snapshot the current editable buffers, for unsaved-edit detection.
    fn current_snapshot(&self) -> ConfigSnapshot {
        ConfigSnapshot {
            vpn_name: self.cfg_vpn_name.clone(),
            backend: self.cfg_backend,
            management: self.cfg_openvpn_management.clone(),
            password_file: self.cfg_openvpn_password_file.clone(),
        }
    }

    /// Build a [`ConfigView`] from the current editor buffers. `config_path` is
    /// left empty: the daemon ignores it (the active path is fixed at launch).
    fn current_config_view(&self) -> ConfigView {
        let password_file = {
            let trimmed = self.cfg_openvpn_password_file.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        };
        ConfigView {
            vpn_name: self.cfg_vpn_name.trim().to_string(),
            vpn_backend: self.cfg_backend,
            openvpn_management: self.cfg_openvpn_management.trim().to_string(),
            openvpn_management_password_file: password_file,
            config_path: String::new(),
        }
    }

    fn connected(&self) -> bool {
        self.connection.health == Health::Connected
    }
}

/// Local alias for the worker's result error type, to keep signatures short.
type ClientResult = splitway_shared::ipc::client::ClientError;

impl eframe::App for SplitwayApp {
    // eframe 0.34 hands the root `Ui` directly (no margin/background) and
    // deprecates the old `update(ctx, frame)`.
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.drain_replies();

        // Time-based re-poll: only when idle (no queue, nothing in flight) so
        // polls never stack behind a slow round-trip.
        if self.pending.is_empty() && !self.inflight && self.last_poll.elapsed() >= POLL_INTERVAL {
            self.enqueue(Request::Status);
            self.last_poll = Instant::now();
        }
        self.pump();

        egui::ScrollArea::vertical().show(ui, |ui| {
            self.ui_header(ui);
            ui.separator();
            self.ui_status_and_toggle(ui);
            ui.separator();
            self.ui_domains(ui);
            ui.separator();
            self.ui_config_editor(ui);
            ui.separator();
            self.ui_config_file(ui);
            self.ui_message(ui);
        });

        // Keep the poll timer ticking even when the window is idle.
        ui.ctx().request_repaint_after(POLL_INTERVAL);
    }
}

impl SplitwayApp {
    fn ui_header(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading("Splitway");
            if self.inflight {
                ui.add(egui::Spinner::new());
                ui.label("working…");
            }
        });

        let (color, text) = match self.connection.health {
            Health::Connected => (
                egui::Color32::from_rgb(60, 160, 60),
                "Connected".to_string(),
            ),
            Health::Unknown => (egui::Color32::GRAY, "Connecting…".to_string()),
            Health::NotRunning => (
                egui::Color32::from_rgb(200, 140, 0),
                "Daemon not running".to_string(),
            ),
            Health::PermissionDenied => (
                egui::Color32::from_rgb(200, 60, 60),
                "Permission denied".to_string(),
            ),
            Health::VersionMismatch => (
                egui::Color32::from_rgb(200, 60, 60),
                "Version mismatch".to_string(),
            ),
            Health::TransientError => (egui::Color32::from_rgb(200, 140, 0), "Error".to_string()),
        };
        ui.colored_label(color, format!("● {text}"));
        if let Some(msg) = &self.connection.message {
            // Reuse the client/daemon guidance verbatim (e.g. the
            // PermissionDenied "run as the daemon's user/group" note, or the
            // "update splitway" version-skew message).
            ui.label(msg);
        }
    }

    fn ui_status_and_toggle(&mut self, ui: &mut egui::Ui) {
        ui.label(egui::RichText::new("Status").strong());
        match &self.status {
            Some(info) => {
                egui::Grid::new("status_grid")
                    .num_columns(2)
                    .spacing([12.0, 4.0])
                    .show(ui, |ui| {
                        ui.label("enabled");
                        ui.label(info.enabled.to_string());
                        ui.end_row();
                        ui.label("interface");
                        ui.label(if info.interface.is_empty() {
                            "(unset)".to_string()
                        } else {
                            info.interface.clone()
                        });
                        ui.end_row();
                        ui.label("vpn up");
                        ui.label(info.vpn_up.to_string());
                        ui.end_row();
                        ui.label("rules applied");
                        ui.label(info.applied.to_string());
                        ui.end_row();
                        ui.label("domains");
                        ui.label(info.domains.len().to_string());
                        ui.end_row();
                    });

                let enabled_now = info.enabled;
                ui.add_enabled_ui(self.connected(), |ui| {
                    if enabled_now {
                        if ui.button("Disable").clicked() {
                            self.enqueue(Request::Disable);
                        }
                    } else if ui.button("Enable").clicked() {
                        self.enqueue(Request::Enable);
                    }
                });
            }
            None => {
                ui.label("status unavailable — the daemon is not reachable");
            }
        }
    }

    fn ui_domains(&mut self, ui: &mut egui::Ui) {
        ui.label(egui::RichText::new("Domains").strong());
        let domains = self
            .status
            .as_ref()
            .map(|s| s.domains.clone())
            .unwrap_or_default();
        let connected = self.connected();

        if domains.is_empty() {
            ui.label("(no domains configured)");
        } else {
            for domain in &domains {
                ui.horizontal(|ui| {
                    ui.add_enabled_ui(connected, |ui| {
                        if ui.small_button("✖").clicked() {
                            self.enqueue(Request::RemoveDomain(domain.clone()));
                        }
                    });
                    ui.label(domain);
                });
            }
        }

        ui.horizontal(|ui| {
            ui.text_edit_singleline(&mut self.new_domain);
            ui.add_enabled_ui(connected, |ui| {
                if ui.button("Add").clicked() {
                    self.submit_add_domain();
                }
            });
        });
    }

    fn submit_add_domain(&mut self) {
        match model::validate_domain(&self.new_domain) {
            Ok(domain) => {
                self.new_domain.clear();
                self.enqueue(Request::AddDomain(domain));
            }
            Err(why) => self.message = Some((MessageKind::Error, why)),
        }
    }

    fn ui_config_editor(&mut self, ui: &mut egui::Ui) {
        ui.label(egui::RichText::new("Configuration").strong());
        if self.loaded.is_none() {
            ui.label("loading config…");
            return;
        }

        let live_interface = self
            .status
            .as_ref()
            .map(|s| s.interface.clone())
            .unwrap_or_default();

        egui::Grid::new("config_grid")
            .num_columns(2)
            .spacing([12.0, 6.0])
            .show(ui, |ui| {
                ui.label("vpn_name");
                ui.text_edit_singleline(&mut self.cfg_vpn_name);
                ui.end_row();

                ui.label("vpn_backend");
                egui::ComboBox::from_id_salt("vpn_backend")
                    .selected_text(backend_label(self.cfg_backend))
                    .show_ui(ui, |ui| {
                        ui.selectable_value(
                            &mut self.cfg_backend,
                            VpnBackend::NetworkManager,
                            backend_label(VpnBackend::NetworkManager),
                        );
                        ui.selectable_value(
                            &mut self.cfg_backend,
                            VpnBackend::OpenVpn,
                            backend_label(VpnBackend::OpenVpn),
                        );
                    });
                ui.end_row();

                ui.label("openvpn.management");
                ui.add_enabled(
                    self.cfg_backend == VpnBackend::OpenVpn,
                    egui::TextEdit::singleline(&mut self.cfg_openvpn_management)
                        .hint_text("127.0.0.1:7505 or /run/openvpn/mgmt.sock"),
                );
                ui.end_row();

                ui.label("openvpn.management_password_file");
                ui.add_enabled(
                    self.cfg_backend == VpnBackend::OpenVpn,
                    egui::TextEdit::singleline(&mut self.cfg_openvpn_password_file)
                        .hint_text("(optional)"),
                );
                ui.end_row();
            });

        // Always surface the restart caveat rather than letting a save imply it
        // took full effect: the detector watch is armed once at startup and is
        // not restarted on a live config change. (`StatusInfo.interface` is the
        // *configured* name, which a save updates immediately, so this is shown
        // unconditionally — a save alone never re-arms the watch.)
        ui.colored_label(
            egui::Color32::from_rgb(150, 150, 150),
            "Note: changing vpn_name or vpn_backend takes effect for auto-apply only after a \
             daemon restart (the interface watch is armed once at startup).",
        );
        // While there is an unsaved interface change, call it out more strongly.
        let interface_changed = !live_interface.is_empty()
            && interface_change_needs_restart(&self.cfg_vpn_name, &live_interface);
        if interface_changed {
            ui.colored_label(
                egui::Color32::from_rgb(200, 140, 0),
                "Unsaved change: vpn_name differs from the daemon's active interface — save, \
                 then restart the daemon to auto-apply on the new interface.",
            );
        }

        ui.add_enabled_ui(self.connected(), |ui| {
            if ui.button("Save configuration").clicked() {
                self.submit_set_config();
            }
        });
    }

    fn submit_set_config(&mut self) {
        let view = self.current_config_view();
        match model::validate_config_fields(&view) {
            Ok(()) => self.enqueue(Request::SetConfig(view)),
            Err(why) => self.message = Some((MessageKind::Error, why)),
        }
    }

    fn ui_config_file(&mut self, ui: &mut egui::Ui) {
        ui.label(egui::RichText::new("Config file").strong());
        ui.horizontal(|ui| {
            ui.label("active:");
            ui.monospace(if self.cfg_path.is_empty() {
                "(unknown)"
            } else {
                &self.cfg_path
            });
        });

        if ui.button("Choose a file…").clicked() {
            if let Some(path) = rfd::FileDialog::new()
                .set_title("Select a splitway config file")
                .add_filter("JSON", &["json"])
                .pick_file()
            {
                self.picked_path = Some(path);
            }
        }

        // Runtime switching of the daemon's active file is deferred (see the
        // PR): the GUI edits the daemon's *current* file and cannot repoint it
        // live. A picked file becomes a launch hint instead.
        if let Some(path) = &self.picked_path {
            ui.label(
                "Runtime switching isn't supported yet. To use this file, restart the daemon \
                 with:",
            );
            // Subcommand first, matching the daemon's parser (`run --config …`);
            // `--config` before the subcommand is rejected.
            ui.monospace(format!("splitway-daemon run --config {}", path.display()));
        }
    }

    fn ui_message(&mut self, ui: &mut egui::Ui) {
        let Some((kind, text)) = self.message.clone() else {
            return;
        };
        ui.separator();
        ui.horizontal(|ui| {
            let color = match kind {
                MessageKind::Info => egui::Color32::from_rgb(60, 160, 60),
                MessageKind::Error => egui::Color32::from_rgb(200, 60, 60),
            };
            ui.colored_label(color, &text);
            if ui.small_button("dismiss").clicked() {
                self.message = None;
            }
        });
    }
}

fn backend_label(backend: VpnBackend) -> &'static str {
    match backend {
        VpnBackend::NetworkManager => "network-manager",
        VpnBackend::OpenVpn => "openvpn",
    }
}
