//! Notes on Postgres. The rules are those of `qbot_memory::notes::MemoryNoteStore`.

use async_trait::async_trait;
use qbot_core::{AccountId, GroupId, UnixMillis};
use qbot_memory::MemoryError;
use qbot_memory::notes::{Note, NoteId, NoteStore};
use sqlx::postgres::PgRow;
use sqlx::{PgPool, Row};

fn backend(error: impl std::fmt::Display) -> MemoryError {
    MemoryError::Backend(error.to_string())
}

const COLS: &str = "note_id, group_id, account_id, text, author_account, created_ms, updated_ms";

fn to_note(row: &PgRow) -> Result<Note, MemoryError> {
    Ok(Note {
        id: NoteId::new(row.get("note_id")),
        group: GroupId::new(row.get("group_id")).map_err(backend)?,
        account: AccountId::new(row.get("account_id")).map_err(backend)?,
        text: row.get("text"),
        author: AccountId::new(row.get("author_account")).map_err(backend)?,
        created: UnixMillis::new(row.get("created_ms")),
        updated: UnixMillis::new(row.get("updated_ms")),
    })
}

#[derive(Debug, Clone)]
pub struct PgNoteStore {
    pool: PgPool,
}

impl PgNoteStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

fn ids(accounts: &[AccountId]) -> Vec<i64> {
    accounts.iter().map(|a| a.get()).collect()
}

#[async_trait]
impl NoteStore for PgNoteStore {
    async fn notes(
        &self,
        group: GroupId,
        accounts: &[AccountId],
    ) -> Result<Vec<Note>, MemoryError> {
        // Accounts in the order given, oldest note first within each.
        let rows = sqlx::query(&format!(
            "SELECT {COLS} FROM note n \
             JOIN unnest($2::bigint[]) WITH ORDINALITY AS a(account_id, place) USING (account_id) \
             WHERE n.group_id = $1 ORDER BY a.place, n.note_id"
        ))
        .bind(group.get())
        .bind(ids(accounts))
        .fetch_all(&self.pool)
        .await
        .map_err(backend)?;
        rows.iter().map(to_note).collect()
    }

    async fn add(
        &self,
        group: GroupId,
        account: AccountId,
        text: &str,
        author: AccountId,
        at: UnixMillis,
    ) -> Result<Note, MemoryError> {
        let row = sqlx::query(&format!(
            "INSERT INTO note (group_id, account_id, text, author_account, created_ms, updated_ms) \
             VALUES ($1, $2, $3, $4, $5, $5) RETURNING {COLS}"
        ))
        .bind(group.get())
        .bind(account.get())
        .bind(text)
        .bind(author.get())
        .bind(at.get())
        .fetch_one(&self.pool)
        .await
        .map_err(backend)?;
        to_note(&row)
    }

    async fn edit(
        &self,
        group: GroupId,
        id: NoteId,
        text: &str,
        author: AccountId,
        at: UnixMillis,
    ) -> Result<bool, MemoryError> {
        let done = sqlx::query(
            "UPDATE note SET text = $3, author_account = $4, updated_ms = greatest($5, created_ms) \
             WHERE group_id = $1 AND note_id = $2",
        )
        .bind(group.get())
        .bind(id.get())
        .bind(text)
        .bind(author.get())
        .bind(at.get())
        .execute(&self.pool)
        .await
        .map_err(backend)?;
        Ok(done.rows_affected() == 1)
    }

    async fn remove(&self, group: GroupId, id: NoteId) -> Result<bool, MemoryError> {
        let done = sqlx::query("DELETE FROM note WHERE group_id = $1 AND note_id = $2")
            .bind(group.get())
            .bind(id.get())
            .execute(&self.pool)
            .await
            .map_err(backend)?;
        Ok(done.rows_affected() == 1)
    }

    async fn clear(&self, group: GroupId, accounts: &[AccountId]) -> Result<u64, MemoryError> {
        let done = sqlx::query("DELETE FROM note WHERE group_id = $1 AND account_id = ANY($2)")
            .bind(group.get())
            .bind(ids(accounts))
            .execute(&self.pool)
            .await
            .map_err(backend)?;
        Ok(done.rows_affected())
    }
}
