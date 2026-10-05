use std::fmt;
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read {path}: {message}")]
    Io { path: PathBuf, message: String },
    /// A problem found while reading or deserializing the layers; the message names the key and
    /// the layer (file or environment variable) it came from.
    #[error("{message}")]
    Load { message: String },
    #[error("`{key}` {reason}")]
    Invalid { key: String, reason: String },
    #[error(
        "secret {name} is not set: provide the environment variable {name}, {name}_FILE pointing at a file, or a file named {file} in the secrets directory"
    )]
    MissingSecret { name: String, file: String },
    #[error("secret {name} is empty")]
    EmptySecret { name: String },
    #[error("directory {path} is not usable: {reason}")]
    Directory { path: PathBuf, reason: String },
}

/// Every problem found, so an operator fixes a deployment in one pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigErrors(pub Vec<ConfigError>);

impl fmt::Display for ConfigErrors {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "{} configuration problem(s):", self.0.len())?;
        for error in &self.0 {
            writeln!(f, "  - {error}")?;
        }
        Ok(())
    }
}

impl std::error::Error for ConfigErrors {}

impl From<ConfigError> for ConfigErrors {
    fn from(error: ConfigError) -> Self {
        ConfigErrors(vec![error])
    }
}
