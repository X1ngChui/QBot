//! The closed table of what may be recorded about a person (`predicates.toml`).

use std::collections::BTreeMap;

use serde::Deserialize;

const BUILTIN: &str = include_str!("../../../prompts/predicates.toml");

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FactKind {
    Attribute,
    Preference,
    Relation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Cardinality {
    /// One value at a time: a new value replaces the old one.
    Single,
    /// Values stand side by side.
    Multi,
}

/// How quickly an unconfirmed fact is forgotten. The half-lives are configuration.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize, serde::Serialize,
)]
#[serde(rename_all = "snake_case")]
pub enum DecayClass {
    Stable,
    Default,
    Fast,
}

impl DecayClass {
    pub fn as_str(self) -> &'static str {
        match self {
            DecayClass::Stable => "stable",
            DecayClass::Default => "default",
            DecayClass::Fast => "fast",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "stable" => Some(DecayClass::Stable),
            "default" => Some(DecayClass::Default),
            "fast" => Some(DecayClass::Fast),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Predicate {
    #[serde(skip)]
    pub name: String,
    pub kind: FactKind,
    pub cardinality: Cardinality,
    pub decay: DecayClass,
    #[serde(default)]
    pub opposite: Option<String>,
    pub rule: String,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PredicateError {
    #[error("the predicate table is not valid TOML: {0}")]
    Parse(String),
    #[error("predicate `{0}` must be lower-case letters, digits and underscores")]
    BadName(String),
    #[error("predicate `{name}` names `{opposite}` as its opposite, which does not name it back")]
    OneSidedOpposite { name: String, opposite: String },
}

/// The predicates, by name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Predicates {
    table: BTreeMap<String, Predicate>,
}

impl Predicates {
    pub fn parse(text: &str) -> Result<Self, PredicateError> {
        let raw: BTreeMap<String, Predicate> =
            toml::from_str(text).map_err(|e| PredicateError::Parse(e.to_string()))?;
        let mut table = BTreeMap::new();
        for (name, mut predicate) in raw {
            if name.is_empty()
                || !name
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
            {
                return Err(PredicateError::BadName(name));
            }
            predicate.name.clone_from(&name);
            table.insert(name, predicate);
        }
        for predicate in table.values() {
            if let Some(opposite) = &predicate.opposite {
                let back = table.get(opposite).and_then(|o| o.opposite.as_deref());
                if back != Some(predicate.name.as_str()) {
                    return Err(PredicateError::OneSidedOpposite {
                        name: predicate.name.clone(),
                        opposite: opposite.clone(),
                    });
                }
            }
        }
        Ok(Self { table })
    }

    /// The table shipped with the binary.
    pub fn builtin() -> Self {
        Self::parse(BUILTIN)
            .unwrap_or_else(|e| unreachable!("the built-in predicate table is valid: {e}"))
    }

    pub fn get(&self, name: &str) -> Option<&Predicate> {
        self.table.get(name)
    }

    pub fn iter(&self) -> impl Iterator<Item = &Predicate> {
        self.table.values()
    }
}
