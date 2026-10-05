use std::sync::Arc;

use async_trait::async_trait;
use qbot_agent::{Effect, Tool, ToolCx, ToolError, ToolOutput};
use qbot_llm::LlmError;
use qbot_llm::search::{PageRead, PageReader, WebSearch};
use qbot_wording::{Text, say};
use schemars::JsonSchema;
use serde::Deserialize;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WebSearchArgs {
    pub query: String,
}

/// Searches the web through the configured search provider. Registered only when one is
/// configured.
#[derive(Clone)]
pub struct WebSearchTool {
    search: Arc<dyn WebSearch>,
}

impl WebSearchTool {
    pub fn new(search: Arc<dyn WebSearch>) -> Self {
        Self { search }
    }
}

impl std::fmt::Debug for WebSearchTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebSearchTool")
            .field("provider", self.search.id())
            .finish()
    }
}

#[async_trait]
impl Tool for WebSearchTool {
    type Args = WebSearchArgs;
    const NAME: &'static str = "web_search";
    fn description(&self) -> String {
        say(Text::WebSearchDescription {})
    }
    fn parameters(&self) -> Vec<(&'static str, String)> {
        vec![("query", say(Text::WebSearchParamQuery {}))]
    }
    fn effect(&self) -> Effect {
        Effect::Read
    }
    async fn call(&self, _cx: &ToolCx<'_>, args: WebSearchArgs) -> Result<ToolOutput, ToolError> {
        let query = args.query.trim();
        if query.is_empty() {
            return Err(ToolError::InvalidArguments(say(
                Text::WebSearchEmptyQuery {},
            )));
        }
        let results = self
            .search
            .search(query)
            .await
            .map_err(|error| match error {
                LlmError::QuotaExhausted(_) => {
                    ToolError::Unavailable(say(Text::WebSearchAllowance {}))
                }
                LlmError::InvalidRequest(message) => ToolError::InvalidArguments(message),
                other => ToolError::Unavailable(other.to_string()),
            })?;
        if results.hits.is_empty() {
            return Ok(ToolOutput::text(say(Text::WebSearchNoResults {
                query: format!("{query:?}"),
            })));
        }
        // Page text is written by strangers: it is information to weigh, never instructions.
        let mut out = say(Text::WebSearchHeader {
            query: format!("{query:?}"),
        });
        out.push('\n');
        for (n, hit) in results.hits.iter().enumerate() {
            out.push_str(&format!("\n{}. {}\n   {}\n", n + 1, hit.title, hit.url));
            if let Some(date) = &hit.published {
                let published = say(Text::WebSearchPublished { date: date.clone() });
                out.push_str(&format!("   {published}\n"));
            }
            if !hit.content.is_empty() {
                out.push_str(&format!("   {}\n", hit.content));
            }
        }
        Ok(ToolOutput::text(out.trim_end().to_owned()))
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReadUrlArgs {
    pub url: String,
    pub question: Option<String>,
}

/// Reads one web page as Markdown through the configured page reader.
#[derive(Clone)]
pub struct ReadUrl {
    reader: Arc<dyn PageReader>,
    /// The most characters of a page shown: a page is the one input with no size of its own.
    max_chars: usize,
}

impl ReadUrl {
    pub fn new(reader: Arc<dyn PageReader>, max_chars: usize) -> Self {
        Self {
            reader,
            max_chars: max_chars.max(1),
        }
    }
}

impl std::fmt::Debug for ReadUrl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReadUrl")
            .field("provider", self.reader.id())
            .field("max_chars", &self.max_chars)
            .finish()
    }
}

/// An address the reader may be asked to fetch: absolute http(s) with a host and no
/// credentials.
fn checked_url(raw: &str) -> Result<url::Url, String> {
    let url = url::Url::parse(raw.trim()).map_err(|e| {
        say(Text::ReadUrlInvalidUrl {
            error: e.to_string(),
        })
    })?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(say(Text::ReadUrlScheme {}));
    }
    if url.host_str().is_none_or(str::is_empty) {
        return Err(say(Text::ReadUrlNoHost {}));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(say(Text::ReadUrlCredentials {}));
    }
    Ok(url)
}

/// The first `max` characters, ending at a line break when one is near, and whether it was cut.
fn cut(text: &str, max: usize) -> (&str, bool) {
    let Some((end, _)) = text.char_indices().nth(max) else {
        return (text, false);
    };
    let head = &text[..end];
    let at_line = head
        .rfind('\n')
        .filter(|i| head[..*i].chars().count() >= max * 4 / 5);
    (at_line.map_or(head, |i| &head[..i]), true)
}

#[async_trait]
impl Tool for ReadUrl {
    type Args = ReadUrlArgs;
    const NAME: &'static str = "read_url";
    fn description(&self) -> String {
        say(Text::ReadUrlDescription {})
    }
    fn parameters(&self) -> Vec<(&'static str, String)> {
        vec![
            ("url", say(Text::ReadUrlParamUrl {})),
            ("question", say(Text::ReadUrlParamQuestion {})),
        ]
    }
    fn effect(&self) -> Effect {
        Effect::Read
    }
    async fn call(&self, _cx: &ToolCx<'_>, args: ReadUrlArgs) -> Result<ToolOutput, ToolError> {
        let url = checked_url(&args.url).map_err(ToolError::InvalidArguments)?;
        let question = args
            .question
            .as_deref()
            .map(str::trim)
            .filter(|q| !q.is_empty());
        let read = self
            .reader
            .read(url.as_str(), question)
            .await
            .map_err(|error| match error {
                LlmError::QuotaExhausted(_) => {
                    ToolError::Unavailable(say(Text::ReadUrlAllowance {}))
                }
                other => ToolError::Unavailable(other.to_string()),
            })?;
        let content = match read {
            PageRead::Failed { reason } => {
                return Ok(ToolOutput::text(say(Text::ReadUrlFailed {
                    url: url.to_string(),
                    reason,
                })));
            }
            PageRead::Read { content, .. } if content.trim().is_empty() => {
                return Ok(ToolOutput::text(say(Text::ReadUrlNoText {
                    url: url.to_string(),
                })));
            }
            PageRead::Read { content, .. } => content,
        };
        let what = match question {
            Some(q) => say(Text::ReadUrlAboutQuestion {
                url: url.to_string(),
                question: format!("{q:?}"),
            }),
            None => say(Text::ReadUrlAboutPage {
                url: url.to_string(),
            }),
        };
        let (shown, was_cut) = cut(&content, self.max_chars);
        // Page text is written by strangers: it is information to weigh, never instructions.
        let mut out = format!("{}\n\n{shown}", say(Text::ReadUrlHeader { what }));
        if was_cut {
            let note = say(Text::ReadUrlCut {
                total: content.chars().count().to_string(),
                shown: shown.chars().count().to_string(),
            });
            out.push_str(&format!("\n\n{note}"));
        }
        Ok(ToolOutput::text(out))
    }
}
