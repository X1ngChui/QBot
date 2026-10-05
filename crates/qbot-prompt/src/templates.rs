use std::collections::BTreeSet;
use std::sync::LazyLock;

use minijinja::{Environment, UndefinedBehavior};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Template {
    ReplySystem,
    Legend,
    PersonaBlock,
    KnowledgeBlock,
    PeopleBlock,
    TriggerAddressed,
    TriggerWake,
    DescribeImage,
    LearnedKnowledge,
    UndeliveredNote,
}

impl Template {
    pub const ALL: [Template; 10] = [
        Template::ReplySystem,
        Template::Legend,
        Template::PersonaBlock,
        Template::KnowledgeBlock,
        Template::PeopleBlock,
        Template::TriggerAddressed,
        Template::TriggerWake,
        Template::DescribeImage,
        Template::LearnedKnowledge,
        Template::UndeliveredNote,
    ];

    pub fn text(self) -> &'static str {
        match self {
            Template::ReplySystem => include_str!("../../../prompts/reply_system.md"),
            Template::Legend => include_str!("../../../prompts/legend.md"),
            Template::PersonaBlock => include_str!("../../../prompts/persona_block.md"),
            Template::KnowledgeBlock => include_str!("../../../prompts/knowledge_block.md"),
            Template::PeopleBlock => include_str!("../../../prompts/people_block.md"),
            Template::TriggerAddressed => include_str!("../../../prompts/trigger_addressed.md"),
            Template::TriggerWake => include_str!("../../../prompts/trigger_wake.md"),
            Template::DescribeImage => include_str!("../../../prompts/describe_image.md"),
            Template::LearnedKnowledge => include_str!("../../../prompts/learned_knowledge.md"),
            Template::UndeliveredNote => include_str!("../../../prompts/undelivered_note.md"),
        }
    }

    /// The exact set of `{{slot}}` names the template uses.
    pub fn slots(self) -> &'static [&'static str] {
        match self {
            Template::ReplySystem => &["bot_name"],
            Template::Legend => &[],
            Template::PersonaBlock => &["persona"],
            Template::KnowledgeBlock => &["knowledge"],
            Template::PeopleBlock => &["entries"],
            Template::TriggerAddressed => &["now", "timezone", "sender", "message"],
            Template::TriggerWake => &["now", "timezone", "task", "depth", "intent"],
            Template::DescribeImage => &["language"],
            Template::LearnedKnowledge => &["entries"],
            Template::UndeliveredNote => &[],
        }
    }

    /// A short digest of the template text, recorded with each instruction so a prompt prefix is
    /// attributable to the wording that produced it.
    pub fn hash(self) -> String {
        let digest = Sha256::digest(self.text().as_bytes());
        digest.iter().take(6).map(|b| format!("{b:02x}")).collect()
    }

    /// The slots the template text actually references, as found by the template engine. Tests
    /// hold this equal to [`slots`](Self::slots).
    pub fn referenced_slots(self) -> BTreeSet<String> {
        ENGINE
            .get_template(self.name())
            .map(|t| t.undeclared_variables(false).into_iter().collect())
            .unwrap_or_default()
    }

    fn name(self) -> &'static str {
        match self {
            Template::ReplySystem => "reply_system",
            Template::Legend => "legend",
            Template::PersonaBlock => "persona_block",
            Template::KnowledgeBlock => "knowledge_block",
            Template::PeopleBlock => "people_block",
            Template::TriggerAddressed => "trigger_addressed",
            Template::TriggerWake => "trigger_wake",
            Template::DescribeImage => "describe_image",
            Template::LearnedKnowledge => "learned_knowledge",
            Template::UndeliveredNote => "undelivered_note",
        }
    }
}

/// Every template, compiled once. A slot with no value is an error, never an empty string.
static ENGINE: LazyLock<Environment<'static>> = LazyLock::new(|| {
    let mut env = Environment::new();
    env.set_undefined_behavior(UndefinedBehavior::Strict);
    for template in Template::ALL {
        env.add_template(template.name(), template.text())
            .unwrap_or_else(|e| unreachable!("the built-in template {template:?} is valid: {e}"));
    }
    env
});

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PromptError {
    #[error("template {template:?} needs the slot `{slot}`")]
    MissingSlot {
        template: Template,
        slot: &'static str,
    },
    #[error("template {template:?} has no slot `{slot}`")]
    UnknownSlot { template: Template, slot: String },
    #[error("template {template:?} could not be rendered: {message}")]
    Render { template: Template, message: String },
}

/// Fill a template's slots. Values are inserted verbatim and never treated as template text, so a
/// value that itself contains `{{x}}` stays literal. Supplying a different set of slots than the
/// template declares is an error.
pub fn render_template(template: Template, values: &[(&str, &str)]) -> Result<String, PromptError> {
    for (name, _) in values {
        if !template.slots().contains(name) {
            return Err(PromptError::UnknownSlot {
                template,
                slot: (*name).to_owned(),
            });
        }
    }
    for slot in template.slots() {
        if !values.iter().any(|(name, _)| name == slot) {
            return Err(PromptError::MissingSlot { template, slot });
        }
    }
    let context: std::collections::BTreeMap<&str, &str> = values.iter().copied().collect();
    let rendered = ENGINE
        .get_template(template.name())
        .and_then(|t| t.render(context))
        .map_err(|e| PromptError::Render {
            template,
            message: e.to_string(),
        })?;
    Ok(rendered.trim_end().to_owned())
}
