//! Background IPC worker. egui/eframe drive their event loop on the main
//! thread, and `ipc::client::send_request` is a *blocking* `UnixStream`
//! round-trip — calling it on the UI thread would freeze the window. So every
//! request is handed to this one worker thread, which performs the blocking
//! call and posts the reply back over a channel; the UI drains replies each
//! frame. No tokio: the client is synchronous and single-shot.
//!
//! This module is thin plumbing (no decision logic) and is not unit-tested; the
//! decisions live in the framework-agnostic `splitway-gui-core` crate.

use std::sync::mpsc::{Receiver, Sender};
use std::thread;

use eframe::egui;
use splitway_shared::ipc::client::{self, ClientError};
use splitway_shared::ipc::{Request, Response};

/// A request handed to the worker.
pub struct Job {
    pub request: Request,
}

/// A reply posted back to the UI. The originating `request` is echoed so the UI
/// knows which action completed and how to fold the result into its state.
pub struct Reply {
    pub request: Request,
    pub result: Result<Response, ClientError>,
}

/// Spawn the worker thread. Returns the job sender (UI → worker) and the reply
/// receiver (worker → UI). The `egui::Context` is used to wake the UI when a
/// reply lands, so a reply is shown promptly even when the window is idle.
pub fn spawn(ctx: egui::Context) -> (Sender<Job>, Receiver<Reply>) {
    let (job_tx, job_rx) = std::sync::mpsc::channel::<Job>();
    let (reply_tx, reply_rx) = std::sync::mpsc::channel::<Reply>();

    thread::Builder::new()
        .name("splitway-ipc".to_string())
        .spawn(move || {
            // Ends when the UI drops the job sender (window closed).
            while let Ok(job) = job_rx.recv() {
                let result = client::send_request(job.request.clone());
                let reply = Reply {
                    request: job.request,
                    result,
                };
                if reply_tx.send(reply).is_err() {
                    break;
                }
                ctx.request_repaint();
            }
        })
        .expect("failed to spawn the splitway IPC worker thread");

    (job_tx, reply_rx)
}
