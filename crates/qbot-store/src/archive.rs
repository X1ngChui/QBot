//! The chat archive and stable member numbers.

use async_trait::async_trait;
use qbot_agent::{Archive, ArchiveCursor, ArchivedLine, EnvError, HistoryQuery, TextQuery};
use qbot_context::{ChatLine, Speaker};
use qbot_core::{AccountId, GroupId, MediaKind, MemberNo, MessageId, UnixMillis};
use sqlx::postgres::PgRow;
use sqlx::{PgPool, Row};

use crate::error::StoreError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NewSpeaker {
    Bot,
    Member(AccountId),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewLine {
    pub group: GroupId,
    pub message: MessageId,
    pub speaker: NewSpeaker,
    pub at: UnixMillis,
    pub text: String,
}

/// A platform reference to one picture, sticker, clip or forwarded record of a line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaRefRow {
    pub kind: MediaKind,
    /// Position among the markers of this kind in the line, from 0.
    pub index: u32,
    pub key: Option<String>,
    pub file: Option<String>,
    pub url: Option<String>,
    pub size: Option<u64>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Appended {
    Stored {
        line: ChatLine,
        seq: ArchiveCursor,
        /// The line's place in the group's archive (1-based, dense).
        ordinal: u64,
    },
    /// The message id was already archived; nothing changed.
    Duplicate,
}

#[derive(Debug, Clone)]
pub struct PgArchive {
    pool: PgPool,
}

/// Lines with their author's member number.
const SELECT_LINES: &str = "\
    SELECT l.seq, l.ordinal, l.message_id, l.speaker, l.account_id, l.at_ms, l.text, m.number \
    FROM chat_line l \
    LEFT JOIN member_number m ON m.group_id = l.group_id AND m.account_id = l.account_id ";

impl PgArchive {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub(crate) fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Archive one line. Members get a stable number on first appearance, assigned densely per
    /// group under a lock, so concurrent first appearances cannot collide or leave gaps.
    pub async fn append(&self, new: NewLine) -> Result<Appended, StoreError> {
        self.append_with_mentions(new, &[]).await
    }

    /// Like [`append`](Self::append) for a line whose text mentions other accounts as
    /// `[at:ACCOUNT]`. Each mentioned account gets a member number (so the model can address
    /// someone who has never spoken) and its marker is rewritten to `[at:NUMBER]` before the
    /// line is stored.
    pub async fn append_with_mentions(
        &self,
        new: NewLine,
        mentions: &[AccountId],
    ) -> Result<Appended, StoreError> {
        self.append_line(new, mentions, &[]).await
    }

    /// Archive a line with its mentions (see [`append_with_mentions`](Self::append_with_mentions))
    /// and the references to its media, all in one transaction.
    pub async fn append_line(
        &self,
        new: NewLine,
        mentions: &[AccountId],
        media: &[MediaRefRow],
    ) -> Result<Appended, StoreError> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('archive:' || $1::text, 0))")
            .bind(new.group.get())
            .execute(&mut *tx)
            .await?;
        let mut numbers = Vec::with_capacity(mentions.len());
        for account in mentions {
            crate::identity::ensure_account(&mut tx, account.get(), new.at.get()).await?;
            numbers.push((
                account.get(),
                ensure_number(&mut tx, new.group, *account).await?,
            ));
        }
        let text = number_mentions(&new.text, &numbers);
        let (speaker, account) = match new.speaker {
            NewSpeaker::Bot => ("bot", None),
            NewSpeaker::Member(account) => ("member", Some(account.get())),
        };
        let inserted = sqlx::query(
            "INSERT INTO chat_line (group_id, ordinal, message_id, speaker, account_id, at_ms, text) \
             SELECT $1, coalesce(max(ordinal), 0) + 1, $2, $3, $4, $5, $6 FROM chat_line WHERE group_id = $1 \
             ON CONFLICT (group_id, message_id) DO NOTHING RETURNING seq, ordinal",
        )
        .bind(new.group.get())
        .bind(new.message.get())
        .bind(speaker)
        .bind(account)
        .bind(new.at.get())
        .bind(&text)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(row) = inserted else {
            tx.rollback().await?;
            return Ok(Appended::Duplicate);
        };
        let seq: i64 = row.get("seq");
        let ordinal: i64 = row.get("ordinal");

        sqlx::query(
            "INSERT INTO group_state (group_id, first_seen_ms) VALUES ($1, $2) ON CONFLICT DO NOTHING",
        )
        .bind(new.group.get())
        .bind(new.at.get())
        .execute(&mut *tx)
        .await?;
        if let Some(account) = account {
            crate::identity::ensure_account(&mut tx, account, new.at.get()).await?;
            ensure_number(
                &mut tx,
                new.group,
                AccountId::new(account).map_err(|e| StoreError::corrupt(e.to_string()))?,
            )
            .await?;
        }
        for item in media {
            sqlx::query(
                "INSERT INTO media_ref (group_id, message_id, kind, idx, key, file, url, size_bytes) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
            )
            .bind(new.group.get())
            .bind(new.message.get())
            .bind(item.kind.marker())
            .bind(i32::try_from(item.index).map_err(|e| StoreError::corrupt(e.to_string()))?)
            .bind(&item.key)
            .bind(&item.file)
            .bind(&item.url)
            .bind(item.size.and_then(|s| i64::try_from(s).ok()))
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;

        let archived = self.line_by_seq(seq).await?;
        Ok(Appended::Stored {
            line: archived.line,
            seq: archived.seq,
            ordinal: u64::try_from(ordinal).map_err(|e| StoreError::corrupt(e.to_string()))?,
        })
    }

    /// The stored reference to one media item of a line.
    pub async fn media_ref(
        &self,
        group: GroupId,
        message: MessageId,
        kind: MediaKind,
        index: u32,
    ) -> Result<Option<MediaRefRow>, StoreError> {
        let row = sqlx::query(
            "SELECT idx, key, file, url, size_bytes FROM media_ref \
             WHERE group_id = $1 AND message_id = $2 AND kind = $3 AND idx = $4",
        )
        .bind(group.get())
        .bind(message.get())
        .bind(kind.marker())
        .bind(i32::try_from(index).unwrap_or(i32::MAX))
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| MediaRefRow {
            kind,
            index: u32::try_from(r.get::<i32, _>("idx")).unwrap_or(0),
            key: r.get("key"),
            file: r.get("file"),
            url: r.get("url"),
            size: r
                .get::<Option<i64>, _>("size_bytes")
                .and_then(|s| u64::try_from(s).ok()),
        }))
    }

    /// Whether `message` is a line the bot itself sent to `group`.
    pub async fn is_bot_message(
        &self,
        group: GroupId,
        message: MessageId,
    ) -> Result<bool, StoreError> {
        let row = sqlx::query(
            "SELECT speaker = 'bot' AS bot FROM chat_line WHERE group_id = $1 AND message_id = $2",
        )
        .bind(group.get())
        .bind(message.get())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.is_some_and(|r| r.get::<bool, _>("bot")))
    }

    /// Replace a stored line's text with `edit(current)`, if `edit` returns one. Used to fill in
    /// what a picture or clip turned out to contain after the line was archived. Returns whether
    /// the text changed. The line is locked while it is edited, so concurrent edits of two
    /// markers in one message both land.
    pub async fn rewrite_text<F>(
        &self,
        group: GroupId,
        message: MessageId,
        edit: F,
    ) -> Result<bool, StoreError>
    where
        F: FnOnce(&str) -> Option<String>,
    {
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query(
            "SELECT text FROM chat_line WHERE group_id = $1 AND message_id = $2 FOR UPDATE",
        )
        .bind(group.get())
        .bind(message.get())
        .fetch_optional(&mut *tx)
        .await?;
        let Some(row) = row else {
            return Ok(false);
        };
        let current: String = row.get("text");
        let Some(next) = edit(&current).filter(|next| *next != current) else {
            return Ok(false);
        };
        sqlx::query("UPDATE chat_line SET text = $3 WHERE group_id = $1 AND message_id = $2")
            .bind(group.get())
            .bind(message.get())
            .bind(&next)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(true)
    }

    /// The last `limit` lines of a group, oldest first, and the cursor after the newest.
    pub async fn recent(
        &self,
        group: GroupId,
        limit: i64,
    ) -> Result<(Vec<ChatLine>, ArchiveCursor), StoreError> {
        let sql = format!(
            "SELECT * FROM ({SELECT_LINES} WHERE l.group_id = $1 ORDER BY l.seq DESC LIMIT $2) recent \
             ORDER BY seq"
        );
        let rows = sqlx::query(&sql)
            .bind(group.get())
            .bind(limit)
            .fetch_all(&self.pool)
            .await?;
        let lines = rows.iter().map(to_line).collect::<Result<Vec<_>, _>>()?;
        let cursor = lines.last().map_or(ArchiveCursor(0), |a| a.seq);
        Ok((lines.into_iter().map(|a| a.line).collect(), cursor))
    }

    /// The accounts behind those of `numbers` that are member numbers of `group`.
    pub async fn members(
        &self,
        group: GroupId,
        numbers: &[MemberNo],
    ) -> Result<Vec<(MemberNo, AccountId)>, StoreError> {
        let numbers: Vec<i32> = numbers
            .iter()
            .filter_map(|n| i32::try_from(n.get()).ok())
            .collect();
        let rows = sqlx::query(
            "SELECT number, account_id FROM member_number WHERE group_id = $1 AND number = ANY($2) \
             ORDER BY number",
        )
        .bind(group.get())
        .bind(&numbers)
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|r| {
                let number = u32::try_from(r.get::<i32, _>("number"))
                    .map(MemberNo::new)
                    .map_err(|e| StoreError::corrupt(e.to_string()))?;
                let account = AccountId::new(r.get("account_id"))
                    .map_err(|e| StoreError::corrupt(e.to_string()))?;
                Ok((number, account))
            })
            .collect()
    }

    /// The ordinal of the group's newest line, 0 when it has none.
    pub async fn last_ordinal(&self, group: GroupId) -> Result<u64, StoreError> {
        let last: Option<i64> =
            sqlx::query("SELECT max(ordinal) AS last FROM chat_line WHERE group_id = $1")
                .bind(group.get())
                .fetch_one(&self.pool)
                .await?
                .get("last");
        Ok(last.map_or(0, |l| u64::try_from(l).unwrap_or(0)))
    }

    /// The group's lines from ordinal `first` on, as `(ordinal, line)` in order, and the cursor
    /// after the newest.
    pub async fn lines_from(
        &self,
        group: GroupId,
        first: u64,
    ) -> Result<(Vec<(u64, ChatLine)>, ArchiveCursor), StoreError> {
        let start = i64::try_from(first).map_err(|e| StoreError::corrupt(e.to_string()))?;
        let sql =
            format!("{SELECT_LINES} WHERE l.group_id = $1 AND l.ordinal >= $2 ORDER BY l.ordinal");
        let rows = sqlx::query(&sql)
            .bind(group.get())
            .bind(start)
            .fetch_all(&self.pool)
            .await?;
        let lines = rows
            .iter()
            .map(|row| {
                let ordinal = u64::try_from(row.get::<i64, _>("ordinal"))
                    .map_err(|e| StoreError::corrupt(e.to_string()))?;
                Ok((ordinal, to_line(row)?))
            })
            .collect::<Result<Vec<_>, StoreError>>()?;
        let cursor = lines.last().map_or(ArchiveCursor(0), |(_, a)| a.seq);
        Ok((
            lines.into_iter().map(|(o, a)| (o, a.line)).collect(),
            cursor,
        ))
    }

    async fn line_by_seq(&self, seq: i64) -> Result<ArchivedLine, StoreError> {
        let sql = format!("{SELECT_LINES} WHERE l.seq = $1");
        let row = sqlx::query(&sql).bind(seq).fetch_one(&self.pool).await?;
        to_line(&row)
    }
}

