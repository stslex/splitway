//! Command-response helpers for a **request/response** frontend (the Tauri shell).
//!
//! The egui harness drives mutations the *poll* way: an intent enqueues a
//! `Request`, the driver round-trips it, and [`GuiCore::apply_reply`] folds the
//! reply (and the confirming refetch) into the shared view-model. A Tauri-style
//! frontend instead issues a mutation as a discrete **command** that returns a
//! per-action `Result`, while the daemon's resulting truth still reaches the
//! screen only through the normal `view-model-changed` event. These helpers are
//! that command path, kept here (in the brain, not the Tauri adapter) so they are
//! unit-tested against a fake daemon and the mutation surface is defined once.
//!
//! The **truth contract** (`docs/architecture.md` §2) holds by construction:
//! both helpers are stateless — they take an injected `send` round-trip and own
//! no view-model — so a mutation or a query result can *never* be written into
//! the displayed state from here. The displayed (domain/config) state changes
//! only in [`GuiCore::apply_reply`], driven by the poll thread. What a
//! command-response frontend may hold is *request-lifecycle* state (in-flight /
//! per-action error / the one-shot query result) — distinct from daemon truth,
//! and never produced here.
//!
//! - [`run_mutation`] issues exactly one of the daemon's existing write verbs
//!   (parity with the CLI / egui — [`Mutation`]) and maps the reply to
//!   `Ok(())` / `Err(reason)`. It changes nothing locally; the caller triggers an
//!   immediate re-poll ("refresh-now") so the new truth arrives via the VM.
//! - [`run_check`] issues the one-shot [`Request::CheckDomain`] route-check and
//!   returns its own [`CheckOutcome`] for an ephemeral result area. It is **never**
//!   folded into the polled view-model — a parameterized query is not ambient
//!   system state (the boundary recorded in `docs/design/tauri-read-only.md`).

use serde::Serialize;

use splitway_shared::ipc::client::ClientError;
use splitway_shared::ipc::{ConfigView, DomainCheckInfo, Request, Response};

/// The mutating actions a command-response frontend may request — **exactly** the
/// daemon's existing write verbs (parity with the CLI's add/remove + the egui
/// editor's enable/disable/set-config/resync), no more. Defining the surface as a
/// closed enum here keeps it greppable and bounds it: a verb the daemon does not
/// already expose at the current protocol version is a separate decision and a
/// protocol bump, not a quiet addition (see `docs/design/tauri-mutations.md`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mutation {
    /// Enable rule application (persisted) — [`Request::Enable`].
    Enable,
    /// Disable rule application and revert (persisted) — [`Request::Disable`].
    Disable,
    /// Add a routing domain (persisted) — [`Request::AddDomain`]. The raw string
    /// is normalized + validated by the daemon (the authoritative validator).
    AddDomain(String),
    /// Remove a routing domain (persisted) — [`Request::RemoveDomain`].
    RemoveDomain(String),
    /// Update the editable config projection (`vpn_name` / `vpn_backend` /
    /// `openvpn.*`) — [`Request::SetConfig`]. This is the "set interface / backend"
    /// path; the CLI has no equivalent, the egui editor does.
    SetConfig(ConfigView),
    /// Re-read the config from disk and reconcile (resync) — [`Request::ReloadConfig`].
    Reload,
}

impl From<Mutation> for Request {
    fn from(mutation: Mutation) -> Self {
        match mutation {
            Mutation::Enable => Request::Enable,
            Mutation::Disable => Request::Disable,
            Mutation::AddDomain(domain) => Request::AddDomain(domain),
            Mutation::RemoveDomain(domain) => Request::RemoveDomain(domain),
            Mutation::SetConfig(view) => Request::SetConfig(view),
            Mutation::Reload => Request::ReloadConfig,
        }
    }
}

/// Drive one mutating round-trip for a command-response frontend and return its
/// per-action outcome. Daemon-first, no optimistic UI: this performs exactly the
/// `mutation`'s round-trip via `send` and maps the reply; it touches no
/// view-model state (it has none to touch). The caller shows the result as
/// request-lifecycle state and, after it resolves, triggers the refresh-now
/// re-poll so the daemon's new truth reaches the screen via `view-model-changed`.
///
/// Mapping:
/// - [`Response::Ok`] → `Ok(())` (the daemon confirmed and persisted the write);
/// - [`Response::Error`] → `Err(message)` — the daemon is the authoritative
///   validator, so this carries validation / conflict / IO / permission failures,
///   *and* the frozen-on-malformed rejection ("the config file on disk could not
///   be read … fix it on disk"), surfaced verbatim as the per-action error;
/// - any other reply → `Err(unexpected …)` (a protocol-shape surprise);
/// - a transport / version-skew [`ClientError`] → `Err(err.to_string())` (its
///   own actionable guidance — "is splitway-daemon running?", "update splitway").
pub fn run_mutation<F>(mutation: Mutation, send: F) -> Result<(), String>
where
    F: FnOnce(Request) -> Result<Response, ClientError>,
{
    match send(mutation.into()) {
        Ok(Response::Ok) => Ok(()),
        Ok(Response::Error(message)) => Err(message),
        Ok(other) => Err(format!("unexpected reply from daemon: {other:?}")),
        Err(err) => Err(err.to_string()),
    }
}

