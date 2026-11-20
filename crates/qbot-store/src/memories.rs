//! What is known about people and the group, on Postgres: the memory side of [`PgMemoryStore`].
//! The in-memory store in `qbot-memory` defines the semantics; the shared conformance suite runs
//! against both.

use std::collections::{BTreeMap, BTreeSet};

use async_trait::async_trait;
use qbot_core::{AccountId, GroupId, MessageId, UnixMillis};
use qbot_memory::memories::{
    Changes, Commit, Committed, HandError, HandMemory, NewMemory, Obs, Op, Suppression, Target,
    memory_key, suppression_of,
};
use qbot_memory::{
    About, Durability, EpisodeId, Grounds, Kind, KnownName, Memory, MemoryError, MemoryId,
    MemoryState, MemoryStore, NameBasis, Observation, ObservationId, Origin, Said, known_names,
    name_key,
};
use sqlx::postgres::PgRow;
use sqlx::{PgConnection, PgPool, Row};

use crate::episodes::{PgMemoryStore, backend, i64_of, insert_episode, lock_slices, taken, u64_of};

fn account(value: i64) -> Result<AccountId, MemoryError> {
    AccountId::new(value).map_err(backend)
}

fn optional_account(value: Option<i64>) -> Result<Option<AccountId>, MemoryError> {
    value.map(account).transpose()
}

const MEMORY_COLS: &str = "memory_id, group_id, subject_account, kind, with_account, term, text, \
    durability, origin, set_by, state, replaced_by, closed_ms, first_ms, last_ms";

const OBSERVATION_COLS: &str = "observation_id, memory_id, episode_id, message_id, ordinal, \
    author_account, quote, at_ms, about, about_account, about_name, kind, text, basis, grounds, \
    anchored, shown_by, term, durability, with_account";

fn to_memory(row: &PgRow) -> Result<Memory, MemoryError> {
    let kind = Kind::parse(row.get("kind")).ok_or_else(|| backend("unknown memory kind"))?;
    let origin = match row.get::<&str, _>("origin") {
        "chat" => Origin::Chat,
        "ruling" => Origin::Ruling,
        "hand" => Origin::Hand {
            by: account(row.get("set_by"))?,
        },
        other => return Err(backend(format!("unknown memory origin {other}"))),
    };
    let closed = || UnixMillis::new(row.get::<Option<i64>, _>("closed_ms").unwrap_or_default());
    let state = match row.get::<&str, _>("state") {
        "current" => MemoryState::Current,
        "replaced" => MemoryState::Replaced {
            by: row.get::<Option<i64>, _>("replaced_by").map(MemoryId::new),
            at: closed(),
        },
        "ended" => MemoryState::Ended { at: closed() },
        other => return Err(backend(format!("unknown memory state {other}"))),
    };
    Ok(Memory {
        id: MemoryId::new(row.get("memory_id")),
        group: GroupId::new(row.get("group_id")).map_err(backend)?,
        subject: optional_account(row.get("subject_account"))?,
        kind,
        with: optional_account(row.get("with_account"))?,
        term: row.get("term"),
        text: row.get("text"),
        durability: Durability::parse(row.get("durability"))
            .ok_or_else(|| backend("unknown durability"))?,
        origin,
        state,
        first: UnixMillis::new(row.get("first_ms")),
        last: UnixMillis::new(row.get("last_ms")),
        support: Vec::new(),
    })
}

