mod parser;

pub trait CommandParser {
    fn parse_command(self) -> Command;
}

pub enum Command {
    Run,
    Revert,
    Status,
    Watch,
}
