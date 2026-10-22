//! `qbot run`: build every part from the configuration and serve until told to stop.
//!
//! Startup order matters: configuration and secrets are checked first (every problem reported
//! together), then the database lease is taken, and only then is anything recovered or started,
//! so two processes can never both recover and run.

use std::sync::Arc;
use std::time::Duration;

use crate::report_sink::OneBotReportSink;
use qbot_agent::{Archive, ContextSource, GroupPolicy, RunDeps, RunLog, Supervisor};
use qbot_asr::{AsrConfig, AsrError, SherpaTranscriber};
use qbot_commands::{CommandRouter, CommandSettings, Deps, PgGroupAdmin};
use qbot_config::{
    ConfigErrors, Env, Loaded, ResolvedPaths, SecretKey, SecretResolver, Secrets, Service,
    prepare_directories,
};
use qbot_core::{Clock, GroupId, SystemClock};
use qbot_gateway::bridge::Bridge;
use qbot_gateway::delivery::{DeliveryTimeouts, OneBotDelivery};
use qbot_gateway::directory::OneBotDirectory;
use qbot_gateway::echo::EchoBoard;
use qbot_gateway::media::{FetchSettings, OneBotFetcher};
use qbot_gateway::pipeline::{BatchFilled, Pipeline, PipelineConfig};
use qbot_gateway::server::{GatewayServer, serve};
use qbot_gateway::trigger::Nicknames;
use qbot_i18n::{LocaleErrors, Locales, Msg};
use qbot_llm::embedding::HttpEmbedder;
use qbot_llm::responses::{KeySource, ReqwestTransport, ResponsesProvider};
use qbot_llm::search::TavilySearch;
use qbot_llm::{Embedder, LlmError, Meter, Provider};
use qbot_media::{
    ArchivedMedia, Describer, LlmDescriber, MediaDeps, MediaService, OpenImages, PgLineEditor,
    Transcriber,
};
use qbot_memory::consolidate::Consolidator;
use qbot_memory::facts::FactStore;
use qbot_memory::predicates::Predicates;
use qbot_memory::{
    BuilderConfig, EpisodeBuilder, EpisodeExtractor, EpisodeJobs, EpisodeStore, IdentityStore,
    Recall,
};
use qbot_ops::{BackupError, Operations, Parts, PgHousekeeping, PgReportSource, check_tools};
use qbot_prompt::{
    PersonaError, Personas, PromptContext, PromptError, PromptRenderer, PromptSettings,
    UnknownZone, describe_image_instructions,
};
use qbot_sched::{JobKind, Recurring, Scheduler, TaskService, TimerStore};
use qbot_store::{
    DEFAULT_KEY, LeaseError, PgAdmin, PgArchive, PgEpisodeStore, PgFactStore, PgGroupPolicy,
    PgIdentityStore, PgMediaCache, PgRunLog, PgTimerStore, PgUsageSink, RuntimeLease, Store,
    StoreError,
};
use std::net::SocketAddr;

use tokio::net::TcpListener;
use tokio::sync::{Notify, oneshot};
use tokio_util::sync::CancellationToken;

/// How often the database lease is checked. Losing it ends the process: another instance may
/// already be running.
const LEASE_CHECK: Duration = Duration::from_secs(10);

