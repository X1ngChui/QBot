//! The name ledger on Postgres: `name_reference` (one row per recorded use of an expression for
//! a person), `name_set` (names a person set), `name_removed` (names a person removed from a
//! member), `name_ruling` (what reading an expression's uses across conversations concluded) and
//! `held_claim` (statements about a person known only by a name nobody has placed).

use std::collections::{BTreeMap, BTreeSet};

use async_trait::async_trait;
use qbot_core::{AccountId, GroupId, MessageId, UnixMillis};
use qbot_memory::memories::{Durability, Support};
use qbot_memory::names::{
    Grounds, HeldClaim, HeldId, KnownName, NameBasis, NameError, NameStore, OpenName, Reference,
    Referent, Ruling, Verdict, name_key, open_names, placed, summarize, tidy_name,
};
use qbot_memory::{EpisodeId, MemoryError};
use sqlx::postgres::PgRow;
use sqlx::{PgExecutor, PgPool, Row};

use crate::identity::ensure_account;

fn backend(error: impl std::fmt::Display) -> MemoryError {
    MemoryError::Backend(error.to_string())
}

#[derive(Debug, Clone)]
pub struct PgNameStore {
    pool: PgPool,
}

impl PgNameStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

const REFERENCE_COLS: &str = "episode_id, message_id, ordinal, speaker_account, key, text, who, \
                              account_id, basis, grounds, anchored, shown_by, at_ms";

fn account(id: i64) -> Result<AccountId, MemoryError> {
    AccountId::new(id).map_err(backend)
}

fn messages(ids: Vec<i64>) -> Vec<MessageId> {
    ids.into_iter()
        .filter_map(|id| MessageId::new(id).ok())
        .collect()
}

fn to_reference(row: &PgRow) -> Result<(String, Reference), MemoryError> {
    let referent = match (
        row.get::<&str, _>("who"),
        row.get::<Option<i64>, _>("account_id"),
    ) {
        ("member", Some(id)) => Referent::Member(account(id)?),
        ("bot", _) => Referent::Bot,
        ("outside", _) => Referent::Outside,
        ("unknown", _) => Referent::Unknown,
        _ => return Err(backend("unknown referent")),
    };
    Ok((
        row.get("key"),
        Reference {
            episode: EpisodeId::new(row.get("episode_id")),
            message: MessageId::new(row.get("message_id")).map_err(backend)?,
            ordinal: u64::try_from(row.get::<i64, _>("ordinal")).map_err(backend)?,
            speaker: account(row.get("speaker_account"))?,
            text: row.get("text"),
            referent,
            basis: NameBasis::parse(row.get::<&str, _>("basis"))
                .ok_or_else(|| backend("unknown name basis"))?,
            grounds: Grounds::parse(row.get::<&str, _>("grounds"))
                .ok_or_else(|| backend("unknown grounds"))?,
            anchored: row.get("anchored"),
            shown_by: messages(row.get("shown_by")),
            at: UnixMillis::new(row.get("at_ms")),
        },
    ))
}

fn to_ruling(row: &PgRow) -> Result<(String, Ruling), MemoryError> {
    let verdict = match (
        row.get::<&str, _>("verdict"),
        row.get::<Option<i64>, _>("account_id"),
    ) {
        ("member", Some(id)) => Verdict::Member(account(id)?),
        ("outside", _) => Verdict::Outside,
        ("general", _) => Verdict::General,
        ("unknown", _) => Verdict::Unknown,
        _ => return Err(backend("unknown verdict")),
    };
    Ok((
        row.get("key"),
        Ruling {
            text: row.get("text"),
            verdict,
            lines: messages(row.get("lines")),
            reason: row.get("reason"),
            seen: u32::try_from(row.get::<i32, _>("seen")).map_err(backend)?,
            at: UnixMillis::new(row.get("at_ms")),
        },
    ))
}

