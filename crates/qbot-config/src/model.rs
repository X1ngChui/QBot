//! The configuration schema: what a deployment may choose. Internal tuning (timeouts, retry
//! policies, pool sizes, bounds on untrusted input) is not here; it is the `Default` of the
//! settings each crate defines. Every field is required: the baked-in `defaults.toml` supplies
//! them all, so a missing key can only mean a broken image, and an unknown key is an error.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub bot: Bot,
    pub gateway: Gateway,
    pub database: Database,
    pub network: Network,
    pub replies: Replies,
    pub history: History,
    pub memory: Memory,
    pub media: Media,
    pub providers: Providers,
    pub maintenance: Maintenance,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Bot {
    /// The bot's own QQ account; zero means "not configured".
    pub account: i64,
    pub owners: Vec<i64>,
    pub nicknames: Vec<String>,
    pub timezone: String,
    /// Member-facing wording, and the language the bot writes its memory and picture
    /// descriptions in (each catalog names it).
    pub locale: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Gateway {
    pub listen: String,
    /// The NAME of the secret holding the access token; empty disables authentication.
    pub access_token_secret: String,
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
}

/// An outbound service, for choosing which of them use the proxy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Service {
    Text,
    Vision,
    Embedding,
    Search,
    /// Downloading pictures from the platform's links.
    Media,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Network {
    /// An `http://` or `https://` proxy; empty connects directly.
    pub proxy: String,
    /// The services that use the proxy; the others connect directly.
    pub proxy_for: Vec<Service>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Replies {
    /// Replies calling the model at once.
    pub concurrency: usize,
    /// A reply not finished this long after it was triggered is abandoned.
    pub deadline_secs: u64,
    /// Messages one reply may send.
    pub max_messages: u32,
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
pub struct Memory {
    pub slice_batches: u32,
    pub recall: Recall,
    pub facts: Facts,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Recall {
    pub max_distance: f32,
    pub half_life_days: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Facts {
    pub half_life_days: HalfLives,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HalfLives {
    pub stable: u32,
    pub default: u32,
    pub fast: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Media {
    /// Voice clips transcribed in the process.
    pub transcribe_voice: bool,
    pub images_per_minute: u32,
    pub clips_per_minute: u32,
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
pub enum ProviderKind {
    Deepseek,
    OpenaiResponses,
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
pub struct TextProvider {
    pub kind: ProviderKind,
    pub endpoint: String,
    pub model: String,
    pub api_key_secret: String,
    /// The reasoning effort of replies.
    pub reasoning: Reasoning,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VisionProvider {
    pub enabled: bool,
    pub kind: ProviderKind,
    pub endpoint: String,
    pub model: String,
    pub api_key_secret: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmbeddingProvider {
    pub endpoint: String,
    pub model: String,
    pub dims: usize,
    /// Inputs per request the endpoint accepts.
    pub max_batch: usize,
    pub api_key_secret: String,
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
    pub api_key_secret: String,
    pub depth: SearchDepth,
    pub extract_depth: SearchDepth,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Maintenance {
    pub nightly_cron: String,
    pub report_cron: String,
    /// Verified backups kept; 0 makes none.
    pub backups: usize,
    /// Where `pg_dump` and `pg_restore` are; empty finds them on `PATH`.
    pub postgres_bin_dir: String,
    /// Finished runs older than this are deleted; 0 keeps them.
    pub runs_keep_days: u32,
    pub napcat_clean_cache: bool,
}
