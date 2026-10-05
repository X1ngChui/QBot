#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

//! A throwaway database per test. Tests need `QBOT_TEST_DATABASE_URL` (a disposable server; the
//! database name in it must start with `qbot_test`). Without it they fail loudly, unless
//! `QBOT_SKIP_DB_TESTS=1` is set, in which case they print SKIPPED and pass.

use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU32, Ordering};
use std::time::Duration;

use qbot_core::{Clock, UnixMillis};
use qbot_store::Store;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};

static COUNTER: AtomicU32 = AtomicU32::new(0);

pub struct TestDb {
    pub store: Store,
    pub url: String,
    name: String,
    admin: sqlx::PgPool,
}

impl TestDb {
    pub async fn try_new() -> Option<TestDb> {
        let base = match std::env::var("QBOT_TEST_DATABASE_URL") {
            Ok(url) => url,
            Err(_) if std::env::var("QBOT_SKIP_DB_TESTS").is_ok() => {
                eprintln!("SKIPPED: QBOT_TEST_DATABASE_URL is not set");
                return None;
            }
            Err(_) => panic!(
                "QBOT_TEST_DATABASE_URL is not set; start a disposable Postgres (see README.md) \
                 or set QBOT_SKIP_DB_TESTS=1 to skip database tests"
            ),
        };
        let options = PgConnectOptions::from_str(&base).expect("a valid database URL");
        let admin_db = options.get_database().unwrap_or_default().to_owned();
        assert!(
            admin_db.starts_with("qbot_test"),
            "refusing to run against database {admin_db:?}: its name must start with qbot_test"
        );
        let admin = PgPoolOptions::new()
            .max_connections(2)
            .connect_with(options)
            .await
            .unwrap();
        let name = format!(
            "qbot_test_{}_{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::SeqCst)
        );
        sqlx::query(&format!("CREATE DATABASE \"{name}\""))
            .execute(&admin)
            .await
            .unwrap();
        let (prefix, _) = base.rsplit_once('/').expect("a URL with a database name");
        let url = format!("{prefix}/{name}");
        let store = Store::connect(&url).await.unwrap();
        store.migrate().await.unwrap();
        Some(TestDb {
            store,
            url,
            name,
            admin,
        })
    }

    pub fn pool(&self) -> &sqlx::PgPool {
        self.store.pool()
    }

    pub async fn drop_db(self) {
        self.store.pool().close().await;
        let _ = sqlx::query(&format!(
            "DROP DATABASE IF EXISTS \"{}\" WITH (FORCE)",
            self.name
        ))
        .execute(&self.admin)
        .await;
    }
}

/// A clock tests move by hand.
#[derive(Debug)]
pub struct ManualClock(AtomicI64);

impl ManualClock {
    pub fn new(start_ms: i64) -> Arc<Self> {
        Arc::new(Self(AtomicI64::new(start_ms)))
    }

    pub fn advance(&self, by: Duration) {
        self.0
            .fetch_add(i64::try_from(by.as_millis()).unwrap(), Ordering::SeqCst);
    }
}

impl Clock for ManualClock {
    fn now(&self) -> UnixMillis {
        UnixMillis::new(self.0.load(Ordering::SeqCst))
    }
}

pub const T0: i64 = 1_800_000_000_000;

#[macro_export]
macro_rules! db {
    () => {
        match $crate::common::TestDb::try_new().await {
            Some(db) => db,
            None => return,
        }
    };
}
