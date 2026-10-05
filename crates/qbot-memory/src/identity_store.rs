//! The storage port for identity, plus a reference in-memory implementation.

use std::collections::BTreeMap;
use std::sync::{Mutex, PoisonError};

use async_trait::async_trait;
use qbot_core::{AccountId, GroupId, MessageId, UnixMillis};

use crate::identity::{
    Alias, AliasId, AliasStatus, AliasTarget, EvidenceKind, EvidenceRecord, Holder, HolderId,
    IdentityError, IdentityPolicy, Invitation, InvitationState, LinkError, MergeOutcome,
    Resolution, confidence, normalize_alias, status_for,
};

#[async_trait]
pub trait IdentityStore: Send + Sync {
    /// Make sure the account exists, with a holder of its own if it is new. Idempotent.
    async fn seen(&self, account: AccountId, at: UnixMillis) -> Result<Holder, IdentityError>;

    async fn holder_of(&self, account: AccountId) -> Result<Option<Holder>, IdentityError>;

    /// Link two accounts into one holder. The older holder (then the lower id) wins; both
    /// revisions change. Linking accounts already linked is not an error.
    async fn merge(&self, a: AccountId, b: AccountId) -> Result<MergeOutcome, IdentityError>;

    /// Move one account to a fresh holder. Holder-scoped records stay with the original holder.
    async fn split(&self, account: AccountId, at: UnixMillis) -> Result<Holder, IdentityError>;

    /// Record evidence that `text` refers to `target` in `group`, creating the alias if needed
    /// and recomputing its confidence and status. The same evidence recorded twice counts once.
    async fn add_evidence(
        &self,
        group: GroupId,
        text: &str,
        target: AliasTarget,
        evidence: EvidenceRecord,
        at: UnixMillis,
    ) -> Result<Alias, IdentityError>;

    /// A person gives a name: manual evidence. Refused if the name is already confirmed for a
    /// different target in the group.
    async fn set_name(
        &self,
        group: GroupId,
        text: &str,
        target: AliasTarget,
        at: UnixMillis,
    ) -> Result<Alias, IdentityError>;

    /// A person removes a name from a target. Returns whether one was active.
    async fn remove_name(
        &self,
        group: GroupId,
        text: &str,
        target: AliasTarget,
    ) -> Result<bool, IdentityError>;

    /// What the name refers to in the group, from confirmed aliases only. Holder targets are
    /// reported as the current holder of the merge chain.
    async fn resolve(&self, group: GroupId, text: &str) -> Result<Resolution, IdentityError>;

    /// The group's aliases for a target, strongest first, inactive ones excluded.
    async fn names_of(
        &self,
        group: GroupId,
        target: AliasTarget,
    ) -> Result<Vec<Alias>, IdentityError>;

    /// Mark candidate aliases with no evidence since `before` inactive, unless a person vouched
    /// for them. Returns how many.
    async fn expire_candidates(&self, before: UnixMillis) -> Result<usize, IdentityError>;

    async fn invite(
        &self,
        group: GroupId,
        initiator: AccountId,
        target: AccountId,
        message: MessageId,
        now: UnixMillis,
    ) -> Result<Invitation, IdentityError>;

    /// The target confirms in `message`. Applying merges the two accounts. Confirming again in
    /// the same message is a no-op that returns the same result.
    async fn confirm(
        &self,
        group: GroupId,
        confirming: AccountId,
        message: MessageId,
        now: UnixMillis,
    ) -> Result<MergeOutcome, IdentityError>;

    /// Cancel the pending invitation the account is part of. Returns whether there was one.
    async fn cancel_invitation(
        &self,
        group: GroupId,
        account: AccountId,
    ) -> Result<bool, IdentityError>;
}

#[derive(Default)]
struct Inner {
    next_holder: i64,
    next_alias: i64,
    holders: BTreeMap<HolderId, HolderRow>,
    accounts: BTreeMap<AccountId, HolderId>,
    aliases: Vec<AliasRow>,
    invitations: Vec<InvitationRow>,
}

struct HolderRow {
    created: UnixMillis,
    revision: u32,
    merged_into: Option<HolderId>,
}

struct AliasRow {
    alias: Alias,
    evidence: Vec<(EvidenceRecord, UnixMillis)>,
    removed: bool,
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
    fn holder(&self, id: HolderId) -> Holder {
        let accounts = self
            .accounts
            .iter()
            .filter(|(_, h)| **h == id)
            .map(|(a, _)| *a)
            .collect();
        Holder {
            id,
            revision: self.holders[&id].revision,
            accounts,
        }
    }

    fn root(&self, mut id: HolderId) -> HolderId {
        while let Some(next) = self.holders[&id].merged_into {
            id = next;
        }
        id
    }