#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error("{0}")]
    Config(ConfigErrors),
    #[error("locale:\n{0}")]
    Locale(LocaleErrors),
    #[error("personas:\n{}", .0.iter().map(|e| format!("- {e}")).collect::<Vec<_>>().join("\n"))]
    Personas(Vec<PersonaError>),
    #[error("{0}")]
    Zone(#[from] UnknownZone),
    #[error("database: {0}")]
    Store(#[from] StoreError),
    #[error("{0}")]
    Lease(#[from] LeaseError),
    #[error("provider: {0}")]
    Provider(#[from] LlmError),
    #[error("{0}")]
    Asr(#[from] AsrError),
    #[error("prompt: {0}")]
    Prompt(#[from] PromptError),
    #[error("media: {0}")]
    Media(String),
    #[error("backups: {0}")]
    Backup(#[from] BackupError),
    #[error("schedule: {0}")]
    Schedule(#[from] qbot_sched::RecurrenceError),
    #[error("tool set: {0}")]
    Tools(String),
    #[error("cannot listen on {addr}: {source}")]
    Listen {
        addr: String,
        source: std::io::Error,
    },
    #[error("recovering after the previous process: {0}")]
    Recovery(String),
    #[error("gateway: {0}")]
    Gateway(String),
    #[error("{0} stopped by itself")]
    Stopped(&'static str),
}

impl From<ConfigErrors> for RunError {
    fn from(errors: ConfigErrors) -> Self {
        RunError::Config(errors)
    }
}

/// No captured log lines: `/logs` reports none.
struct NoLogs;

impl qbot_commands::RecentLogs for NoLogs {
    fn recent(&self, _: GroupId, _: usize) -> Vec<qbot_commands::LogLine> {
        Vec::new()
    }
}

/// Queues a durable extraction job for a group whose batch just filled.
struct ExtractOnBatch {
    timers: Arc<PgTimerStore>,
    clock: Arc<dyn Clock>,
    wake: Arc<Notify>,
}

#[async_trait::async_trait]
impl BatchFilled for ExtractOnBatch {
    async fn filled(&self, group: GroupId) {
        match self
            .timers
            .insert_job(self.clock.now(), JobKind::Extract, Some(group))
            .await
        {
            Ok(_) => self.wake.notify_one(),
            // The group's next filled batch queues another.
            Err(error) => {
                tracing::warn!(%error, group = group.get(), "an extraction job could not be queued")
            }
        }
    }
}

pub(crate) fn http_key(name: &str, resolver: &SecretResolver) -> KeySource {
    KeySource::Resolver(Arc::new(SecretKey {
        name: name.to_owned(),
        resolver: resolver.clone(),
    }))
}

/// What the caller controls about a run: where environment values come from, when to stop, and
/// (for tests and supervisors) where to report the address that was bound.
pub struct Options {
    pub env: Arc<dyn Env>,
    pub shutdown: CancellationToken,
    pub ready: Option<oneshot::Sender<SocketAddr>>,
    /// Recent warnings and errors for `/logs`; `None` shows none.
    pub logs: Option<Arc<dyn qbot_commands::RecentLogs>>,
}

impl std::fmt::Debug for Options {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Options").finish_non_exhaustive()
    }
}

/// Everything about a deployment that can be checked without the database or the platform:
/// configuration, secrets, state directories, locale, personas, time zone, the voice model's
/// files and the backup tools. `run` checks it all before it touches either, and `check-config`
/// reports exactly this, so a deployment that passes the check also starts.
pub struct Preflight {
    pub paths: ResolvedPaths,
    pub resolver: SecretResolver,
    pub secrets: Secrets,
    pub locales: Locales,
    pub personas: Personas,
    pub zone: jiff::tz::TimeZone,
}

impl std::fmt::Debug for Preflight {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Preflight").finish_non_exhaustive()
    }
}

pub async fn preflight(loaded: &Loaded, env: Arc<dyn Env>) -> Result<Preflight, RunError> {
    let config = &loaded.config;
    let paths = config.paths(&loaded.layout);
    prepare_directories(&paths)?;
    let resolver = SecretResolver::new(env, loaded.layout.secrets_dir.clone());
    let secrets = config.ready_to_run(&resolver)?;
    let locales =
        Locales::load(&config.bot.locale, &paths.locales_dir).map_err(RunError::Locale)?;
    let personas = Personas::load(&paths.personas_dir).map_err(RunError::Personas)?;
    let zone = jiff::tz::TimeZone::get(&config.bot.timezone)
        .map_err(|_| UnknownZone(config.bot.timezone.clone()))?;
    if config.media.transcribe_voice {
        qbot_asr::check_files(&AsrConfig::new(&paths.models_dir))?;
    }
    let ops = config.ops_config(&paths, secrets.database_password.expose(), zone.clone());
    if let Some(backup) = &ops.backup {
        // A missing client is an error now, not at two in the morning.
        check_tools(backup).await?;
    }
    Ok(Preflight {
        paths,
        resolver,
        secrets,
        locales,
        personas,
        zone,
    })
}

pub async fn run(loaded: Loaded, options: Options) -> Result<(), RunError> {
    let Options {
        env,
        shutdown,
        ready,
        logs,
    } = options;
    let Preflight {
        paths,
        resolver,
        secrets,
        locales,
        personas,
        zone,
    } = preflight(&loaded, env).await?;
    let config = loaded.config;
    // Validated by `validate` and `ready_to_run`.
    let (Some(bot), Some(listen)) = (config.bot_account(), config.gateway_listen()) else {
        return Err(RunError::Config(ConfigErrors(Vec::new())));
    };

    // The database, and the single-instance lease before anything is recovered or started.
    let url = config.database_url(secrets.database_password.expose());
    let store = Store::connect(&url).await?;
    // The lease first: a second process must not change the schema under a running one.
    let lease = RuntimeLease::acquire(&url, DEFAULT_KEY).await?;
    store.migrate().await?;
    let lease_lost = CancellationToken::new();
    let lease = lease.watch(LEASE_CHECK, lease_lost.clone());

    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let pool = store.pool().clone();
    let archive = PgArchive::new(pool.clone());
    let policy = PgGroupPolicy::new(pool.clone(), clock.clone());
    let admin = PgAdmin::new(pool.clone(), clock.clone());
    let run_log = PgRunLog::new(pool.clone(), clock.clone());
    let timers = Arc::new(PgTimerStore::new(pool.clone(), clock.clone()));
    let identity: Arc<dyn IdentityStore> =
        Arc::new(PgIdentityStore::new(pool.clone(), Default::default()));
    let episodes: Arc<dyn EpisodeStore> =
        Arc::new(PgEpisodeStore::new(pool.clone(), clock.clone()));
    let facts: Arc<dyn FactStore> = Arc::new(PgFactStore::new(pool.clone()));
    let notes: Arc<dyn qbot_memory::NoteStore> =
        Arc::new(qbot_store::PgNoteStore::new(pool.clone()));
    let predicates = Arc::new(Predicates::builtin());
    let recorder = PgUsageSink::start(pool.clone(), clock.clone());

    // A run that was open when the previous process died has no end; close it before serving.
    let closed = run_log
        .recover_open_runs()
        .await
        .map_err(|e| RunError::Recovery(e.to_string()))?;
    if closed > 0 {
        tracing::warn!(closed, "closed runs left open by the previous process");
    }

    // Providers.
    let text_transport = ReqwestTransport::new(
        config.providers.text.endpoint.clone(),
        http_key(&config.providers.text.api_key_secret, &resolver),
        &config.route(Service::Text),
    )?;
    // Every provider records each call it makes in the usage records, whatever the work.
    let meter: Arc<dyn Meter> = recorder.sink();
    let provider: Arc<dyn Provider> = Arc::new(
        ResponsesProvider::new(config.text_provider(), Arc::new(text_transport))?
            .with_meter(meter.clone()),
    );
    let embed_transport = ReqwestTransport::new(
        config.providers.embedding.endpoint.clone(),
        http_key(&config.providers.embedding.api_key_secret, &resolver),
        &config.route(Service::Embedding),
    )?;
    let embedder: Arc<dyn Embedder> = Arc::new(
        HttpEmbedder::new(config.embedding_provider(), Arc::new(embed_transport))
            .with_meter(meter.clone()),
    );

    // The platform side.
    let bridge = Arc::new(Bridge::new());
    let echoes = Arc::new(EchoBoard::new());
    let timeouts = DeliveryTimeouts::default();
    let delivery = Arc::new(OneBotDelivery::new(
        bridge.clone(),
        echoes.clone(),
        timeouts,
    ));
    let directory = Arc::new(OneBotDirectory::new(bridge.clone(), timeouts.action));

    // Tasks, tools, prompt, supervisor, scheduler.
    let wake = Arc::new(Notify::new());
    let tasks = TaskService::new(
        timers.clone(),
        clock.clone(),
        Default::default(),
        wake.clone(),
        qbot_sched::Zone::get(&config.bot.timezone)
            .map_err(|_| UnknownZone(config.bot.timezone.clone()))?,
    );
    // Pictures and clips are fetched through the platform connection.
    let fetcher: Arc<OneBotFetcher> = Arc::new(
        OneBotFetcher::new(
            bridge.clone(),
            FetchSettings::default(),
            &config.route(Service::Media),
        )
        .map_err(|e| RunError::Media(e.to_string()))?,
    );
    let archive_port: Arc<dyn Archive> = Arc::new(archive.clone());
    let tools = qbot_tools::standard_tools(
        delivery.clone(),
        archive_port.clone(),
        tasks.clone(),
        config.tool_settings(),
    )
    .and_then(|set| {
        let recall = Arc::new(Recall::new(
            episodes.clone(),
            embedder.clone(),
            clock.clone(),
            config.recall_params(),
        ));
        qbot_tools::add_memory_tools(set, recall, episodes.clone())
    })
    .map_err(|e| RunError::Tools(e.to_string()))?;
    // The model may look at pictures itself only if it accepts images at all.
    let (tools, media_store) = if provider.info().capabilities.image_input.is_some() {
        let store = Arc::new(ArchivedMedia::new(
            Arc::new(archive.clone()),
            fetcher.clone(),
            config.media_config().max_image_bytes,
        ));
        let tools = tools
            .with(OpenImages(store.clone()))
            .map_err(|e| RunError::Tools(e.to_string()))?;
        (tools, Some(store as Arc<dyn qbot_llm::MediaStore>))
    } else {
        tracing::info!("the text model takes no images, so open_images is not offered");
        (tools, None)
    };

    // Web search and page reading are offered only when a search provider is configured.
    let tools = if config.providers.search.enabled {
        let transport = ReqwestTransport::new(
            qbot_llm::search::ENDPOINT,
            http_key(&config.providers.search.api_key_secret, &resolver),
            &config.route(Service::Search),
        )
        .map_err(|e| RunError::Tools(e.to_string()))?;
        let search = Arc::new(
            TavilySearch::new(config.search_provider(), Arc::new(transport))
                .with_meter(meter.clone()),
        );
        tools
            .with(qbot_tools::WebSearchTool::new(search.clone()))
            .and_then(|set| {
                set.with(qbot_tools::ReadUrl::new(
                    search,
                    qbot_tools::READ_URL_MAX_CHARS,
                ))
            })
            .map_err(|e| RunError::Tools(e.to_string()))?
    } else {
        tracing::info!("web search and page reading are off (providers.search.enabled = false)");
        tools
    };

    let prompt = PromptContext::new(
        Arc::new(archive.clone()),
        personas,
        clock.clone(),
        PromptSettings {
            window: config.history_window(),
            timezone: config.bot.timezone.clone(),
        },
    )?
    .with_knowledge(Arc::new(qbot_prompt::FactKnowledge::new(
        facts.clone(),
        config.group_terms(),
        config.decay_policy(),
    )))
    .with_people(
        Arc::new(qbot_prompt::ArchivePeople::new(
            archive.clone(),
            config.decay_policy(),
        )),
        config.person_facts(),
    )
    .with_bot_nicknames(&config.bot.nicknames)
    .with_directory(directory.clone())
    .with_episodes(episodes.clone());
    let prompt = Arc::new(prompt);
    let renderer = Arc::new(PromptRenderer::new(prompt.zone().clone()));
    let deps = Arc::new(RunDeps {
        provider: provider.clone(),
        tools,
        archive: archive_port,
        log: Arc::new(run_log.clone()) as Arc<dyn RunLog>,
        sink: recorder.sink(),
        renderer,
        clock: clock.clone(),
        limits: Default::default(),
        reasoning: config.reply_reasoning(),
        media: media_store,
    });
    let supervisor = Supervisor::new(
        config.supervisor(),
        deps,
        prompt.clone() as Arc<dyn ContextSource>,
        Arc::new(policy.clone()) as Arc<dyn GroupPolicy>,
    );
    // Background work: the nightly pipeline, backups and the daily report.
    let ops_config = config.ops_config(&paths, secrets.database_password.expose(), zone.clone());
    // The language the bot writes in comes with the locale.
    let writing_language = locales.render(&Msg::WritingLanguage {});
    let builder = EpisodeBuilder::new(
        EpisodeExtractor::new(provider.clone()).with_reasoning(config.extraction_reasoning()),
        embedder.clone(),
        BuilderConfig {
            language: writing_language.clone(),
        },
    );
    let operations = Arc::new(Operations::new(Parts {
        cfg: ops_config,
        clock: clock.clone(),
        extractor: Arc::new(
            EpisodeJobs::new(episodes.clone(), builder, config.slice_grid())
                .with_consolidator(Arc::new(Consolidator::new(
                    facts.clone(),
                    identity.clone(),
                    predicates.clone(),
                )))
                // Extraction reads the group as replies do: the same people and knowledge.
                .with_background(prompt.clone()),
        ),
        housekeeping: Arc::new(PgHousekeeping::new(
            admin.clone(),
            PgMediaCache::new(pool.clone(), clock.clone()),
        )),
        report: Arc::new(PgReportSource(admin.clone())),
        sink: Arc::new(OneBotReportSink::new(bridge.clone(), timeouts.action)),
        locales: locales.clone(),
        platform_cache: config.maintenance.napcat_clean_cache.then(|| {
            Arc::new(crate::report_sink::NapCatCache::new(
                bridge.clone(),
                timeouts.action,
            )) as Arc<dyn qbot_ops::PlatformCache>
        }),
    }));
    // A filled batch asks for its episode now, so the summary exists before the chat leaves the
    // verbatim tier.
    let batch_jobs = Arc::new(ExtractOnBatch {
        timers: timers.clone(),
        clock: clock.clone(),
        wake: wake.clone(),
    });
    let recurring = Arc::new(Recurring::new(
        timers.clone(),
        clock.clone(),
        &config.bot.timezone,
        config.recurrences(),
        wake.clone(),
    )?);
    let scheduler = Arc::new(Scheduler::new(
        timers,
        supervisor.clone(),
        clock.clone(),
        operations,
        Default::default(),
        wake,
    ));

    // Pictures and voice: described and transcribed as messages arrive.
    let describer: Option<Arc<dyn Describer>> = if config.providers.vision.enabled {
        let transport = ReqwestTransport::new(
            config.providers.vision.endpoint.clone(),
            http_key(&config.providers.vision.api_key_secret, &resolver),
            &config.route(Service::Vision),
        )?;
        let vision = Arc::new(
            ResponsesProvider::new(config.vision_provider(), Arc::new(transport))?
                .with_meter(meter.clone()),
        );
        let instructions = describe_image_instructions(&writing_language)?;
        Some(Arc::new(LlmDescriber::new(vision, instructions)))
    } else {
        tracing::info!("picture description is off (providers.vision.enabled = false)");
        None
    };
    let transcriber: Option<Arc<dyn Transcriber>> = if config.media.transcribe_voice {
        let asr = AsrConfig::new(&paths.models_dir);
        // Loading the model is slow and blocking; a missing or damaged one stops startup here.
        let loaded = tokio::task::spawn_blocking(move || SherpaTranscriber::load(&asr))
            .await
            .map_err(|e| RunError::Media(e.to_string()))??;
        Some(Arc::new(loaded))
    } else {
        tracing::info!("voice transcription is off (media.transcribe_voice = false)");
        None
    };
    let media = Arc::new(MediaService::new(
        config.media_config(),
        MediaDeps {
            fetcher: fetcher.clone(),
            describer,
            transcriber,
            cache: Arc::new(PgMediaCache::new(pool.clone(), clock.clone())),
            editor: Arc::new(PgLineEditor(archive.clone())),
        },
    ));

    // Commands and the pipeline.
    let commands = Arc::new(CommandRouter::new(Deps {
        identity: identity.clone(),
        facts: facts.clone(),
        notes: notes.clone(),
        admin: Arc::new(PgGroupAdmin::new(admin, policy)),
        runs: Arc::new(run_log.clone()),
        logs: logs.unwrap_or_else(|| Arc::new(NoLogs)),
        tasks,
        directory: directory.clone(),
        delivery: delivery.clone(),
        locales,
        clock: clock.clone(),
        settings: CommandSettings::new(bot, config.owners(), zone, config.decay_policy()),
    }));
    let pipeline = Arc::new(
        Pipeline::new(
            PipelineConfig {
                spontaneous_chance: config.replies.spontaneous_chance,
                ..PipelineConfig::new(bot)
            },
            Arc::new(archive),
            commands,
            Arc::new(supervisor.clone()),
            Nicknames::new(&config.bot.nicknames),
            echoes,
        )
        .with_media(media.clone(), config.media_wait())
        .with_batches(batch_jobs, config.history.batch_lines),
    );

    let stop = CancellationToken::new();
    let access_token = secrets
        .onebot_access_token
        .as_ref()
        .map(|t| Arc::<str>::from(t.expose()));
    if access_token.is_none() {
        tracing::warn!("gateway authentication is off (gateway.access_token_secret is empty)");
    }
    let bridge_watch = bridge.clone();
    let server = GatewayServer {
        pipeline: pipeline.clone(),
        bridge,
        bot,
        access_token,
        path: qbot_gateway::server::PATH.into(),
        cancel: stop.clone(),
    };
    let listener = TcpListener::bind(listen)
        .await
        .map_err(|source| RunError::Listen {
            addr: config.gateway.listen.clone(),
            source,
        })?;
    let bound = listener.local_addr().map_err(|source| RunError::Listen {
        addr: config.gateway.listen.clone(),
        source,
    })?;
    tracing::info!(listen = %bound, path = qbot_gateway::server::PATH, bot = bot.get(), "listening for the platform");
    if let Some(ready) = ready {
        // The receiver may have gone away (a caller that stopped waiting); that is not an error.
        let _ = ready.send(bound);
    }

    let mut scheduler_task = {
        let (scheduler, stop) = (scheduler.clone(), stop.clone());
        tokio::spawn(async move { scheduler.run(stop).await })
    };
    let mut recurring_task = {
        let (recurring, stop) = (recurring.clone(), stop.clone());
        tokio::spawn(async move { recurring.run(stop).await })
    };
    let watch_task = tokio::spawn(qbot_gateway::server::warn_while_disconnected(
        bridge_watch,
        qbot_gateway::server::DISCONNECTED_WARN_AFTER,
        stop.clone(),
    ));
    let mut server_task = tokio::spawn(serve(listener, server, stop.clone()));

    // Each of these ends only when told to. One that ends by itself (a panic, say) leaves a
    // process that hears nothing or schedules nothing: it stops too, so the container restarts.
    let mut served = None;
    let mut ended_early: Option<&'static str> = None;
    let reason = tokio::select! {
        () = shutdown.cancelled() => "shutdown requested",
        () = lease_lost.cancelled() => "the database lease was lost",
        ended = &mut server_task => {
            served = Some(ended);
            "the gateway server stopped"
        }
        ended = &mut scheduler_task => {
            if let Err(error) = ended {
                tracing::error!(%error, "the scheduler task panicked");
            }
            ended_early = Some("the scheduler");
            "the scheduler stopped"
        }
        ended = &mut recurring_task => {
            if let Err(error) = ended {
                tracing::error!(%error, "the recurring schedules task panicked");
            }
            ended_early = Some("the recurring schedules");
            "the recurring schedules stopped"
        }
    };
    if shutdown.is_cancelled() {
        tracing::info!(reason, "stopping");
    } else {
        tracing::warn!(reason, "stopping");
    }

    // Stop taking input, then end what is in flight without waiting out long work: runs are
    // cancelled (recorded as such) before anything that could wait on one, commands finish,
    // media work and running jobs are dropped (a job is queued again at the next start), and
    // what was recorded is written.
    stop.cancel();
    let served = match served {
        Some(ended) => ended,
        None => server_task.await,
    };
    supervisor.shutdown().await;
    pipeline.finish().await;
    media.shutdown().await;
    if ended_early != Some("the scheduler")
        && let Err(error) = scheduler_task.await
    {
        tracing::error!(%error, "the scheduler task panicked");
    }
    if ended_early != Some("the recurring schedules")
        && let Err(error) = recurring_task.await
    {
        tracing::error!(%error, "the recurring schedules task panicked");
    }
    if let Err(error) = watch_task.await {
        tracing::error!(%error, "the connection watch task panicked");
    }
    recorder.shutdown().await;
    lease.release().await;
    if lease_lost.is_cancelled() {
        return Err(RunError::Lease(LeaseError::Lost));
    }
    if let Some(what) = ended_early {
        return Err(RunError::Stopped(what));
    }
    match served {
        // Serving ends only when told to; ending before that is a failure.
        Ok(Ok(())) if shutdown.is_cancelled() => Ok(()),
        Ok(Ok(())) => Err(RunError::Gateway("the server stopped by itself".into())),
        Ok(Err(error)) => Err(RunError::Gateway(error.to_string())),
        Err(error) => Err(RunError::Gateway(format!(
            "the server task failed: {error}"
        ))),
    }
}
