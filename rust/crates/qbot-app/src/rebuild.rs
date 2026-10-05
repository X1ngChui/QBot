//! `qbot rebuild-memory`: extract every group's complete slices now, as the nightly run would.
//!
//! Episodes (with their embeddings) and what consolidation learns from them (names, facts, group
//! knowledge) are built from the archive alone. Used after a history import, so memory exists
//! before the first nightly run. The bot must not be running: the database lease is taken.

use std::sync::Arc;
use std::time::Instant;

use qbot_config::{ConfigErrors, Env, Loaded, SecretResolver, prepare_directories};
use qbot_core::{Clock, SystemClock};
use qbot_llm::embedding::HttpEmbedder;
use qbot_llm::responses::{ReqwestTransport, ResponsesProvider};
use qbot_llm::{Embedder, LlmError, Provider};
use qbot_memory::consolidate::Consolidator;
use qbot_memory::predicates::Predicates;
use qbot_memory::{EpisodeBuilder, EpisodeExtractor, EpisodeJobs};
use qbot_sched::JobError;
use qbot_store::{
    DEFAULT_KEY, LeaseError, PgAdmin, PgEpisodeStore, PgFactStore, PgIdentityStore, RuntimeLease,
    Store, StoreError,
};

use crate::run::http_key;

#[derive(Debug, thiserror::Error)]
pub enum RebuildError {
    #[error("{0}")]
    Config(#[from] ConfigErrors),
    #[error("database: {0}")]
    Store(#[from] StoreError),
    #[error("{0}")]
    Lease(#[from] LeaseError),
    #[error("provider: {0}")]
    Provider(#[from] LlmError),
    #[error("{failed} of {groups} groups failed:\n{details}")]
    Groups {
        failed: usize,
        groups: usize,
        details: String,
    },
}

/// Extract every group; a group that fails does not stop the others. Returns the episodes
/// stored.
pub async fn rebuild_memory(loaded: Loaded, env: Arc<dyn Env>) -> Result<usize, RebuildError> {
    let config = loaded.config;
    let paths = config.paths(&loaded.layout);
    prepare_directories(&paths)?;
    let resolver = SecretResolver::new(env, loaded.layout.secrets_dir.clone());
    let secrets = config.ready_to_run(&resolver)?;

    let url = config.database_url(secrets.database_password.expose());
    let store = Store::connect(
        &url,
        config.database.max_connections,
        config.database_connect_timeout(),
    )
    .await?;
    store.migrate().await?;
    let lease = RuntimeLease::acquire(&url, DEFAULT_KEY).await?;

    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let pool = store.pool().clone();
    let text_transport = ReqwestTransport::new(
        config.providers.text.endpoint.clone(),
        http_key(&config.providers.text.api_key_secret, &resolver),
        config.text_connect_timeout(),
    )?;
    let provider: Arc<dyn Provider> = Arc::new(ResponsesProvider::new(
        config.text_provider(),
        Arc::new(text_transport),
    )?);
    let embed_transport = ReqwestTransport::new(
        config.providers.embedding.endpoint.clone(),
        http_key(&config.providers.embedding.api_key_secret, &resolver),
        config.embedding_connect_timeout(),
    )?;
    let embedder: Arc<dyn Embedder> = Arc::new(HttpEmbedder::new(
        config.embedding_provider(),
        Arc::new(embed_transport),
    ));
    let builder = EpisodeBuilder::new(
        EpisodeExtractor::new(provider, config.extractor_config()),
        embedder,
        config.builder_config(),
    );
    let jobs = EpisodeJobs::new(
        Arc::new(PgEpisodeStore::new(pool.clone(), clock.clone())),
        builder,
        config.slice_grid(),
    )
    .with_consolidator(Arc::new(Consolidator::new(
        Arc::new(PgFactStore::new(pool.clone())),
        Arc::new(PgIdentityStore::new(pool.clone(), config.identity_policy())),
        Arc::new(Predicates::builtin()),
    )));

    let groups = PgAdmin::new(pool, clock).groups().await?;
    let (mut stored, mut failures) = (0, Vec::new());
    for (n, group) in groups.iter().enumerate() {
        let started = Instant::now();
        tracing::info!(group = group.get(), "group {} of {}", n + 1, groups.len());
        match jobs.extract_group(*group).await {
            Ok(episodes) => {
                stored += episodes;
                tracing::info!(
                    group = group.get(),
                    episodes,
                    secs = started.elapsed().as_secs(),
                    "group done"
                );
            }
            Err(JobError(error)) => {
                tracing::error!(group = group.get(), %error, "group failed");
                failures.push(format!("group {}: {error}", group.get()));
            }
        }
    }
    drop(lease);
    if failures.is_empty() {
        Ok(stored)
    } else {
        Err(RebuildError::Groups {
            failed: failures.len(),
            groups: groups.len(),
            details: failures.join("\n"),
        })
    }
}