/// `text` with each `[at:ACCOUNT]` of `numbers` written `[at:NUMBER]`, in one pass: a number
/// written for one account is never read again as another account.
fn number_mentions(text: &str, numbers: &[(i64, i32)]) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find("[at:") {
        out.push_str(&rest[..at]);
        let marker = &rest[at..];
        let replaced = marker.find(']').and_then(|close| {
            let account: i64 = marker[4..close].parse().ok()?;
            let (_, number) = numbers.iter().find(|(a, _)| *a == account)?;
            Some((format!("[at:{number}]"), close + 1))
        });
        match replaced {
            Some((marker, len)) => {
                out.push_str(&marker);
                rest = &rest[at + len..];
            }
            None => {
                out.push_str("[at:");
                rest = &rest[at + 4..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// The member number of `account` in `group`, assigned densely on first use. The caller holds
/// the group's archive lock.
async fn ensure_number(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    group: GroupId,
    account: AccountId,
) -> Result<i32, StoreError> {
    sqlx::query(
        "INSERT INTO member_number (group_id, account_id, number) \
         SELECT $1, $2, coalesce(max(number), 0) + 1 FROM member_number WHERE group_id = $1 \
         ON CONFLICT (group_id, account_id) DO NOTHING",
    )
    .bind(group.get())
    .bind(account.get())
    .execute(&mut **tx)
    .await?;
    let row =
        sqlx::query("SELECT number FROM member_number WHERE group_id = $1 AND account_id = $2")
            .bind(group.get())
            .bind(account.get())
            .fetch_one(&mut **tx)
            .await?;
    Ok(row.get("number"))
}

fn to_line(row: &PgRow) -> Result<ArchivedLine, StoreError> {
    let seq: i64 = row.get("seq");
    let message =
        MessageId::new(row.get("message_id")).map_err(|e| StoreError::corrupt(e.to_string()))?;
    let speaker = match row.get::<String, _>("speaker").as_str() {
        "bot" => Speaker::Bot,
        "member" => {
            let account = AccountId::new(row.get("account_id"))
                .map_err(|e| StoreError::corrupt(e.to_string()))?;
            let number: Option<i32> = row.get("number");
            let number = number
                .and_then(|n| u32::try_from(n).ok())
                .ok_or_else(|| StoreError::corrupt("member line without a member number"))?;
            Speaker::Member {
                account,
                number: MemberNo::new(number),
            }
        }
        other => return Err(StoreError::corrupt(format!("unknown speaker {other:?}"))),
    };
    Ok(ArchivedLine {
        seq: ArchiveCursor(u64::try_from(seq).map_err(|_| StoreError::corrupt("negative seq"))?),
        line: ChatLine {
            message,
            speaker,
            at: UnixMillis::new(row.get("at_ms")),
            text: row.get("text"),
        },
    })
}

fn env(error: impl std::fmt::Display) -> EnvError {
    EnvError(error.to_string())
}

#[async_trait]
impl Archive for PgArchive {
    async fn since(
        &self,
        group: GroupId,
        cursor: ArchiveCursor,
    ) -> Result<Vec<ArchivedLine>, EnvError> {
        let sql = format!("{SELECT_LINES} WHERE l.group_id = $1 AND l.seq > $2 ORDER BY l.seq");
        let rows = sqlx::query(&sql)
            .bind(group.get())
            .bind(i64::try_from(cursor.0).map_err(env)?)
            .fetch_all(&self.pool)
            .await
            .map_err(env)?;
        rows.iter()
            .map(to_line)
            .collect::<Result<_, _>>()
            .map_err(env)
    }

    /// Case-insensitive substring match, newest first. `position` is used instead of `LIKE` so
    /// that `%` and `_` in the query are literal characters.
    async fn search(
        &self,
        group: GroupId,
        query: &HistoryQuery,
        limit: usize,
    ) -> Result<Vec<ChatLine>, EnvError> {
        // $1 is the group; terms follow, then the speaker and the limit. Only placeholders and
        // fixed text go into the SQL.
        let mut terms = Vec::new();
        let condition = text_condition(&query.text, &mut terms, 2);
        let mut next = 2 + terms.len();
        let speaker = query.speaker.map(|_| {
            let clause = format!(" AND m.number = ${next}");
            next += 1;
            clause
        });
        let sql = format!(
            "{SELECT_LINES} WHERE l.group_id = $1 AND ({condition}){} ORDER BY l.seq DESC LIMIT ${next}",
            speaker.as_deref().unwrap_or("")
        );
        let mut statement = sqlx::query(&sql).bind(group.get());
        for term in terms {
            statement = statement.bind(term);
        }
        if let Some(number) = query.speaker {
            statement = statement.bind(i32::try_from(number.get()).map_err(env)?);
        }
        let rows = statement
            .bind(i64::try_from(limit).map_err(env)?)
            .fetch_all(&self.pool)
            .await
            .map_err(env)?;
        rows.iter()
            .map(|row| to_line(row).map(|a| a.line))
            .collect::<Result<_, _>>()
            .map_err(env)
    }
}

/// `query` as a condition on `l.text`. Each term becomes a bind parameter numbered from `first`
/// and matches as a case-insensitive substring (`position`, not `LIKE`, so `%` and `_` in a term
/// are literal, and not full-text search, whose word splitting does not fit Chinese).
fn text_condition(query: &TextQuery, terms: &mut Vec<String>, first: usize) -> String {
    match query {
        TextQuery::Contains(term) => {
            terms.push(term.clone());
            format!(
                "position(lower(${}) in lower(l.text)) > 0",
                first + terms.len() - 1
            )
        }
        TextQuery::Not(inner) => format!("NOT ({})", text_condition(inner, terms, first)),
        TextQuery::All(all) if all.is_empty() => "TRUE".into(),
        TextQuery::Any(any) if any.is_empty() => "FALSE".into(),
        TextQuery::All(all) => all
            .iter()
            .map(|q| format!("({})", text_condition(q, terms, first)))
            .collect::<Vec<_>>()
            .join(" AND "),
        TextQuery::Any(any) => any
            .iter()
            .map(|q| format!("({})", text_condition(q, terms, first)))
            .collect::<Vec<_>>()
            .join(" OR "),
    }
}

#[cfg(test)]
mod tests {
    use super::number_mentions;

    #[test]
    fn mentions_are_numbered_in_one_pass() {
        // Account 5 becomes member 1 and account 1 member 2: the 1 written for account 5 must
        // not then be taken for account 1.
        assert_eq!(
            number_mentions(
                "[at:5] and [at:1], [at:bot] [at:all] [at:77]",
                &[(5, 1), (1, 2)]
            ),
            "[at:1] and [at:2], [at:bot] [at:all] [at:77]"
        );
        assert_eq!(
            number_mentions("no marker [at:", &[(5, 1)]),
            "no marker [at:"
        );
    }
}
