mod parser;

use std::path::PathBuf;

pub trait CommandParser {
    fn parse_command(self) -> Result<Command, String>;
}

pub enum Command {
    /// Run the long-running daemon (watch VPN + serve IPC). `config` is the
    /// optional `--config <PATH>` override for the active config file; `None`
    /// uses the default location.
    Run { config: Option<PathBuf> },
    /// Print daemon status by querying the running daemon over IPC.
    Status,
    /// Emergency one-shot direct-backend revert (works with no daemon).
    /// `config` is the optional `--config <PATH>` override.
    Revert { config: Option<PathBuf> },
}
