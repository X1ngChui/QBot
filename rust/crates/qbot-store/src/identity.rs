//! Identity on Postgres. Topology changes (a new account, merge, split, applying an invitation)
//! take one global advisory lock, always after any group lock, so they cannot interleave.

use async_trait::async_trait;
use qbot_core::{AccountId, GroupId, MessageId, UnixMillis};
use qbot_memory::IdentityStore;
use qbot_memory::identity::{
    Alias, AliasId, AliasStatus, AliasTarget, EvidenceKind, EvidenceRecord, Holder, HolderId,
    IdentityError, IdentityPolicy, Invitation, InvitationState, LinkError, MergeOutcome,
    Resolution, confidence, normalize_alias, status_for,
};
use sqlx::postgres::PgRow;
use sqlx::{PgPool, Postgres, Row, Transaction};

use crate::error::StoreError;

type Tx<'a> = Transaction<'a, Postgres>;

const TOPOLOGY_LOCK: &str = "SELECT pg_advisory_xact_lock(hashtextextended('identity', 0))";

fn backend(error: impl std::fmt::Display) -> IdentityError {
    IdentityError::Backend(error.to_string())
}

/// Make sure the account and a holder of its own exist. Used by the archive in the same
/// transaction that stores a line, so a line never refers to an unknown account.
pub(crate) async fn ensure_account(
    tx: &mut Tx<'_>,
    account: i64,
    at_ms: i64,
) -> Result<(), StoreError> {
    let known = sqlx::query("SELECT 1 FROM account WHERE account_id = $1")
        .bind(account)
        .fetch_optional(&mut **tx)
        .await?;
    if known.is_some() {
        return Ok(());
    }
    sqlx::query(TOPOLOGY_LOCK).execute(&mut **tx).await?;
    let known = sqlx::query("SELECT 1 FROM account WHERE account_id = $1")
        .bind(account)
        .fetch_optional(&mut **tx)
        .await?;
    if known.is_none() {
        let holder: i64 =
            sqlx::query("INSERT INTO holder (created_ms) VALUES ($1) RETURNING holder_id")
                .bind(at_ms)
                .fetch_one(&mut **tx)
                .await?
                .get("holder_id");
        sqlx::query(
            "INSERT INTO account (account_id, holder_id, first_seen_ms) VALUES ($1, $2, $3)",
        )
        .bind(account)
        .bind(holder)
        .bind(at_ms)
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
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

fn holder_of_tx_sql() -> &'static str {
    "SELECT holder_id FROM account WHERE account_id = $1"
}

async fn holder_row(tx: &mut Tx<'_>, id: i64) -> Result<Holder, IdentityError> {
    let revision: i32 = sqlx::query("SELECT revision FROM holder WHERE holder_id = $1")
        .bind(id)
        .fetch_one(&mut **tx)
        .await
        .map_err(backend)?
        .get("revision");
    let accounts =
        sqlx::query("SELECT account_id FROM account WHERE holder_id = $1 ORDER BY account_id")
            .bind(id)
            .fetch_all(&mut **tx)
            .await
            .map_err(backend)?
            .iter()
            .map(|r| AccountId::new(r.get("account_id")).map_err(backend))
            .collect::<Result<Vec<_>, _>>()?;
    Ok(Holder {
        id: HolderId(id),
        revision: u32::try_from(revision).map_err(backend)?,
        accounts,
    })
}

async fn holder_id_of(tx: &mut Tx<'_>, account: AccountId) -> Result<i64, IdentityError> {
    sqlx::query(holder_of_tx_sql())
        .bind(account.get())
        .fetch_optional(&mut **tx)
        .await
        .map_err(backend)?
        .map(|r| r.get("holder_id"))
        .ok_or(IdentityError::UnknownAccount)
}

async fn root_of(tx: &mut Tx<'_>, mut id: i64) -> Result<i64, IdentityError> {
    loop {
        let next: Option<i64> = sqlx::query("SELECT merged_into FROM holder WHERE holder_id = $1")
            .bind(id)
            .fetch_one(&mut **tx)
            .await
            .map_err(backend)?
            .get("merged_into");
        match next {
            Some(n) => id = n,
            None => return Ok(id),
        }
    }
}

async fn merge_tx(
    tx: &mut Tx<'_>,
    a: AccountId,
    b: AccountId,
) -> Result<MergeOutcome, IdentityError> {
    let (ha, hb) = (holder_id_of(tx, a).await?, holder_id_of(tx, b).await?);
    if ha == hb {
        return Ok(MergeOutcome::AlreadyLinked(HolderId(ha)));
    }
    let key = |row: PgRow| {
        (
            row.get::<i64, _>("created_ms"),
            row.get::<i64, _>("holder_id"),
        )
    };
    let ka = key(
        sqlx::query("SELECT created_ms, holder_id FROM holder WHERE holder_id = $1")
            .bind(ha)
            .fetch_one(&mut **tx)
            .await
            .map_err(backend)?,
    );
    let kb = key(
        sqlx::query("SELECT created_ms, holder_id FROM holder WHERE holder_id = $1")
            .bind(hb)
            .fetch_one(&mut **tx)
            .await
            .map_err(backend)?,
    );
    let (winner, loser) = if ka <= kb { (ha, hb) } else { (hb, ha) };
    sqlx::query("UPDATE account SET holder_id = $1 WHERE holder_id = $2")
        .bind(winner)
        .bind(loser)
        .execute(&mut **tx)
        .await
        .map_err(backend)?;
    sqlx::query("UPDATE holder SET merged_into = $1, revision = revision + 1 WHERE holder_id = $2")
        .bind(winner)
        .bind(loser)
        .execute(&mut **tx)
        .await
        .map_err(backend)?;
    sqlx::query("UPDATE holder SET revision = revision + 1 WHERE holder_id = $1")
        .bind(winner)
        .execute(&mut **tx)
        .await
        .map_err(backend)?;
    Ok(MergeOutcome::Merged {
        winner: HolderId(winner),
        loser: HolderId(loser),
    })
}

struct TargetCols {
    kind: &'static str,
    account: Option<i64>,
    holder: Option<i64>,
}

fn target_cols(target: AliasTarget) -> TargetCols {
    match target {
        AliasTarget::Account(a) => TargetCols {
            kind: "account",
            account: Some(a.get()),
            holder: None,
        },
        AliasTarget::Holder(h) => TargetCols {
            kind: "holder",
            account: None,
            holder: Some(h.0),
        },
    }
}

fn to_alias(row: &PgRow) -> Result<Alias, IdentityError> {
    let target = match row.get::<String, _>("target_kind").as_str() {
        "account" => AliasTarget::Account(
            AccountId::new(row.get::<Option<i64>, _>("target_account").unwrap_or(0))
                .map_err(backend)?,
        ),
        _ => AliasTarget::Holder(HolderId(
            row.get::<Option<i64>, _>("target_holder").unwrap_or(0),
        )),
    };
    let status = match row.get::<String, _>("status").as_str() {
        "confirmed" => AliasStatus::Confirmed,
        "candidate" => AliasStatus::Candidate,
        _ => AliasStatus::Inactive,
    };
    Ok(Alias {
        id: AliasId(row.get("alias_id")),
        group: GroupId::new(row.get("group_id")).map_err(backend)?,
        text: row.get("text"),
        target,
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

const ALIAS_COLS: &str = "alias_id, group_id, text, target_kind, target_account, target_holder, status, confidence, last_evidence_ms";

impl PgIdentityStore {
    pub fn new(pool: PgPool, policy: IdentityPolicy) -> Self {
        Self { pool, policy }
    }

    async fn add_evidence_tx(
        &self,
        tx: &mut Tx<'_>,
        group: GroupId,
        text: &str,
        target: AliasTarget,
        evidence: EvidenceRecord,
        at: UnixMillis,
    ) -> Result<Alias, IdentityError> {
        let t = target_cols(target);
        if let Some(account) = t.account {
            let exists = sqlx::query("SELECT 1 FROM account WHERE account_id = $1")
                .bind(account)
                .fetch_optional(&mut **tx)
                .await
                .map_err(backend)?;
            if exists.is_none() {
                return Err(IdentityError::UnknownAccount);
            }
        }
        sqlx::query(
            "INSERT INTO alias (group_id, text, target_kind, target_account, target_holder, status, confidence, last_evidence_ms) \
             VALUES ($1, $2, $3, $4, $5, 'candidate', 0, $6) ON CONFLICT DO NOTHING",
        )
        .bind(group.get())
        .bind(text)
        .bind(t.kind)
        .bind(t.account)
        .bind(t.holder)
        .bind(at.get())
        .execute(&mut **tx)
        .await
        .map_err(backend)?;
        let alias_id: i64 = sqlx::query(
            "SELECT alias_id FROM alias WHERE group_id = $1 AND text = $2 AND target_kind = $3 \
             AND target_account IS NOT DISTINCT FROM $4 AND target_holder IS NOT DISTINCT FROM $5 FOR UPDATE",
        )
        .bind(group.get())
        .bind(text)
        .bind(t.kind)
        .bind(t.account)
        .bind(t.holder)
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
        let status = match status_for(conf, &self.policy) {
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
}

#[async_trait]
impl IdentityStore for PgIdentityStore {
    async fn seen(&self, account: AccountId, at: UnixMillis) -> Result<Holder, IdentityError> {
        let mut tx = self.pool.begin().await.map_err(backend)?;
        ensure_account(&mut tx, account.get(), at.get())
            .await
            .map_err(backend)?;
        let id = holder_id_of(&mut tx, account).await?;
        let holder = holder_row(&mut tx, id).await?;
        tx.commit().await.map_err(backend)?;
        Ok(holder)
    }

    async fn holder_of(&self, account: AccountId) -> Result<Option<Holder>, IdentityError> {
        let mut tx = self.pool.begin().await.map_err(backend)?;
        match holder_id_of(&mut tx, account).await {
            Ok(id) => Ok(Some(holder_row(&mut tx, id).await?)),
            Err(IdentityError::UnknownAccount) => Ok(None),
            Err(error) => Err(error),
        }
    }

    async fn merge(&self, a: AccountId, b: AccountId) -> Result<MergeOutcome, IdentityError> {
        let mut tx = self.pool.begin().await.map_err(backend)?;
        sqlx::query(TOPOLOGY_LOCK)
            .execute(&mut *tx)
            .await
            .map_err(backend)?;
        let outcome = merge_tx(&mut tx, a, b).await?;
        tx.commit().await.map_err(backend)?;
        Ok(outcome)
    }

    async fn split(&self, account: AccountId, at: UnixMillis) -> Result<Holder, IdentityError> {
        let mut tx = self.pool.begin().await.map_err(backend)?;
        sqlx::query(TOPOLOGY_LOCK)
            .execute(&mut *tx)
            .await
            .map_err(backend)?;
        let old = holder_id_of(&mut tx, account).await?;
        let count: i64 = sqlx::query("SELECT count(*) AS n FROM account WHERE holder_id = $1")
            .bind(old)
            .fetch_one(&mut *tx)
            .await
            .map_err(backend)?
            .get("n");
        if count < 2 {
            return Err(IdentityError::NotLinked);
        }
        let fresh: i64 = sqlx::query(
            "INSERT INTO holder (created_ms, revision) VALUES ($1, 1) RETURNING holder_id",
        )
        .bind(at.get())
        .fetch_one(&mut *tx)
        .await
        .map_err(backend)?
        .get("holder_id");
        sqlx::query("UPDATE account SET holder_id = $1 WHERE account_id = $2")
            .bind(fresh)
            .bind(account.get())
            .execute(&mut *tx)
            .await
            .map_err(backend)?;
        sqlx::query("UPDATE holder SET revision = revision + 1 WHERE holder_id = $1")
            .bind(old)
            .execute(&mut *tx)
            .await
            .map_err(backend)?;
        let holder = holder_row(&mut tx, fresh).await?;
        tx.commit().await.map_err(backend)?;
        Ok(holder)
    }

    async fn add_evidence(
        &self,
        group: GroupId,
        text: &str,
        target: AliasTarget,
        evidence: EvidenceRecord,
        at: UnixMillis,
    ) -> Result<Alias, IdentityError> {
        let text = normalize_alias(text)?;
        let mut tx = self.pool.begin().await.map_err(backend)?;
        let alias = self
            .add_evidence_tx(&mut tx, group, &text, target, evidence, at)
            .await?;
        tx.commit().await.map_err(backend)?;
        Ok(alias)
    }

    async fn set_name(
        &self,
        group: GroupId,
        text: &str,
        target: AliasTarget,
        at: UnixMillis,
    ) -> Result<Alias, IdentityError> {
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
        let t = target_cols(target);
        let taken = sqlx::query(
            "SELECT 1 FROM alias WHERE group_id = $1 AND text = $2 AND status = 'confirmed' AND NOT removed \
             AND NOT (target_kind = $3 AND target_account IS NOT DISTINCT FROM $4 AND target_holder IS NOT DISTINCT FROM $5)",
        )
        .bind(group.get())
        .bind(&text)
        .bind(t.kind)
        .bind(t.account)
        .bind(t.holder)
        .fetch_optional(&mut *tx)
        .await
        .map_err(backend)?;
        if taken.is_some() {
            return Err(IdentityError::NameTaken);
        }
        let alias = self
            .add_evidence_tx(
                &mut tx,
                group,
                &text,
                target,
                EvidenceRecord {
                    kind: EvidenceKind::Manual,
                    support: None,
                },
                at,
            )
            .await?;
        tx.commit().await.map_err(backend)?;
        Ok(alias)
    }

    async fn remove_name(
        &self,
        group: GroupId,
        text: &str,
        target: AliasTarget,
    ) -> Result<bool, IdentityError> {
        let text = normalize_alias(text)?;
        let t = target_cols(target);
        let result = sqlx::query(
            "UPDATE alias SET removed = true, status = 'inactive' WHERE group_id = $1 AND text = $2 AND target_kind = $3 \
             AND target_account IS NOT DISTINCT FROM $4 AND target_holder IS NOT DISTINCT FROM $5 AND NOT removed",
        )
        .bind(group.get())
        .bind(text)
        .bind(t.kind)
        .bind(t.account)
        .bind(t.holder)
        .execute(&self.pool)
        .await
        .map_err(backend)?;
        Ok(result.rows_affected() > 0)
    }

    async fn resolve(&self, group: GroupId, text: &str) -> Result<Resolution, IdentityError> {
        let text = normalize_alias(text)?;
        let mut tx = self.pool.begin().await.map_err(backend)?;
        let rows = sqlx::query("SELECT target_kind, target_account, target_holder FROM alias WHERE group_id = $1 AND text = $2 AND status = 'confirmed'")
            .bind(group.get())
            .bind(text)
            .fetch_all(&mut *tx)
            .await
            .map_err(backend)?;
        let mut targets = Vec::new();
        for row in rows {
            targets.push(match row.get::<String, _>("target_kind").as_str() {
                "account" => AliasTarget::Account(
                    AccountId::new(row.get::<Option<i64>, _>("target_account").unwrap_or(0))
                        .map_err(backend)?,
                ),
                _ => AliasTarget::Holder(HolderId(
                    root_of(
                        &mut tx,
                        row.get::<Option<i64>, _>("target_holder").unwrap_or(0),
                    )
                    .await?,
                )),
            });
        }
        Ok(Resolution::from_targets(targets))
    }

    async fn names_of(
        &self,
        group: GroupId,
        target: AliasTarget,
    ) -> Result<Vec<Alias>, IdentityError> {
        let rows = match target {
            AliasTarget::Account(a) => sqlx::query(&format!(
                "SELECT {ALIAS_COLS} FROM alias WHERE group_id = $1 AND target_kind = 'account' AND target_account = $2 AND status <> 'inactive' \
                 ORDER BY confidence DESC, text COLLATE \"C\""
            ))
            .bind(group.get())
            .bind(a.get())
            .fetch_all(&self.pool)
            .await,
            AliasTarget::Holder(h) => {
                let mut tx = self.pool.begin().await.map_err(backend)?;
                let root = root_of(&mut tx, h.0).await?;
                sqlx::query(&format!(
                    "WITH RECURSIVE tree(id) AS (SELECT $2::bigint UNION ALL SELECT h.holder_id FROM holder h JOIN tree t ON h.merged_into = t.id) \
                     SELECT {ALIAS_COLS} FROM alias WHERE group_id = $1 AND target_kind = 'holder' AND target_holder IN (SELECT id FROM tree) \
                     AND status <> 'inactive' ORDER BY confidence DESC, text COLLATE \"C\""
                ))
                .bind(group.get())
                .bind(root)
                .fetch_all(&mut *tx)
                .await
            }
        }
        .map_err(backend)?;
        rows.iter().map(to_alias).collect()
    }

    async fn expire_candidates(&self, before: UnixMillis) -> Result<usize, IdentityError> {
        let result = sqlx::query(
            "UPDATE alias SET removed = true, status = 'inactive' WHERE NOT removed AND status = 'candidate' AND last_evidence_ms < $1 \
             AND NOT EXISTS (SELECT 1 FROM alias_evidence e WHERE e.alias_id = alias.alias_id AND e.kind = 'manual')",
        )
        .bind(before.get())
        .execute(&self.pool)
        .await
        .map_err(backend)?;
        Ok(usize::try_from(result.rows_affected()).unwrap_or(usize::MAX))
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
        let mut tx = self.pool.begin().await.map_err(backend)?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('link:' || $1::text, 0))")
            .bind(group.get())
            .execute(&mut *tx)
            .await
            .map_err(backend)?;
        let (hi, ht) = (
            holder_id_of(&mut tx, initiator).await?,
            holder_id_of(&mut tx, target).await?,
        );
        if hi == ht {
            return Err(LinkError::AlreadyLinked.into());
        }
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
        let ri: i32 = sqlx::query("SELECT revision FROM holder WHERE holder_id = $1")
            .bind(hi)
            .fetch_one(&mut *tx)
            .await
            .map_err(backend)?
            .get("revision");
        let rt: i32 = sqlx::query("SELECT revision FROM holder WHERE holder_id = $1")
            .bind(ht)
            .fetch_one(&mut *tx)
            .await
            .map_err(backend)?
            .get("revision");
        sqlx::query(
            "INSERT INTO link_invitation (group_id, initiator, target, created_by, created_ms, initiator_revision, target_revision, state) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, 'pending')",
        )
        .bind(group.get())
        .bind(initiator.get())
        .bind(target.get())
        .bind(message.get())
        .bind(now.get())
        .bind(ri)
        .bind(rt)
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
            initiator_revision: u32::try_from(ri).map_err(backend)?,
            target_revision: u32::try_from(rt).map_err(backend)?,
            state: InvitationState::Pending,
        })
    }

    async fn confirm(
        &self,
        group: GroupId,
        confirming: AccountId,
        message: MessageId,
        now: UnixMillis,
    ) -> Result<MergeOutcome, IdentityError> {
        let mut tx = self.pool.begin().await.map_err(backend)?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('link:' || $1::text, 0))")
            .bind(group.get())
            .execute(&mut *tx)
            .await
            .map_err(backend)?;
        sqlx::query(TOPOLOGY_LOCK)
            .execute(&mut *tx)
            .await
            .map_err(backend)?;

        // Replaying the confirming message is a no-op.
        let replay = sqlx::query("SELECT initiator FROM link_invitation WHERE group_id = $1 AND state = 'applied' AND confirmed_by = $2 AND target = $3")
            .bind(group.get())
            .bind(message.get())
            .bind(confirming.get())
            .fetch_optional(&mut *tx)
            .await
            .map_err(backend)?;
        if let Some(row) = replay {
            let initiator = AccountId::new(row.get("initiator")).map_err(backend)?;
            let holder = holder_id_of(&mut tx, initiator).await?;
            return Ok(MergeOutcome::AlreadyLinked(HolderId(holder)));
        }

        let row = sqlx::query(
            "SELECT invitation_id, initiator, target, created_by, created_ms, initiator_revision, target_revision FROM link_invitation \
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
            initiator_revision: u32::try_from(row.get::<i32, _>("initiator_revision"))
                .map_err(backend)?,
            target_revision: u32::try_from(row.get::<i32, _>("target_revision"))
                .map_err(backend)?,
            state: InvitationState::Pending,
        };
        let hi = holder_id_of(&mut tx, invitation.initiator).await?;
        let ht = holder_id_of(&mut tx, invitation.target).await?;
        let ri = holder_row(&mut tx, hi).await?.revision;
        let rt = holder_row(&mut tx, ht).await?.revision;
        if let Err(error) =
            invitation.check_confirm(confirming, message, now, self.policy.invitation_ttl, ri, rt)
        {
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
        let outcome = merge_tx(&mut tx, invitation.initiator, invitation.target).await?;
        sqlx::query("UPDATE link_invitation SET state = 'applied', confirmed_by = $2 WHERE invitation_id = $1").bind(id).bind(message.get()).execute(&mut *tx).await.map_err(backend)?;
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
