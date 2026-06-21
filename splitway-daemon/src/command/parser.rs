use std::env::Args;
use std::path::PathBuf;

use crate::command::{Command, CommandParser};

const USAGE: &str =
    "usage: splitway-daemon <run|status|revert> [--config <PATH>] [--socket-group <NAME>]";

impl CommandParser for Args {
    fn parse_command(self) -> Result<Command, String> {
        parse_args(self)
    }
}

/// The optional flags shared by the subcommands. `--socket-group` is only
/// meaningful for `run` (only `run` binds the socket); the per-command match
/// rejects it elsewhere.
#[derive(Default)]
struct Flags {
    config: Option<PathBuf>,
    socket_group: Option<String>,
}

/// Parse a command from any argument iterator (program name first), so the
/// parsing is unit-testable without the process's real `env::args`.
fn parse_args<I: Iterator<Item = String>>(mut args: I) -> Result<Command, String> {
    let _program = args.next();
    let command = args
        .next()
        .ok_or_else(|| format!("no command provided\n{USAGE}"))?;

    let flags = parse_flags(&mut args)?;

    match command.as_str() {
        "run" => Ok(Command::Run {
            config: flags.config,
            socket_group: flags.socket_group,
        }),
        "status" => {
            // `status` queries the running daemon over IPC; it reads no config
            // file and binds no socket, so neither flag is meaningful here.
            if flags.config.is_some() || flags.socket_group.is_some() {
                return Err(format!(
                    "`status` queries the running daemon over IPC and takes no \
                     --config/--socket-group\n{USAGE}"
                ));
            }
            Ok(Command::Status)
        }
        "revert" => {
            // `revert` does a one-shot direct-backend revert; it never binds the
            // control socket, so --socket-group would be a silent no-op — reject it.
            if flags.socket_group.is_some() {
                return Err(format!(
                    "`revert` does not bind the control socket and takes no \
                     --socket-group\n{USAGE}"
                ));
            }
            Ok(Command::Revert {
                config: flags.config,
            })
        }
        other => Err(format!("unknown command: {other}\n{USAGE}")),
    }
}

/// Parse the optional `--config <PATH>` and `--socket-group <NAME>` flags (each
/// also accepting the `--flag=value` form). Any other trailing argument is an
/// error.
fn parse_flags<I: Iterator<Item = String>>(args: &mut I) -> Result<Flags, String> {
    let mut flags = Flags::default();
    while let Some(arg) = args.next() {
        if let Some(value) = take_value(&arg, "--config", args)? {
            set_once(&mut flags.config, PathBuf::from(value), "--config")?;
        } else if let Some(value) = take_value(&arg, "--socket-group", args)? {
            set_once(&mut flags.socket_group, value, "--socket-group")?;
        } else {
            return Err(format!("unexpected argument: {arg}\n{USAGE}"));
        }
    }
    Ok(flags)
}

/// If `arg` is `--flag <VALUE>` or `--flag=<VALUE>`, return its non-empty value;
/// `Ok(None)` means `arg` is not this flag. The space form pulls the next item
/// from `args`. An empty value is rejected (almost always a quoting bug).
fn take_value<I: Iterator<Item = String>>(
    arg: &str,
    flag: &str,
    args: &mut I,
) -> Result<Option<String>, String> {
    let value = if arg == flag {
        args.next()
            .ok_or_else(|| format!("{flag} requires an argument\n{USAGE}"))?
    } else if let Some(value) = arg.strip_prefix(&format!("{flag}=")) {
        value.to_string()
    } else {
        return Ok(None);
    };
    if value.is_empty() {
        return Err(format!("{flag} requires a non-empty value\n{USAGE}"));
    }
    Ok(Some(value))
}

