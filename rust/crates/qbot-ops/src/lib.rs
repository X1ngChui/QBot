//! Operations: the work the bot does for itself rather than for a group.
//!
//! - the **nightly pipeline** (memory extraction, decay, backup, cleanup including the chat
//!   platform's file cache, in that order, each stage independent of the others' success);
//! - **verified backups** of the database with rotation;
//! - the **daily report** to the owners.
//!
//! Each is a timer job (see `qbot-sched`), run by [`Operations`]. Everything touching the outside
//! world is behind a port so the logic is tested without a database, a dump tool or a chat
//! connection.

mod backup;
mod operations;
mod pg;
mod ports;
mod report;

pub use backup::{
    BackupConfig, BackupError, PgTarget, VerifiedBackup, check_tools, create_verified_backup,
    newest_backup_age, rotate_backups,
};
pub use operations::{Operations, OpsConfig, Parts};
pub use pg::{PgHousekeeping, PgReportSource};
pub use ports::{Extractor, Housekeeping, OpsError, PlatformCache, ReportSink, ReportSource};
pub use report::render_report;