fn to_observation(row: &PgRow) -> Result<Observation, MemoryError> {
    let about = match row.get::<&str, _>("about") {
        "member" => About::Member(account(row.get("about_account"))?),
        "group" => About::Group,
        "name" => About::Name(row.get("about_name")),
        other => return Err(backend(format!("unknown observation subject {other}"))),
    };
    let text: String = row.get("text");
    let said = match row.get::<&str, _>("kind") {
        "name" => Said::Name {
            text,
            basis: NameBasis::parse(row.get("basis")).ok_or_else(|| backend("unknown basis"))?,
            grounds: Grounds::parse(row.get("grounds"))
                .ok_or_else(|| backend("unknown grounds"))?,
            anchored: row.get("anchored"),
            shown_by: row
                .get::<Option<Vec<i64>>, _>("shown_by")
                .unwrap_or_default()
                .into_iter()
                .map(|m| MessageId::new(m).map_err(backend))
                .collect::<Result<_, _>>()?,
        },
        _ => Said::Statement {
            text,
            term: row.get("term"),
            durability: Durability::parse(row.get("durability"))
                .ok_or_else(|| backend("unknown durability"))?,
            with: optional_account(row.get("with_account"))?,
        },
    };
    Ok(Observation {
        id: ObservationId::new(row.get("observation_id")),
        episode: EpisodeId::new(row.get("episode_id")),
        message: MessageId::new(row.get("message_id")).map_err(backend)?,
        ordinal: u64_of(row.get("ordinal"))?,
        author: account(row.get("author_account"))?,
        quote: row.get("quote"),
        at: UnixMillis::new(row.get("at_ms")),
        about,
        said,
    })
}

/// The memories with their support, in the order given.
async fn with_support(
    conn: &mut PgConnection,
    mut memories: Vec<Memory>,
) -> Result<Vec<Memory>, MemoryError> {
    if memories.is_empty() {
        return Ok(memories);
    }
    let ids: Vec<i64> = memories.iter().map(|m| m.id.get()).collect();
    let rows = sqlx::query(&format!(
        "SELECT {OBSERVATION_COLS} FROM observation WHERE memory_id = ANY($1) \
         ORDER BY at_ms, ordinal, observation_id"
    ))
    .bind(&ids)
    .fetch_all(&mut *conn)
    .await
    .map_err(backend)?;
    let mut by_memory: BTreeMap<i64, Vec<Observation>> = BTreeMap::new();
    for row in &rows {
        by_memory
            .entry(row.get("memory_id"))
            .or_default()
            .push(to_observation(row)?);
    }
    for memory in &mut memories {
        memory.support = by_memory.remove(&memory.id.get()).unwrap_or_default();
    }
    Ok(memories)
}

/// Memories matching `filter` (a condition on the memory table, `$1` the group), with support.
async fn select(
    conn: &mut PgConnection,
    filter: &str,
    group: GroupId,
    bind: impl FnOnce(
        sqlx::query::Query<'_, sqlx::Postgres, sqlx::postgres::PgArguments>,
    ) -> sqlx::query::Query<'_, sqlx::Postgres, sqlx::postgres::PgArguments>,
) -> Result<Vec<Memory>, MemoryError> {
    let sql = format!(
        "SELECT {MEMORY_COLS} FROM memory WHERE group_id = $1 AND {filter} ORDER BY memory_id"
    );
    let rows = bind(sqlx::query(&sql).bind(group.get()))
        .fetch_all(&mut *conn)
        .await
        .map_err(backend)?;
    let memories = rows.iter().map(to_memory).collect::<Result<Vec<_>, _>>()?;
    with_support(conn, memories).await
}

fn key_of(kind: Kind, term: Option<&str>, text: &str) -> Result<String, HandError> {
    match kind {
        Kind::Name => name_key(text).map_err(|_| HandError::Refused),
        Kind::Statement => Ok(memory_key(term, text)),
    }
}

/// The current memory of a subject with this kind and key.
async fn current_of(
    conn: &mut PgConnection,
    group: GroupId,
    subject: Option<AccountId>,
    kind: Kind,
    key: &str,
) -> Result<Option<MemoryId>, MemoryError> {
    let id: Option<i64> = sqlx::query_scalar(
        "SELECT memory_id FROM memory WHERE group_id = $1 \
            AND subject_account IS NOT DISTINCT FROM $2 AND kind = $3 AND key = $4 \
            AND state = 'current'",
    )
    .bind(group.get())
    .bind(subject.map(AccountId::get))
    .bind(kind.as_str())
    .bind(key)
    .fetch_optional(&mut *conn)
    .await
    .map_err(backend)?;
    Ok(id.map(MemoryId::new))
}

