//! From configuration to the plain settings structs each crate defines. Crates never read the
//! configuration; the composition root builds their settings here, from one place.

use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use qbot_agent::{RunLimits, SupervisorConfig};
use qbot_core::{AccountId, HistoryWindow, SliceGrid};
use qbot_llm::embedding::EmbeddingConfig;
use qbot_llm::responses::{Flavor, ResponsesConfig, RetryPolicy, StateMode};
use qbot_llm::{Params, ProviderId, ReasoningEffort};
use qbot_media::MediaConfig;
use qbot_memory::identity::IdentityPolicy;
use qbot_memory::{BuilderConfig, ExtractorConfig, RecallParams};
use qbot_ops::{BackupConfig, OpsConfig, PgTarget};
use qbot_sched::{SchedulerConfig, TaskLimits};
use qbot_tools::{SearchSettings, ToolSettings};

use crate::error::{ConfigError, ConfigErrors};
use crate::load::{Layout, resolve_dir};
use crate::model::{Config, Reasoning, State, TextKind};

/// Absolute locations, with relative configured paths resolved against their base.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedPaths {
    pub config_dir: PathBuf,
    pub data_dir: PathBuf,
    pub secrets_dir: PathBuf,
    pub models_dir: PathBuf,
    pub backups_dir: PathBuf,
    pub personas_dir: PathBuf,
    pub locales_dir: PathBuf,
}

fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

fn days(n: u64) -> Duration {
    secs(n.saturating_mul(86_400))
}

impl Config {
    pub fn paths(&self, layout: &Layout) -> ResolvedPaths {
        ResolvedPaths {
            config_dir: layout.config_dir.clone(),
            data_dir: layout.data_dir.clone(),
            secrets_dir: layout.secrets_dir.clone(),
            models_dir: resolve_dir(&layout.data_dir, &self.paths.models_dir),
            backups_dir: resolve_dir(&layout.data_dir, &self.paths.backups_dir),
            personas_dir: resolve_dir(&layout.config_dir, &self.paths.personas_dir),
            locales_dir: resolve_dir(&layout.config_dir, &self.paths.locales_dir),
        }
    }

    pub fn history_window(&self) -> HistoryWindow {
        HistoryWindow {
            batch_lines: self.history.batch_lines,
            raw_batches: self.history.raw_batches,
            summary_batches: self.history.summary_batches,
        }
    }

    pub fn slice_grid(&self) -> SliceGrid {
        SliceGrid::from_window(
            self.history_window(),
            self.memory.slice_batches,
            self.memory.previous_context_batches,
            self.memory.next_context_batches,
        )
    }

    pub fn run_limits(&self) -> RunLimits {
        RunLimits {
            max_turns: self.agent.max_turns,
        }
    }

    pub fn supervisor(&self) -> SupervisorConfig {
        SupervisorConfig {
            capacity: self.runtime.reply_capacity,
            concurrency: self.runtime.reply_concurrency,
            reply_deadline: secs(self.runtime.reply_deadline_secs),
        }
    }

    /// Sampling parameters of a reply's model calls.
    pub fn params(&self) -> Params {
        Params {
            max_output_tokens: self.agent.max_output_tokens,
            reasoning: match self.agent.reasoning {
                Reasoning::Off => ReasoningEffort::Off,
                Reasoning::Low => ReasoningEffort::Low,
                Reasoning::Medium => ReasoningEffort::Medium,
                Reasoning::High => ReasoningEffort::High,
            },
            temperature: None,
        }
    }

    pub fn tool_settings(&self) -> ToolSettings {
        ToolSettings {
            max_sends_per_run: self.agent.max_sends_per_run,
            search: SearchSettings {
                default_limit: self.tools.search_history.default_limit,
                max_limit: self.tools.search_history.max_limit,
            },
        }
    }

    pub fn task_limits(&self) -> TaskLimits {
        TaskLimits {
            min_delay: secs(self.tasks.min_delay_secs),
            max_pending_per_group: self.tasks.max_pending_per_group,
            max_chain_depth: self.tasks.max_chain_depth,
        }
    }

    pub fn scheduler_config(&self) -> SchedulerConfig {
        SchedulerConfig {
            job_lease: secs(self.scheduler.job_lease_secs),
            max_job_attempts: self.scheduler.max_job_attempts,
            job_backoff: self
                .scheduler
                .job_backoff_secs
                .iter()
                .copied()
                .map(secs)
                .collect(),
            max_concurrent_jobs: self.scheduler.max_concurrent_jobs,
        }
    }

    pub fn identity_policy(&self) -> IdentityPolicy {
        IdentityPolicy {
            confirm_at: self.identity.confirm_at,
            invitation_ttl: secs(self.identity.invitation_ttl_secs),
        }
    }

    pub fn extractor_config(&self) -> ExtractorConfig {
        ExtractorConfig {
            max_output_tokens: self.memory.extraction.max_output_tokens,
            max_attempts: self.memory.extraction.max_attempts,
        }
    }

