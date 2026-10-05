//! From configuration to the settings each crate defines. Crates never read the configuration;
//! the composition root builds their settings here, from the operator's choices, and everything
//! the configuration does not cover keeps the crate's own default.

use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use qbot_agent::SupervisorConfig;
use qbot_core::{AccountId, HistoryWindow, SliceGrid};
use qbot_llm::ReasoningEffort;
use qbot_llm::embedding::EmbeddingConfig;
use qbot_llm::net::Route;
use qbot_llm::responses::{ResponsesConfig, StateMode};
use qbot_llm::search::TavilyConfig;
use qbot_media::MediaConfig;
use qbot_memory::RecallParams;
use qbot_memory::facts::DecayPolicy;
use qbot_ops::{BackupConfig, OpsConfig, PgTarget};
use qbot_tools::ToolSettings;

use crate::error::{ConfigError, ConfigErrors};
use crate::load::Layout;
use crate::model::{Config, ProviderKind, Reasoning, SearchDepth, Service};

/// Replies that may wait for a model slot, per slot. Beyond that a trigger is dropped: a reply
/// that waits longer than the queue drains would mostly miss its deadline anyway.
const QUEUE_PER_SLOT: usize = 10;

/// Deadline for one text-model call, retries included. Replies are bounded by their own deadline;
/// this bounds the calls nothing else does (memory extraction), generously, because the output
/// length is left to the provider and a long reasoning answer takes minutes.
const TEXT_CALL_DEADLINE: Duration = Duration::from_secs(10 * 60);
/// Deadline for describing one picture, retries included: a sentence or two.
const VISION_CALL_DEADLINE: Duration = Duration::from_secs(2 * 60);

/// The fixed layout under the configuration and data directories.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedPaths {
    pub config_dir: PathBuf,
    pub data_dir: PathBuf,
    pub secrets_dir: PathBuf,
    /// `<data>/models`: the voice model.
    pub models_dir: PathBuf,
    /// `<data>/backups`.
    pub backups_dir: PathBuf,
    /// `<config>/personas`.
    pub personas_dir: PathBuf,
    /// `<config>/locales`: catalogs that add to or replace the built-in ones.
    pub locales_dir: PathBuf,
}

fn days(n: u32) -> Duration {
    Duration::from_secs(u64::from(n) * 86_400)
}

fn reasoning(r: Reasoning) -> ReasoningEffort {
    match r {
        Reasoning::Off => ReasoningEffort::Off,
        Reasoning::Low => ReasoningEffort::Low,
        Reasoning::Medium => ReasoningEffort::Medium,
        Reasoning::High => ReasoningEffort::High,
    }
}

fn depth(d: SearchDepth) -> qbot_llm::search::SearchDepth {
    match d {
        SearchDepth::Basic => qbot_llm::search::SearchDepth::Basic,
        SearchDepth::Advanced => qbot_llm::search::SearchDepth::Advanced,
    }
}

/// A Responses API model in the dialect of `kind`. DeepSeek keeps no server-side state; an
/// OpenAI-style provider continues from its stored previous response.
fn responses(kind: ProviderKind, model: &str) -> ResponsesConfig {
    match kind {
        ProviderKind::Deepseek => ResponsesConfig::deepseek(model),
        ProviderKind::OpenaiResponses => ResponsesConfig::standard(model, StateMode::ServerState),
    }
}