    fn holder_of(&self, account: AccountId) -> Result<HolderId, IdentityError> {
        self.accounts
            .get(&account)
            .copied()
            .ok_or(IdentityError::UnknownAccount)
    }

    fn merge(&mut self, a: AccountId, b: AccountId) -> Result<MergeOutcome, IdentityError> {
        let (ha, hb) = (self.holder_of(a)?, self.holder_of(b)?);
        if ha == hb {
            return Ok(MergeOutcome::AlreadyLinked(ha));
        }
        let key = |h: HolderId| (self.holders[&h].created, h);
        let (winner, loser) = if key(ha) <= key(hb) {
            (ha, hb)
        } else {
            (hb, ha)
        };
        for holder in self.accounts.values_mut().filter(|h| **h == loser) {
            *holder = winner;
        }
        if let Some(h) = self.holders.get_mut(&loser) {
            h.merged_into = Some(winner);
        }
        for id in [winner, loser] {
            if let Some(h) = self.holders.get_mut(&id) {
                h.revision += 1;
            }
        }
        Ok(MergeOutcome::Merged { winner, loser })
    }

    fn recompute(row: &mut AliasRow, policy: &IdentityPolicy) {
        let records: Vec<EvidenceRecord> = row.evidence.iter().map(|(e, _)| *e).collect();
        row.alias.confidence = confidence(&records);
        row.alias.last_evidence = row
            .evidence
            .iter()
            .map(|(_, at)| *at)
            .max()
            .unwrap_or(row.alias.last_evidence);
        row.alias.status = if row.removed {
            AliasStatus::Inactive
        } else {
            status_for(row.alias.confidence, policy)
        };
    }
}

#[async_trait]
impl IdentityStore for MemoryIdentityStore {
    async fn seen(&self, account: AccountId, at: UnixMillis) -> Result<Holder, IdentityError> {
        let mut inner = self.lock();
        if !inner.accounts.contains_key(&account) {
            inner.next_holder += 1;
            let id = HolderId(inner.next_holder);
            inner.holders.insert(
                id,
                HolderRow {
                    created: at,
                    revision: 0,
                    merged_into: None,
                },
            );
            inner.accounts.insert(account, id);
        }
        let id = inner.accounts[&account];
        Ok(inner.holder(id))
    }

    async fn holder_of(&self, account: AccountId) -> Result<Option<Holder>, IdentityError> {
        let inner = self.lock();
        Ok(inner.accounts.get(&account).map(|id| inner.holder(*id)))
    }

    async fn merge(&self, a: AccountId, b: AccountId) -> Result<MergeOutcome, IdentityError> {
        self.lock().merge(a, b)
    }

    async fn split(&self, account: AccountId, at: UnixMillis) -> Result<Holder, IdentityError> {
        let mut inner = self.lock();
        let old = inner.holder_of(account)?;
        if inner.accounts.values().filter(|h| **h == old).count() < 2 {
            return Err(IdentityError::NotLinked);
        }
        inner.next_holder += 1;
        let fresh = HolderId(inner.next_holder);
        inner.holders.insert(
            fresh,
            HolderRow {
                created: at,
                revision: 0,
                merged_into: None,
            },
        );
        inner.accounts.insert(account, fresh);
        if let Some(h) = inner.holders.get_mut(&old) {
            h.revision += 1;
        }
        if let Some(h) = inner.holders.get_mut(&fresh) {
            h.revision += 1;
        }
        Ok(inner.holder(fresh))
    }

    async fn add_evidence(
        &self,
        group: GroupId,
        text: &str,
        target: AliasTarget,
        evidence: EvidenceRecord,
        at: UnixMillis,
    ) -> Result<Alias, IdentityError> {
        let policy = self.policy;
        let text = normalize_alias(text)?;
        let mut inner = self.lock();
        if let AliasTarget::Account(a) = target {
            inner.holder_of(a)?;
        }
        let existing = inner.aliases.iter().position(|r| {
            r.alias.group == group && r.alias.text == text && r.alias.target == target
        });
        let index = match existing {
            Some(i) => i,
            None => {
                inner.next_alias += 1;
                let alias = Alias {
                    id: AliasId(inner.next_alias),
                    group,
                    text,
                    target,
                    status: AliasStatus::Candidate,
                    confidence: 0.0,
                    last_evidence: at,
                };
                inner.aliases.push(AliasRow {
                    alias,
                    evidence: Vec::new(),
                    removed: false,
                });
                inner.aliases.len() - 1
            }
        };
        let row = &mut inner.aliases[index];
        if !row.evidence.iter().any(|(e, _)| *e == evidence) {
            row.evidence.push((evidence, at));
        } else if let Some(slot) = row.evidence.iter_mut().find(|(e, _)| *e == evidence) {
            slot.1 = at;
        }
        row.removed = false;
        Inner::recompute(row, &policy);
        Ok(row.alias.clone())
    }

