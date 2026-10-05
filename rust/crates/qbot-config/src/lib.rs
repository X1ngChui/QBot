//! Configuration: one typed schema, loaded in layers, validated at startup.
//!
//! Layout (container conventions):
//! - the image bakes the binary and `defaults.toml` (compiled in, copied to
//!   `/usr/share/qbot/defaults.toml` as a reference);
//! - `/etc/qbot` is mounted read-only: `config.toml`, `conf.d/*.toml`, personas, locales;
//! - `/var/lib/qbot` is a persistent volume: models, backups, state;
//! - `/run/secrets` holds mounted secret files.
//!
//! Precedence, lowest to highest: defaults, `config.toml`, `conf.d/*.toml`, `QBOT__*` environment
//! variables. Credentials are never configuration: a config key names a secret, and the value is
//! read from the environment or a mounted file by [`SecretResolver`].

mod convert;
mod error;
mod load;
mod model;
mod secrets;
mod validate;

pub use convert::{ResolvedPaths, prepare_directories};
pub use error::{ConfigError, ConfigErrors};
pub use load::{
    DEFAULT_CONFIG_DIR, DEFAULT_DATA_DIR, DEFAULT_SECRETS_DIR, DEFAULTS, Env, Layout, Loaded,
    MapEnv, ProcessEnv, Source, load, load_in,
};
pub use model::*;
pub use secrets::{Secret, SecretKey, SecretResolver, Secrets};
