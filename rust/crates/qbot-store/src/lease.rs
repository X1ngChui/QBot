//! The single-instance guarantee: a session-level advisory lock held on a dedicated connection.
//! Startup recovery (interrupting claimed timers and open runs) is only safe while it is held.

use std::time::Duration;

use sqlx::postgres::PgConnection;
use sqlx::{Connection, Row};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// Default lock key: the ASCII bytes of "QBOT".
pub const DEFAULT_KEY: i64 = 0x5142_4F54;

#[derive(Debug, thiserror::Error)]
pub enum LeaseError {
    #[error("another instance holds the runtime lease")]
    Held,
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
}

pub struct RuntimeLease {
    conn: PgConnection,
    key: i64,
}

impl std::fmt::Debug for RuntimeLease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RuntimeLease")
            .field("key", &self.key)
            .finish_non_exhaustive()
    }
}

impl RuntimeLease {
    pub async fn acquire(url: &str, key: i64) -> Result<Self, LeaseError> {
        let mut conn = PgConnection::connect(url).await?;
        let got: bool = sqlx::query("SELECT pg_try_advisory_lock($1) AS got")
            .bind(key)
            .fetch_one(&mut conn)
            .await?
            .get("got");
        if got {
            Ok(Self { conn, key })
        } else {
            Err(LeaseError::Held)
        }
    }

    /// Whether this connection is alive and still holds the lock.
    pub async fn is_held(&mut self) -> bool {
        sqlx::query(
            "SELECT EXISTS (SELECT 1 FROM pg_locks WHERE locktype = 'advisory' AND pid = pg_backend_pid() \
             AND ((classid::bigint << 32) | (objid::bigint & 4294967295)) = $1) AS held",
        )
        .bind(self.key)
        .fetch_one(&mut self.conn)
        .await
        .is_ok_and(|row| row.get::<bool, _>("held"))
    }

    /// Check the lease every `every`; cancel `lost` the moment it is gone. The lock is released
    /// when the watch is stopped or dropped.
    pub fn watch(mut self, every: Duration, lost: CancellationToken) -> LeaseWatch {
        let stop = CancellationToken::new();
        let stopped = stop.clone();
        let task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    () = stopped.cancelled() => break,
                    () = tokio::time::sleep(every) => {
                        if !self.is_held().await {
                            lost.cancel();
                            break;
                        }
                    }
                }
            }
        });
        LeaseWatch { stop, task }
    }
}

#[derive(Debug)]
pub struct LeaseWatch {
    stop: CancellationToken,
    task: JoinHandle<()>,
}

impl LeaseWatch {
    /// Stop watching and release the lease.
    pub async fn release(self) {
        self.stop.cancel();
        let _ = self.task.await;
    }
}
