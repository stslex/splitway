mod parser;

use std::path::PathBuf;

pub trait CommandParser {
    fn parse_command(self) -> Result<Command, String>;
}

pub enum Command {
    /// Run the long-running daemon (watch VPN + serve IPC). `config` is the
    /// optional `--config <PATH>` override for the active config file; `None`
    /// uses the default location. `socket_group` is the optional
    /// `--socket-group <NAME>`: when set, the control socket and its runtime dir
    /// are group-owned (`0660`/`0750`) so an in-group user can connect without
    /// `sudo` (the unprivileged-GUI deployment); `None` keeps the default
    /// owner-only (`0600`) socket.
    Run {
        config: Option<PathBuf>,
        socket_group: Option<String>,
    },
    /// Print daemon status by querying the running daemon over IPC.
    Status,
    /// Emergency one-shot direct-backend revert (works with no daemon).
    /// `config` is the optional `--config <PATH>` override.
    Revert { config: Option<PathBuf> },
}
