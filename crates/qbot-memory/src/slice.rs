use async_trait::async_trait;
use jiff::Timestamp;
use jiff::tz::TimeZone;
use qbot_core::{AccountId, GroupId, MessageId, UnixMillis};

use crate::memories::MemoryId;
use crate::store::MemoryError;

/// An archived line as the memory pipeline sees it. `ordinal` is the dense per-group number the
/// batch grid is built on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SliceLine {
    pub ordinal: u64,
    pub message: MessageId,
    /// `None` for the bot's own lines.
    pub speaker: Option<AccountId>,
    /// The speaker's stable member number in the group, for display.
    pub member_no: Option<u32>,
    pub at: UnixMillis,
    pub text: String,
    /// The display name the line was sent under (a member's group nickname, else account
    /// nickname), when the archive has it.
    pub shown: Option<String>,
}

impl SliceLine {
    /// How the line is shown to a model, as the reply model reads chat:
    /// `[msg:ID] MM-DD HH:MM member:N: text`, the time local to `zone`.
    pub fn render(&self, zone: &TimeZone) -> String {
        let who = match (self.speaker, self.member_no) {
            (None, _) => "bot".to_owned(),
            (Some(_), Some(n)) => format!("member:{n}"),
            (Some(a), None) => format!("account:{}", a.get()),
        };
        let time = Timestamp::from_millisecond(self.at.get())
            .map(|t| t.to_zoned(zone.clone()).strftime("%m-%d %H:%M").to_string())
            .unwrap_or_default();
        format!("[msg:{}] {time} {who}: {}", self.message.get(), self.text)
    }
}

/// What the extractor is told besides the lines: who the people in them are and what the group
/// is, rendered, the time zone the lines' times are shown in, and the chat the lines continue.
#[derive(Debug, Clone)]
pub struct Background {
    /// Empty when nothing is known.
    pub text: String,
    pub zone: TimeZone,
    /// The title and summary of the group's episode just before the slice: what the
    /// conversation was about when the slice begins.
    pub earlier: Option<(String, String)>,
    /// Lines the slice and its context quote (`[reply:ID]`) from before what is shown, oldest
    /// first: without them a reply such as "agreed" means nothing.
    pub quoted: Vec<SliceLine>,
    /// What members call the bot (its persona's name, its nicknames): never a member's name.
    pub bot_names: Vec<String>,
    /// Accounts of the members `text` lists, by number: whom a line may be recorded as naming
    /// besides those who wrote the lines.
    pub accounts: Vec<(u32, AccountId)>,
    /// The current memories `text` lists, with their subjects: the ones a statement may name.
    pub known: Vec<(MemoryId, Option<AccountId>)>,
}

impl Default for Background {
    fn default() -> Self {
        Self {
            text: String::new(),
            zone: TimeZone::UTC,
            earlier: None,
            quoted: Vec::new(),
            bot_names: Vec::new(),
            accounts: Vec::new(),
            known: Vec::new(),
        }
    }
}

/// A member the lines of a slice involve.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SliceMember {
    pub number: u32,
    /// Known when the member wrote one of the lines.
    pub account: Option<AccountId>,
    /// The display names their lines were sent under, in order of first appearance.
    pub shown: Vec<String>,
}

/// Where a slice's background comes from: the records about its members and the group.
#[async_trait]
pub trait SliceBackground: Send + Sync {
    /// The background for a slice of `group` whose lines (context included) involve `members`,
    /// by member number.
    async fn background(
        &self,
        group: GroupId,
        members: &[SliceMember],
    ) -> Result<Background, MemoryError>;
}