    async fn set_name(
        &self,
        group: GroupId,
        text: &str,
        target: AliasTarget,
        at: UnixMillis,
    ) -> Result<Alias, IdentityError> {
        let normalized = normalize_alias(text)?;
        {
            let inner = self.lock();
            let taken = inner.aliases.iter().any(|r| {
                r.alias.group == group
                    && r.alias.text == normalized
                    && r.alias.target != target
                    && r.alias.status == AliasStatus::Confirmed
            });
            if taken {
                return Err(IdentityError::NameTaken);
            }
        }
        self.add_evidence(
            group,
            text,
            target,
            EvidenceRecord {
                kind: EvidenceKind::Manual,
                support: None,
            },
            at,
        )
        .await
    }

    async fn remove_name(
        &self,
        group: GroupId,
        text: &str,
        target: AliasTarget,
    ) -> Result<bool, IdentityError> {
        let policy = self.policy;
        let normalized = normalize_alias(text)?;
        let mut inner = self.lock();
        let Some(row) = inner.aliases.iter_mut().find(|r| {
            r.alias.group == group
                && r.alias.text == normalized
                && r.alias.target == target
                && !r.removed
        }) else {
            return Ok(false);
        };
        row.removed = true;
        Inner::recompute(row, &policy);
        Ok(true)
    }

    async fn resolve(&self, group: GroupId, text: &str) -> Result<Resolution, IdentityError> {
        let normalized = normalize_alias(text)?;
        let inner = self.lock();
        let targets = inner
            .aliases
            .iter()
            .filter(|r| {
                r.alias.group == group
                    && r.alias.text == normalized
                    && r.alias.status == AliasStatus::Confirmed
            })
            .map(|r| match r.alias.target {
                AliasTarget::Holder(h) => AliasTarget::Holder(inner.root(h)),
                account => account,
            })
            .collect();
        Ok(Resolution::from_targets(targets))
    }

    async fn names_of(
        &self,
        group: GroupId,
        target: AliasTarget,
    ) -> Result<Vec<Alias>, IdentityError> {
        let inner = self.lock();
        let mut names: Vec<Alias> = inner
            .aliases
            .iter()
            .filter(|r| {
                r.alias.group == group && !r.removed && r.alias.status != AliasStatus::Inactive
            })
            .filter(|r| match (r.alias.target, target) {
                (AliasTarget::Holder(a), AliasTarget::Holder(b)) => inner.root(a) == inner.root(b),
                (a, b) => a == b,
            })
            .map(|r| r.alias.clone())
            .collect();
        names.sort_by(|a, b| {
            b.confidence
                .total_cmp(&a.confidence)
                .then(a.text.cmp(&b.text))
        });
        Ok(names)
    }

    async fn expire_candidates(&self, before: UnixMillis) -> Result<usize, IdentityError> {
        let policy = self.policy;
        let mut inner = self.lock();
        let mut expired = 0;
        for row in &mut inner.aliases {
            let vouched = row
                .evidence
                .iter()
                .any(|(e, _)| e.kind == EvidenceKind::Manual);
            if !row.removed
                && row.alias.status == AliasStatus::Candidate
                && !vouched
                && row.alias.last_evidence < before
            {
                row.removed = true;
                Inner::recompute(row, &policy);
                expired += 1;
            }
        }
        Ok(expired)
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
        let (hi, ht) = (inner.holder_of(initiator)?, inner.holder_of(target)?);
        if hi == ht {
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
            initiator_revision: inner.holders[&hi].revision,
            target_revision: inner.holders[&ht].revision,
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
    ) -> Result<MergeOutcome, IdentityError> {
        let ttl = self.policy.invitation_ttl;
        let mut inner = self.lock();
        // Replaying the confirming message is a no-op.
        if let Some(row) = inner.invitations.iter().find(|r| {
            r.invitation.group == group
                && r.invitation.state == InvitationState::Applied
                && r.confirmed_by == Some(message)
                && r.invitation.target == confirming
        }) {
            // After a merge both accounts share a holder.
            let holder = inner.holder_of(row.invitation.initiator)?;
            return Ok(MergeOutcome::AlreadyLinked(holder));
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
        let (hi, ht) = (
            inner.holder_of(invitation.initiator)?,
            inner.holder_of(invitation.target)?,
        );
        let (ri, rt) = (inner.holders[&hi].revision, inner.holders[&ht].revision);
        match invitation.check_confirm(confirming, message, now, ttl, ri, rt) {
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
        let outcome = inner.merge(invitation.initiator, invitation.target)?;
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
