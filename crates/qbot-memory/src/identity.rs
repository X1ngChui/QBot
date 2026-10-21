//! Identity: who is who.
//!
//! An *account* is a platform login. Accounts are *linked* in a group: a link is a record of that
//! group, and a person in a group is the accounts linked to each other there, directly or through
//! others. Nothing about a link reaches another group. Names (*aliases*) are scoped to a group and
//! point at an account, each backed by evidence whose strength is fused here, in one place,
//! rather than in SQL.

use std::collections::BTreeSet;
use std::time::Duration;

use qbot_core::{AccountId, GroupId, MessageId, UnixMillis};
use serde::{Deserialize, Serialize};
use unicode_normalization::UnicodeNormalization;

use crate::belief::{credibility, persistence};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AliasId(pub i64);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AliasStatus {
    /// A lead: learned from chat, nobody vouched for it. It does not resolve the name.
    Candidate,
    /// A person vouched for it (`/name`): it resolves the name.
    Confirmed,
    /// Removed by a person.
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
    /// A person set it: an assignment, not an inference.
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

/// Deployment policy for identity.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct IdentityPolicy {
    /// How long an invitation to link accounts stays open.
    pub invitation_ttl: Duration,
}

impl Default for IdentityPolicy {
    fn default() -> Self {
        Self {
            invitation_ttl: Duration::from_secs(600),
        }
    }
}

/// The credibility of a name from its evidence (`crate::belief::credibility`): 1 for a name a
/// person set, otherwise that of its distinct supporting episodes; 0 without evidence. How long
/// ago it was supported is left to [`Alias::confidence_at`].
pub fn confidence(evidence: &[EvidenceRecord]) -> f32 {
    if evidence.iter().any(|e| e.kind == EvidenceKind::Manual) {
        return 1.0;
    }
    let episodes: BTreeSet<Option<i64>> = evidence
        .iter()
        .filter(|e| e.kind == EvidenceKind::Extracted)
        .map(|e| e.support)
        .collect();
    if episodes.is_empty() {
        return 0.0;
    }
    // In 0.5..1, so the narrowing is harmless.
    credibility(u32::try_from(episodes.len()).unwrap_or(u32::MAX)) as f32
}

/// Confirmed is provenance, not a threshold: a name is confirmed when a person vouched for it.
pub fn status_for(evidence: &[EvidenceRecord]) -> AliasStatus {
    if evidence.iter().any(|e| e.kind == EvidenceKind::Manual) {
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
    pub account: AccountId,
    pub status: AliasStatus,
    /// Credibility from the evidence ([`confidence`]).
    pub confidence: f32,
    pub last_evidence: UnixMillis,
}

impl Alias {
    /// The probability that the name is theirs at `now`: a confirmed name stands until removed;
    /// a lead's credibility ages with `half_life` (that of stable facts) since its last support.
    pub fn confidence_at(&self, half_life: Duration, now: UnixMillis) -> f64 {
        let credibility = f64::from(self.confidence);
        match self.status {
            AliasStatus::Confirmed => credibility,
            AliasStatus::Candidate => credibility * persistence(half_life, self.last_evidence, now),
            AliasStatus::Inactive => 0.0,
        }
    }
}

/// What a name refers to in a group, among confirmed aliases only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    Unknown,
    One(AccountId),
    /// More than one confirmed account; the caller must not guess.
    Ambiguous(Vec<AccountId>),
}

impl Resolution {
    pub fn from_targets(mut targets: Vec<AccountId>) -> Self {
        targets.sort();
        targets.dedup();
        match targets.len() {
            0 => Resolution::Unknown,
            1 => Resolution::One(targets[0]),
            _ => Resolution::Ambiguous(targets),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkOutcome {
    /// The two accounts were already one person in the group.
    AlreadyLinked,
    Linked,
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
    /// The accounts each side was linked to in the group when it was created, with itself.
    pub initiator_linked: Vec<AccountId>,
    pub target_linked: Vec<AccountId>,
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
    #[error(
        "an account involved was linked or unlinked since the invitation, so it no longer applies"
    )]
    Stale,
}

impl Invitation {
    pub fn is_expired(&self, now: UnixMillis, ttl: Duration) -> bool {
        now.since(self.created_at) >= ttl
    }

    /// The rules for a confirmation, independent of storage. A confirmation cannot be replayed:
    /// the archive runs each message once, and a confirmed invitation is no longer pending. Message
    /// ids say nothing about order (the platform's are hashes), so they are not compared.
    pub fn check_confirm(
        &self,
        confirming: AccountId,
        now: UnixMillis,
        ttl: Duration,
        initiator_linked: &[AccountId],
        target_linked: &[AccountId],
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
        if initiator_linked != self.initiator_linked || target_linked != self.target_linked {
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
    #[error("the account is not linked to another account in this group")]
    NotLinked,
    #[error("unknown account")]
    UnknownAccount,
    #[error(transparent)]
    Link(#[from] LinkError),
    #[error("storage failure: {0}")]
    Backend(String),
}
