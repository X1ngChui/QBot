//! Identity: who is who.
//!
//! An *account* is a platform login. Accounts are *linked* in a group: a link is a record of that
//! group, and a person in a group is the accounts linked to each other there, directly or through
//! others. Nothing about a link reaches another group. Names are not identity records: they are
//! uses, kept by [`crate::names`].

use std::time::Duration;

use qbot_core::{AccountId, GroupId, MessageId, UnixMillis};

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
    #[error("the account is not linked to another account in this group")]
    NotLinked,
    #[error("unknown account")]
    UnknownAccount,
    #[error(transparent)]
    Link(#[from] LinkError),
    #[error("storage failure: {0}")]
    Backend(String),
}
