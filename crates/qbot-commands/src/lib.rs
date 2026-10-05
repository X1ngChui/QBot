//! Chat commands: `/who`, `/alias`, `/link`, `/tasks`, `/block`, `/mute` and the rest.
//!
//! A command is recognised by the gateway (the first typed word, exactly), archived like any
//! message, and then handled here without a model call. Every reply is a quote of the command
//! plus a mention of its sender, worded by the locale catalog.

mod args;
mod catalog;
mod fail;
mod handlers;
mod ports;
mod reply;
mod router;

pub use catalog::Command;
pub use ports::{AdminError, GroupAdmin, LogLine, PgGroupAdmin, RecentLogs, RunHistory};
pub use qbot_agent::Directory;
pub use router::{CommandRouter, CommandSettings, Deps};
