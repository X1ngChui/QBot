//! Identity on Postgres. Links are rows of `account_link`, each with the group it was made in;
//! changes to a group's links (linking, unlinking, invitations) take that group's link lock.

use async_trait::async_trait;
use qbot_core::{AccountId, GroupId, MessageId, UnixMillis};
use qbot_memory::IdentityStore;
use qbot_memory::identity::{
    Alias, AliasId, AliasStatus, EvidenceKind, EvidenceRecord, IdentityError, IdentityPolicy,
    Invitation, InvitationState, LinkError, LinkOutcome, Resolution, confidence, normalize_alias,
    status_for,
};
use sqlx::postgres::PgRow;
use sqlx::{PgExecutor, PgPool, Postgres, Row, Transaction};

use crate::error::StoreError;

type Tx<'a> = Transaction<'a, Postgres>;

const LINK_LOCK: &str = "SELECT pg_advisory_xact_lock(hashtextextended('link:' || $1::text, 0))";

fn backend(error: impl std::fmt::Display) -> IdentityError {
    IdentityError::Backend(error.to_string())
}

/// Make sure the account exists. Used by the archive in the same transaction that stores a line,
/// so a line never refers to an unknown account.
pub(crate) async fn ensure_account(
    tx: &mut Tx<'_>,
    account: i64,
    at_ms: i64,
) -> Result<(), StoreError> {
    sqlx::query(
        "INSERT INTO account (account_id, first_seen_ms) VALUES ($1, $2) ON CONFLICT DO NOTHING",
    )
    .bind(account)
    .bind(at_ms)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// The accounts linked to `account` in `group` through that group's links, with `account`;
/// ascending.
pub(crate) async fn linked_in<'e>(
    executor: impl PgExecutor<'e>,
    group: GroupId,
    account: AccountId,
) -> Result<Vec<AccountId>, sqlx::Error> {
    let ids: Vec<i64> = sqlx::query_scalar(
        "WITH RECURSIVE c(a) AS ( \
             SELECT $2::bigint \
           UNION \
             SELECT CASE WHEN l.low_account = c.a THEN l.high_account ELSE l.low_account END \
             FROM account_link l JOIN c ON l.group_id = $1 \
                                      AND (l.low_account = c.a OR l.high_account = c.a)) \
         SELECT a FROM c ORDER BY a",
    )
    .bind(group.get())
    .bind(account.get())
    .fetch_all(executor)
    .await?;
    ids.into_iter()
        .map(|id| AccountId::new(id).map_err(|e| sqlx::Error::Decode(e.to_string().into())))
        .collect()
}

#[derive(Clone)]
pub struct PgIdentityStore {
    pool: PgPool,
    policy: IdentityPolicy,
}

impl std::fmt::Debug for PgIdentityStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PgIdentityStore")
            .field("policy", &self.policy)
            .finish_non_exhaustive()
    }
}

async fn require_known(tx: &mut Tx<'_>, account: AccountId) -> Result<(), IdentityError> {
    sqlx::query("SELECT 1 FROM account WHERE account_id = $1")
        .bind(account.get())
        .fetch_optional(&mut **tx)
        .await
        .map_err(backend)?
        .map(|_| ())
        .ok_or(IdentityError::UnknownAccount)
}

async fn linked_tx(
    tx: &mut Tx<'_>,
    group: GroupId,
    account: AccountId,
) -> Result<Vec<AccountId>, IdentityError> {
    linked_in(&mut **tx, group, account).await.map_err(backend)
}

/// Link two known accounts in `group`, under the group's link lock.
async fn link_tx(
    tx: &mut Tx<'_>,
    group: GroupId,
    a: AccountId,
    b: AccountId,
    at: UnixMillis,
) -> Result<LinkOutcome, IdentityError> {
    if linked_tx(tx, group, a).await?.contains(&b) {
        return Ok(LinkOutcome::AlreadyLinked);
    }
    sqlx::query(
        "INSERT INTO account_link (group_id, low_account, high_account, created_ms) \
         VALUES ($1, $2, $3, $4)",
    )
    .bind(group.get())
    .bind(a.get().min(b.get()))
    .bind(a.get().max(b.get()))
    .bind(at.get())
    .execute(&mut **tx)
    .await
    .map_err(backend)?;
    Ok(LinkOutcome::Linked)
}

