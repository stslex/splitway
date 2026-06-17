use std::env::Args;
use std::path::PathBuf;

use crate::command::{Command, CommandParser};

const USAGE: &str = "usage: splitway-daemon <run|status|revert> [--config <PATH>]";

impl CommandParser for Args {
    fn parse_command(self) -> Result<Command, String> {
        parse_args(self)
    }
}

/// Parse a command from any argument iterator (program name first), so the
/// parsing is unit-testable without the process's real `env::args`.
fn parse_args<I: Iterator<Item = String>>(mut args: I) -> Result<Command, String> {
    let _program = args.next();
    let command = args
        .next()
        .ok_or_else(|| format!("no command provided\n{USAGE}"))?;

    let config = parse_config_flag(&mut args)?;

    match command.as_str() {
        "run" => Ok(Command::Run { config }),
        "status" => {
            // `status` queries the running daemon over IPC and reads no config
            // file, so a --config override would be meaningless here.
            if config.is_some() {
                return Err(format!(
                    "`status` queries the running daemon over IPC and does not take --config\n{USAGE}"
                ));
            }
            Ok(Command::Status)
        }
        "revert" => Ok(Command::Revert { config }),
        other => Err(format!("unknown command: {other}\n{USAGE}")),
    }
}

/// Parse an optional `--config <PATH>` (or `--config=<PATH>`) from the
/// remaining arguments. Any other trailing argument is an error.
fn parse_config_flag<I: Iterator<Item = String>>(args: &mut I) -> Result<Option<PathBuf>, String> {
    let mut config = None;
    while let Some(arg) = args.next() {
        let value = if arg == "--config" {
            args.next()
                .ok_or_else(|| format!("--config requires a path argument\n{USAGE}"))?
        } else if let Some(value) = arg.strip_prefix("--config=") {
            value.to_string()
        } else {
            return Err(format!("unexpected argument: {arg}\n{USAGE}"));
        };
        if value.is_empty() {
            return Err(format!("--config requires a non-empty path\n{USAGE}"));
        }
        // Reject a repeated flag rather than silently letting the last win: a
        // duplicate is almost always a typo or a wrapper-script bug, and a
        // "looks right" invocation using a different file is hard to diagnose.
        if config.is_some() {
            return Err(format!("--config may only be given once\n{USAGE}"));
        }
        config = Some(PathBuf::from(value));
    }
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Command, String> {
        parse_args(args.iter().map(|s| s.to_string()))
    }

    #[test]
    fn run_without_config_is_none() {
        let cmd = parse(&["splitway-daemon", "run"]).unwrap();
        assert!(matches!(cmd, Command::Run { config: None }));
    }

    #[test]
    fn run_with_config_space_form() {
        let cmd = parse(&["splitway-daemon", "run", "--config", "/etc/sw.json"]).unwrap();
        match cmd {
            Command::Run { config: Some(p) } => assert_eq!(p, PathBuf::from("/etc/sw.json")),
            _ => panic!("expected Run with a config path"),
        }
    }

    #[test]
    fn run_with_config_equals_form() {
        let cmd = parse(&["splitway-daemon", "run", "--config=/etc/sw.json"]).unwrap();
        match cmd {
            Command::Run { config: Some(p) } => assert_eq!(p, PathBuf::from("/etc/sw.json")),
            _ => panic!("expected Run with a config path"),
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
    fn status_rejects_config() {
        assert!(parse(&["splitway-daemon", "status", "--config", "/x"]).is_err());
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
}
