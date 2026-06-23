//! Resolving and running the external tools the daemon shells out to
//! (`nmcli`, `resolvectl`), independent of any service manager's PATH.
//!
//! Two failure modes this module exists to prevent:
//!
//! 1. **PATH dependence.** A systemd unit runs with systemd's minimal default
//!    unit PATH, *not* the host/login PATH — so a bare `Command::new("nmcli")`
//!    fails to spawn with `ENOENT` even on a host where NetworkManager is
//!    enabled. [`tool`] honors an optional absolute-path override in an env var
//!    (the packaging injects the store path) and otherwise falls back to the
//!    bare name (PATH lookup) for shells and non-Nix distros, so resolution
//!    never depends on the ambient PATH when the override is set.
//!
//! 2. **Silent failure.** A missing required tool means the daemon cannot
//!    perform its core function (VPN detection / split-DNS apply). [`run`] turns
//!    a not-found spawn failure into a loud, actionable `error!` — distinct from
//!    the normal "tool ran but returned non-zero / empty" path the callers
//!    already handle (the up-ness gate, the rollback path, the read-back
//!    degrade). Previously such a failure was swallowed: it surfaced only as a
//!    suppressed `warn`, so a packaging regression went unnoticed.

use std::ffi::OsString;
use std::process::{Command, Output};

use splitway_shared::platform::PlatformError;

/// Build a [`Command`] for an external tool, honoring an optional absolute-path
/// override in `env_key` and otherwise using the bare `default` name (resolved
/// via PATH). Packaging sets the override so tool resolution is independent of
/// any service manager's PATH; the bare-name fallback keeps shells and non-Nix
/// distros working unchanged. The override keys are packaging-internal, not a
/// user-facing config surface.
pub(crate) fn tool(env_key: &str, default: &str) -> Command {
    tool_from(std::env::var_os(env_key), default)
}

/// [`tool`] with the env lookup factored out, so the override/fallback choice is
/// unit-testable without mutating the process environment.
fn tool_from(override_value: Option<OsString>, default: &str) -> Command {
    Command::new(override_value.unwrap_or_else(|| default.into()))
}

/// Run `cmd` to completion and capture its output. A spawn failure with
/// [`std::io::ErrorKind::NotFound`] — the required tool is not on PATH and no
/// override points at it — is logged at `error!` with an actionable message
/// before being returned, since the daemon cannot perform `capability` without
/// `tool_name`. Every other I/O error and any non-zero exit is left untouched
/// for the caller's existing handling.
pub(crate) fn run(
    cmd: &mut Command,
    tool_name: &str,
    capability: &str,
) -> Result<Output, PlatformError> {
    cmd.output().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            log::error!(
                "required tool `{tool_name}` not found on PATH; {capability} cannot work \
                 — check the service PATH (or the SPLITWAY_* tool override)"
            );
        }
        PlatformError::Io(e)
    })
}

#[cfg(test)]
mod tests {
    use super::{run, tool_from};
    use splitway_shared::platform::PlatformError;
    use std::ffi::OsStr;
    use std::process::Command;

    #[test]
    fn override_value_wins_over_default() {
        let cmd = tool_from(
            Some("/nix/store/abc-networkmanager/bin/nmcli".into()),
            "nmcli",
        );
        assert_eq!(
            cmd.get_program(),
            OsStr::new("/nix/store/abc-networkmanager/bin/nmcli")
        );
    }

    #[test]
    fn falls_back_to_bare_name_when_override_absent() {
        let cmd = tool_from(None, "nmcli");
        assert_eq!(cmd.get_program(), OsStr::new("nmcli"));
    }

    #[test]
    fn missing_tool_maps_to_notfound_io_error() {
        // A name that cannot resolve on any PATH: the spawn fails with NotFound,
        // which `run` maps to PlatformError::Io (and logs at error!; no logger is
        // installed in tests, so nothing is printed).
        let mut cmd = Command::new("splitway-nonexistent-tool-xyz");
        let err = run(&mut cmd, "splitway-nonexistent-tool-xyz", "testing").unwrap_err();
        assert!(
            matches!(err, PlatformError::Io(ref e) if e.kind() == std::io::ErrorKind::NotFound),
            "expected a NotFound Io error, got {err:?}"
        );
    }
}
