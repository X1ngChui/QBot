//! Everything the model reads that is not a tool result: instruction templates, personas, the
//! rendering of chat lines, and the assembly of a run's opening context.
//!
//! Wording lives in `prompts/*.md` (English) and in the deployment's persona files; code
//! only fills named slots. The set of slots of each template is a closed contract that is
//! checked at render time and by tests.

mod persona;
mod render;
mod source;
mod templates;

pub use persona::{Persona, PersonaError, Personas};
pub use qbot_store::GroupPeople;
pub use render::PromptRenderer;
pub use source::{
    FactKnowledge, HistorySource, Knowledge, KnowledgeSource, PeopleSource, PromptContext,
    PromptSettings, UnknownZone,
};
pub use templates::{PromptError, Template, render_template};

/// The instructions for describing one picture, in `language`.
pub fn describe_image_instructions(language: &str) -> Result<String, PromptError> {
    render_template(Template::DescribeImage, &[("language", language)])
}