async fn insert_memory(
    conn: &mut PgConnection,
    group: GroupId,
    memory: &NewMemory,
    key: &str,
) -> Result<MemoryId, MemoryError> {
    let set_by = match memory.origin {
        Origin::Hand { by } => Some(by.get()),
        Origin::Chat | Origin::Ruling => None,
    };
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO memory (group_id, subject_account, kind, with_account, term, text, key, \
            durability, origin, set_by, state, first_ms, last_ms) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, 'current', $11, $11) \
         RETURNING memory_id",
    )
    .bind(group.get())
    .bind(memory.subject.map(AccountId::get))
    .bind(memory.kind.as_str())
    .bind(memory.with.map(AccountId::get))
    .bind(&memory.term)
    .bind(&memory.text)
    .bind(key)
    .bind(memory.durability.as_str())
    .bind(memory.origin.as_str())
    .bind(set_by)
    .bind(memory.at.get())
    .fetch_one(&mut *conn)
    .await
    .map_err(backend)?;
    Ok(MemoryId::new(id))
}

/// Attach an observation to a memory, which then reaches back to it.
async fn attach(
    conn: &mut PgConnection,
    observation: ObservationId,
    memory: MemoryId,
) -> Result<(), MemoryError> {
    let at: Option<i64> = sqlx::query_scalar(
        "UPDATE observation o SET memory_id = m.memory_id, \
            about = CASE WHEN m.subject_account IS NULL THEN 'group' ELSE 'member' END, \
            about_account = m.subject_account, about_name = NULL \
         FROM memory m WHERE o.observation_id = $1 AND m.memory_id = $2 \
         RETURNING o.at_ms",
    )
    .bind(observation.get())
    .bind(memory.get())
    .fetch_optional(&mut *conn)
    .await
    .map_err(backend)?;
    if let Some(at) = at {
        sqlx::query(
            "UPDATE memory SET first_ms = least(first_ms, $2), last_ms = greatest(last_ms, $2) \
             WHERE memory_id = $1",
        )
        .bind(memory.get())
        .bind(at)
        .execute(&mut *conn)
        .await
        .map_err(backend)?;
    }
    Ok(())
}

async fn is_current(
    conn: &mut PgConnection,
    group: GroupId,
    id: MemoryId,
) -> Result<bool, MemoryError> {
    let current: Option<bool> = sqlx::query_scalar(
        "SELECT state = 'current' FROM memory WHERE memory_id = $1 AND group_id = $2",
    )
    .bind(id.get())
    .bind(group.get())
    .fetch_optional(&mut *conn)
    .await
    .map_err(backend)?;
    Ok(current == Some(true))
}

/// Whether a subject's changes still apply: everything they name exists as they expect, and
/// nothing about the subject was forgotten since the batch read it.
async fn applies(
    conn: &mut PgConnection,
    commit: &Commit,
    changes: &Changes,
) -> Result<bool, MemoryError> {
    let overtaken: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM suppression WHERE group_id = $1 \
            AND subject_account IS NOT DISTINCT FROM $2 AND at_ms > $3)",
    )
    .bind(commit.group.get())
    .bind(changes.subject.map(AccountId::get))
    .bind(commit.read.get())
    .fetch_one(&mut *conn)
    .await
    .map_err(backend)?;
    if overtaken {
        return Ok(false);
    }
    let mut named: Vec<MemoryId> = Vec::new();
    for op in &changes.ops {
        match op {
            Op::Support {
                memory: Target::Existing(id),
                ..
            } => named.push(*id),
            Op::Close { memory, by, .. } => {
                named.push(*memory);
                if let Some(Target::Existing(id)) = by {
                    named.push(*id);
                }
            }
            Op::Merge { from, into } => {
                named.push(*from);
                if let Target::Existing(id) = into {
                    named.push(*id);
                }
            }
            _ => {}
        }
    }
    for id in named {
        if !is_current(conn, commit.group, id).await? {
            return Ok(false);
        }
    }
    Ok(true)
}

