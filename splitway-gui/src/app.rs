//! The egui front-end: a thin renderer + socket-plumbing client over the
//! daemon's control socket. It holds **no** truth-contract state of its own —
//! all of that lives in [`splitway_gui_core::GuiCore`], which this module
//! drives: it feeds each worker reply to [`GuiCore::apply_reply`], renders
//! [`GuiCore::view`], binds the config inputs to [`GuiCore::editor_mut`], and
//! sends exactly the requests [`GuiCore::take_next_request`] hands it.
//!
//! Everything here is rendering + plumbing: which widget paints what, the
//! periodic-poll *timing*, the file-picker, and the worker channel. The
//! decisions (error classification, validation, the connection reducer, reply
//! folding, the reconnect-refetch policy, unsaved-edit preservation) all live in
//! `splitway-gui-core` and are unit-tested there.

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender};
use std::time::{Duration, Instant};

use eframe::egui;

use splitway_shared::config::VpnBackend;

use splitway_gui_core::model::{self, Health};
use splitway_gui_core::{GuiCore, MessageKind};

use crate::worker::{self, Job, Reply};

/// How often the UI re-polls `Status` so the toggle/applied/vpn_up display
/// stays live without a push channel (the protocol has none).
const POLL_INTERVAL: Duration = Duration::from_millis(1500);

/// Run the GUI event loop. Blocks until the window is closed.
pub fn run() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([540.0, 720.0])
            .with_min_inner_size([420.0, 480.0])
            .with_title("Splitway")
            // Wayland app_id: maps the window to the installed
            // `io.github.stslex.splitway.desktop` entry + hicolor icon (the deb/
            // rpm/pacman GUI package ships both under that basename).
            .with_app_id("io.github.stslex.splitway"),
        ..Default::default()
    };
    eframe::run_native(
        "Splitway",
        options,
        Box::new(|cc| Ok(Box::new(SplitwayApp::new(cc)))),
    )
}

struct SplitwayApp {
    /// All truth-contract state + orchestration lives here; egui only renders it
    /// and shuttles requests/replies.
    core: GuiCore,

    // --- egui-side IPC plumbing ---
    job_tx: Sender<Job>,
    reply_rx: Receiver<Reply>,
    /// When the periodic `Status` re-poll last fired. Pure UI timing: the core
    /// decides *what* to poll ([`GuiCore::poll`]), egui decides *when*.
    last_poll: Instant,

    // --- transient UI-local input (not part of the truth contract) ---
    /// The domain text field, validated by the core on submit.
    new_domain: String,
    /// A file the user picked to *launch* a daemon against (runtime switching is
    /// deferred), shown as a launch hint. Never sent to the daemon.
    picked_path: Option<PathBuf>,
}

impl SplitwayApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let (job_tx, reply_rx) = worker::spawn(cc.egui_ctx.clone());
        // The core is primed with an initial `Status` request; the first `pump`
        // sends it.
        SplitwayApp {
            core: GuiCore::new(),
            job_tx,
            reply_rx,
            last_poll: Instant::now(),
            new_domain: String::new(),
            picked_path: None,
        }
    }

    /// Drain all replies that have arrived since the last frame into the core.
    fn drain_replies(&mut self) {
        while let Ok(reply) = self.reply_rx.try_recv() {
            self.core.apply_reply(reply.request, reply.result);
        }
    }

    /// Send the next request the core hands us, if any. The core enforces at
    /// most one request in flight at a time, so this serializes round-trips.
    fn pump(&mut self) {
        if let Some(request) = self.core.take_next_request() {
            // If the worker is gone the window is closing; nothing to do.
            let _ = self.job_tx.send(Job { request });
        }
    }
}

impl eframe::App for SplitwayApp {
    // eframe 0.34 hands the root `Ui` directly (no margin/background) and
    // deprecates the old `update(ctx, frame)`, so we keep `ui` and paint our own
    // opaque background via a `CentralPanel` (see below).
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.drain_replies();

        // Time-based re-poll: only when the core is idle (no queue, nothing in
        // flight) so polls never stack behind a slow round-trip. The core
        // decides *what* to poll (Status + ListInterfaces, plus GetConfig while
        // the editor is clean).
        if self.core.is_idle() && self.last_poll.elapsed() >= POLL_INTERVAL {
            self.core.poll();
            self.last_poll = Instant::now();
        }
        self.pump();

