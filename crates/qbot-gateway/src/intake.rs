//! What the pipeline needs from the archive.

use async_trait::async_trait;
use qbot_agent::EnvError;
use qbot_context::ChatLine;
use qbot_core::{AccountId, GroupId, MessageId, UnixMillis};
use qbot_store::{Appended, MediaRefRow, NewLine, NewSpeaker, PgArchive};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Incoming {
    pub group: GroupId,
    pub message: MessageId,
    /// `None` for the bot itself.
    pub author: Option<AccountId>,
    pub at: UnixMillis,
    pub text: String,
    /// Accounts whose `[at:ACCOUNT]` markers appear in `text`.
    pub mentions: Vec<AccountId>,
    /// References to the line's pictures, stickers and clips, stored with it.
    pub media: Vec<MediaRefRow>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Stored {
    /// Archived as the group's `ordinal`-th line.
    New { line: ChatLine, ordinal: u64 },
    /// The message id was already archived; nothing happened and nothing should follow from it.
    Duplicate,
}

#[async_trait]
pub trait Intake: Send + Sync {
    /// Archive one line. This is the only dedup gate: a redelivered event is `Duplicate`.
    async fn append(&self, line: Incoming) -> Result<Stored, EnvError>;
    async fn is_bot_message(&self, group: GroupId, message: MessageId) -> Result<bool, EnvError>;
}

#[async_trait]
impl Intake for PgArchive {
    async fn append(&self, line: Incoming) -> Result<Stored, EnvError> {
        let speaker = line.author.map_or(NewSpeaker::Bot, NewSpeaker::Member);
        let appended = PgArchive::append_line(
            self,
            NewLine {
                group: line.group,
                message: line.message,
                speaker,
                at: line.at,
                text: line.text,
            },
            &line.mentions,
            &line.media,
        )
        .await
        .map_err(|e| EnvError(e.to_string()))?;
        Ok(match appended {
            Appended::Stored { line, ordinal, .. } => Stored::New { line, ordinal },
            Appended::Duplicate => Stored::Duplicate,
        })
    }

    async fn is_bot_message(&self, group: GroupId, message: MessageId) -> Result<bool, EnvError> {
        PgArchive::is_bot_message(self, group, message)
            .await
            .map_err(|e| EnvError(e.to_string()))
    }
}
