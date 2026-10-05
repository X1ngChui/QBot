use std::sync::Arc;

use async_trait::async_trait;
use qbot_agent::{Archive, Effect, HistoryQuery, Tool, ToolCx, ToolError, ToolOutput};
use qbot_context::Speaker;
use qbot_wording::{Text, say};
use schemars::JsonSchema;
use serde::Deserialize;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SearchArgs {
    pub query: String,
    pub speaker: Option<u32>,
    pub limit: Option<u32>,
}

/// How many matches a search returns. A search can match the whole history, so the tool bounds
/// its own output; the bounds are deployment policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SearchSettings {
    pub default_limit: u32,
    pub max_limit: u32,
}

impl Default for SearchSettings {
    /// A screenful of matches by default; the model may ask for more, up to what one prompt can
    /// reasonably carry.
    fn default() -> Self {
        Self {
            default_limit: 8,
            max_limit: 50,
        }
    }
}

#[derive(Clone)]
pub struct SearchHistory {
    archive: Arc<dyn Archive>,
    settings: SearchSettings,
}

impl SearchHistory {
    pub fn new(archive: Arc<dyn Archive>, settings: SearchSettings) -> Self {
        Self { archive, settings }
    }
}

impl std::fmt::Debug for SearchHistory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SearchHistory")
            .field("settings", &self.settings)
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl Tool for SearchHistory {
    type Args = SearchArgs;
    const NAME: &'static str = "search_history";
    fn description(&self) -> String {
        say(Text::SearchHistoryDescription {})
    }
    fn parameters(&self) -> Vec<(&'static str, String)> {
        vec![
            ("query", say(Text::SearchHistoryParamQuery {})),
            ("speaker", say(Text::SearchHistoryParamSpeaker {})),
            ("limit", say(Text::SearchHistoryParamLimit {})),
        ]
    }
    fn effect(&self) -> Effect {
        Effect::Read
    }
    async fn call(&self, cx: &ToolCx<'_>, args: SearchArgs) -> Result<ToolOutput, ToolError> {
        let query = HistoryQuery {
            text: crate::query::parse_query(&args.query).map_err(ToolError::InvalidArguments)?,
            speaker: args.speaker.map(qbot_core::MemberNo::new),
        };
        let limit = args.limit.unwrap_or(self.settings.default_limit);
        if !(1..=self.settings.max_limit).contains(&limit) {
            return Err(ToolError::InvalidArguments(say(
                Text::SearchHistoryLimitRange {
                    max: self.settings.max_limit.to_string(),
                },
            )));
        }
        let lines = self
            .archive
            .search(cx.group, &query, limit as usize)
            .await
            .map_err(|e| ToolError::Unavailable(e.to_string()))?;
        if lines.is_empty() {
            return Ok(ToolOutput::text(say(Text::SearchHistoryNoMatches {})));
        }
        let rendered: Vec<String> = lines
            .iter()
            .map(|line| match line.speaker {
                Speaker::Bot => format!("[msg:{}] bot: {}", line.message.get(), line.text),
                Speaker::Member { number, .. } => {
                    format!(
                        "[msg:{}] member:{}: {}",
                        line.message.get(),
                        number.get(),
                        line.text
                    )
                }
            })
            .collect();
        Ok(ToolOutput::text(rendered.join("\n")))
    }
}
