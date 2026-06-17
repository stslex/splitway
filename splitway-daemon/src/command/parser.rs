use std::env::Args;

use crate::command::{Command, CommandParser};

const USAGE: &str = "usage: splitway-daemon <run|status|revert>";

impl CommandParser for Args {
    fn parse_command(mut self) -> Result<Command, String> {
        let command = self
            .nth(1)
            .ok_or_else(|| format!("no command provided\n{USAGE}"))?;

        match command.as_str() {
            "run" => Ok(Command::Run),
            "status" => Ok(Command::Status),
            "revert" => Ok(Command::Revert),
            other => Err(format!("unknown command: {other}\n{USAGE}")),
        }
    }
}
