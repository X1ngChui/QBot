use std::collections::HashMap;
use std::path::{Path, PathBuf};

use qbot_core::GroupId;
use serde::Deserialize;

/// Who the bot is in a group: its name, its voice, and standing background about the group. This
/// is deployment content (it may be in any language) and is shown to the model verbatim.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Persona {
    pub name: String,
    pub system_prompt: String,
    /// Standing background about the group. Empty means none.
    #[serde(default)]
    pub group_knowledge: String,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PersonaError {
    #[error("{path}: cannot read: {message}")]
    Io { path: PathBuf, message: String },
    #[error("{path}: {message}")]
    Invalid { path: PathBuf, message: String },
    #[error(
        "{dir}: there is no default.toml; copy default.toml.example from the deployment config and edit it"
    )]
    NoDefault { dir: PathBuf },
    #[error("{path}: `group_<id>.toml` must have a positive numeric group id")]
    BadGroupFile { path: PathBuf },
}

/// The default persona plus per-group replacements (`group_<id>.toml`, a whole file each).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Personas {
    default: Persona,
    groups: HashMap<GroupId, Persona>,
}

fn read(path: &Path) -> Result<Persona, PersonaError> {
    let text = std::fs::read_to_string(path).map_err(|e| PersonaError::Io {
        path: path.to_owned(),
        message: e.to_string(),
    })?;
    let persona: Persona = toml::from_str(&text).map_err(|e| PersonaError::Invalid {
        path: path.to_owned(),
        message: e.to_string(),
    })?;
    if persona.name.trim().is_empty() {
        return Err(PersonaError::Invalid {
            path: path.to_owned(),
            message: "`name` must not be empty".into(),
        });
    }
    if persona.system_prompt.trim().is_empty() {
        return Err(PersonaError::Invalid {
            path: path.to_owned(),
            message: "`system_prompt` must not be empty".into(),
        });
    }
    Ok(persona)
}

impl Personas {
    pub fn single(default: Persona) -> Self {
        Self {
            default,
            groups: HashMap::new(),
        }
    }

    /// Load every persona file in `dir`, reporting all problems together.
    pub fn load(dir: &Path) -> Result<Self, Vec<PersonaError>> {
        let mut errors = Vec::new();
        let default_path = dir.join("default.toml");
        let default = if default_path.is_file() {
            read(&default_path).map_err(|e| errors.push(e)).ok()
        } else {
            errors.push(PersonaError::NoDefault {
                dir: dir.to_owned(),
            });
            None
        };
        let mut groups = HashMap::new();
        match std::fs::read_dir(dir) {
            Ok(entries) => {
                let mut paths: Vec<PathBuf> =
                    entries.filter_map(Result::ok).map(|e| e.path()).collect();
                paths.sort();
                for path in paths {
                    let Some(stem) = path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .and_then(|n| n.strip_suffix(".toml"))
                    else {
                        continue;
                    };
                    let Some(id) = stem.strip_prefix("group_") else {
                        continue;
                    };
                    let Some(group) = id.parse::<i64>().ok().and_then(|g| GroupId::new(g).ok())
                    else {
                        errors.push(PersonaError::BadGroupFile { path });
                        continue;
                    };
                    match read(&path) {
                        Ok(persona) => {
                            groups.insert(group, persona);
                        }
                        Err(error) => errors.push(error),
                    }
                }
            }
            Err(error) if default_path.is_file() => {
                errors.push(PersonaError::Io {
                    path: dir.to_owned(),
                    message: error.to_string(),
                });
            }
            Err(_) => {}
        }
        match (default, errors.is_empty()) {
            (Some(default), true) => Ok(Self { default, groups }),
            _ => Err(errors),
        }
    }

    pub fn for_group(&self, group: GroupId) -> &Persona {
        self.groups.get(&group).unwrap_or(&self.default)
    }

    pub fn default_persona(&self) -> &Persona {
        &self.default
    }
}
