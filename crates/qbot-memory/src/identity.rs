//! Identity: who is who.
//!
//! An *account* is a platform login. A *holder* is the person behind one or more accounts.
//! Linking accounts merges their holders; splitting moves one account to a fresh holder. Names
//! (*aliases*) are scoped to a group and point at an account or at a holder, each backed by
//! evidence whose strength is fused here, in one place, rather than in SQL.

use std::collections::BTreeSet;
use std::time::Duration;

use qbot_core::{AccountId, GroupId, MessageId, UnixMillis};
use serde::{Deserialize, Serialize};
use unicode_normalization::UnicodeNormalization;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct HolderId(pub i64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AliasId(pub i64);

/// What a name refers to. An account target follows that one login; a holder target follows the
/// person across every account they have linked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum AliasTarget {
    Account(AccountId),
    Holder(HolderId),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AliasStatus {
    /// Some evidence, not enough to resolve the name.
    Candidate,
    /// Strong enough to resolve the name.
    Confirmed,
    /// Removed by a person, or expired unused.
    Inactive,
}

/// Why we believe a stored name refers to a target.
///
/// The member's current group display name is not stored evidence: it is the platform's own
/// answer, read live when needed (`qbot_agent::Directory`) and taking precedence over every
/// stored name. Stored names are what people call someone besides that: names set by hand and
/// names learned from chat.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvidenceKind {
    /// A person set it. Decisive.
    Manual,
    /// A model proposed it from chat, supported by a validated quote in an episode.
    Extracted,
}

/// One piece of evidence. `support` distinguishes independent observations of the same kind: for
/// `Extracted`, the episode that supports the name, so repeating one episode adds nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EvidenceRecord {
    pub kind: EvidenceKind,
    pub support: Option<i64>,
}

/// Deployment policy for identity. These are choices an operator can reasonably make; the
/// evidence weights below are tuning internal to the fusion rule and stay constants.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct IdentityPolicy {
    /// A name is confirmed from this confidence up.
    pub confirm_at: f32,
    /// How long an invitation to link accounts stays open.
    pub invitation_ttl: Duration,
}

impl Default for IdentityPolicy {
    fn default() -> Self {
        Self {
            confirm_at: 0.75,
            invitation_ttl: Duration::from_secs(600),
        }
    }
}

const MANUAL: f32 = 1.0;
const EXTRACTED_EACH: f32 = 0.25;
const EXTRACTED_CAP: f32 = 0.7;

/// Fuse evidence into one confidence in `0.0..=1.0`.
///
/// Each kind is a channel with its own strength, and channels combine as independent evidence
/// (`1 - prod(1 - c)`). Within `Extracted`, each distinct supporting episode counts once and
/// they accumulate toward a cap below the confirmation line, so extraction alone never confirms
/// a name: only a person does.
pub fn confidence(evidence: &[EvidenceRecord]) -> f32 {
    let manual = evidence.iter().any(|e| e.kind == EvidenceKind::Manual);
    let extracted: BTreeSet<Option<i64>> = evidence
        .iter()
        .filter(|e| e.kind == EvidenceKind::Extracted)
        .map(|e| e.support)
        .collect();
    let extracted_c = if extracted.is_empty() {
        0.0
    } else {
        (1.0 - (1.0 - EXTRACTED_EACH).powi(i32::try_from(extracted.len()).unwrap_or(i32::MAX)))
            .min(EXTRACTED_CAP)
    };
    let none = (1.0 - if manual { MANUAL } else { 0.0 }) * (1.0 - extracted_c);
    1.0 - none
}

pub fn status_for(confidence: f32, policy: &IdentityPolicy) -> AliasStatus {
    if confidence >= policy.confirm_at {
        AliasStatus::Confirmed
    } else {
        AliasStatus::Candidate
    }
}

/// The longest name stored. A name is a few words; this keeps absurd input out of the index.
pub const MAX_ALIAS_CHARS: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AliasTextError {
    #[error("a name must not be empty")]
    Empty,
    #[error("a name is at most {MAX_ALIAS_CHARS} characters")]
    TooLong,
}

