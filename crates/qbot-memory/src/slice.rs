use qbot_core::{AccountId, MessageId, UnixMillis};

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
    /// How the line is shown to a model: who said it, then the text.
    pub fn render(&self) -> String {
        let who = match (self.speaker, self.member_no) {
            (None, _) => "bot".to_owned(),
            (Some(_), Some(n)) => format!("member:{n}"),
            (Some(a), None) => format!("account:{}", a.get()),
        };
        format!("[{who}] {}", self.text)
    }
}