fn ids(accounts: &[AccountId]) -> Vec<i64> {
    accounts.iter().map(|a| a.get()).collect()
}

fn accounts_of(ids: Vec<i64>) -> Result<Vec<AccountId>, IdentityError> {
    ids.into_iter()
        .map(|id| AccountId::new(id).map_err(backend))
        .collect()
}

fn to_alias(row: &PgRow) -> Result<Alias, IdentityError> {
    let status = match row.get::<String, _>("status").as_str() {
        "confirmed" => AliasStatus::Confirmed,
        "candidate" => AliasStatus::Candidate,
        _ => AliasStatus::Inactive,
    };
    Ok(Alias {
        id: AliasId(row.get("alias_id")),
        group: GroupId::new(row.get("group_id")).map_err(backend)?,
        text: row.get("text"),
        account: AccountId::new(row.get("account_id")).map_err(backend)?,
        status,
        confidence: row.get("confidence"),
        last_evidence: UnixMillis::new(row.get("last_evidence_ms")),
    })
}

fn kind_text(kind: EvidenceKind) -> &'static str {
    match kind {
        EvidenceKind::Manual => "manual",
        EvidenceKind::Extracted => "extracted",
    }
}

fn kind_parse(text: &str) -> EvidenceKind {
    match text {
        "manual" => EvidenceKind::Manual,
        _ => EvidenceKind::Extracted,
    }
}

const ALIAS_COLS: &str =
    "alias_id, group_id, text, account_id, status, confidence, last_evidence_ms";

impl PgIdentityStore {
    pub fn new(pool: PgPool, policy: IdentityPolicy) -> Self {
        Self { pool, policy }
    }

    async fn add_evidence_tx(
        &self,
        tx: &mut Tx<'_>,
        group: GroupId,
        text: &str,
        account: AccountId,
        evidence: EvidenceRecord,
        at: UnixMillis,
    ) -> Result<Alias, IdentityError> {
        require_known(tx, account).await?;
        sqlx::query(
            "INSERT INTO alias (group_id, text, account_id, status, confidence, last_evidence_ms) \
             VALUES ($1, $2, $3, 'candidate', 0, $4) ON CONFLICT DO NOTHING",
        )
        .bind(group.get())
        .bind(text)
        .bind(account.get())
        .bind(at.get())
        .execute(&mut **tx)
        .await
        .map_err(backend)?;
        let alias_id: i64 = sqlx::query(
            "SELECT alias_id FROM alias WHERE group_id = $1 AND text = $2 AND account_id = $3 \
             FOR UPDATE",
        )
        .bind(group.get())
        .bind(text)
        .bind(account.get())
        .fetch_one(&mut **tx)
        .await
        .map_err(backend)?
        .get("alias_id");
        sqlx::query(
            "INSERT INTO alias_evidence (alias_id, kind, support, at_ms) VALUES ($1, $2, $3, $4) \
             ON CONFLICT (alias_id, kind, (COALESCE(support, -1))) DO UPDATE SET at_ms = EXCLUDED.at_ms",
        )
        .bind(alias_id)
        .bind(kind_text(evidence.kind))
        .bind(evidence.support)
        .bind(at.get())
        .execute(&mut **tx)
        .await
        .map_err(backend)?;
        let rows =
            sqlx::query("SELECT kind, support, at_ms FROM alias_evidence WHERE alias_id = $1")
                .bind(alias_id)
                .fetch_all(&mut **tx)
                .await
                .map_err(backend)?;
        let records: Vec<EvidenceRecord> = rows
            .iter()
            .map(|r| EvidenceRecord {
                kind: kind_parse(&r.get::<String, _>("kind")),
                support: r.get("support"),
            })
            .collect();
        let last = rows
            .iter()
            .map(|r| r.get::<i64, _>("at_ms"))
            .max()
            .unwrap_or(at.get());
        let conf = confidence(&records);
        let status = match status_for(&records) {
            AliasStatus::Confirmed => "confirmed",
            _ => "candidate",
        };
        let row = sqlx::query(&format!(
            "UPDATE alias SET confidence = $2, status = $3, removed = false, last_evidence_ms = $4 WHERE alias_id = $1 RETURNING {ALIAS_COLS}"
        ))
        .bind(alias_id)
        .bind(conf)
        .bind(status)
        .bind(last)
        .fetch_one(&mut **tx)
        .await
        .map_err(backend)?;
        to_alias(&row)
    }