async fn rulings<'e>(
    executor: impl PgExecutor<'e>,
    group: GroupId,
) -> Result<Vec<(String, Ruling)>, MemoryError> {
    sqlx::query(
        "SELECT key, text, verdict, account_id, lines, reason, seen, at_ms FROM name_ruling \
         WHERE group_id = $1 ORDER BY key",
    )
    .bind(group.get())
    .fetch_all(executor)
    .await
    .map_err(backend)?
    .iter()
    .map(to_ruling)
    .collect()
}

/// The group's names, per member and name, for the given accounts or all of them.
pub(crate) async fn known_names(
    pool: &PgPool,
    group: GroupId,
    accounts: Option<&[AccountId]>,
) -> Result<Vec<KnownName>, MemoryError> {
    let wanted: Option<Vec<i64>> = accounts.map(|a| a.iter().map(|x| x.get()).collect());
    let rows = sqlx::query(&format!(
        "SELECT {REFERENCE_COLS} FROM name_reference \
         WHERE group_id = $1 AND who = 'member' \
           AND ($2::bigint[] IS NULL OR account_id = ANY($2)) \
         ORDER BY reference_id"
    ))
    .bind(group.get())
    .bind(&wanted)
    .fetch_all(pool)
    .await
    .map_err(backend)?;
    let uses: Vec<(String, Reference)> = rows.iter().map(to_reference).collect::<Result<_, _>>()?;
    let mut set = Vec::new();
    for row in sqlx::query(
        "SELECT account_id, key, text, at_ms FROM name_set \
         WHERE group_id = $1 AND ($2::bigint[] IS NULL OR account_id = ANY($2)) \
         ORDER BY account_id, key",
    )
    .bind(group.get())
    .bind(&wanted)
    .fetch_all(pool)
    .await
    .map_err(backend)?
    {
        set.push((
            account(row.get("account_id"))?,
            row.get::<String, _>("key"),
            row.get::<String, _>("text"),
            UnixMillis::new(row.get("at_ms")),
        ));
    }
    let rulings = rulings(pool, group).await?;
    let mut names = summarize(uses.iter().map(|(k, u)| (k.as_str(), u)), &set, &rulings);
    names.retain(|name| accounts.is_none_or(|wanted| wanted.contains(&name.account)));
    Ok(names)
}

#[async_trait]
impl NameStore for PgNameStore {
    async fn record(&self, group: GroupId, references: &[Reference]) -> Result<u64, MemoryError> {
        let mut tx = self.pool.begin().await.map_err(backend)?;
        let ruled: BTreeMap<String, Verdict> = rulings(&mut *tx, group)
            .await?
            .into_iter()
            .map(|(key, ruling)| (key, ruling.verdict))
            .collect();
        let mut recorded = 0;
        for one in references {
            let Ok(key) = name_key(&one.text) else {
                continue;
            };
            if one.basis == NameBasis::Set {
                continue;
            }
            let one = placed(one.clone(), ruled.get(&key).copied());
            let member = one.referent.member().map(|a| a.get());
            let shown_by: Vec<i64> = one.shown_by.iter().map(|m| m.get()).collect();
            let done = sqlx::query(
                "INSERT INTO name_reference (group_id, episode_id, message_id, ordinal, \
                 speaker_account, key, text, who, account_id, basis, grounds, anchored, \
                 shown_by, at_ms) \
                 SELECT $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14 \
                 WHERE EXISTS (SELECT 1 FROM account WHERE account_id = $5) \
                   AND ($9::bigint IS NULL OR EXISTS (SELECT 1 FROM account WHERE account_id = $9)) \
                   AND NOT EXISTS (SELECT 1 FROM name_removed \
                                   WHERE group_id = $1 AND account_id = $9 AND key = $6) \
                 ON CONFLICT DO NOTHING",
            )
            .bind(group.get())
            .bind(one.episode.get())
            .bind(one.message.get())
            .bind(i64::try_from(one.ordinal).map_err(backend)?)
            .bind(one.speaker.get())
            .bind(&key)
            .bind(tidy_name(&one.text))
            .bind(one.referent.as_str())
            .bind(member)
            .bind(one.basis.as_str())
            .bind(one.grounds.as_str())
            .bind(one.anchored)
            .bind(&shown_by)
            .bind(one.at.get())
            .execute(&mut *tx)
            .await
            .map_err(backend)?;
            recorded += done.rows_affected();
        }
        tx.commit().await.map_err(backend)?;
        Ok(recorded)
    }

