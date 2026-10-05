//! Postgres adapters for the ports the runtime defines: the chat archive, group policy, the run
//! log, usage accounting and the timer store, plus the single-instance lease.
//!
//! The schema lives in `migrations/` and is forward-only. Queries are plain `sqlx::query` with
//! bound parameters; every query on group data filters by `group_id`.

mod admin;
mod archive;
mod episodes;
mod error;
mod facts;
mod identity;
mod lease;
mod media;
mod notes;
mod policy;
mod runlog;
mod timers;
mod usage;

use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

pub use admin::{BlockRule, PgAdmin, ReportData, RosterRow, TopRow, UsageTotals};
pub use archive::{Appended, GroupPeople, MediaRefRow, NewLine, NewSpeaker, PgArchive};
pub use episodes::PgEpisodeStore;
pub use error::StoreError;
pub use facts::PgFactStore;
pub use identity::PgIdentityStore;
pub use lease::{DEFAULT_KEY, LeaseError, LeaseWatch, RuntimeLease};
pub use media::PgMediaCache;
pub use notes::PgNoteStore;
pub use policy::PgGroupPolicy;
pub use runlog::{PgRunLog, RunRecord, TriggerKind};
pub use timers::PgTimerStore;
pub use usage::{PgUsageSink, UsageRecorder};

/// The connection pool and the schema.
#[derive(Debug, Clone)]
pub struct Store {
    pool: PgPool,
}

impl Store {
    /// `acquire_timeout` bounds both opening a connection and waiting for a free one.
    /// Connect a pool of 16: replies, media work, commands and jobs each hold a connection for
    /// one query at a time, so this is well above what runs at once. A connection not available
    /// within 10 seconds means the database is down or saturated.
    pub async fn connect(url: &str) -> Result<Self, StoreError> {
        let pool = PgPoolOptions::new()
            .max_connections(16)
            .acquire_timeout(std::time::Duration::from_secs(10))
            .connect(url)
            .await?;
        Ok(Self { pool })
    }

    /// Apply pending migrations. Idempotent.
    pub async fn migrate(&self) -> Result<(), StoreError> {
        sqlx::migrate!("./migrations").run(&self.pool).await?;
        Ok(())
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }
}