    async fn begin_linking(&self, group: GroupId) -> Result<Tx<'static>, IdentityError> {
        let mut tx = self.pool.begin().await.map_err(backend)?;
        sqlx::query(LINK_LOCK)
            .bind(group.get())
            .execute(&mut *tx)
            .await
            .map_err(backend)?;
        Ok(tx)
    }
}

#[async_trait]
impl IdentityStore for PgIdentityStore {
    async fn seen(&self, account: AccountId, at: UnixMillis) -> Result<(), IdentityError> {
        let mut tx = self.pool.begin().await.map_err(backend)?;
        ensure_account(&mut tx, account.get(), at.get())
            .await
            .map_err(backend)?;
        tx.commit().await.map_err(backend)
    }

    async fn is_known(&self, account: AccountId) -> Result<bool, IdentityError> {
        Ok(sqlx::query("SELECT 1 FROM account WHERE account_id = $1")
            .bind(account.get())
            .fetch_optional(&self.pool)
            .await
            .map_err(backend)?
            .is_some())
    }

    async fn linked(
        &self,
        group: GroupId,
        account: AccountId,
    ) -> Result<Vec<AccountId>, IdentityError> {
        linked_in(&self.pool, group, account).await.map_err(backend)
    }

    async fn link(
        &self,
        group: GroupId,
        a: AccountId,
        b: AccountId,
        at: UnixMillis,
    ) -> Result<LinkOutcome, IdentityError> {
        if a == b {
            return Err(LinkError::SelfLink.into());
        }
        let mut tx = self.begin_linking(group).await?;
        require_known(&mut tx, a).await?;
        require_known(&mut tx, b).await?;
        let outcome = link_tx(&mut tx, group, a, b, at).await?;
        tx.commit().await.map_err(backend)?;
        Ok(outcome)
    }

    async fn unlink(&self, group: GroupId, account: AccountId) -> Result<(), IdentityError> {
        let mut tx = self.begin_linking(group).await?;
        require_known(&mut tx, account).await?;
        let removed = sqlx::query(
            "DELETE FROM account_link WHERE group_id = $1 AND (low_account = $2 OR high_account = $2)",
        )
        .bind(group.get())
        .bind(account.get())
        .execute(&mut *tx)
        .await
        .map_err(backend)?
        .rows_affected();
        if removed == 0 {
            return Err(IdentityError::NotLinked);
        }
        tx.commit().await.map_err(backend)
    }

    async fn add_evidence(
        &self,
        group: GroupId,
        text: &str,
        account: AccountId,
        evidence: EvidenceRecord,
        at: UnixMillis,
    ) -> Result<Alias, IdentityError> {
        let text = normalize_alias(text)?;
        let mut tx = self.pool.begin().await.map_err(backend)?;
        let alias = self
            .add_evidence_tx(&mut tx, group, &text, account, evidence, at)
            .await?;
        tx.commit().await.map_err(backend)?;
        Ok(alias)
    }

    async fn set_name(
        &self,
        group: GroupId,
        text: &str,
        accounts: &[AccountId],
        at: UnixMillis,
    ) -> Result<Vec<Alias>, IdentityError> {
        let text = normalize_alias(text)?;
        let mut tx = self.pool.begin().await.map_err(backend)?;
        sqlx::query(
            "SELECT pg_advisory_xact_lock(hashtextextended('name:' || $1::text || ':' || $2, 0))",
        )
        .bind(group.get())
        .bind(&text)
        .execute(&mut *tx)
        .await
        .map_err(backend)?;
        for account in accounts {
            require_known(&mut tx, *account).await?;
        }
        let taken = sqlx::query(
            "SELECT 1 FROM alias WHERE group_id = $1 AND text = $2 AND status = 'confirmed' \
             AND NOT removed AND NOT (account_id = ANY($3))",
        )
        .bind(group.get())
        .bind(&text)
        .bind(ids(accounts))
        .fetch_optional(&mut *tx)
        .await
        .map_err(backend)?;
        if taken.is_some() {
            return Err(IdentityError::NameTaken);
        }
        let manual = EvidenceRecord {
            kind: EvidenceKind::Manual,
            support: None,
        };
        let mut aliases = Vec::with_capacity(accounts.len());
        for account in accounts {
            aliases.push(
                self.add_evidence_tx(&mut tx, group, &text, *account, manual, at)
                    .await?,
            );
        }
        tx.commit().await.map_err(backend)?;
        Ok(aliases)
    }