/// The outcome of a one-shot [`Request::CheckDomain`] route-check, for a
/// command-response frontend's **ephemeral** result area.
///
/// Internally tagged on `state` (like [`VerifyView`](crate::VerifyView)) for an
/// ergonomic discriminated union on the TypeScript side. This is deliberately a
/// separate return value, never a field of the polled [`ViewModelSnapshot`](crate::ViewModelSnapshot):
/// a parameterized query result is not ambient config truth, and folding it in
/// would mean N live resolutions per poll cycle and a query result masquerading
/// as state (the boundary from 7b — `docs/design/tauri-read-only.md`).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "state")]
pub enum CheckOutcome {
    /// The daemon answered the route-check (coverage + best-effort live
    /// resolution). A resolution failure is *not* an error — it rides inside
    /// [`DomainCheckInfo::resolution`] as `None`.
    Checked { result: DomainCheckInfo },
    /// The query could not be completed: bad input rejected by the daemon, a
    /// transport / version-skew failure, or an unexpected reply. The string is a
    /// user-facing reason.
    Error { message: String },
}

/// Run a one-shot [`Request::CheckDomain`] round-trip and map the reply to a
/// [`CheckOutcome`]. Stateless by construction — it owns no view-model, so a
/// query result can never leak into the polled VM (the contract). `raw` is the
/// user's pasted URL or bare host; the daemon normalizes it.
pub fn run_check<F>(raw: String, send: F) -> CheckOutcome
where
    F: FnOnce(Request) -> Result<Response, ClientError>,
{
    match send(Request::CheckDomain(raw)) {
        Ok(Response::DomainCheck(result)) => CheckOutcome::Checked { result },
        Ok(Response::Error(message)) => CheckOutcome::Error { message },
        Ok(other) => CheckOutcome::Error {
            message: format!("unexpected reply from daemon: {other:?}"),
        },
        Err(err) => CheckOutcome::Error {
            message: err.to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use splitway_shared::config::VpnBackend;
    use splitway_shared::ipc::{DomainCheckInfo, ResolutionInfo, RoutingState};
    use std::cell::Cell;
    use std::io;

    fn not_running() -> ClientError {
        ClientError::NotRunning(io::Error::new(io::ErrorKind::NotFound, "no socket"))
    }

    fn sample_config_view() -> ConfigView {
        ConfigView {
            vpn_name: "tun0".to_string(),
            vpn_backend: VpnBackend::NetworkManager,
            openvpn_management: String::new(),
            openvpn_management_password_file: None,
            config_path: String::new(),
        }
    }

    // --- Mutation -> Request (issues the right verb) ----------------------

    #[test]
    fn each_mutation_maps_to_its_write_verb() {
        assert_eq!(Request::from(Mutation::Enable), Request::Enable);
        assert_eq!(Request::from(Mutation::Disable), Request::Disable);
        assert_eq!(
            Request::from(Mutation::AddDomain("corp.example.com".to_string())),
            Request::AddDomain("corp.example.com".to_string())
        );
        assert_eq!(
            Request::from(Mutation::RemoveDomain("corp.example.com".to_string())),
            Request::RemoveDomain("corp.example.com".to_string())
        );
        assert_eq!(
            Request::from(Mutation::SetConfig(sample_config_view())),
            Request::SetConfig(sample_config_view())
        );
        assert_eq!(Request::from(Mutation::Reload), Request::ReloadConfig);
    }

    #[test]
    fn run_mutation_issues_exactly_the_requested_verb_once() {
        let seen: Cell<Option<Request>> = Cell::new(None);
        let calls = Cell::new(0u32);
        let result = run_mutation(
            Mutation::AddDomain("a.example.com".to_string()),
            |request| {
                calls.set(calls.get() + 1);
                seen.set(Some(request));
                Ok(Response::Ok)
            },
        );
        assert_eq!(result, Ok(()));
        assert_eq!(calls.get(), 1, "the round-trip must run exactly once");
        assert_eq!(
            seen.into_inner(),
            Some(Request::AddDomain("a.example.com".to_string()))
        );
    }

    // --- run_mutation reply mapping --------------------------------------

    #[test]
    fn run_mutation_ok_on_response_ok() {
        assert_eq!(run_mutation(Mutation::Enable, |_| Ok(Response::Ok)), Ok(()));
    }

    #[test]
    fn run_mutation_surfaces_a_daemon_error_verbatim() {
        // The daemon is the authoritative validator; its message is the per-action
        // error, unprefixed (the frontend owns presentation).
        let result = run_mutation(Mutation::AddDomain("bad domain".to_string()), |_| {
            Ok(Response::Error(
                "invalid domain: contains whitespace".to_string(),
            ))
        });
        assert_eq!(
            result,
            Err("invalid domain: contains whitespace".to_string())
        );
    }

    #[test]
    fn run_mutation_surfaces_the_frozen_on_malformed_rejection() {
        // The daemon refuses every write while the on-disk config is unreadable
        // (it would clobber the fields it preserves from disk). 7c surfaces that
        // rejection as the per-action error — the VM separately shows the frozen
        // RoutingState::ConfigInvalid.
        let frozen = "cannot change settings: the config file on disk could not be read \
                      (expected value at line 1) — fix it on disk; the daemon keeps running \
                      on the last-good config";
        let result = run_mutation(Mutation::Enable, |_| {
            Ok(Response::Error(frozen.to_string()))
        });
        assert_eq!(result, Err(frozen.to_string()));
    }

    #[test]
    fn run_mutation_maps_a_transport_error_to_its_guidance() {
        let result = run_mutation(Mutation::Disable, |_| Err(not_running()));
        let message = result.unwrap_err();
        assert!(
            message.contains("splitway-daemon running"),
            "expected the client's actionable guidance, got: {message}"
        );
    }

    #[test]
    fn run_mutation_rejects_an_unexpected_reply() {
        let result = run_mutation(Mutation::Enable, |_| Ok(Response::Domains(vec![])));
        assert!(result
            .unwrap_err()
            .starts_with("unexpected reply from daemon"));
    }

    // --- run_check outcome mapping (never touches the VM) -----------------

    fn domain_check(host: &str, covered: bool) -> DomainCheckInfo {
        DomainCheckInfo {
            host: host.to_string(),
            covered,
            matched_domain: covered.then(|| "example.com".to_string()),
            vpn_interface: "tun0".to_string(),
            resolution: Some(ResolutionInfo {
                addresses: vec!["10.0.0.1".to_string()],
                via_interface: Some("tun0".to_string()),
                via_dns: None,
            }),
            enabled: true,
            vpn_up: true,
            routing_state: RoutingState::Applied,
        }
    }

    #[test]
    fn run_check_returns_the_daemon_route_check() {
        let info = domain_check("vault.example.com", true);
        let outcome = run_check("https://vault.example.com/x".to_string(), |request| {
            assert_eq!(
                request,
                Request::CheckDomain("https://vault.example.com/x".to_string())
            );
            Ok(Response::DomainCheck(info.clone()))
        });
        assert_eq!(outcome, CheckOutcome::Checked { result: info });
    }

    #[test]
    fn run_check_maps_a_daemon_error_to_the_error_outcome() {
        let outcome = run_check("..".to_string(), |_| {
            Ok(Response::Error("invalid domain: empty host".to_string()))
        });
        assert_eq!(
            outcome,
            CheckOutcome::Error {
                message: "invalid domain: empty host".to_string()
            }
        );
    }

    #[test]
    fn run_check_maps_a_transport_error_to_the_error_outcome() {
        let outcome = run_check("example.com".to_string(), |_| Err(not_running()));
        match outcome {
            CheckOutcome::Error { message } => assert!(message.contains("splitway-daemon running")),
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn run_check_rejects_an_unexpected_reply() {
        let outcome = run_check("example.com".to_string(), |_| Ok(Response::Ok));
        match outcome {
            CheckOutcome::Error { message } => {
                assert!(message.starts_with("unexpected reply from daemon"))
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    // --- serde shape (the TypeScript mirror must match) -------------------

    #[test]
    fn check_outcome_serializes_internally_tagged_on_state() {
        use serde_json::Value;
        let checked = CheckOutcome::Checked {
            result: domain_check("example.com", true),
        };
        let json = serde_json::to_value(&checked).unwrap();
        assert_eq!(json["state"], Value::String("Checked".to_string()));
        assert_eq!(
            json["result"]["host"],
            Value::String("example.com".to_string())
        );
        // The embedded RoutingState is a bare-string unit variant, like in the VM.
        assert_eq!(
            json["result"]["routing_state"],
            Value::String("Applied".to_string())
        );

        let errored = CheckOutcome::Error {
            message: "nope".to_string(),
        };
        let json = serde_json::to_value(&errored).unwrap();
        assert_eq!(json["state"], Value::String("Error".to_string()));
        assert_eq!(json["message"], Value::String("nope".to_string()));
    }
}