    async fn set(
        &self,
        group: GroupId,
        text: &str,
        accounts: &[AccountId],
        at: UnixMillis,
    ) -> Result<(), NameError> {
        let key = name_key(text)?;
        let ids: Vec<i64> = accounts.iter().map(|a| a.get()).collect();
        let mut tx = self.pool.begin().await.map_err(backend)?;
        // One setter at a time per group and name, so two people cannot set it for two members.
        sqlx::query(
            "SELECT pg_advisory_xact_lock(hashtextextended('name:' || $1::text || ':' || $2, 0))",
        )
        .bind(group.get())
        .bind(&key)
        .execute(&mut *tx)
        .await
        .map_err(backend)?;
        let taken: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM name_set WHERE group_id = $1 AND key = $2 \
             AND NOT (account_id = ANY($3)))",
        )
        .bind(group.get())
        .bind(&key)
        .bind(&ids)
        .fetch_one(&mut *tx)
        .await
        .map_err(backend)?;
        if taken {
            return Err(NameError::Taken);
        }
        for account in accounts {
            ensure_account(&mut tx, account.get(), at.get())
                .await
                .map_err(backend)?;
            sqlx::query(
                "DELETE FROM name_removed WHERE group_id = $1 AND account_id = $2 AND key = $3",
            )
            .bind(group.get())
            .bind(account.get())
            .bind(&key)
            .execute(&mut *tx)
            .await
            .map_err(backend)?;
            sqlx::query(
                "INSERT INTO name_set (group_id, account_id, key, text, at_ms) \
                 VALUES ($1, $2, $3, $4, $5) \
                 ON CONFLICT (group_id, account_id, key) \
                 DO UPDATE SET text = EXCLUDED.text, at_ms = EXCLUDED.at_ms",
            )
            .bind(group.get())
            .bind(account.get())
            .bind(&key)
            .bind(tidy_name(text))
            .bind(at.get())
            .execute(&mut *tx)
            .await
            .map_err(backend)?;
        }
        tx.commit().await.map_err(backend)?;
        Ok(())
    }

    async fn remove(
        &self,
        group: GroupId,
        text: &str,
        accounts: &[AccountId],
        at: UnixMillis,
    ) -> Result<bool, NameError> {
        let key = name_key(text)?;
        let ids: Vec<i64> = accounts.iter().map(|a| a.get()).collect();
        let mut tx = self.pool.begin().await.map_err(backend)?;
        let mut gone = 0;
        for statement in [
            "DELETE FROM name_set WHERE group_id = $1 AND key = $2 AND account_id = ANY($3)",
            "DELETE FROM name_reference WHERE group_id = $1 AND key = $2 AND account_id = ANY($3)",
            "DELETE FROM name_ruling WHERE group_id = $1 AND key = $2 AND account_id = ANY($3)",
        ] {
            gone += sqlx::query(statement)
                .bind(group.get())
                .bind(&key)
                .bind(&ids)
                .execute(&mut *tx)
                .await
                .map_err(backend)?
                .rows_affected();
        }
        sqlx::query(
            "INSERT INTO name_removed (group_id, account_id, key, removed_ms) \
             SELECT $1, a, $2, $4 FROM unnest($3::bigint[]) a \
             WHERE EXISTS (SELECT 1 FROM account WHERE account_id = a) \
             ON CONFLICT (group_id, account_id, key) DO UPDATE SET removed_ms = EXCLUDED.removed_ms",
        )
        .bind(group.get())
        .bind(&key)
        .bind(&ids)
        .bind(at.get())
        .execute(&mut *tx)
        .await
        .map_err(backend)?;
        tx.commit().await.map_err(backend)?;
        Ok(gone > 0)
    }

    async fn names(
        &self,
        group: GroupId,
        accounts: Option<&[AccountId]>,
    ) -> Result<Vec<KnownName>, MemoryError> {
        known_names(&self.pool, group, accounts).await
    }

    async fn open(&self, group: GroupId, limit: usize) -> Result<Vec<OpenName>, MemoryError> {
        let rows = sqlx::query(&format!(
            "SELECT {REFERENCE_COLS} FROM name_reference \
             WHERE group_id = $1 AND who = 'unknown' ORDER BY reference_id"
        ))
        .bind(group.get())
        .fetch_all(&self.pool)
        .await
        .map_err(backend)?;
        let unplaced: Vec<(String, Reference)> =
            rows.iter().map(to_reference).collect::<Result<_, _>>()?;
        let set: BTreeSet<String> =
            sqlx::query_scalar("SELECT DISTINCT key FROM name_set WHERE group_id = $1")
                .bind(group.get())
                .fetch_all(&self.pool)
                .await
                .map_err(backend)?
                .into_iter()
                .collect();
        let rulings: BTreeMap<String, Ruling> =
            rulings(&self.pool, group).await?.into_iter().collect();
        Ok(open_names(
            unplaced.iter().map(|(k, r)| (k.as_str(), r)),
            &set,
            &rulings,
            limit,
        ))
    }

    async fn uses(
        &self,
        group: GroupId,
        key: &str,
        limit: usize,
    ) -> Result<Vec<Reference>, MemoryError> {
        let rows = sqlx::query(&format!(
            "SELECT {REFERENCE_COLS} FROM ( \
                 SELECT DISTINCT ON (episode_id) * FROM name_reference \
                 WHERE group_id = $1 AND key = $2 \
                 ORDER BY episode_id, ordinal, reference_id \
             ) u ORDER BY episode_id DESC LIMIT $3"
        ))
        .bind(group.get())
        .bind(key)
        .bind(i64::try_from(limit).unwrap_or(i64::MAX))
        .fetch_all(&self.pool)
        .await
        .map_err(backend)?;
        rows.iter()
            .map(|row| to_reference(row).map(|(_, one)| one))
            .collect()
    }

    async fn rule(&self, group: GroupId, ruling: &Ruling) -> Result<(), MemoryError> {
        let Ok(key) = name_key(&ruling.text) else {
            return Ok(());
        };
        let mut tx = self.pool.begin().await.map_err(backend)?;
        let mut verdict = ruling.verdict;
        if let Verdict::Member(account) = verdict {
            let removed: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM name_removed \
                 WHERE group_id = $1 AND account_id = $2 AND key = $3) \
                 OR NOT EXISTS (SELECT 1 FROM account WHERE account_id = $2)",
            )
            .bind(group.get())
            .bind(account.get())
            .bind(&key)
            .fetch_one(&mut *tx)
            .await
            .map_err(backend)?;
            if removed {
                verdict = Verdict::Unknown;
            }
        }
        let member = match verdict {
            Verdict::Member(account) => Some(account.get()),
            _ => None,
        };
        let lines: Vec<i64> = ruling.lines.iter().map(|m| m.get()).collect();
        sqlx::query(
            "INSERT INTO name_ruling (group_id, key, text, verdict, account_id, lines, reason, \
             seen, at_ms) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) \
             ON CONFLICT (group_id, key) DO UPDATE SET text = EXCLUDED.text, \
             verdict = EXCLUDED.verdict, account_id = EXCLUDED.account_id, \
             lines = EXCLUDED.lines, reason = EXCLUDED.reason, seen = EXCLUDED.seen, \
             at_ms = EXCLUDED.at_ms",
        )
        .bind(group.get())
        .bind(&key)
        .bind(tidy_name(&ruling.text))
        .bind(verdict.as_str())
        .bind(member)
        .bind(&lines)
        .bind(&ruling.reason)
        .bind(i32::try_from(ruling.seen).unwrap_or(i32::MAX))
        .bind(ruling.at.get())
        .execute(&mut *tx)
        .await
        .map_err(backend)?;
        // What nobody could place is placed as the ruling says.
        match verdict {
            Verdict::Member(account) => {
                sqlx::query(
                    "UPDATE name_reference SET who = 'member', account_id = $3, \
                     grounds = 'ruling', \
                     basis = CASE WHEN speaker_account = $3 THEN 'own' ELSE basis END \
                     WHERE group_id = $1 AND key = $2 AND who = 'unknown'",
                )
                .bind(group.get())
                .bind(&key)
                .bind(account.get())
                .execute(&mut *tx)
                .await
                .map_err(backend)?;
            }
            Verdict::Outside => {
                sqlx::query(
                    "UPDATE name_reference SET who = 'outside', grounds = 'ruling' \
                     WHERE group_id = $1 AND key = $2 AND who = 'unknown'",
                )
                .bind(group.get())
                .bind(&key)
                .execute(&mut *tx)
                .await
                .map_err(backend)?;
            }
            Verdict::General | Verdict::Unknown => {}
        }
        tx.commit().await.map_err(backend)?;
        Ok(())
    }

    async fn hold(&self, group: GroupId, claims: &[HeldClaim]) -> Result<(), MemoryError> {
        let mut tx = self.pool.begin().await.map_err(backend)?;
        for claim in claims {
            let Ok(key) = name_key(&claim.name) else {
                continue;
            };
            sqlx::query(
                "INSERT INTO held_claim (group_id, key, name, text, durability, episode_id, \
                 message_id, author_account, quote, at_ms) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) ON CONFLICT DO NOTHING",
            )
            .bind(group.get())
            .bind(&key)
            .bind(tidy_name(&claim.name))
            .bind(&claim.text)
            .bind(claim.durability.as_str())
            .bind(claim.support.episode.get())
            .bind(claim.support.message.get())
            .bind(claim.support.author.map(|a| a.get()))
            .bind(&claim.support.quote)
            .bind(claim.support.at.get())
            .execute(&mut *tx)
            .await
            .map_err(backend)?;
        }
        tx.commit().await.map_err(backend)?;
        Ok(())
    }

    async fn held(&self, group: GroupId) -> Result<Vec<(HeldId, HeldClaim)>, MemoryError> {
        let rows = sqlx::query(
            "SELECT held_id, name, text, durability, episode_id, message_id, author_account, \
             quote, at_ms FROM held_claim \
             WHERE group_id = $1 AND filed_account IS NULL ORDER BY held_id",
        )
        .bind(group.get())
        .fetch_all(&self.pool)
        .await
        .map_err(backend)?;
        let mut held = Vec::with_capacity(rows.len());
        for row in rows {
            let author = match row.get::<Option<i64>, _>("author_account") {
                Some(id) => Some(account(id)?),
                None => None,
            };
            held.push((
                HeldId(row.get("held_id")),
                HeldClaim {
                    name: row.get("name"),
                    text: row.get("text"),
                    durability: Durability::parse(row.get::<&str, _>("durability"))
                        .ok_or_else(|| backend("unknown durability"))?,
                    support: Support {
                        episode: EpisodeId::new(row.get("episode_id")),
                        message: MessageId::new(row.get("message_id")).map_err(backend)?,
                        author,
                        quote: row.get("quote"),
                        at: UnixMillis::new(row.get("at_ms")),
                    },
                },
            ));
        }
        Ok(held)
    }

    async fn file(
        &self,
        group: GroupId,
        id: HeldId,
        account: AccountId,
    ) -> Result<(), MemoryError> {
        sqlx::query(
            "UPDATE held_claim SET filed_account = $3 WHERE group_id = $1 AND held_id = $2",
        )
        .bind(group.get())
        .bind(id.0)
        .bind(account.get())
        .execute(&self.pool)
        .await
        .map_err(backend)?;
        Ok(())
    }
}