    async fn remove_name(
        &self,
        group: GroupId,
        text: &str,
        accounts: &[AccountId],
    ) -> Result<bool, IdentityError> {
        let text = normalize_alias(text)?;
        let result = sqlx::query(
            "UPDATE alias SET removed = true, status = 'inactive' WHERE group_id = $1 AND text = $2 \
             AND account_id = ANY($3) AND NOT removed",
        )
        .bind(group.get())
        .bind(text)
        .bind(ids(accounts))
        .execute(&self.pool)
        .await
        .map_err(backend)?;
        Ok(result.rows_affected() > 0)
    }

    async fn resolve(&self, group: GroupId, text: &str) -> Result<Resolution, IdentityError> {
        let text = normalize_alias(text)?;
        let found: Vec<i64> = sqlx::query_scalar(
            "SELECT account_id FROM alias WHERE group_id = $1 AND text = $2 AND status = 'confirmed'",
        )
        .bind(group.get())
        .bind(text)
        .fetch_all(&self.pool)
        .await
        .map_err(backend)?;
        Ok(Resolution::from_targets(accounts_of(found)?))
    }

    async fn names_of(
        &self,
        group: GroupId,
        accounts: &[AccountId],
    ) -> Result<Vec<Alias>, IdentityError> {
        let rows = sqlx::query(&format!(
            "SELECT DISTINCT ON (text) {ALIAS_COLS} FROM alias \
             WHERE group_id = $1 AND account_id = ANY($2) AND status <> 'inactive' \
             ORDER BY text, confidence DESC, alias_id"
        ))
        .bind(group.get())
        .bind(ids(accounts))
        .fetch_all(&self.pool)
        .await
        .map_err(backend)?;
        let mut names = rows.iter().map(to_alias).collect::<Result<Vec<_>, _>>()?;
        names.sort_by(|a, b| {
            b.confidence
                .total_cmp(&a.confidence)
                .then(a.text.cmp(&b.text))
        });
        Ok(names)
    }

    async fn invite(
        &self,
        group: GroupId,
        initiator: AccountId,
        target: AccountId,
        message: MessageId,
        now: UnixMillis,
    ) -> Result<Invitation, IdentityError> {
        if initiator == target {
            return Err(LinkError::SelfLink.into());
        }
        let ttl_ms = i64::try_from(self.policy.invitation_ttl.as_millis()).unwrap_or(i64::MAX);
        let mut tx = self.begin_linking(group).await?;
        require_known(&mut tx, initiator).await?;
        require_known(&mut tx, target).await?;
        let initiator_linked = linked_tx(&mut tx, group, initiator).await?;
        if initiator_linked.contains(&target) {
            return Err(LinkError::AlreadyLinked.into());
        }
        let target_linked = linked_tx(&mut tx, group, target).await?;
        sqlx::query("UPDATE link_invitation SET state = 'expired' WHERE group_id = $1 AND state = 'pending' AND created_ms <= $2")
            .bind(group.get())
            .bind(now.get() - ttl_ms)
            .execute(&mut *tx)
            .await
            .map_err(backend)?;
        let busy = sqlx::query(
            "SELECT 1 FROM link_invitation WHERE group_id = $1 AND state = 'pending' AND (initiator IN ($2, $3) OR target IN ($2, $3))",
        )
        .bind(group.get())
        .bind(initiator.get())
        .bind(target.get())
        .fetch_optional(&mut *tx)
        .await
        .map_err(backend)?;
        if busy.is_some() {
            return Err(LinkError::Busy.into());
        }
        sqlx::query(
            "INSERT INTO link_invitation (group_id, initiator, target, created_by, created_ms, initiator_linked, target_linked, state) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, 'pending')",
        )
        .bind(group.get())
        .bind(initiator.get())
        .bind(target.get())
        .bind(message.get())
        .bind(now.get())
        .bind(ids(&initiator_linked))
        .bind(ids(&target_linked))
        .execute(&mut *tx)
        .await
        .map_err(backend)?;
        tx.commit().await.map_err(backend)?;
        Ok(Invitation {
            group,
            initiator,
            target,
            created_by: message,
            created_at: now,
            initiator_linked,
            target_linked,
            state: InvitationState::Pending,
        })
    }

