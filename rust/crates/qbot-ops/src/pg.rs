use async_trait::async_trait;
use qbot_core::{GroupId, UnixMillis};
use qbot_store::{PgAdmin, PgMediaCache, ReportData};

use crate::ports::{Housekeeping, OpsError, ReportSource};

fn ops(error: impl std::fmt::Display) -> OpsError {
    OpsError(error.to_string())
}

/// Postgres-backed [`Housekeeping`].
#[derive(Debug, Clone)]
pub struct PgHousekeeping {
    admin: PgAdmin,
    media: PgMediaCache,
}

impl PgHousekeeping {
    pub fn new(admin: PgAdmin, media: PgMediaCache) -> Self {
        Self { admin, media }
    }
}

#[async_trait]
impl Housekeeping for PgHousekeeping {
    async fn groups(&self) -> Result<Vec<GroupId>, OpsError> {
        self.admin.groups().await.map_err(ops)
    }

    async fn expire_descriptions(&self, before: UnixMillis) -> Result<u64, OpsError> {
        self.media.expire(before).await.map_err(ops)
    }

    async fn delete_finished_timers(&self, before: UnixMillis) -> Result<u64, OpsError> {
        self.admin.delete_finished_timers(before).await.map_err(ops)
    }

    async fn delete_runs(&self, before: UnixMillis) -> Result<u64, OpsError> {
        self.admin.delete_runs(before).await.map_err(ops)
    }
}

/// Postgres-backed [`ReportSource`].
#[derive(Debug, Clone)]
pub struct PgReportSource(pub PgAdmin);

#[async_trait]
impl ReportSource for PgReportSource {
    async fn collect(&self, from: UnixMillis, to: UnixMillis) -> Result<ReportData, OpsError> {
        self.0.report(from, to).await.map_err(ops)
    }
}