/// Set a flag's value, rejecting a repeat rather than silently letting the last
/// win: a duplicate is almost always a typo or a wrapper-script bug, and a
/// "looks right" invocation using a different value is hard to diagnose.
fn set_once<T>(slot: &mut Option<T>, value: T, flag: &str) -> Result<(), String> {
    if slot.is_some() {
        return Err(format!("{flag} may only be given once\n{USAGE}"));
    }
    *slot = Some(value);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Command, String> {
        parse_args(args.iter().map(|s| s.to_string()))
    }

    #[test]
    fn run_without_flags_is_all_none() {
        let cmd = parse(&["splitway-daemon", "run"]).unwrap();
        assert!(matches!(
            cmd,
            Command::Run {
                config: None,
                socket_group: None
            }
        ));
    }

    #[test]
    fn run_with_config_space_form() {
        let cmd = parse(&["splitway-daemon", "run", "--config", "/etc/sw.json"]).unwrap();
        match cmd {
            Command::Run {
                config: Some(p),
                socket_group: None,
            } => assert_eq!(p, PathBuf::from("/etc/sw.json")),
            _ => panic!("expected Run with a config path and no socket group"),
        }
    }

    #[test]
    fn run_with_config_equals_form() {
        let cmd = parse(&["splitway-daemon", "run", "--config=/etc/sw.json"]).unwrap();
        match cmd {
            Command::Run {
                config: Some(p), ..
            } => assert_eq!(p, PathBuf::from("/etc/sw.json")),
            _ => panic!("expected Run with a config path"),
        }
    }

    #[test]
    fn run_with_socket_group_space_form() {
        let cmd = parse(&["splitway-daemon", "run", "--socket-group", "splitway"]).unwrap();
        match cmd {
            Command::Run {
                config: None,
                socket_group: Some(g),
            } => assert_eq!(g, "splitway"),
            _ => panic!("expected Run with a socket group and no config"),
        }
    }

    #[test]
    fn run_with_socket_group_equals_form() {
        let cmd = parse(&["splitway-daemon", "run", "--socket-group=splitway"]).unwrap();
        match cmd {
            Command::Run {
                socket_group: Some(g),
                ..
            } => assert_eq!(g, "splitway"),
            _ => panic!("expected Run with a socket group"),
        }
    }

    #[test]
    fn run_accepts_both_flags_in_either_order() {
        let cmd = parse(&[
            "splitway-daemon",
            "run",
            "--socket-group",
            "splitway",
            "--config",
            "/etc/sw.json",
        ])
        .unwrap();
        match cmd {
            Command::Run {
                config: Some(p),
                socket_group: Some(g),
            } => {
                assert_eq!(p, PathBuf::from("/etc/sw.json"));
                assert_eq!(g, "splitway");
            }
            _ => panic!("expected Run with both a config and a socket group"),
        }
    }

    #[test]
    fn revert_accepts_config() {
        let cmd = parse(&["splitway-daemon", "revert", "--config", "/tmp/c.json"]).unwrap();
        match cmd {
            Command::Revert { config: Some(p) } => assert_eq!(p, PathBuf::from("/tmp/c.json")),
            _ => panic!("expected Revert with a config path"),
        }
    }

    #[test]
    fn revert_rejects_socket_group() {
        // `revert` never binds the socket, so --socket-group would be a silent
        // no-op; reject it rather than accept a flag that does nothing.
        assert!(parse(&["splitway-daemon", "revert", "--socket-group", "splitway"]).is_err());
    }

    #[test]
    fn status_rejects_config() {
        assert!(parse(&["splitway-daemon", "status", "--config", "/x"]).is_err());
    }

    #[test]
    fn status_rejects_socket_group() {
        assert!(parse(&["splitway-daemon", "status", "--socket-group", "splitway"]).is_err());
    }

    #[test]
    fn status_without_config_is_ok() {
        assert!(matches!(
            parse(&["splitway-daemon", "status"]).unwrap(),
            Command::Status
        ));
    }

    #[test]
    fn missing_command_is_error() {
        assert!(parse(&["splitway-daemon"]).is_err());
    }

    #[test]
    fn unknown_command_is_error() {
        assert!(parse(&["splitway-daemon", "frobnicate"]).is_err());
    }

    #[test]
    fn config_without_value_is_error() {
        assert!(parse(&["splitway-daemon", "run", "--config"]).is_err());
        assert!(parse(&["splitway-daemon", "run", "--config="]).is_err());
    }

    #[test]
    fn socket_group_without_value_is_error() {
        assert!(parse(&["splitway-daemon", "run", "--socket-group"]).is_err());
        assert!(parse(&["splitway-daemon", "run", "--socket-group="]).is_err());
    }

    #[test]
    fn unexpected_trailing_argument_is_error() {
        assert!(parse(&["splitway-daemon", "run", "extra"]).is_err());
    }

    #[test]
    fn duplicate_config_is_error() {
        // A repeated --config is almost always a mistake; reject it rather than
        // silently letting the last occurrence win.
        assert!(parse(&["splitway-daemon", "run", "--config", "/a", "--config", "/b"]).is_err());
        assert!(parse(&["splitway-daemon", "run", "--config=/a", "--config=/b"]).is_err());
    }

    #[test]
    fn duplicate_socket_group_is_error() {
        assert!(parse(&[
            "splitway-daemon",
            "run",
            "--socket-group",
            "a",
            "--socket-group",
            "b"
        ])
        .is_err());
        assert!(parse(&[
            "splitway-daemon",
            "run",
            "--socket-group=a",
            "--socket-group=b"
        ])
        .is_err());
    }
}