async fn apply(
    conn: &mut PgConnection,
    group: GroupId,
    changes: &Changes,
    stored: &[Option<ObservationId>],
) -> Result<(), MemoryError> {
    let obs = |o: &Obs| match o {
        Obs::New(index) => stored.get(*index).copied().flatten(),
        Obs::Stored(id) => Some(*id),
    };
    let mut created: Vec<MemoryId> = Vec::new();
    let target = |t: &Target, created: &[MemoryId]| match t {
        Target::Existing(id) => Some(*id),
        Target::Created(index) => created.get(*index).copied(),
    };
    for op in &changes.ops {
        match op {
            Op::Create(new) => {
                let key = match new.kind {
                    Kind::Name => name_key(&new.text).unwrap_or_default(),
                    Kind::Statement => memory_key(new.term.as_deref(), &new.text),
                };
                let id = match current_of(conn, group, new.subject, new.kind, &key).await? {
                    Some(id) => id,
                    None => insert_memory(conn, group, new, &key).await?,
                };
                created.push(id);
            }
            Op::Support { obs: o, memory } => {
                if let (Some(o), Some(m)) = (obs(o), target(memory, &created)) {
                    attach(conn, o, m).await?;
                }
            }
            Op::Close { memory, by, at } => {
                let by = by.as_ref().and_then(|t| target(t, &created));
                sqlx::query(
                    "UPDATE memory SET state = $2, replaced_by = $3, closed_ms = $4 \
                     WHERE memory_id = $1 AND state = 'current'",
                )
                .bind(memory.get())
                .bind(if by.is_some() { "replaced" } else { "ended" })
                .bind(by.map(MemoryId::get))
                .bind(at.get())
                .execute(&mut *conn)
                .await
                .map_err(backend)?;
            }
            Op::Merge { from, into } => {
                let Some(into) = target(into, &created) else {
                    continue;
                };
                let moving: Vec<i64> = sqlx::query_scalar(
                    "SELECT observation_id FROM observation WHERE memory_id = $1",
                )
                .bind(from.get())
                .fetch_all(&mut *conn)
                .await
                .map_err(backend)?;
                for id in moving {
                    attach(conn, ObservationId::new(id), into).await?;
                }
                sqlx::query("UPDATE memory SET replaced_by = $2 WHERE replaced_by = $1")
                    .bind(from.get())
                    .bind(into.get())
                    .execute(&mut *conn)
                    .await
                    .map_err(backend)?;
                sqlx::query("DELETE FROM memory WHERE memory_id = $1")
                    .bind(from.get())
                    .execute(&mut *conn)
                    .await
                    .map_err(backend)?;
            }
            Op::Drop(id) => {
                sqlx::query(
                    "DELETE FROM observation WHERE observation_id = $1 AND group_id = $2 \
                        AND memory_id IS NULL",
                )
                .bind(id.get())
                .bind(group.get())
                .execute(&mut *conn)
                .await
                .map_err(backend)?;
            }
            Op::Refile { obs: o, account } => {
                if let Some(id) = obs(o) {
                    sqlx::query(
                        "UPDATE observation SET about = 'member', about_account = $2, \
                            about_name = NULL WHERE observation_id = $1",
                    )
                    .bind(id.get())
                    .bind(account.get())
                    .execute(&mut *conn)
                    .await
                    .map_err(backend)?;
                }
            }
            Op::Evict(id) => {
                sqlx::query("DELETE FROM memory WHERE memory_id = $1 AND group_id = $2")
                    .bind(id.get())
                    .bind(group.get())
                    .execute(&mut *conn)
                    .await
                    .map_err(backend)?;
            }
        }
    }
    Ok(())
}

