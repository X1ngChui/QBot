#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("migration failed: {0}")]
    Migration(#[from] sqlx::migrate::MigrateError),
    /// Stored data violates an invariant the code relies on.
    #[error("corrupt data: {0}")]
    Corrupt(String),
}

impl StoreError {
    pub(crate) fn corrupt(message: impl Into<String>) -> Self {
        StoreError::Corrupt(message.into())
    }
}
