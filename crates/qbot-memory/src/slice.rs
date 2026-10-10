use async_trait::async_trait;
use jiff::Timestamp;
use jiff::tz::TimeZone;
use qbot_core::{AccountId, GroupId, MessageId, UnixMillis};

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
/// is, rendered, and the time zone the lines' times are shown in.
#[derive(Debug, Clone)]
pub struct Background {
    /// Empty when nothing is known.
    pub text: String,
    pub zone: TimeZone,
}

impl Default for Background {
    fn default() -> Self {
        Self {
            text: String::new(),
            zone: TimeZone::UTC,
        }
    }
}

/// Where a slice's background comes from: the records about its members and the group.
#[async_trait]
pub trait SliceBackground: Send + Sync {
    /// The background for a slice of `group` whose lines (context included) involve `members`:
    /// each member number, with its account when the lines show it.
    async fn background(
        &self,
        group: GroupId,
        members: &[(u32, Option<AccountId>)],
    ) -> Result<Background, MemoryError>;
}