fn about_columns(about: &About) -> (&'static str, Option<i64>, Option<&str>) {
    match about {
        About::Member(account) => ("member", Some(account.get()), None),
        About::Group => ("group", None, None),
        About::Name(name) => ("name", None, Some(name.as_str())),
    }
}

#[async_trait]
impl MemoryStore for PgMemoryStore {
    async fn about(
        &self,
        group: GroupId,
        subject: Option<AccountId>,
    ) -> Result<Vec<Memory>, MemoryError> {
        let mut conn = self.pool.acquire().await.map_err(backend)?;
        select(
            &mut conn,
            "subject_account IS NOT DISTINCT FROM $2",
            group,
            |q| q.bind(subject.map(AccountId::get)),
        )
        .await
    }

    async fn current(
        &self,
        group: GroupId,
        subjects: &[Option<AccountId>],
        kind: Kind,
    ) -> Result<Vec<Memory>, MemoryError> {
        let accounts: Vec<i64> = subjects.iter().flatten().map(|a| a.get()).collect();
        let the_group = subjects.contains(&None);
        let mut conn = self.pool.acquire().await.map_err(backend)?;
        select(
            &mut conn,
            "kind = $2 AND state = 'current' \
                AND (subject_account = ANY($3) OR ($4 AND subject_account IS NULL))",
            group,
            |q| q.bind(kind.as_str()).bind(accounts).bind(the_group),
        )
        .await
    }

    async fn names(&self, group: GroupId) -> Result<Vec<Memory>, MemoryError> {
        let mut conn = self.pool.acquire().await.map_err(backend)?;
        select(
            &mut conn,
            "kind = 'name' AND state = 'current' AND subject_account IS NOT NULL",
            group,
            |q| q,
        )
        .await
    }

    async fn memory(&self, group: GroupId, id: MemoryId) -> Result<Option<Memory>, MemoryError> {
        let mut conn = self.pool.acquire().await.map_err(backend)?;
        Ok(
            select(&mut conn, "memory_id = $2", group, |q| q.bind(id.get()))
                .await?
                .pop(),
        )
    }

    async fn pending(&self, group: GroupId) -> Result<Vec<Observation>, MemoryError> {
        let rows = sqlx::query(&format!(
            "SELECT {OBSERVATION_COLS} FROM observation WHERE group_id = $1 AND memory_id IS NULL \
             ORDER BY ordinal, observation_id"
        ))
        .bind(group.get())
        .fetch_all(&self.pool)
        .await
        .map_err(backend)?;
        rows.iter().map(to_observation).collect()
    }

    async fn suppressions(&self, group: GroupId) -> Result<Vec<Suppression>, MemoryError> {
        let rows = sqlx::query(
            "SELECT subject_account, kind, text, key, messages, by_account, at_ms \
             FROM suppression WHERE group_id = $1 ORDER BY suppression_id",
        )
        .bind(group.get())
        .fetch_all(&self.pool)
        .await
        .map_err(backend)?;
        rows.iter()
            .map(|row| {
                Ok(Suppression {
                    subject: optional_account(row.get("subject_account"))?,
                    kind: Kind::parse(row.get("kind")).ok_or_else(|| backend("unknown kind"))?,
                    text: row.get("text"),
                    key: row.get("key"),
                    messages: row
                        .get::<Vec<i64>, _>("messages")
                        .into_iter()
                        .map(|m| MessageId::new(m).map_err(backend))
                        .collect::<Result<_, _>>()?,
                    by: account(row.get("by_account"))?,
                    at: UnixMillis::new(row.get("at_ms")),
                })
            })
            .collect()
    }