    async fn confirm(
        &self,
        group: GroupId,
        confirming: AccountId,
        message: MessageId,
        now: UnixMillis,
    ) -> Result<LinkOutcome, IdentityError> {
        let mut tx = self.begin_linking(group).await?;

        // Replaying the confirming message is a no-op.
        let replay = sqlx::query("SELECT 1 FROM link_invitation WHERE group_id = $1 AND state = 'applied' AND confirmed_by = $2 AND target = $3")
            .bind(group.get())
            .bind(message.get())
            .bind(confirming.get())
            .fetch_optional(&mut *tx)
            .await
            .map_err(backend)?;
        if replay.is_some() {
            return Ok(LinkOutcome::AlreadyLinked);
        }

        let row = sqlx::query(
            "SELECT invitation_id, initiator, target, created_by, created_ms, initiator_linked, target_linked FROM link_invitation \
             WHERE group_id = $1 AND state = 'pending' AND (initiator = $2 OR target = $2)",
        )
        .bind(group.get())
        .bind(confirming.get())
        .fetch_optional(&mut *tx)
        .await
        .map_err(backend)?
        .ok_or(LinkError::NoInvitation)?;
        let id: i64 = row.get("invitation_id");
        let invitation = Invitation {
            group,
            initiator: AccountId::new(row.get("initiator")).map_err(backend)?,
            target: AccountId::new(row.get("target")).map_err(backend)?,
            created_by: MessageId::new(row.get("created_by")).map_err(backend)?,
            created_at: UnixMillis::new(row.get("created_ms")),
            initiator_linked: accounts_of(row.get("initiator_linked"))?,
            target_linked: accounts_of(row.get("target_linked"))?,
            state: InvitationState::Pending,
        };
        let now_initiator = linked_tx(&mut tx, group, invitation.initiator).await?;
        let now_target = linked_tx(&mut tx, group, invitation.target).await?;
        if let Err(error) = invitation.check_confirm(
            confirming,
            now,
            self.policy.invitation_ttl,
            &now_initiator,
            &now_target,
        ) {
            // An expired or stale invitation is closed for good; keep that change.
            let closed = match error {
                LinkError::Expired => Some("expired"),
                LinkError::Stale => Some("cancelled"),
                _ => None,
            };
            if let Some(state) = closed {
                sqlx::query("UPDATE link_invitation SET state = $2 WHERE invitation_id = $1")
                    .bind(id)
                    .bind(state)
                    .execute(&mut *tx)
                    .await
                    .map_err(backend)?;
                tx.commit().await.map_err(backend)?;
            }
            return Err(error.into());
        }
        let outcome = link_tx(&mut tx, group, invitation.initiator, invitation.target, now).await?;
        sqlx::query("UPDATE link_invitation SET state = 'applied', confirmed_by = $2 WHERE invitation_id = $1")
            .bind(id)
            .bind(message.get())
            .execute(&mut *tx)
            .await
            .map_err(backend)?;
        tx.commit().await.map_err(backend)?;
        Ok(outcome)
    }

    async fn cancel_invitation(
        &self,
        group: GroupId,
        account: AccountId,
    ) -> Result<bool, IdentityError> {
        let result = sqlx::query("UPDATE link_invitation SET state = 'cancelled' WHERE group_id = $1 AND state = 'pending' AND (initiator = $2 OR target = $2)")
            .bind(group.get())
            .bind(account.get())
            .execute(&self.pool)
            .await
            .map_err(backend)?;
        Ok(result.rows_affected() > 0)
    }
}
