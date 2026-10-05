use qbot_core::{AccountId, MemberNo, MessageId, UnixMillis};
use serde::{Deserialize, Serialize};

/// Who wrote a chat line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Speaker {
    Bot,
    /// A member: the platform account, and its number in this group (assigned on the account's
    /// first appearance in the group and never changed).
    Member {
        account: AccountId,
        number: MemberNo,
    },
}

/// One archived group line, in the form the model sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatLine {
    pub message: MessageId,
    pub speaker: Speaker,
    pub at: UnixMillis,
    pub text: String,
}

/// A run of group lines as the model sees them, in archive order.
///
/// Every archived line is included, whoever wrote it. Blocking a member only stops that
/// member's own message from starting a run; it never removes their lines from context.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatBatch {
    lines: Vec<ChatLine>,
}

impl ChatBatch {
    pub fn new(lines: Vec<ChatLine>) -> Self {
        Self { lines }
    }

    pub fn lines(&self) -> &[ChatLine] {
        &self.lines
    }

    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }
}