    pub fn builder_config(&self) -> BuilderConfig {
        BuilderConfig {
            language: self.memory.language.clone(),
        }
    }

    pub fn recall_params(&self) -> RecallParams {
        RecallParams {
            limit: self.memory.recall.limit,
            max_distance: self.memory.recall.max_distance,
            half_life: days(u64::from(self.memory.recall.half_life_days)),
        }
    }

    pub fn decay_policy(&self) -> qbot_memory::facts::DecayPolicy {
        let f = &self.memory.facts;
        qbot_memory::facts::DecayPolicy {
            stable: days(f.half_life_days.stable),
            default: days(f.half_life_days.default),
            fast: days(f.half_life_days.fast),
            forget_below: f.forget_below,
        }
    }

    pub fn text_provider(&self) -> ResponsesConfig {
        let t = &self.providers.text;
        ResponsesConfig {
            model: t.model.clone(),
            flavor: match t.kind {
                TextKind::Deepseek => Flavor::DeepSeek,
                TextKind::OpenaiResponses => Flavor::Standard,
            },
            state: match t.state {
                State::Stateless => StateMode::Stateless,
                State::ServerState => StateMode::ServerState,
            },
            timeout: Some(secs(t.request_timeout_secs)),
            retry: RetryPolicy {
                retries: t.retries,
                base: Duration::from_millis(t.retry_base_ms),
                jitter: t.retry_jitter,
            },
        }
    }

    pub fn text_connect_timeout(&self) -> Duration {
        secs(self.providers.text.connect_timeout_secs)
    }

    pub fn embedding_provider(&self) -> EmbeddingConfig {
        let e = &self.providers.embedding;
        EmbeddingConfig {
            id: ProviderId::new("embedding"),
            model: e.model.clone(),
            dims: e.dims,
            max_batch: e.max_batch,
            timeout: secs(e.request_timeout_secs),
            retry: RetryPolicy {
                retries: e.retries,
                base: Duration::from_millis(e.retry_base_ms),
                jitter: e.retry_jitter,
            },
        }
    }

    pub fn embedding_connect_timeout(&self) -> Duration {
        secs(self.providers.embedding.connect_timeout_secs)
    }

    /// The connection URL, given the resolved password (percent-encoded).
    pub fn database_url(&self, password: &str) -> String {
        let d = &self.database;
        format!(
            "postgres://{}:{}@{}:{}/{}?sslmode={}",
            encode(&d.user),
            encode(password),
            d.host,
            d.port,
            encode(&d.name),
            d.ssl_mode.as_str()
        )
    }

    /// The recurring schedules: the nightly pipeline always, the report when someone can receive it.
    pub fn recurrences(&self) -> Vec<qbot_sched::Recurrence> {
        let m = &self.maintenance;
        let mut out = Vec::new();
        if let Ok(nightly) =
            qbot_sched::Recurrence::new("nightly", &m.nightly_cron, qbot_sched::JobKind::Nightly)
        {
            out.push(nightly);
        }
        if !self.bot.owners.is_empty()
            && let Ok(report) =
                qbot_sched::Recurrence::new("report", &m.report_cron, qbot_sched::JobKind::Report)
        {
            out.push(report);
        }
        out
    }

    /// Settings for backups, or `None` when they are switched off.
    pub fn backup_config(&self, paths: &ResolvedPaths, password: &str) -> Option<BackupConfig> {
        let m = &self.maintenance;
        m.backups_enabled.then(|| BackupConfig {
            dir: paths.backups_dir.clone(),
            prefix: "qbot".to_owned(),
            keep: m.backup_keep,
            timeout: secs(m.backup_timeout_secs),
            bin_dir: Some(m.postgres_bin_dir.as_str())
                .filter(|d| !d.is_empty())
                .map(PathBuf::from),
            target: PgTarget {
                host: self.database.host.clone(),
                port: self.database.port,
                user: self.database.user.clone(),
                database: self.database.name.clone(),
                password: password.to_owned(),
            },
        })
    }

    pub fn ops_config(
        &self,
        paths: &ResolvedPaths,
        password: &str,
        zone: jiff::tz::TimeZone,
    ) -> OpsConfig {
        let m = &self.maintenance;
        OpsConfig {
            backup: self.backup_config(paths, password),
            alias_unused: days(m.alias_unused_days),
            description_ttl: (m.description_ttl_days > 0).then(|| days(m.description_ttl_days)),
            timers_keep: days(m.timers_keep_days),
            runs_keep: (m.runs_keep_days > 0).then(|| days(m.runs_keep_days)),
            owners: self.owners().into_iter().collect(),
            zone,
            fact_decay: self.decay_policy(),
        }
    }

    pub fn media_config(&self) -> MediaConfig {
        let m = &self.media;
        MediaConfig {
            images_per_minute: m.images_per_minute,
            clips_per_minute: m.clips_per_minute,
            max_image_bytes: m.max_image_mb.saturating_mul(1024 * 1024),
            max_audio: secs(m.max_audio_secs),
            concurrency: m.concurrency,
            capacity: m.capacity,
            unreadable_hold: secs(m.unreadable_hold_secs),
        }
    }

