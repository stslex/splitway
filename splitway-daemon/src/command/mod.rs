mod parser;

pub trait CommandParser {
    fn parse_command(self) -> Result<Command, String>;
}

pub enum Command {
    /// Run the long-running daemon (watch VPN + serve IPC).
    Run,
    /// Print daemon status by querying the running daemon over IPC.
    Status,
    /// Emergency one-shot direct-backend revert (works with no daemon).
    Revert,
}
