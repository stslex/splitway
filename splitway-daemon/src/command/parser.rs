use crate::command::{Command, CommandParser};
use std::env::Args;

impl CommandParser for Args {
    fn parse_command(self) -> Command {
        let args: Vec<String> = std::env::args().collect();
        let command_str = args
            .get(1)
            .expect("No command provided, usage: splitway-daemon <apply|revert|status>");

        match command_str.as_str() {
            "run" => Command::Run,
            "revert" => Command::Revert,
            "status" => Command::Status,
            _ => panic!(
                "Unknown command: {command_str}, available options are [run, revert, status]"
            ),
        }
    }
}