    pub fn media_wait(&self) -> Duration {
        secs(self.media.wait_secs)
    }

    pub fn media_http_timeout(&self) -> Duration {
        secs(self.media.http_timeout_secs)
    }

    pub fn media_protocol_timeout(&self) -> Duration {
        secs(self.media.protocol_timeout_secs)
    }

    /// The picture-describing model: stateless, in the configured dialect.
    pub fn vision_provider(&self) -> ResponsesConfig {
        let v = &self.providers.vision;
        ResponsesConfig {
            model: v.model.clone(),
            flavor: match v.kind {
                TextKind::Deepseek => Flavor::DeepSeek,
                TextKind::OpenaiResponses => Flavor::Standard,
            },
            state: StateMode::Stateless,
            timeout: Some(secs(v.request_timeout_secs)),
            retry: RetryPolicy {
                retries: v.retries,
                base: Duration::from_millis(v.retry_base_ms),
                jitter: v.retry_jitter,
            },
        }
    }

    /// The most characters of a page `read_url` shows.
    pub fn read_url_max_chars(&self) -> usize {
        self.tools.read_url.max_chars
    }

    /// The web search adapter's settings; meaningful when `providers.search.enabled`.
    pub fn search_provider(&self) -> qbot_llm::search::TavilyConfig {
        let s = &self.providers.search;
        qbot_llm::search::TavilyConfig {
            max_results: s.max_results,
            depth: depth(s.depth),
            timeout: secs(s.request_timeout_secs),
            retry: RetryPolicy {
                retries: s.retries,
                base: Duration::from_millis(s.retry_base_ms),
                jitter: s.retry_jitter,
            },
            extract: qbot_llm::search::ExtractConfig {
                depth: depth(s.extract_depth),
                chunks_per_source: s.chunks_per_source,
            },
        }
    }

    pub fn search_connect_timeout(&self) -> Duration {
        secs(self.providers.search.connect_timeout_secs)
    }

    /// The search-only proxy, `None` for a direct connection.
    pub fn search_proxy(&self) -> Option<&str> {
        Some(self.providers.search.proxy.as_str()).filter(|p| !p.is_empty())
    }

    pub fn vision_params(&self) -> Params {
        Params {
            max_output_tokens: self.providers.vision.max_output_tokens,
            reasoning: ReasoningEffort::Off,
            temperature: None,
        }
    }

    pub fn vision_connect_timeout(&self) -> Duration {
        secs(self.providers.vision.connect_timeout_secs)
    }

    pub fn bot_account(&self) -> Option<AccountId> {
        AccountId::new(self.bot.account).ok()
    }

    pub fn owners(&self) -> BTreeSet<AccountId> {
        self.bot
            .owners
            .iter()
            .filter_map(|o| AccountId::new(*o).ok())
            .collect()
    }

    pub fn gateway_listen(&self) -> Option<SocketAddr> {
        self.gateway.listen.parse().ok()
    }

    pub fn action_timeout(&self) -> Duration {
        secs(self.gateway.action_timeout_secs)
    }

    pub fn echo_timeout(&self) -> Duration {
        secs(self.gateway.echo_timeout_secs)
    }

    pub fn link_ttl(&self) -> Duration {
        secs(self.identity.invitation_ttl_secs)
    }

    pub fn database_connect_timeout(&self) -> Duration {
        secs(self.database.connect_timeout_secs)
    }
}

fn encode(text: &str) -> String {
    text.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (b as char).to_string()
            }
            other => format!("%{other:02X}"),
        })
        .collect()
}

/// Make sure the persistent state directories exist and can be written. The data directory is a
/// mounted volume; a read-only or missing one is a deployment mistake to report at startup, not
/// at the first backup.
pub fn prepare_directories(paths: &ResolvedPaths) -> Result<(), ConfigErrors> {
    let mut errors = Vec::new();
    for dir in [&paths.data_dir, &paths.models_dir, &paths.backups_dir] {
        if let Err(e) = std::fs::create_dir_all(dir) {
            errors.push(ConfigError::Directory {
                path: dir.clone(),
                reason: e.to_string(),
            });
        }
    }
    if errors.is_empty() {
        let probe = paths.data_dir.join(".qbot-write-check");
        match std::fs::write(&probe, b"ok").and_then(|()| std::fs::remove_file(&probe)) {
            Ok(()) => {}
            Err(e) => errors.push(ConfigError::Directory {
                path: paths.data_dir.clone(),
                reason: format!("not writable: {e}"),
            }),
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(ConfigErrors(errors))
    }
}

fn depth(d: crate::model::SearchDepth) -> qbot_llm::search::SearchDepth {
    match d {
        crate::model::SearchDepth::Basic => qbot_llm::search::SearchDepth::Basic,
        crate::model::SearchDepth::Advanced => qbot_llm::search::SearchDepth::Advanced,
    }
}