    async fn commit(&self, commit: &Commit) -> Result<Committed, MemoryError> {
        let group = commit.group;
        let mut tx = self.pool.begin().await.map_err(backend)?;
        lock_slices(&mut tx, group).await?;
        // Everything is checked before anything is written.
        let mut ranges: Vec<(u64, u64)> = commit
            .episodes
            .iter()
            .map(|(e, _, _)| (e.first_ordinal, e.last_ordinal))
            .collect();
        ranges.extend(commit.skipped.iter().map(|(a, b, _)| (*a, *b)));
        for (index, (first, last)) in ranges.iter().enumerate() {
            if first > last {
                return Err(MemoryError::InvalidRange);
            }
            let among = ranges[..index].iter().any(|(a, b)| a <= last && first <= b);
            if among || taken(&mut tx, group, i64_of(*first)?, i64_of(*last)?).await? {
                return Err(MemoryError::Overlap);
            }
        }
        let mut applying = Vec::new();
        for changes in &commit.changes {
            applying.push(applies(&mut tx, commit, changes).await?);
        }

        let now = self.clock.now();
        let mut episodes: Vec<EpisodeId> = Vec::new();
        for (episode, vector, model) in &commit.episodes {
            episodes.push(
                insert_episode(&mut tx, episode, vector, model, now)
                    .await?
                    .id,
            );
        }
        if let Some((first, last, reason)) = &commit.skipped {
            sqlx::query(
                "INSERT INTO slice_skip (group_id, first_ordinal, last_ordinal, reason, created_ms) \
                 VALUES ($1, $2, $3, $4, $5)",
            )
            .bind(group.get())
            .bind(i64_of(*first)?)
            .bind(i64_of(*last)?)
            .bind(reason)
            .bind(now.get())
            .execute(&mut *tx)
            .await
            .map_err(crate::episodes::insert_error)?;
        }
        // What was forgotten since the batch read: no new observation about its subject reads
        // those lines again.
        let mut forgotten: BTreeSet<(Option<i64>, i64)> = BTreeSet::new();
        for row in sqlx::query(
            "SELECT subject_account, messages FROM suppression WHERE group_id = $1 AND at_ms > $2",
        )
        .bind(group.get())
        .bind(commit.read.get())
        .fetch_all(&mut *tx)
        .await
        .map_err(backend)?
        {
            let subject: Option<i64> = row.get("subject_account");
            for message in row.get::<Vec<i64>, _>("messages") {
                forgotten.insert((subject, message));
            }
        }
        let mut stored: Vec<Option<ObservationId>> = Vec::new();
        for new in &commit.observations {
            let reads_forgotten = !matches!(new.about, About::Name(_))
                && forgotten
                    .contains(&(new.about.subject().map(AccountId::get), new.message.get()));
            let Some(episode) = episodes.get(new.episode).filter(|_| !reads_forgotten) else {
                stored.push(None);
                continue;
            };
            let (about, about_account, about_name) = about_columns(&new.about);
            let (kind, text, basis, grounds, anchored, shown_by, term, durability, with) =
                match &new.said {
                    Said::Name {
                        text,
                        basis,
                        grounds,
                        anchored,
                        shown_by,
                    } => (
                        "name",
                        text.as_str(),
                        Some(basis.as_str()),
                        Some(grounds.as_str()),
                        Some(*anchored),
                        Some(shown_by.iter().map(|m| m.get()).collect::<Vec<i64>>()),
                        None,
                        None,
                        None,
                    ),
                    Said::Statement {
                        text,
                        term,
                        durability,
                        with,
                    } => (
                        "statement",
                        text.as_str(),
                        None,
                        None,
                        None,
                        None,
                        term.as_deref(),
                        Some(durability.as_str()),
                        with.map(AccountId::get),
                    ),
                };
            let id: i64 = sqlx::query_scalar(
                "INSERT INTO observation (group_id, episode_id, message_id, ordinal, author_account, \
                    quote, at_ms, about, about_account, about_name, kind, text, basis, grounds, \
                    anchored, shown_by, term, durability, with_account) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, \
                    $17, $18, $19) RETURNING observation_id",
            )
            .bind(group.get())
            .bind(episode.get())
            .bind(new.message.get())
            .bind(i64_of(new.ordinal)?)
            .bind(new.author.get())
            .bind(&new.quote)
            .bind(new.at.get())
            .bind(about)
            .bind(about_account)
            .bind(about_name)
            .bind(kind)
            .bind(text)
            .bind(basis)
            .bind(grounds)
            .bind(anchored)
            .bind(shown_by)
            .bind(term)
            .bind(durability)
            .bind(with)
            .fetch_one(&mut *tx)
            .await
            .map_err(backend)?;
            stored.push(Some(ObservationId::new(id)));
        }
        let mut deferred = 0;
        for (changes, apply_them) in commit.changes.iter().zip(applying) {
            if apply_them {
                apply(&mut tx, group, changes, &stored).await?;
            } else {
                deferred += 1;
            }
        }
        sqlx::query(
            "INSERT INTO history_window (group_id, batched_ms, cut) VALUES ($1, $2, 0) \
             ON CONFLICT (group_id) DO UPDATE SET batched_ms = EXCLUDED.batched_ms, cut = 0",
        )
        .bind(group.get())
        .bind(commit.at.get())
        .execute(&mut *tx)
        .await
        .map_err(backend)?;
        tx.commit().await.map_err(backend)?;
        Ok(Committed {
            episodes: episodes.len(),
            deferred,
        })
    }

