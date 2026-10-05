use async_trait::async_trait;
use qbot_core::{AccountId, GroupId, UnixMillis};
use qbot_memory::EpisodeJobs;
use qbot_sched::JobError;
use qbot_store::ReportData;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct OpsError(pub String);

/// Turns a group's archived chat into episodes.
#[async_trait]
pub trait Extractor: Send + Sync {
    /// Extract every complete slice that has no episode yet; returns how many were made.
    async fn extract_group(&self, group: GroupId) -> Result<usize, JobError>;
}

#[async_trait]
impl Extractor for EpisodeJobs {
    async fn extract_group(&self, group: GroupId) -> Result<usize, JobError> {
        EpisodeJobs::extract_group(self, group).await
    }
}

/// The deletions and listings maintenance needs from storage.
#[async_trait]
pub trait Housekeeping: Send + Sync {
    /// Every group the bot has seen.
    async fn groups(&self) -> Result<Vec<GroupId>, OpsError>;
    /// Forget picture descriptions older than `before`.
    async fn expire_descriptions(&self, before: UnixMillis) -> Result<u64, OpsError>;
    /// Delete finished timers created before `before`.
    async fn delete_finished_timers(&self, before: UnixMillis) -> Result<u64, OpsError>;
    /// Delete finished runs (with their items) started before `before`.
    async fn delete_runs(&self, before: UnixMillis) -> Result<u64, OpsError>;
}

#[async_trait]
pub trait ReportSource: Send + Sync {
    async fn collect(&self, from: UnixMillis, to: UnixMillis) -> Result<ReportData, OpsError>;
}

/// Delivers a report to one owner.
#[async_trait]
pub trait ReportSink: Send + Sync {
    async fn send(&self, to: AccountId, text: &str) -> Result<(), OpsError>;
}

/// The chat platform's own file cache (pictures, voice, videos it downloaded). Cleaned through
/// the platform's supported action, never by touching its files.
#[async_trait]
pub trait PlatformCache: Send + Sync {
    async fn clean(&self) -> Result<(), OpsError>;
}