/// Canonical form used for matching: compatibility-normalized (full-width and half-width forms
/// agree), whitespace collapsed, lower-cased.
pub fn normalize_alias(text: &str) -> Result<String, AliasTextError> {
    let folded: String = text.nfkc().collect::<String>().to_lowercase();
    let collapsed = folded.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() {
        return Err(AliasTextError::Empty);
    }
    if collapsed.chars().count() > MAX_ALIAS_CHARS {
        return Err(AliasTextError::TooLong);
    }
    Ok(collapsed)
}

#[derive(Debug, Clone, PartialEq)]
pub struct Alias {
    pub id: AliasId,
    pub group: GroupId,
    /// Normalized text.
    pub text: String,
    pub target: AliasTarget,
    pub status: AliasStatus,
    pub confidence: f32,
    pub last_evidence: UnixMillis,
}

/// What a name refers to in a group, among confirmed aliases only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    Unknown,
    One(AliasTarget),
    /// More than one confirmed target; the caller must not guess.
    Ambiguous(Vec<AliasTarget>),
}

impl Resolution {
    pub fn from_targets(mut targets: Vec<AliasTarget>) -> Self {
        targets.sort();
        targets.dedup();
        match targets.len() {
            0 => Resolution::Unknown,
            1 => Resolution::One(targets[0]),
            _ => Resolution::Ambiguous(targets),
        }
    }
}

/// A person behind one or more accounts, with a revision that changes whenever the set of
/// accounts does, so a confirmation made against an older state can be rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Holder {
    pub id: HolderId,
    pub revision: u32,
    pub accounts: Vec<AccountId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeOutcome {
    /// The two accounts were already one person.
    AlreadyLinked(HolderId),
    Merged {
        winner: HolderId,
        loser: HolderId,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvitationState {
    Pending,
    Applied,
    Cancelled,
    Expired,
}

/// An invitation from one account to link with another, in one group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invitation {
    pub group: GroupId,
    pub initiator: AccountId,
    pub target: AccountId,
    /// The message that created it; a confirmation must come after it.
    pub created_by: MessageId,
    pub created_at: UnixMillis,
    /// Each side's holder revision when it was created.
    pub initiator_revision: u32,
    pub target_revision: u32,
    pub state: InvitationState,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LinkError {
    #[error("an account cannot link with itself")]
    SelfLink,
    #[error("they are already linked")]
    AlreadyLinked,
    #[error("one of the two already has a pending invitation in this group")]
    Busy,
    #[error("there is no pending invitation for this account")]
    NoInvitation,
    #[error("the invitation has expired")]
    Expired,
    #[error("only the invited account can confirm")]
    NotTheTarget,
    #[error("the confirmation does not come after the invitation")]
    OutOfOrder,
    #[error(
        "an account involved was linked or split since the invitation, so it no longer applies"
    )]
    Stale,
}

impl Invitation {
    pub fn is_expired(&self, now: UnixMillis, ttl: Duration) -> bool {
        now.since(self.created_at) >= ttl
    }

    /// The rules for a confirmation, independent of storage. `confirming_message` is the message
    /// in which the target confirmed; message ids increase with time.
    pub fn check_confirm(
        &self,
        confirming: AccountId,
        confirming_message: MessageId,
        now: UnixMillis,
        ttl: Duration,
        initiator_revision: u32,
        target_revision: u32,
    ) -> Result<(), LinkError> {
        if self.state != InvitationState::Pending {
            return Err(LinkError::NoInvitation);
        }
        if confirming != self.target {
            return Err(LinkError::NotTheTarget);
        }
        if self.is_expired(now, ttl) {
            return Err(LinkError::Expired);
        }
        if confirming_message <= self.created_by {
            return Err(LinkError::OutOfOrder);
        }
        if initiator_revision != self.initiator_revision || target_revision != self.target_revision
        {
            return Err(LinkError::Stale);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IdentityError {
    #[error(transparent)]
    Name(#[from] AliasTextError),
    /// A person tried to give a name that already refers to someone else in the group.
    #[error("that name already refers to someone else in this group")]
    NameTaken,
    #[error("the account is not linked to any other account")]
    NotLinked,
    #[error("unknown account")]
    UnknownAccount,
    #[error(transparent)]
    Link(#[from] LinkError),
    #[error("storage failure: {0}")]
    Backend(String),
}
