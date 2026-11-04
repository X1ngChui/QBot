//! The storage port for identity, plus a reference in-memory implementation.

use std::collections::BTreeSet;
use std::sync::{Mutex, PoisonError};

use async_trait::async_trait;
use qbot_core::{AccountId, GroupId, MessageId, UnixMillis};

use crate::identity::{
    IdentityError, IdentityPolicy, Invitation, InvitationState, LinkError, LinkOutcome,
};

#[async_trait]
pub trait IdentityStore: Send + Sync {
    /// Make sure the account exists. Idempotent.
    async fn seen(&self, account: AccountId, at: UnixMillis) -> Result<(), IdentityError>;

    async fn is_known(&self, account: AccountId) -> Result<bool, IdentityError>;

    /// The accounts linked to `account` in `group`, directly or through others, with `account`
    /// itself; ascending. Links made in other groups play no part.
    async fn linked(
        &self,
        group: GroupId,
        account: AccountId,
    ) -> Result<Vec<AccountId>, IdentityError>;

    /// Link two accounts in `group`. Linking accounts already linked there is not an error.
    async fn link(
        &self,
        group: GroupId,
        a: AccountId,
        b: AccountId,
        at: UnixMillis,
    ) -> Result<LinkOutcome, IdentityError>;

    /// Remove every link of `account` in `group`. The accounts it connected stay linked only
    /// where they have links of their own.
    async fn unlink(&self, group: GroupId, account: AccountId) -> Result<(), IdentityError>;

    async fn invite(
        &self,
        group: GroupId,
        initiator: AccountId,
        target: AccountId,
        message: MessageId,
        now: UnixMillis,
    ) -> Result<Invitation, IdentityError>;

    /// The target confirms in `message`. Applying links the two accounts in the group.
    /// Confirming again in the same message is a no-op that reports them already linked.
    async fn confirm(
        &self,
        group: GroupId,
        confirming: AccountId,
        message: MessageId,
        now: UnixMillis,
    ) -> Result<LinkOutcome, IdentityError>;

    /// Cancel the pending invitation the account is part of. Returns whether there was one.
    async fn cancel_invitation(
        &self,
        group: GroupId,
        account: AccountId,
    ) -> Result<bool, IdentityError>;
}

#[derive(Default)]
struct Inner {
    accounts: BTreeSet<AccountId>,
    /// `(group, lower account, higher account)`.
    links: BTreeSet<(GroupId, AccountId, AccountId)>,
    invitations: Vec<InvitationRow>,
}

struct InvitationRow {
    invitation: Invitation,
    confirmed_by: Option<MessageId>,
}

pub struct MemoryIdentityStore {
    policy: IdentityPolicy,
    inner: Mutex<Inner>,
}

impl std::fmt::Debug for MemoryIdentityStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryIdentityStore")
            .finish_non_exhaustive()
    }
}

