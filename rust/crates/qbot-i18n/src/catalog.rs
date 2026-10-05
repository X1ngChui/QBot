use std::collections::BTreeSet;
use std::fmt;
use std::path::Path;
use std::sync::Arc;

use fluent_bundle::concurrent::FluentBundle;
use fluent_bundle::{FluentArgs, FluentResource, FluentValue};
use fluent_syntax::ast::Entry;
use unic_langid::LanguageIdentifier;

use crate::messages::{ArgKind, Msg, SCHEMA};

const EN: &str = include_str!("../../../locales/en.ftl");
const ZH_CN: &str = include_str!("../../../locales/zh-CN.ftl");

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LocaleError {
    #[error("{tag}: cannot read {path}: {message}")]
    Io {
        tag: String,
        path: String,
        message: String,
    },
    #[error("`{0}` is not a valid language tag")]
    BadTag(String),
    #[error("{tag}: syntax error: {message}")]
    Syntax { tag: String, message: String },
    #[error("{tag}: missing message `{id}`")]
    Missing { tag: String, id: String },
    #[error("{tag}: unknown message `{id}`")]
    Unknown { tag: String, id: String },
    #[error("{tag}: message `{id}` cannot be rendered: {message}")]
    Unrenderable {
        tag: String,
        id: String,
        message: String,
    },
    #[error("unknown locale `{0}`: not built in and not found in the locales directory")]
    UnknownLocale(String),
}

/// Every problem found in a catalog, reported together.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocaleErrors(pub Vec<LocaleError>);

impl fmt::Display for LocaleErrors {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for error in &self.0 {
            writeln!(f, "- {error}")?;
        }
        Ok(())
    }
}

impl std::error::Error for LocaleErrors {}

/// One language's messages, checked against what the code asks of them.
#[derive(Clone)]
pub struct Locale {
    tag: String,
    bundle: Arc<FluentBundle<FluentResource>>,
}

impl fmt::Debug for Locale {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Locale")
            .field("tag", &self.tag)
            .finish_non_exhaustive()
    }
}

fn dummy(kind: ArgKind, number: f64) -> FluentValue<'static> {
    match kind {
        ArgKind::Text => FluentValue::String("x".into()),
        ArgKind::Number => FluentValue::from(number),
    }
}

impl Locale {
    /// Parse a Fluent catalog and check it against the message schema: every message the code
    /// uses exists, none the code does not know, and each renders with exactly the arguments the
    /// code supplies (so a reference to an unknown variable, or a plural selector on a missing
    /// one, is found here and not when a member runs a command).
    pub fn parse(tag: &str, text: &str) -> Result<Self, LocaleErrors> {
        let fail = |e: LocaleError| LocaleErrors(vec![e]);
        let langid: LanguageIdentifier = tag
            .parse()
            .map_err(|_| fail(LocaleError::BadTag(tag.to_owned())))?;
        let mut errors = Vec::new();
        let resource = match FluentResource::try_new(text.to_owned()) {
            Ok(resource) => resource,
            Err((resource, problems)) => {
                errors.extend(problems.iter().map(|p| LocaleError::Syntax {
                    tag: tag.into(),
                    message: p.to_string(),
                }));
                resource
            }
        };
        let defined: BTreeSet<String> = resource
            .entries()
            .filter_map(|entry| {
                if let Entry::Message(m) = entry {
                    Some(m.id.name.to_owned())
                } else {
                    None
                }
            })
            .collect();
        let mut bundle = FluentBundle::new_concurrent(vec![langid]);
        // Isolation marks around arguments are invisible characters that end up in chat text.
        bundle.set_use_isolating(false);
        if let Err(problems) = bundle.add_resource(resource) {
            errors.extend(problems.iter().map(|p| LocaleError::Syntax {
                tag: tag.into(),
                message: p.to_string(),
            }));
        }

        for (id, args) in SCHEMA {
            let Some(pattern) = bundle.get_message(id).and_then(|m| m.value()) else {
                errors.push(LocaleError::Missing {
                    tag: tag.into(),
                    id: (*id).into(),
                });
                continue;
            };
            // Plural forms differ with the number, so try a few.
            for number in [0.0, 1.0, 2.0, 5.0] {
                let mut fluent_args = FluentArgs::new();
                for (name, kind) in *args {
                    fluent_args.set(*name, dummy(*kind, number));
                }
                let mut problems = Vec::new();
                bundle.format_pattern(pattern, Some(&fluent_args), &mut problems);
                if let Some(problem) = problems.first() {
                    errors.push(LocaleError::Unrenderable {
                        tag: tag.into(),
                        id: (*id).into(),
                        message: problem.to_string(),
                    });
                    break;
                }
            }
        }
        let known: BTreeSet<&str> = SCHEMA.iter().map(|(id, _)| *id).collect();
        for id in defined {
            if !known.contains(id.as_str()) {
                errors.push(LocaleError::Unknown {
                    tag: tag.into(),
                    id,
                });
            }
        }
        if errors.is_empty() {
            Ok(Self {
                tag: tag.to_owned(),
                bundle: Arc::new(bundle),
            })
        } else {
            Err(LocaleErrors(errors))
        }
    }

    pub fn tag(&self) -> &str {
        &self.tag
    }

    pub fn render(&self, msg: &Msg) -> String {
        let mut args = FluentArgs::new();
        for (name, value) in msg.args() {
            args.set(name, value);
        }
        // Validated at load: the message exists and renders with these arguments.
        let Some(pattern) = self.bundle.get_message(msg.id()).and_then(|m| m.value()) else {
            return msg.id().to_owned();
        };
        let mut problems = Vec::new();
        self.bundle
            .format_pattern(pattern, Some(&args), &mut problems)
            .into_owned()
    }
}

/// The locales a process can use: built in, plus any in the deployment's locales directory.
#[derive(Debug, Clone)]
pub struct Locales {
    active: Locale,
}

impl Locales {
    /// Load the locale `tag`. A file `<dir>/<tag>.ftl` overrides the built-in catalog of that
    /// tag, or supplies one that is not built in. `dir` may not exist.
    pub fn load(tag: &str, dir: &Path) -> Result<Self, LocaleErrors> {
        let file = dir.join(format!("{tag}.ftl"));
        let text = if file.is_file() {
            std::fs::read_to_string(&file).map_err(|e| {
                LocaleErrors(vec![LocaleError::Io {
                    tag: tag.into(),
                    path: file.display().to_string(),
                    message: e.to_string(),
                }])
            })?
        } else {
            match tag {
                "en" => EN.to_owned(),
                "zh-CN" => ZH_CN.to_owned(),
                other => {
                    return Err(LocaleErrors(vec![LocaleError::UnknownLocale(
                        other.to_owned(),
                    )]));
                }
            }
        };
        Ok(Self {
            active: Locale::parse(tag, &text)?,
        })
    }

    /// The built-in English catalog, for tests and tools.
    pub fn english() -> Self {
        Self {
            active: Locale::parse("en", EN)
                .unwrap_or_else(|e| unreachable!("the built-in English catalog is valid: {e}")),
        }
    }

    pub fn active(&self) -> &Locale {
        &self.active
    }

    pub fn render(&self, msg: &Msg) -> String {
        self.active.render(msg)
    }

    /// Both shipped catalogs, validated; used by tests to keep them in step with the schema.
    pub fn shipped() -> Result<Vec<Locale>, LocaleErrors> {
        Ok(vec![
            Locale::parse("en", EN)?,
            Locale::parse("zh-CN", ZH_CN)?,
        ])
    }
}