        // Render inside an opaque `CentralPanel`: eframe 0.34's root `Ui` has no
        // background and the default framebuffer clear is semi-transparent, so a
        // bare layout renders as a broken-looking see-through window. The panel
        // fills the whole client area with the theme's opaque panel colour.
        egui::CentralPanel::default().show_inside(ui, |ui| {
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
        });

        // Keep the poll timer ticking even when the window is idle.
        ui.ctx().request_repaint_after(POLL_INTERVAL);
    }
}

impl SplitwayApp {
    fn ui_header(&mut self, ui: &mut egui::Ui) {
        // Copy out what the header renders so the core's borrow is released
        // before the Resync button's closure mutates it.
        let view = self.core.view();
        let working = view.working;
        let connected = view.connected;
        let health = view.connection.health;
        let banner_message = view.connection.message.clone();

        ui.horizontal(|ui| {
            ui.heading("Splitway");
            if working {
                ui.add(egui::Spinner::new());
                ui.label("working…");
            }
            // Resync: ask the daemon to re-read its config and reconcile, then
            // refresh Status + GetConfig + ListInterfaces (the core's
            // ReloadConfig fold). Unsaved edits are kept, not discarded — the
            // GetConfig refresh goes through the same dirty-guard as the
            // poll/reconnect refresh. Only meaningful while connected.
            ui.add_enabled_ui(connected, |ui| {
                if ui.button("Resync").clicked() {
                    self.core.reload_config();
                }
            });
        });

        let (color, text) = match health {
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
        if let Some(msg) = &banner_message {
            // Reuse the client/daemon guidance verbatim (e.g. the
            // PermissionDenied "run as the daemon's user/group" note, or the
            // "update splitway" version-skew message).
            ui.label(msg);
        }
    }

    fn ui_status_and_toggle(&mut self, ui: &mut egui::Ui) {
        ui.label(egui::RichText::new("Status").strong());
        // Clone the status out so rendering + the toggle's mutable core call do
        // not overlap a borrow of the core.
        let status = self.core.view().status.cloned();
        let connected = self.core.view().connected;
        match &status {
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
                        // The daemon's own belief, surfaced for verification:
                        // why routing is/ isn't active, what is applied (the
                        // interface → domains → DNS mapping), and the watch's
                        // health. Phrasings are the shared `Display` impls.
                        ui.label("routing");
                        ui.label(info.routing_state.to_string());
                        ui.end_row();
                        ui.label("applied");
                        ui.label(model::applied_summary(&info.applied));
                        ui.end_row();
                        ui.label("detector");
                        ui.label(info.detector_health.to_string());
                        ui.end_row();
                        ui.label("domains");
                        ui.label(info.domains.len().to_string());
                        ui.end_row();
                    });

                let enabled_now = info.enabled;
                ui.add_enabled_ui(connected, |ui| {
                    if enabled_now {
                        if ui.button("Disable").clicked() {
                            self.core.disable();
                        }
                    } else if ui.button("Enable").clicked() {
                        self.core.enable();
                    }
                });
            }
            None => {
                // Don't assert "not reachable": status is also dropped on
                // permission-denied / version-mismatch, where the daemon *is*
                // reachable. The banner above already states the precise reason.
                ui.label("Live status unavailable — see the status above.");
            }
        }
    }

    fn ui_domains(&mut self, ui: &mut egui::Ui) {
        ui.label(egui::RichText::new("Domains").strong());
        let domains = self
            .core
            .view()
            .status
            .map(|s| s.domains.clone())
            .unwrap_or_default();
        let connected = self.core.view().connected;

        if domains.is_empty() {
            ui.label("(no domains configured)");
        } else {
            for domain in &domains {
                ui.horizontal(|ui| {
                    ui.add_enabled_ui(connected, |ui| {
                        if ui.small_button("✖").clicked() {
                            self.core.remove_domain(domain.clone());
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

    /// Submit the domain text field through the core, which validates it. Clear
    /// the field only when the core accepted it (queued the request).
    fn submit_add_domain(&mut self) {
        if self.core.add_domain(&self.new_domain) {
            self.new_domain.clear();
        }
    }

    fn ui_config_editor(&mut self, ui: &mut egui::Ui) {
        ui.label(egui::RichText::new("Configuration").strong());
        if !self.core.view().config_loaded {
            ui.label("loading config…");
            return;
        }

        // The picker entries: the live interfaces plus the configured value when
        // it is not currently present (so a VPN that is down right now is still
        // shown and selectable). Computed (owned) before the editor is borrowed
        // mutably for the grid.
        let interface_choices =
            model::interface_choices(self.core.view().interfaces, &self.core.editor().vpn_name);

        let editor = self.core.editor_mut();
        egui::Grid::new("config_grid")
            .num_columns(2)
            .spacing([12.0, 6.0])
            .show(ui, |ui| {
                ui.label("vpn_name");
                ui.vertical(|ui| {
                    let selected = if editor.vpn_name.trim().is_empty() {
                        "(none)".to_string()
                    } else {
                        editor.vpn_name.clone()
                    };
                    egui::ComboBox::from_id_salt("vpn_name_combo")
                        .selected_text(selected)
                        .show_ui(ui, |ui| {
                            for choice in &interface_choices {
                                ui.selectable_value(
                                    &mut editor.vpn_name,
                                    choice.name.clone(),
                                    &choice.label,
                                );
                            }
                        });
                    // Free-text fallback: type a VPN interface that is not present
                    // yet, or edit when enumeration is unavailable.
                    ui.add(
                        egui::TextEdit::singleline(&mut editor.vpn_name)
                            .hint_text("or type an interface name"),
                    );
                });
                ui.end_row();

                ui.label("vpn_backend");
                egui::ComboBox::from_id_salt("vpn_backend")
                    .selected_text(backend_label(editor.backend))
                    .show_ui(ui, |ui| {
                        ui.selectable_value(
                            &mut editor.backend,
                            VpnBackend::NetworkManager,
                            backend_label(VpnBackend::NetworkManager),
                        );
                        ui.selectable_value(
                            &mut editor.backend,
                            VpnBackend::OpenVpn,
                            backend_label(VpnBackend::OpenVpn),
                        );
                    });
                ui.end_row();

                ui.label("openvpn.management");
                ui.add_enabled(
                    editor.backend == VpnBackend::OpenVpn,
                    egui::TextEdit::singleline(&mut editor.openvpn_management)
                        .hint_text("127.0.0.1:7505 or /run/openvpn/mgmt.sock"),
                );
                ui.end_row();

                ui.label("openvpn.management_password_file");
                ui.add_enabled(
                    editor.backend == VpnBackend::OpenVpn,
                    egui::TextEdit::singleline(&mut editor.openvpn_password_file)
                        .hint_text("(optional)"),
                );
                ui.end_row();
            });

        // A save now takes effect live — changing vpn_name / vpn_backend /
        // openvpn re-arms the daemon's watch with no restart — so the former
        // restart caveats are gone. The status block above shows the result
        // (routing state / applied mapping / detector health) after the save.
        let connected = self.core.view().connected;
        ui.add_enabled_ui(connected, |ui| {
            if ui.button("Save configuration").clicked() {
                // The core validates the buffers and, only on a confirming reply,
                // marks them synced (no optimistic UI).
                self.core.save_config();
            }
        });
    }

    fn ui_config_file(&mut self, ui: &mut egui::Ui) {
        ui.label(egui::RichText::new("Config file").strong());
        let config_path = self.core.view().config_path.to_string();
        ui.horizontal(|ui| {
            ui.label("active:");
            ui.monospace(if config_path.is_empty() {
                "(unknown)"
            } else {
                &config_path
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

        // Runtime switching of the daemon's active file is deferred: the GUI
        // edits the daemon's *current* file and cannot repoint it live. A picked
        // file becomes a launch hint instead.
        if let Some(path) = &self.picked_path {
            ui.label(
                "Runtime switching isn't supported yet. To use this file, restart the daemon \
                 with:",
            );
            // Subcommand first, matching the daemon's parser (`run --config …`);
            // `--config` before the subcommand is rejected. The path is quoted
            // (`{:?}`) so a path with spaces stays copy/paste-able into a shell.
            ui.monospace(format!("splitway-daemon run --config {path:?}"));
        }
    }

    fn ui_message(&mut self, ui: &mut egui::Ui) {
        // Copy the message out so the dismiss button's mutable core call does not
        // overlap the view borrow.
        let Some((kind, text)) = self
            .core
            .view()
            .message
            .map(|(kind, text)| (kind, text.to_string()))
        else {
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
                self.core.dismiss_message();
            }
        });
    }
}

fn backend_label(backend: VpnBackend) -> &'static str {
    // The canonical kebab-case token, shared with the config/IPC representation.
    backend.as_str()
}
