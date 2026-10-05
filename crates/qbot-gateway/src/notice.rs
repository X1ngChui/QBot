//! Group notices as archived marker lines.
//!
//! A notice has no platform message id, so it gets a synthetic one derived from its content.
//! Synthetic ids live in `[SYNTHETIC_BASE, 2^62)`, far above anything the platform issues, and
//! redelivery of the same notice maps to the same id, so the archive's dedup applies.

use qbot_core::MessageId;
use sha2::{Digest, Sha256};

use crate::wire::{GroupNotice, NoticeKind};

const SYNTHETIC_BASE: i64 = 1 << 52;
const SYNTHETIC_SPAN: u64 = (1 << 62) - (1 << 52);

pub fn archive_text(notice: &GroupNotice) -> String {
    match notice.kind {
        NoticeKind::Recall { by_admin: true } => "[notice:message_recalled_by_admin]".into(),
        NoticeKind::Recall { by_admin: false } => "[notice:recalled_own_message]".into(),
        NoticeKind::Joined => "[notice:joined_group]".into(),
        NoticeKind::Left { kicked: true } => "[notice:removed_from_group]".into(),
        NoticeKind::Left { kicked: false } => "[notice:left_group]".into(),
        NoticeKind::Ban { seconds: 0 } => "[notice:unmuted]".into(),
        NoticeKind::Ban { seconds } => format!("[notice:muted:{seconds}s]"),
        NoticeKind::Poke {
            target_is_bot: true,
        } => "[notice:poked_you]".into(),
        NoticeKind::Poke {
            target_is_bot: false,
        } => "[notice:poked_someone_else]".into(),
    }
}

/// The notice's archive key; stable under redelivery.
pub fn archive_id(notice: &GroupNotice) -> MessageId {
    let mut hasher = Sha256::new();
    hasher.update(archive_text(notice).as_bytes());
    hasher.update(notice.group.get().to_be_bytes());
    hasher.update(notice.subject.get().to_be_bytes());
    hasher.update(notice.at.get().to_be_bytes());
    hasher.update(notice.discriminator.as_bytes());
    let digest = hasher.finalize();
    let mut head = [0u8; 8];
    head.copy_from_slice(&digest[..8]);
    let offset = i64::try_from(u64::from_be_bytes(head) % SYNTHETIC_SPAN).unwrap_or(0);
    MessageId::new(SYNTHETIC_BASE + offset)
        .unwrap_or_else(|_| unreachable!("the synthetic range is positive"))
}