impl MemoryIdentityStore {
    pub fn new(policy: IdentityPolicy) -> Self {
        Self {
            policy,
            inner: Mutex::default(),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Inner {
    fn known(&self, account: AccountId) -> Result<(), IdentityError> {
        if self.accounts.contains(&account) {
            Ok(())
        } else {
            Err(IdentityError::UnknownAccount)
        }
    }

    fn linked(&self, group: GroupId, account: AccountId) -> Vec<AccountId> {
        let mut found = BTreeSet::from([account]);
        let mut frontier = vec![account];
        while let Some(next) = frontier.pop() {
            for (g, low, high) in &self.links {
                let other = match (*low == next, *high == next) {
                    (true, _) => *high,
                    (_, true) => *low,
                    _ => continue,
                };
                if *g == group && found.insert(other) {
                    frontier.push(other);
                }
            }
        }
        found.into_iter().collect()
    }

    fn link(&mut self, group: GroupId, a: AccountId, b: AccountId) -> LinkOutcome {
        if self.linked(group, a).contains(&b) {
            return LinkOutcome::AlreadyLinked;
        }
        self.links.insert((group, a.min(b), a.max(b)));
        LinkOutcome::Linked
    }
}

#[async_trait]
impl IdentityStore for MemoryIdentityStore {
    async fn seen(&self, account: AccountId, _at: UnixMillis) -> Result<(), IdentityError> {
        self.lock().accounts.insert(account);
        Ok(())
    }

    async fn is_known(&self, account: AccountId) -> Result<bool, IdentityError> {
        Ok(self.lock().accounts.contains(&account))
    }

    async fn linked(
        &self,
        group: GroupId,
        account: AccountId,
    ) -> Result<Vec<AccountId>, IdentityError> {
        Ok(self.lock().linked(group, account))
    }

    async fn link(
        &self,
        group: GroupId,
        a: AccountId,
        b: AccountId,
        _at: UnixMillis,
    ) -> Result<LinkOutcome, IdentityError> {
        if a == b {
            return Err(LinkError::SelfLink.into());
        }
        let mut inner = self.lock();
        inner.known(a)?;
        inner.known(b)?;
        Ok(inner.link(group, a, b))
    }

    async fn unlink(&self, group: GroupId, account: AccountId) -> Result<(), IdentityError> {
        let mut inner = self.lock();
        inner.known(account)?;
        let before = inner.links.len();
        inner
            .links
            .retain(|(g, low, high)| !(*g == group && (*low == account || *high == account)));
        if inner.links.len() == before {
            return Err(IdentityError::NotLinked);
        }
        Ok(())
    }

    async fn invite(
        &self,
        group: GroupId,
        initiator: AccountId,
        target: AccountId,
        message: MessageId,
        now: UnixMillis,
    ) -> Result<Invitation, IdentityError> {
        let ttl = self.policy.invitation_ttl;
        if initiator == target {
            return Err(LinkError::SelfLink.into());
        }
        let mut inner = self.lock();
        inner.known(initiator)?;
        inner.known(target)?;
        let initiator_linked = inner.linked(group, initiator);
        if initiator_linked.contains(&target) {
            return Err(LinkError::AlreadyLinked.into());
        }
        for row in &mut inner.invitations {
            if row.invitation.state == InvitationState::Pending
                && row.invitation.is_expired(now, ttl)
            {
                row.invitation.state = InvitationState::Expired;
            }
        }
        let involves = |i: &Invitation| {
            i.group == group
                && i.state == InvitationState::Pending
                && [i.initiator, i.target]
                    .iter()
                    .any(|a| *a == initiator || *a == target)
        };
        if inner.invitations.iter().any(|r| involves(&r.invitation)) {
            return Err(LinkError::Busy.into());
        }
        let invitation = Invitation {
            group,
            initiator,
            target,
            created_by: message,
            created_at: now,
            initiator_linked,
            target_linked: inner.linked(group, target),
            state: InvitationState::Pending,
        };
        inner.invitations.push(InvitationRow {
            invitation: invitation.clone(),
            confirmed_by: None,
        });
        Ok(invitation)
    }

    async fn confirm(
        &self,
        group: GroupId,
        confirming: AccountId,
        message: MessageId,
        now: UnixMillis,
    ) -> Result<LinkOutcome, IdentityError> {
        let ttl = self.policy.invitation_ttl;
        let mut inner = self.lock();
        // Replaying the confirming message is a no-op.
        if inner.invitations.iter().any(|r| {
            r.invitation.group == group
                && r.invitation.state == InvitationState::Applied
                && r.confirmed_by == Some(message)
                && r.invitation.target == confirming
        }) {
            return Ok(LinkOutcome::AlreadyLinked);
        }
        let index = inner
            .invitations
            .iter()
            .position(|r| {
                r.invitation.group == group
                    && r.invitation.state == InvitationState::Pending
                    && [r.invitation.initiator, r.invitation.target].contains(&confirming)
            })
            .ok_or(LinkError::NoInvitation)?;
        let invitation = inner.invitations[index].invitation.clone();
        let (now_initiator, now_target) = (
            inner.linked(group, invitation.initiator),
            inner.linked(group, invitation.target),
        );
        match invitation.check_confirm(confirming, now, ttl, &now_initiator, &now_target) {
            Ok(()) => {}
            Err(error @ (LinkError::Expired | LinkError::Stale)) => {
                inner.invitations[index].invitation.state = if error == LinkError::Expired {
                    InvitationState::Expired
                } else {
                    InvitationState::Cancelled
                };
                return Err(error.into());
            }
            Err(error) => return Err(error.into()),
        }
        let outcome = inner.link(group, invitation.initiator, invitation.target);
        let row = &mut inner.invitations[index];
        row.invitation.state = InvitationState::Applied;
        row.confirmed_by = Some(message);
        Ok(outcome)
    }

    async fn cancel_invitation(
        &self,
        group: GroupId,
        account: AccountId,
    ) -> Result<bool, IdentityError> {
        let mut inner = self.lock();
        let Some(row) = inner.invitations.iter_mut().find(|r| {
            r.invitation.group == group
                && r.invitation.state == InvitationState::Pending
                && [r.invitation.initiator, r.invitation.target].contains(&account)
        }) else {
            return Ok(false);
        };
        row.invitation.state = InvitationState::Cancelled;
        Ok(true)
    }
}
