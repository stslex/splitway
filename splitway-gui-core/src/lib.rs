//! `splitway-gui-core`: the framework-agnostic GUI logic shared by every client
//! that drives the daemon's control socket.
//!
//! It holds two things and **no UI framework** (no egui/eframe, no Tauri — only
//! `splitway-shared`):
//!
//! - [`model`] — the pure view-model helpers: classify a client error, reduce a
//!   `Status` round-trip to a connection banner, validate user input, build the
//!   interface picker. Stateless functions, unit-tested in isolation.
//! - [`GuiCore`] — the stateful orchestration that implements the **GUI mutation
//!   truth contract** (`docs/architecture.md` §2) once, for all frontends:
//!   connection state plus the (re)connection-edge refetch policy, reply folding
//!   (no optimistic UI — displayed state only ever changes from a daemon reply),
//!   the post-mutation/resync refresh, unsaved-edit preservation, and the
//!   read-only [`ViewModel`] a frontend renders.
//!
//! A frontend (the interim egui harness today, the Tauri backend in Phase 7b)
//! owns only rendering and the socket plumbing: it feeds each reply to
//! [`GuiCore::apply_reply`], renders [`GuiCore::view`], binds its config inputs
//! to [`GuiCore::editor_mut`], and sends exactly the requests
//! [`GuiCore::take_next_request`] hands it. The blocking IPC client itself stays
//! in `splitway_shared::ipc::client`.
//!
//! Unix-only, like that client (it speaks a Unix domain socket): the modules are
//! `cfg(unix)` so a whole-workspace build on a non-Unix target still compiles
//! this crate (as empty), mirroring how `splitway-gui` guards its egui stack.

#[cfg(unix)]
pub mod model;
#[cfg(unix)]
mod state;

#[cfg(unix)]
pub use state::{ConfigEditor, GuiCore, MessageKind, ViewModel};
