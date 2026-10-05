//! Layered loading on [Figment](https://docs.rs/figment), with fixed precedence. Later layers
//! override earlier ones key by key:
//!
//! 1. the baked-in `defaults.toml` (complete: every key the application reads);
//! 2. `$QBOT_CONFIG_DIR/config.toml` (default `/etc/qbot/config.toml`), if present;
//! 3. `$QBOT_CONFIG_DIR/conf.d/*.toml`, in file-name order;
//! 4. environment variables `QBOT__SECTION__KEY` (`__` separates levels).
//!
//! Tables merge; arrays and scalars replace. The merged result is deserialized into [`Config`],
//! which rejects unknown keys and wrong types; Figment names the key and the layer in each error.
//! Environment values are parsed by Figment's rules (`true`, numbers and `[...]` arrays are
//! typed; anything else is a string), so a string that looks like a number must be quoted:
//! `QBOT__DATABASE__NAME='"2024"'`. Secrets are not configuration and never come through here.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use figment::providers::{Env as FigmentEnv, Format, Toml};
use figment::{Figment, Metadata, Profile, Provider, value};

use crate::error::{ConfigError, ConfigErrors};
use crate::model::Config;

pub const DEFAULTS: &str = include_str!("../defaults.toml");

pub const DEFAULT_CONFIG_DIR: &str = "/etc/qbot";
pub const DEFAULT_DATA_DIR: &str = "/var/lib/qbot";
pub const DEFAULT_SECRETS_DIR: &str = "/run/secrets";

/// The process environment, injectable so loading is testable.
pub trait Env: Send + Sync {
    fn get(&self, name: &str) -> Option<String>;
    fn all(&self) -> Vec<(String, String)>;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ProcessEnv;

impl Env for ProcessEnv {
    fn get(&self, name: &str) -> Option<String> {
        std::env::var(name).ok()
    }

    fn all(&self) -> Vec<(String, String)> {
        std::env::vars().collect()
    }
}

#[derive(Debug, Clone, Default)]
pub struct MapEnv(pub BTreeMap<String, String>);

impl MapEnv {
    pub fn new<const N: usize>(pairs: [(&str, &str); N]) -> Self {
        Self(
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
        )
    }
}

impl Env for MapEnv {
    fn get(&self, name: &str) -> Option<String> {
        self.0.get(name).cloned()
    }

    fn all(&self) -> Vec<(String, String)> {
        self.0.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
    }
}

/// Where things live in the container. These three bootstrap variables are the only environment
/// variables outside the `QBOT__` scheme, because they say where to find the rest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    /// Mounted, read-only: `config.toml`, `conf.d/`, personas, locales.
    pub config_dir: PathBuf,
    /// A persistent volume: models, backups and other state.
    pub data_dir: PathBuf,
    /// Mounted secret files, one file per secret named in lower case.
    pub secrets_dir: PathBuf,
}

impl Layout {
    pub fn from_env(env: &dyn Env) -> Self {
        let dir = |name: &str, default: &str| {
            PathBuf::from(
                env.get(name)
                    .filter(|v| !v.is_empty())
                    .unwrap_or_else(|| default.to_owned()),
            )
        };
        Self {
            config_dir: dir("QBOT_CONFIG_DIR", DEFAULT_CONFIG_DIR),
            data_dir: dir("QBOT_DATA_DIR", DEFAULT_DATA_DIR),
            secrets_dir: dir("QBOT_SECRETS_DIR", DEFAULT_SECRETS_DIR),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    Defaults,
    File(PathBuf),
    DropIn(PathBuf),
    /// An environment variable that overrode a key (the name only, never the value).
    Env(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Loaded {
    pub config: Config,
    pub layout: Layout,
    /// Every layer that contributed, in the order applied.
    pub sources: Vec<Source>,
}

/// The baked-in defaults, labelled so an error in them says so.
struct Defaults;

impl Provider for Defaults {
    fn metadata(&self) -> Metadata {
        Metadata::named("built-in defaults")
    }

    fn data(&self) -> Result<value::Map<Profile, value::Dict>, figment::Error> {
        Toml::string(DEFAULTS).data()
    }
}

/// Load from the process environment and the standard locations.
pub fn load() -> Result<Loaded, ConfigErrors> {
    load_in(Layout::from_env(&ProcessEnv))
}

/// Load with the given layout. Overrides come from the process environment (`QBOT__*`).
pub fn load_in(layout: Layout) -> Result<Loaded, ConfigErrors> {
    let mut sources = vec![Source::Defaults];
    let mut errors = Vec::new();
    let mut figment = Figment::from(Defaults);

    let main = layout.config_dir.join("config.toml");
    if main.is_file() {
        figment = figment.merge(Toml::file(&main));
        sources.push(Source::File(main));
    }
    let drop_ins = layout.config_dir.join("conf.d");
    if drop_ins.is_dir() {
        match std::fs::read_dir(&drop_ins) {
            Ok(entries) => {
                let mut files: Vec<PathBuf> = entries
                    .filter_map(Result::ok)
                    .map(|e| e.path())
                    .filter(|p| p.is_file() && p.extension().is_some_and(|x| x == "toml"))
                    .collect();
                files.sort();
                for file in files {
                    figment = figment.merge(Toml::file(&file));
                    sources.push(Source::DropIn(file));
                }
            }
            Err(e) => errors.push(ConfigError::Io {
                path: drop_ins,
                message: e.to_string(),
            }),
        }
    }
    figment = figment.merge(FigmentEnv::prefixed(ENV_PREFIX).split("__"));
    let mut names: Vec<String> = std::env::vars()
        .map(|(k, _)| k)
        .filter(|k| k.starts_with(ENV_PREFIX))
        .collect();
    names.sort();
    sources.extend(names.into_iter().map(Source::Env));

    let config = match figment.extract::<Config>() {
        Ok(config) => Some(config),
        Err(error) => {
            // Figment chains every error it found; report each with its key and layer.
            errors.extend(error.into_iter().map(|e| ConfigError::Load {
                message: e.to_string(),
            }));
            None
        }
    };
    let Some(config) = config.filter(|_| errors.is_empty()) else {
        return Err(ConfigErrors(errors));
    };
    config.validate()?;
    Ok(Loaded {
        config,
        layout,
        sources,
    })
}

/// Environment variables with this prefix override configuration keys.
const ENV_PREFIX: &str = "QBOT__";

pub(crate) fn resolve_dir(base: &Path, configured: &Path) -> PathBuf {
    if configured.is_absolute() {
        configured.to_path_buf()
    } else {
        base.join(configured)
    }
}