    async fn add(&self, group: GroupId, memory: &HandMemory) -> Result<MemoryId, HandError> {
        let key = key_of(memory.kind, memory.term.as_deref(), &memory.text)?;
        let mut tx = self.pool.begin().await.map_err(backend)?;
        lock_memories(&mut tx, group).await?;
        if let Some(id) = current_of(&mut tx, group, memory.subject, memory.kind, &key).await? {
            return Err(HandError::Known(id));
        }
        lift(&mut tx, group, memory.subject, memory.kind, &key).await?;
        let id = insert_memory(
            &mut tx,
            group,
            &NewMemory {
                subject: memory.subject,
                kind: memory.kind,
                with: None,
                term: memory.term.clone(),
                text: memory.text.clone(),
                durability: Durability::Lasting,
                origin: Origin::Hand { by: memory.by },
                at: memory.at,
            },
            &key,
        )
        .await?;
        tx.commit().await.map_err(backend)?;
        Ok(id)
    }

    async fn fix(
        &self,
        group: GroupId,
        id: MemoryId,
        text: &str,
        by: AccountId,
        at: UnixMillis,
    ) -> Result<MemoryId, HandError> {
        let mut tx = self.pool.begin().await.map_err(backend)?;
        lock_memories(&mut tx, group).await?;
        let old = select(
            &mut tx,
            "memory_id = $2 AND state = 'current'",
            group,
            |q| q.bind(id.get()),
        )
        .await?
        .pop()
        .ok_or(HandError::NotFound)?;
        let key = key_of(old.kind, old.term.as_deref(), text)?;
        if let Some(existing) = current_of(&mut tx, group, old.subject, old.kind, &key).await?
            && existing != id
        {
            return Err(HandError::Known(existing));
        }
        lift(&mut tx, group, old.subject, old.kind, &key).await?;
        sqlx::query("UPDATE memory SET state = 'replaced', closed_ms = $2 WHERE memory_id = $1")
            .bind(id.get())
            .bind(at.get())
            .execute(&mut *tx)
            .await
            .map_err(backend)?;
        let new = insert_memory(
            &mut tx,
            group,
            &NewMemory {
                subject: old.subject,
                kind: old.kind,
                with: old.with,
                term: old.term.clone(),
                text: text.to_owned(),
                durability: old.durability,
                origin: Origin::Hand { by },
                at,
            },
            &key,
        )
        .await?;
        sqlx::query("UPDATE memory SET replaced_by = $2 WHERE memory_id = $1")
            .bind(id.get())
            .bind(new.get())
            .execute(&mut *tx)
            .await
            .map_err(backend)?;
        tx.commit().await.map_err(backend)?;
        Ok(new)
    }

