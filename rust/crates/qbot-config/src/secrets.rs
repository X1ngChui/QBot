//! Credentials. A config file names a secret; its value comes only from the environment or a
//! mounted secret file, resolved with fixed precedence:
//!
//! 1. `<NAME>_FILE`: the path of a file holding the value (the Docker/Kubernetes convention);
//! 2. `<NAME>`: the value itself in the environment;
//! 3. a file named `<name in lower case>` in the secrets directory (default `/run/secrets`).
//!
//! The value is trimmed of surrounding whitespace; an empty value is an error, never a fallback.

use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

use qbot_llm::LlmError;
use qbot_llm::responses::KeyResolver;

use crate::error::{ConfigError, ConfigErrors};
use crate::load::Env;
use crate::model::Config;

/// A credential. It never prints.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

#[derive(Clone)]
pub struct SecretResolver {
    env: Arc<dyn Env>,
    dir: PathBuf,
}

impl fmt::Debug for SecretResolver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SecretResolver")
            .field("dir", &self.dir)
            .finish_non_exhaustive()
    }
}

fn read(path: &std::path::Path) -> Result<String, ConfigError> {
    std::fs::read_to_string(path).map_err(|e| ConfigError::Io {
        path: path.to_path_buf(),
        message: e.to_string(),
    })
}

impl SecretResolver {
    pub fn new(env: Arc<dyn Env>, dir: PathBuf) -> Self {
        Self { env, dir }
    }

    pub fn resolve(&self, name: &str) -> Result<Secret, ConfigError> {
        let file_name = name.to_lowercase();
        let value = if let Some(path) = self
            .env
            .get(&format!("{name}_FILE"))
            .filter(|p| !p.is_empty())
        {
            read(std::path::Path::new(&path))?
        } else if let Some(value) = self.env.get(name) {
            value
        } else {
            let path = self.dir.join(&file_name);
            if !path.is_file() {
                return Err(ConfigError::MissingSecret {
                    name: name.to_owned(),
                    file: path.display().to_string(),
                });
            }
            read(&path)?
        };
        let value = value.trim().to_owned();
        if value.is_empty() {
            return Err(ConfigError::EmptySecret {
                name: name.to_owned(),
            });
        }
        Ok(Secret(value))
    }
}

/// The credentials the configuration names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Secrets {
    pub database_password: Secret,
    pub text_api_key: Secret,
    pub embedding_api_key: Secret,
    /// `None` when `gateway.access_token_secret` is empty: authentication is off by choice.
    pub onebot_access_token: Option<Secret>,
    /// `None` when `providers.vision.enabled` is false.
    pub vision_api_key: Option<Secret>,
    /// `None` when `providers.search.enabled` is false.
    pub search_api_key: Option<Secret>,
}

impl Config {
    /// Resolve every secret the configuration names, reporting all that are missing at once.
    pub fn resolve_secrets(&self, resolver: &SecretResolver) -> Result<Secrets, ConfigErrors> {
        let mut errors = Vec::new();
        let mut get = |name: &str| match resolver.resolve(name) {
            Ok(secret) => Some(secret),
            Err(e) => {
                errors.push(e);
                None
            }
        };
        let database_password = get(&self.database.password_secret);
        let text_api_key = get(&self.providers.text.api_key_secret);
        let embedding_api_key = get(&self.providers.embedding.api_key_secret);
        let token_name = &self.gateway.access_token_secret;
        let onebot_access_token = if token_name.is_empty() {
            None
        } else {
            get(token_name)
        };
        let token_ok = token_name.is_empty() || onebot_access_token.is_some();
        let vision_api_key = if self.providers.vision.enabled {
            get(&self.providers.vision.api_key_secret)
        } else {
            None
        };
        let vision_ok = !self.providers.vision.enabled || vision_api_key.is_some();
        let search_api_key = if self.providers.search.enabled {
            get(&self.providers.search.api_key_secret)
        } else {
            None
        };
        let search_ok = !self.providers.search.enabled || search_api_key.is_some();
        match (database_password, text_api_key, embedding_api_key) {
            (Some(database_password), Some(text_api_key), Some(embedding_api_key))
                if token_ok && vision_ok && search_ok =>
            {
                Ok(Secrets {
                    database_password,
                    text_api_key,
                    embedding_api_key,
                    onebot_access_token,
                    vision_api_key,
                    search_api_key,
                })
            }
            _ => Err(ConfigErrors(errors)),
        }
    }
}

impl Config {
    /// Everything that must hold before the bot can run: required settings and every secret,
    /// with all problems reported together.
    pub fn ready_to_run(&self, resolver: &SecretResolver) -> Result<Secrets, ConfigErrors> {
        let mut problems = Vec::new();
        if let Err(errors) = self.require_deployment() {
            problems.extend(errors.0);
        }
        let secrets = match self.resolve_secrets(resolver) {
            Ok(secrets) => Some(secrets),
            Err(errors) => {
                problems.extend(errors.0);
                None
            }
        };
        match secrets {
            Some(secrets) if problems.is_empty() => Ok(secrets),
            _ => Err(ConfigErrors(problems)),
        }
    }
}

/// An API key resolved at call time, so a replaced secret file is picked up without a restart.
#[derive(Debug, Clone)]
pub struct SecretKey {
    pub name: String,
    pub resolver: SecretResolver,
}

impl KeyResolver for SecretKey {
    fn resolve(&self) -> Result<String, LlmError> {
        self.resolver
            .resolve(&self.name)
            .map(|s| s.expose().to_owned())
            .map_err(|_| LlmError::Auth)
    }
}
