//! The configuration schema. Every field is required: the baked-in `defaults.toml` supplies
//! them all, so a missing key can only mean a broken image, and an unknown key is always an error.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub paths: Paths,
    pub bot: Bot,
    pub gateway: Gateway,
    pub commands: Commands,
    pub media: Media,
    pub asr: Asr,
    pub maintenance: Maintenance,
    pub database: Database,
    pub runtime: Runtime,
    pub agent: Agent,
    pub history: History,
    pub providers: Providers,
    pub tasks: Tasks,
    pub scheduler: Scheduler,
    pub tools: Tools,
    pub memory: Memory,
    pub identity: Identity,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Paths {
    pub models_dir: PathBuf,
    pub backups_dir: PathBuf,
    pub personas_dir: PathBuf,
    pub locales_dir: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Bot {
    /// The bot's own QQ account; zero means "not configured".
    pub account: i64,
    pub owners: Vec<i64>,
    pub nicknames: Vec<String>,
    pub timezone: String,
    pub locale: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Gateway {
    pub listen: String,
    pub path: String,
    /// The NAME of the secret holding the access token; empty disables authentication.
    pub access_token_secret: String,
    pub action_timeout_secs: u64,
    pub echo_timeout_secs: u64,
    pub max_message_chars: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Commands {
    pub members_max_rows: usize,
    pub top_max_rows: usize,
    pub runs_max_rows: usize,
    pub notes_per_account: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SslMode {
    #[serde(rename = "disable")]
    Disable,
    #[serde(rename = "prefer")]
    Prefer,
    #[serde(rename = "require")]
    Require,
    #[serde(rename = "verify-ca")]
    VerifyCa,
    #[serde(rename = "verify-full")]
    VerifyFull,
}

impl SslMode {
    pub fn as_str(self) -> &'static str {
        match self {
            SslMode::Disable => "disable",
            SslMode::Prefer => "prefer",
            SslMode::Require => "require",
            SslMode::VerifyCa => "verify-ca",
            SslMode::VerifyFull => "verify-full",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Database {
    pub host: String,
    pub port: u16,
    pub name: String,
    pub user: String,
    pub ssl_mode: SslMode,
    /// The NAME of the secret holding the password, never the password.
    pub password_secret: String,
    pub max_connections: u32,
    pub connect_timeout_secs: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Runtime {
    pub log_buffer_lines: usize,
    pub reply_capacity: usize,
    pub reply_concurrency: usize,
    pub reply_deadline_secs: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reasoning {
    Off,
    Low,
    Medium,
    High,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Agent {
    pub max_turns: u32,
    pub max_sends_per_run: u32,
    pub max_output_tokens: u32,
    pub reasoning: Reasoning,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct History {
    pub batch_lines: u32,
    pub raw_batches: u32,
    pub summary_batches: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Providers {
    pub text: TextProvider,
    pub vision: VisionProvider,
    pub embedding: EmbeddingProvider,
    pub search: SearchProvider,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TextKind {
    Deepseek,
    OpenaiResponses,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Stateless,
    ServerState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TextProvider {
    pub kind: TextKind,
    pub endpoint: String,
    pub model: String,
    pub api_key_secret: String,
    pub state: State,
    pub request_timeout_secs: u64,
    pub connect_timeout_secs: u64,
    pub retries: u32,
    pub retry_base_ms: u64,
    pub retry_jitter: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmbeddingProvider {
    pub endpoint: String,
    pub model: String,
    pub dims: usize,
    pub max_batch: usize,
    pub api_key_secret: String,
    pub request_timeout_secs: u64,
    pub connect_timeout_secs: u64,
    pub retries: u32,
    pub retry_base_ms: u64,
    pub retry_jitter: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchDepth {
    Basic,
    Advanced,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SearchProvider {
    pub enabled: bool,
    pub endpoint: String,
    pub api_key_secret: String,
    pub max_results: u32,
    pub depth: SearchDepth,
    pub extract_depth: SearchDepth,
    pub chunks_per_source: u32,
    /// HTTP(S) proxy for the search client only; empty connects directly.
    pub proxy: String,
    pub request_timeout_secs: u64,
    pub connect_timeout_secs: u64,
    pub retries: u32,
    pub retry_base_ms: u64,
    pub retry_jitter: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tasks {
    pub min_delay_secs: u64,
    pub max_pending_per_group: usize,
    pub max_chain_depth: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scheduler {
    pub job_lease_secs: u64,
    pub max_job_attempts: u32,
    pub job_backoff_secs: Vec<u64>,
    pub max_concurrent_jobs: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tools {
    pub search_history: SearchHistory,
    pub read_url: ReadUrl,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadUrl {
    pub max_chars: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SearchHistory {
    pub default_limit: u32,
    pub max_limit: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Memory {
    pub language: String,
    pub slice_batches: u32,
    pub previous_context_batches: u32,
    pub next_context_batches: u32,
    pub extraction: Extraction,
    pub recall: Recall,
    pub knowledge: KnowledgeSettings,
    pub facts: Facts,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KnowledgeSettings {
    pub max_terms: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Extraction {
    pub max_output_tokens: u32,
    pub max_attempts: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Recall {
    pub limit: usize,
    pub max_distance: f32,
    pub half_life_days: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Facts {
    pub half_life_days: HalfLives,
    pub forget_below: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HalfLives {
    pub stable: u64,
    pub default: u64,
    pub fast: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Identity {
    pub confirm_at: f32,
    pub invitation_ttl_secs: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VisionProvider {
    pub enabled: bool,
    pub kind: TextKind,
    pub endpoint: String,
    pub model: String,
    pub api_key_secret: String,
    pub max_output_tokens: u32,
    pub request_timeout_secs: u64,
    pub connect_timeout_secs: u64,
    pub retries: u32,
    pub retry_base_ms: u64,
    pub retry_jitter: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Media {
    pub images_per_minute: u32,
    pub clips_per_minute: u32,
    pub max_image_mb: u64,
    pub max_audio_secs: u64,
    pub concurrency: usize,
    pub capacity: usize,
    pub wait_secs: u64,
    pub http_timeout_secs: u64,
    pub protocol_timeout_secs: u64,
    pub unreadable_hold_secs: u64,
    pub description_language: String,
    pub forward_max_lines: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Asr {
    pub enabled: bool,
    pub model_dir: PathBuf,
    pub language: String,
    pub threads: i32,
    pub workers: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Maintenance {
    pub nightly_cron: String,
    pub report_cron: String,
    pub backups_enabled: bool,
    pub backup_keep: usize,
    pub backup_timeout_secs: u64,
    pub postgres_bin_dir: String,
    pub alias_unused_days: u64,
    pub description_ttl_days: u64,
    pub timers_keep_days: u64,
    pub runs_keep_days: u64,
    pub napcat_clean_cache: bool,
}