    async fn end(&self, group: GroupId, id: MemoryId, at: UnixMillis) -> Result<(), HandError> {
        let ended = sqlx::query(
            "UPDATE memory SET state = 'ended', closed_ms = $3 \
             WHERE memory_id = $1 AND group_id = $2 AND state = 'current'",
        )
        .bind(id.get())
        .bind(group.get())
        .bind(at.get())
        .execute(&self.pool)
        .await
        .map_err(backend)?
        .rows_affected();
        if ended == 0 {
            return Err(HandError::NotFound);
        }
        Ok(())
    }

    async fn forget(
        &self,
        group: GroupId,
        id: MemoryId,
        by: AccountId,
        at: UnixMillis,
    ) -> Result<Option<Memory>, MemoryError> {
        let mut tx = self.pool.begin().await.map_err(backend)?;
        lock_memories(&mut tx, group).await?;
        let Some(memory) = select(&mut tx, "memory_id = $2", group, |q| q.bind(id.get()))
            .await?
            .pop()
        else {
            return Ok(None);
        };
        let suppression = suppression_of(&memory, by, at);
        sqlx::query("DELETE FROM memory WHERE memory_id = $1")
            .bind(id.get())
            .execute(&mut *tx)
            .await
            .map_err(backend)?;
        sqlx::query(
            "INSERT INTO suppression (group_id, subject_account, kind, text, key, messages, \
                by_account, at_ms) VALUES ($1, $2, $3, $4, $5, $6, $7, $8) \
             ON CONFLICT (group_id, coalesce(subject_account, 0), kind, key) DO UPDATE SET \
                text = EXCLUDED.text, messages = EXCLUDED.messages, \
                by_account = EXCLUDED.by_account, at_ms = EXCLUDED.at_ms",
        )
        .bind(group.get())
        .bind(suppression.subject.map(AccountId::get))
        .bind(suppression.kind.as_str())
        .bind(&suppression.text)
        .bind(&suppression.key)
        .bind(
            suppression
                .messages
                .iter()
                .map(|m| m.get())
                .collect::<Vec<i64>>(),
        )
        .bind(by.get())
        .bind(at.get())
        .execute(&mut *tx)
        .await
        .map_err(backend)?;
        tx.commit().await.map_err(backend)?;
        Ok(Some(memory))
    }
}

/// One writer of a group's memories by hand at a time: a check that a name or statement is not
/// known yet holds until it is entered.
async fn lock_memories(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    group: GroupId,
) -> Result<(), MemoryError> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('memory:' || $1::text, 0))")
        .bind(group.get())
        .execute(&mut **tx)
        .await
        .map_err(backend)?;
    Ok(())
}

/// A person's word lifts what forgot this.
async fn lift(
    conn: &mut PgConnection,
    group: GroupId,
    subject: Option<AccountId>,
    kind: Kind,
    key: &str,
) -> Result<(), MemoryError> {
    sqlx::query(
        "DELETE FROM suppression WHERE group_id = $1 \
            AND subject_account IS NOT DISTINCT FROM $2 AND kind = $3 AND key = $4",
    )
    .bind(group.get())
    .bind(subject.map(AccountId::get))
    .bind(kind.as_str())
    .bind(key)
    .execute(&mut *conn)
    .await
    .map_err(backend)?;
    Ok(())
}

/// The group's names as the roster reads them.
pub(crate) async fn group_names(
    pool: &PgPool,
    group: GroupId,
) -> Result<Vec<KnownName>, MemoryError> {
    let mut conn = pool.acquire().await.map_err(backend)?;
    let memories = select(
        &mut conn,
        "kind = 'name' AND state = 'current' AND subject_account IS NOT NULL",
        group,
        |q| q,
    )
    .await?;
    Ok(known_names(&memories))
}
