use async_trait::async_trait;
use qbot_llm::{
    Content, ConvItem, Conversation, LlmError, LoadedMedia, MediaStore, Message, Provider,
    ReasoningEffort, Request, Role, ToolChoice, collect,
};

use crate::clean::clean_description;
use crate::ports::{DescribeError, Describer};

const KEY: &str = "picture";

struct OnePicture {
    mime: String,
    bytes: Vec<u8>,
}

#[async_trait]
impl MediaStore for OnePicture {
    async fn load(&self, key: &str) -> Result<LoadedMedia, LlmError> {
        if key == KEY {
            Ok(LoadedMedia {
                mime: self.mime.clone(),
                bytes: self.bytes.clone(),
            })
        } else {
            Err(LlmError::InvalidRequest(format!("unknown picture {key:?}")))
        }
    }
}

/// Describes pictures with a vision-capable model, one call per picture.
pub struct LlmDescriber {
    provider: std::sync::Arc<dyn Provider>,
    instructions: String,
}

impl std::fmt::Debug for LlmDescriber {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LlmDescriber").finish_non_exhaustive()
    }
}

impl LlmDescriber {
    pub fn new(provider: std::sync::Arc<dyn Provider>, instructions: String) -> Self {
        Self {
            provider,
            instructions,
        }
    }
}

#[async_trait]
impl Describer for LlmDescriber {
    async fn describe(&self, bytes: &[u8], mime: &str) -> Result<String, DescribeError> {
        let conversation = Conversation::new(vec![
            ConvItem::Message(Message {
                role: Role::System,
                content: vec![Content::Text(self.instructions.clone())],
            }),
            ConvItem::Message(Message {
                role: Role::User,
                content: vec![Content::Image {
                    key: KEY.to_owned(),
                }],
            }),
        ]);
        let store = OnePicture {
            mime: mime.to_owned(),
            bytes: bytes.to_vec(),
        };
        let request = Request {
            conversation: &conversation,
            tools: &[],
            tool_choice: ToolChoice::None,
            parallel_tool_calls: false,
            // A description is a sentence or two; there is nothing to reason through.
            reasoning: ReasoningEffort::Off,
            continuation: None,
            media: Some(&store),
        };
        let stream = self.provider.stream(request).await.map_err(classify)?;
        let response = collect(stream).await.map_err(classify)?;
        let text = clean_description(&response.turn.text());
        if text.is_empty() {
            Err(DescribeError::Failed(
                "the model returned no description".into(),
            ))
        } else {
            Ok(text)
        }
    }
}

fn classify(error: LlmError) -> DescribeError {
    match error {
        // The backend looked at the picture and will not take it; asking again changes nothing.
        LlmError::ContentFiltered | LlmError::InvalidRequest(_) => DescribeError::Declined,
        other => DescribeError::Failed(other.to_string()),
    }
}