impl Config {
    pub fn paths(&self, layout: &Layout) -> ResolvedPaths {
        ResolvedPaths {
            config_dir: layout.config_dir.clone(),
            data_dir: layout.data_dir.clone(),
            secrets_dir: layout.secrets_dir.clone(),
            models_dir: layout.data_dir.join("models"),
            backups_dir: layout.data_dir.join("backups"),
            personas_dir: layout.config_dir.join("personas"),
            locales_dir: layout.config_dir.join("locales"),
        }
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

    pub fn supervisor(&self) -> SupervisorConfig {
        let r = &self.replies;
        SupervisorConfig {
            capacity: r.concurrency.saturating_mul(QUEUE_PER_SLOT),
            concurrency: r.concurrency,
            reply_deadline: Duration::from_secs(r.deadline_secs),
        }
    }

    pub fn reply_reasoning(&self) -> ReasoningEffort {
        reasoning(self.providers.text.reasoning)
    }

    pub fn tool_settings(&self) -> ToolSettings {
        ToolSettings {
            max_sends_per_run: self.replies.max_messages,
            search: Default::default(),
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
        SliceGrid::for_window(self.history_window(), self.memory.slice_batches)
    }

    pub fn recall_params(&self) -> RecallParams {
        RecallParams {
            max_distance: self.memory.recall.max_distance,
            half_life: days(self.memory.recall.half_life_days),
            ..RecallParams::default()
        }
    }

    pub fn decay_policy(&self) -> DecayPolicy {
        let h = &self.memory.facts.half_life_days;
        DecayPolicy {
            stable: days(h.stable),
            default: days(h.default),
            fast: days(h.fast),
            ..DecayPolicy::default()
        }
    }

    pub fn media_config(&self) -> MediaConfig {
        MediaConfig {
            images_per_minute: self.media.images_per_minute,
            clips_per_minute: self.media.clips_per_minute,
            ..MediaConfig::default()
        }
    }

    pub fn text_provider(&self) -> ResponsesConfig {
        let t = &self.providers.text;
        ResponsesConfig {
            timeout: Some(TEXT_CALL_DEADLINE),
            ..responses(t.kind, &t.model)
        }
    }

    pub fn vision_provider(&self) -> ResponsesConfig {
        let v = &self.providers.vision;
        ResponsesConfig {
            state: StateMode::Stateless,
            timeout: Some(VISION_CALL_DEADLINE),
            ..responses(v.kind, &v.model)
        }
    }

    pub fn embedding_provider(&self) -> EmbeddingConfig {
        let e = &self.providers.embedding;
        EmbeddingConfig::new(e.model.clone(), e.dims, e.max_batch)
    }

    pub fn search_provider(&self) -> TavilyConfig {
        let s = &self.providers.search;
        TavilyConfig::new(depth(s.depth), depth(s.extract_depth))
    }

    /// How `service` reaches the network: through the proxy if one is set and the service is
    /// listed for it, else directly.
    pub fn route(&self, service: Service) -> Route {
        let n = &self.network;
        if n.proxy.is_empty() || !n.proxy_for.contains(&service) {
            Route::Direct
        } else {
            Route::Proxy(n.proxy.clone())
        }
    }

    /// The recurring schedules: the nightly run always, the report when someone receives it.
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

    /// Backups, or `None` when none are kept.
    pub fn backup_config(&self, paths: &ResolvedPaths, password: &str) -> Option<BackupConfig> {
        let d = &self.database;
        let m = &self.maintenance;
        (m.backups > 0).then(|| BackupConfig {
            bin_dir: Some(m.postgres_bin_dir.as_str())
                .filter(|dir| !dir.is_empty())
                .map(PathBuf::from),
            ..BackupConfig::new(
                paths.backups_dir.clone(),
                m.backups,
                PgTarget {
                    host: d.host.clone(),
                    port: d.port,
                    user: d.user.clone(),
                    database: d.name.clone(),
                    password: password.to_owned(),
                },
            )
        })
    }

    pub fn ops_config(
        &self,
        paths: &ResolvedPaths,
        password: &str,
        zone: jiff::tz::TimeZone,
    ) -> OpsConfig {
        let keep = self.maintenance.runs_keep_days;
        OpsConfig::new(
            self.backup_config(paths, password),
            (keep > 0).then(|| days(keep)),
            self.owners().into_iter().collect(),
            zone,
            self.decay_policy(),
        )
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
        if let Err(e) = std::fs::write(&probe, b"ok").and_then(|()| std::fs::remove_file(&probe)) {
            errors.push(ConfigError::Directory {
                path: paths.data_dir.clone(),
                reason: format!("not writable: {e}"),
            });
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(ConfigErrors(errors))
    }
}
