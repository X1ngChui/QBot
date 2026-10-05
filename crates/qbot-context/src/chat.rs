use qbot_core::{AccountId, MemberNo, MessageId, UnixMillis};
use serde::{Deserialize, Serialize};

/// Whether the bot may reply to a member. Blocked members stay visible as context; the marker
/// lets the prompt tell the model not to address them. Blocking itself is enforced at admission.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemberStanding {
    Normal,
    Blocked,
}

/// Who wrote a chat line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Speaker {
    Bot,
    Member {
        account: AccountId,
        number: MemberNo,
        standing: MemberStanding,
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
