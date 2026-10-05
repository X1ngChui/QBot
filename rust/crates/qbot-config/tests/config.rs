#![allow(clippy::unwrap_used, clippy::expect_used, clippy::result_large_err)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use qbot_config::{
    Config, ConfigError, Layout, MapEnv, SecretKey, SecretResolver, Source, load_in,
    prepare_directories,
};
use qbot_llm::responses::KeyResolver;

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// A fresh scratch directory per test.
fn scratch() -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "qbot-config-test-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn write(dir: &Path, name: &str, text: &str) {
    let path = dir.join(name);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn layout(root: &Path) -> Layout {
    Layout {
        config_dir: root.join("etc"),
        data_dir: root.join("data"),
        secrets_dir: root.join("secrets"),
    }
}

/// Load with the given environment variables set. Figment reads the process environment, so the
/// variables are set inside its `Jail`, which serializes such tests and restores the environment.
fn load(root: &Path, env: &MapEnv) -> Result<qbot_config::Loaded, qbot_config::ConfigErrors> {
    let mut result = None;
    figment::Jail::expect_with(|jail| {
        for (name, value) in &env.0 {
            jail.set_env(name, value);
        }
        result = Some(load_in(layout(root)));
        Ok(())
    });
    result.unwrap()
}

fn errors(result: Result<qbot_config::Loaded, qbot_config::ConfigErrors>) -> Vec<ConfigError> {
    result.unwrap_err().0
}

#[test]
fn the_baked_in_defaults_are_a_complete_valid_configuration() {
    let root = scratch();
    let loaded = load(&root, &MapEnv::default()).unwrap();
    assert_eq!(loaded.sources, [Source::Defaults]);
    let c = &loaded.config;
    assert_eq!(
        (
            c.runtime.reply_capacity,
            c.runtime.reply_concurrency,
            c.runtime.reply_deadline_secs
        ),
        (32, 3, 180)
    );
    assert_eq!(
        (
            c.history.batch_lines,
            c.history.raw_batches,
            c.history.summary_batches
        ),
        (30, 4, 9)
    );
    assert_eq!(c.memory.slice_batches, 3);
    assert!(c.to_toml_roundtrip());
}

trait Roundtrip {
    fn to_toml_roundtrip(&self) -> bool;
}

impl Roundtrip for Config {
    fn to_toml_roundtrip(&self) -> bool {
        let text = toml::to_string_pretty(self).unwrap();
        toml::from_str::<Config>(&text).unwrap() == *self
    }
}

#[test]
fn layers_apply_in_a_fixed_order_and_override_key_by_key() {
    let root = scratch();
    write(
        &root,
        "etc/config.toml",
        "[runtime]\nreply_capacity = 10\nreply_concurrency = 2\n[memory]\nlanguage = \"French\"\n",
    );
    write(
        &root,
        "etc/conf.d/20-b.toml",
        "[runtime]\nreply_capacity = 20\n",
    );
    write(
        &root,
        "etc/conf.d/10-a.toml",
        "[runtime]\nreply_capacity = 15\n[agent]\nmax_turns = 7\n",
    );
    write(
        &root,
        "etc/conf.d/ignored.txt",
        "this is not toml and is not read",
    );
    let env = MapEnv::new([("QBOT__AGENT__MAX_TURNS", "9")]);
    let loaded = load(&root, &env).unwrap();
    let c = &loaded.config;

    assert_eq!(
        c.runtime.reply_capacity, 20,
        "drop-ins apply in file-name order, so 20-b wins over 10-a"
    );
    assert_eq!(
        c.runtime.reply_concurrency, 2,
        "a key set in config.toml survives later layers that do not set it"
    );
    assert_eq!(c.agent.max_turns, 9, "the environment beats every file");
    assert_eq!(c.memory.language, "French");
    assert_eq!(
        c.runtime.reply_deadline_secs, 180,
        "a key nobody sets keeps its default"
    );
    assert_eq!(
        c.agent.max_sends_per_run, 4,
        "siblings of an overridden key are untouched"
    );

    let described: Vec<String> = loaded.sources.iter().map(|s| format!("{s:?}")).collect();
    assert_eq!(loaded.sources.len(), 5, "{described:?}");
    assert!(matches!(&loaded.sources[0], Source::Defaults));
    assert!(matches!(&loaded.sources[1], Source::File(p) if p.ends_with("config.toml")));
    assert!(matches!(&loaded.sources[2], Source::DropIn(p) if p.ends_with("10-a.toml")));
    assert!(matches!(&loaded.sources[3], Source::DropIn(p) if p.ends_with("20-b.toml")));
    assert!(
        matches!(&loaded.sources[4], Source::Env(n) if n == "QBOT__AGENT__MAX_TURNS"),
        "only the name is recorded, never the value"
    );
}

#[test]
fn arrays_replace_rather_than_merge() {
    let root = scratch();
    write(
        &root,
        "etc/config.toml",
        "[scheduler]\njob_backoff_secs = [5, 10]\n",
    );
    assert_eq!(
        load(&root, &MapEnv::default())
            .unwrap()
            .config
            .scheduler
            .job_backoff_secs,
        [5, 10]
    );
    let env = MapEnv::new([("QBOT__SCHEDULER__JOB_BACKOFF_SECS", "[1, 2, 3]")]);
    assert_eq!(
        load(&root, &env).unwrap().config.scheduler.job_backoff_secs,
        [1, 2, 3]
    );
}

fn message(found: &[ConfigError]) -> String {
    found
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn environment_values_follow_figments_typing_rules() {
    let root = scratch();
    let ok = MapEnv::new([
        ("QBOT__MEMORY__LANGUAGE", "French"),
        ("QBOT__PROVIDERS__TEXT__RETRY_JITTER", "false"),
        ("QBOT__IDENTITY__CONFIRM_AT", "0.9"),
        ("QBOT__IDENTITY__INVITATION_TTL_SECS", "30"),
        ("QBOT__DATABASE__SSL_MODE", "verify-full"),
    ]);
    let c = load(&root, &ok).unwrap().config;
    assert_eq!(c.memory.language, "French");
    assert!(!c.providers.text.retry_jitter);
    assert!((c.identity.confirm_at - 0.9).abs() < 1e-6);
    assert_eq!(c.identity.invitation_ttl_secs, 30);
    assert_eq!(c.database.ssl_mode.as_str(), "verify-full");

    // A string that looks like a number must be quoted; unquoted it is a number, and a number is
    // not accepted where text belongs (no silent coercion either way).
    let quoted = load(&root, &MapEnv::new([("QBOT__MEMORY__LANGUAGE", "\"123\"")]))
        .unwrap()
        .config;
    assert_eq!(quoted.memory.language, "123");
    let text = message(&errors(load(
        &root,
        &MapEnv::new([("QBOT__MEMORY__LANGUAGE", "123")]),
    )));
    assert!(
        text.to_lowercase().contains("language") && text.contains("expected a string"),
        "{text}"
    );
}

#[test]
fn bad_environment_overrides_name_the_key_and_the_variable() {
    let root = scratch();
    for (name, value, key) in [
        ("QBOT__RUNTIME__REPLY_CAPACITY", "many", "reply_capacity"),
        (
            "QBOT__PROVIDERS__TEXT__RETRY_JITTER",
            "maybe",
            "retry_jitter",
        ),
        (
            "QBOT__SCHEDULER__JOB_BACKOFF_SECS",
            "60",
            "job_backoff_secs",
        ),
        ("QBOT__RUNTIME__NO_SUCH", "1", "no_such"),
        ("QBOT__NO_SUCH__KEY", "1", "no_such"),
    ] {
        let text = message(&errors(load(&root, &MapEnv::new([(name, value)]))));
        assert!(text.to_lowercase().contains(key), "{name}: {text}");
        assert!(
            text.contains("QBOT__"),
            "{name}: the layer is named: {text}"
        );
    }
}

#[test]
fn unknown_keys_and_wrong_types_name_the_key_and_the_file_they_came_from() {
    let root = scratch();
    write(&root, "etc/config.toml", "[runtime]\nreply_capacty = 4\n");
    let text = message(&errors(load(&root, &MapEnv::default())));
    assert!(
        text.contains("reply_capacty") && text.contains("config.toml"),
        "{text}"
    );

    write(
        &root,
        "etc/config.toml",
        "[runtime]\nreply_deadline_secs = \"soon\"\n",
    );
    let text = message(&errors(load(&root, &MapEnv::default())));
    assert!(
        text.contains("reply_deadline_secs") && text.contains("config.toml"),
        "{text}"
    );

    write(&root, "etc/config.toml", "[nope]\nx = 1\n");
    let text = message(&errors(load(&root, &MapEnv::default())));
    assert!(
        text.contains("nope") && text.contains("config.toml"),
        "{text}"
    );
}

#[test]
fn a_credential_pasted_into_a_config_file_is_rejected_without_echoing_it() {
    let root = scratch();
    write(
        &root,
        "etc/config.toml",
        "[providers.text]\napi_key = \"sk-super-secret-value-123\"\n",
    );
    let text = message(&errors(load(&root, &MapEnv::default())));
    assert!(text.contains("api_key"), "{text}");
    assert!(
        !text.contains("sk-super-secret"),
        "the value is never repeated: {text}"
    );

    // Putting the secret itself where its NAME belongs is caught by validation, also without echo.
    write(
        &root,
        "etc/config.toml",
        "[providers.text]\napi_key_secret = \"sk-super-secret-value-123\"\n",
    );
    let found = errors(load(&root, &MapEnv::default()));
    assert!(
        matches!(&found[0], ConfigError::Invalid { key, reason } if key == "providers.text.api_key_secret" && reason.contains("NAME of a secret"))
    );
    assert!(!found[0].to_string().contains("sk-super-secret"));
}

#[test]
fn invalid_toml_is_reported_with_its_file() {
    let root = scratch();
    write(&root, "etc/conf.d/broken.toml", "[runtime\nx = ");
    let text = message(&errors(load(&root, &MapEnv::default())));
    assert!(text.contains("broken.toml"), "{text}");
}

#[test]
fn validation_reports_every_problem_together() {
    let root = scratch();
    write(
        &root,
        "etc/config.toml",
        r#"
[runtime]
reply_capacity = 2
reply_concurrency = 5
[providers.text]
kind = "deepseek"
state = "server_state"
endpoint = "https://user:pass@api.example.com"
[providers.embedding]
dims = 0
[tasks]
min_delay_secs = 0
[scheduler]
job_backoff_secs = []
[tools.search_history]
default_limit = 10
max_limit = 5
[memory]
slice_batches = 0
[memory.recall]
max_distance = 3.0
[identity]
confirm_at = 1.5
"#,
    );
    let found = errors(load(&root, &MapEnv::default()));
    let keys: Vec<String> = found
        .iter()
        .filter_map(|e| {
            if let ConfigError::Invalid { key, .. } = e {
                Some(key.clone())
            } else {
                None
            }
        })
        .collect();
    for expected in [
        "runtime.reply_concurrency",
        "providers.text.state",
        "providers.text.endpoint",
        "providers.embedding.dims",
        "tasks.min_delay_secs",
        "scheduler.job_backoff_secs",
        "tools.search_history.max_limit",
        "memory.slice_batches",
        "memory.recall.max_distance",
        "identity.confirm_at",
    ] {
        assert!(
            keys.contains(&expected.to_owned()),
            "{expected} missing from {keys:?}"
        );
    }
    assert_eq!(keys.len(), 10);
    assert!(
        !found.iter().any(|e| e.to_string().contains("pass@")),
        "an embedded credential is not echoed"
    );
}

#[test]
fn the_bootstrap_directories_come_from_the_environment_and_empty_means_unset() {
    let l = Layout::from_env(&MapEnv::new([
        ("QBOT_CONFIG_DIR", "/srv/conf"),
        ("QBOT_DATA_DIR", ""),
        ("QBOT_SECRETS_DIR", "/s"),
    ]));
    assert_eq!(l.config_dir, PathBuf::from("/srv/conf"));
    assert_eq!(l.data_dir, PathBuf::from("/var/lib/qbot"));
    assert_eq!(l.secrets_dir, PathBuf::from("/s"));
    let d = Layout::from_env(&MapEnv::default());
    assert_eq!(
        (d.config_dir, d.secrets_dir),
        (PathBuf::from("/etc/qbot"), PathBuf::from("/run/secrets"))
    );
    // A missing config directory just means no files.
    let layout = Layout {
        config_dir: PathBuf::from("/nonexistent/qbot-config"),
        data_dir: PathBuf::from("/nonexistent/data"),
        secrets_dir: PathBuf::from("/nonexistent/secrets"),
    };
    let mut sources = None;
    figment::Jail::expect_with(|_jail| {
        sources = Some(load_in(layout.clone()).unwrap().sources);
        Ok(())
    });
    assert_eq!(sources.unwrap(), [Source::Defaults]);
}

#[test]
fn relative_paths_resolve_under_their_base_and_absolute_ones_are_kept() {
    let root = scratch();
    write(
        &root,
        "etc/config.toml",
        "[paths]\nbackups_dir = \"/mnt/backups\"\n",
    );
    let loaded = load(&root, &MapEnv::default()).unwrap();
    let p = loaded.config.paths(&loaded.layout);
    assert_eq!(p.models_dir, root.join("data/models"));
    assert_eq!(p.backups_dir, PathBuf::from("/mnt/backups"));
    assert_eq!(p.personas_dir, root.join("etc/personas"));
    assert_eq!(p.locales_dir, root.join("etc/locales"));
}

#[test]
fn state_directories_are_created_and_a_bad_one_is_a_typed_error() {
    let root = scratch();
    let loaded = load(&root, &MapEnv::default()).unwrap();
    let paths = loaded.config.paths(&loaded.layout);
    prepare_directories(&paths).unwrap();
    assert!(paths.models_dir.is_dir() && paths.backups_dir.is_dir());
    assert!(
        !paths.data_dir.join(".qbot-write-check").exists(),
        "the probe cleans up after itself"
    );

    // The data directory path is a file: not a directory a process can use.
    let blocker = root.join("blocked");
    std::fs::write(&blocker, "x").unwrap();
    let mut bad = paths.clone();
    bad.data_dir = blocker.clone();
    bad.models_dir = blocker.join("models");
    bad.backups_dir = blocker.join("backups");
    let found = prepare_directories(&bad).unwrap_err().0;
    assert!(
        found
            .iter()
            .all(|e| matches!(e, ConfigError::Directory { .. })),
        "{found:?}"
    );
}

// ----- secrets -----

fn resolver(root: &Path, env: &MapEnv) -> SecretResolver {
    SecretResolver::new(Arc::new(env.clone()), root.join("secrets"))
}

#[test]
fn secrets_resolve_from_a_file_variable_then_the_environment_then_the_secrets_directory() {
    let root = scratch();
    write(
        &root,
        "secrets/text_api_key",
        "from-the-mounted-directory\n",
    );
    write(&root, "elsewhere/key", "  from-the-file-variable  \n");

    // 3. the secrets directory, trimmed
    let r = resolver(&root, &MapEnv::default());
    assert_eq!(
        r.resolve("TEXT_API_KEY").unwrap().expose(),
        "from-the-mounted-directory"
    );

    // 2. the plain variable beats the directory
    let r = resolver(
        &root,
        &MapEnv::new([("TEXT_API_KEY", "from-the-environment")]),
    );
    assert_eq!(
        r.resolve("TEXT_API_KEY").unwrap().expose(),
        "from-the-environment"
    );

    // 1. NAME_FILE beats both
    let file = root.join("elsewhere/key");
    let r = resolver(
        &root,
        &MapEnv::new([
            ("TEXT_API_KEY", "from-the-environment"),
            ("TEXT_API_KEY_FILE", file.to_str().unwrap()),
        ]),
    );
    assert_eq!(
        r.resolve("TEXT_API_KEY").unwrap().expose(),
        "from-the-file-variable"
    );
}

#[test]
fn missing_and_empty_secrets_are_errors_never_fallbacks_and_all_are_reported() {
    let root = scratch();
    let loaded = load(&root, &MapEnv::default()).unwrap();
    let r = resolver(&root, &MapEnv::new([("EMBEDDING_API_KEY", "   ")]));
    let found = loaded.config.resolve_secrets(&r).unwrap_err().0;
    assert_eq!(found.len(), 4, "{found:?}");
    assert!(found.iter().any(
        |e| matches!(e, ConfigError::MissingSecret { name, .. } if name == "DATABASE_PASSWORD")
    ));
    assert!(
        found.iter().any(
            |e| matches!(e, ConfigError::MissingSecret { name, .. } if name == "TEXT_API_KEY")
        )
    );
    assert!(
        found
            .iter()
            .any(|e| matches!(e, ConfigError::EmptySecret { name } if name == "EMBEDDING_API_KEY"))
    );
    assert!(found.iter().any(
        |e| matches!(e, ConfigError::MissingSecret { name, .. } if name == "ONEBOT_ACCESS_TOKEN")
    ));
    let message = found[0].to_string();
    assert!(
        message.contains("_FILE") && message.contains("secrets directory"),
        "the error says how to fix it: {message}"
    );

    // A _FILE variable that points nowhere is an error, not a fall-through to another source.
    let r = resolver(
        &root,
        &MapEnv::new([
            ("TEXT_API_KEY_FILE", "/no/such/file"),
            ("TEXT_API_KEY", "ignored"),
        ]),
    );
    assert!(matches!(
        r.resolve("TEXT_API_KEY"),
        Err(ConfigError::Io { .. })
    ));
}

#[test]
fn secrets_never_print_and_all_of_them_resolve_together() {
    let root = scratch();
    write(&root, "secrets/database_password", "db-secret-value");
    write(&root, "secrets/onebot_access_token", "token-secret-value");
    write(&root, "secrets/text_api_key", "text-secret-value");
    write(&root, "secrets/embedding_api_key", "embed-secret-value");
    let loaded = load(&root, &MapEnv::default()).unwrap();
    let secrets = loaded
        .config
        .resolve_secrets(&resolver(&root, &MapEnv::default()))
        .unwrap();
    let printed = format!("{secrets:?}");
    assert!(
        !printed.contains("secret-value") && printed.contains("redacted"),
        "{printed}"
    );
    assert_eq!(secrets.database_password.expose(), "db-secret-value");
    assert_eq!(
        secrets.onebot_access_token.as_ref().map(|t| t.expose()),
        Some("token-secret-value")
    );
    assert!(
        !toml::to_string_pretty(&loaded.config)
            .unwrap()
            .contains("secret-value")
    );
}

#[test]
fn a_rotated_key_is_picked_up_without_a_restart() {
    let root = scratch();
    write(&root, "secrets/text_api_key", "first-key");
    let key = SecretKey {
        name: "TEXT_API_KEY".into(),
        resolver: resolver(&root, &MapEnv::default()),
    };
    assert_eq!(key.resolve().unwrap(), "first-key");
    write(&root, "secrets/text_api_key", "second-key\n");
    assert_eq!(
        key.resolve().unwrap(),
        "second-key",
        "resolved on every call"
    );
    std::fs::remove_file(root.join("secrets/text_api_key")).unwrap();
    assert!(key.resolve().is_err());
}

// ----- conversions -----

#[test]
fn the_configuration_defaults_equal_the_defaults_of_the_crates_that_consume_them() {
    // One source of truth: if a crate's `Default` and `defaults.toml` ever drift, this fails.
    let c = load(&scratch(), &MapEnv::default()).unwrap().config;

    assert_eq!(c.run_limits(), qbot_agent::RunLimits::default());
    assert_eq!(c.task_limits(), qbot_sched::TaskLimits::default());
    assert_eq!(c.scheduler_config(), qbot_sched::SchedulerConfig::default());
    assert_eq!(
        c.identity_policy(),
        qbot_memory::identity::IdentityPolicy::default()
    );
    assert_eq!(
        c.extractor_config(),
        qbot_memory::ExtractorConfig::default()
    );
    assert_eq!(
        c.builder_config().language,
        qbot_memory::BuilderConfig::default().language
    );
    assert_eq!(c.recall_params(), qbot_memory::RecallParams::default());
    assert_eq!(c.decay_policy(), qbot_memory::facts::DecayPolicy::default());
    assert_eq!(c.history_window(), qbot_core::HistoryWindow::default());
    let retry = qbot_llm::responses::RetryPolicy::default();
    assert_eq!(c.text_provider().retry, retry);
    assert_eq!(c.embedding_provider().retry, retry);

    let embedding = c.embedding_provider();
    let reference = qbot_llm::embedding::EmbeddingConfig::dashscope_v4(2048);
    assert_eq!(
        (
            embedding.model.as_str(),
            embedding.dims,
            embedding.max_batch
        ),
        (
            reference.model.as_str(),
            reference.dims,
            reference.max_batch
        )
    );

    let tools = c.tool_settings();
    assert_eq!(
        (
            tools.max_sends_per_run,
            tools.search.default_limit,
            tools.search.max_limit
        ),
        (4, 8, 50)
    );
}

#[test]
fn settings_flow_through_to_the_runtime_structs() {
    let root = scratch();
    write(
        &root,
        "etc/config.toml",
        r#"
[runtime]
reply_capacity = 8
reply_concurrency = 2
reply_deadline_secs = 45
[history]
batch_lines = 20
raw_batches = 3
summary_batches = 0
[memory]
slice_batches = 4
previous_context_batches = 2
next_context_batches = 3
[providers.text]
kind = "openai_responses"
state = "server_state"
request_timeout_secs = 90
retries = 5
"#,
    );
    let c = load(&root, &MapEnv::default()).unwrap().config;
    let s = c.supervisor();
    assert_eq!(
        (s.capacity, s.concurrency, s.reply_deadline),
        (8, 2, Duration::from_secs(45))
    );
    let grid = c.slice_grid();
    assert_eq!(grid.grid.lines_per_batch, 20);
    assert_eq!(
        (
            grid.slice_batches,
            grid.previous_context_batches,
            grid.next_context_batches
        ),
        (4, 2, 3)
    );
    assert_eq!(grid.retained_raw_batches, 3, "the verbatim tier");
    assert_eq!(
        c.history_window(),
        qbot_core::HistoryWindow {
            batch_lines: 20,
            raw_batches: 3,
            summary_batches: 0
        }
    );
    let t = c.text_provider();
    assert_eq!(t.flavor, qbot_llm::responses::Flavor::Standard);
    assert_eq!(t.state, qbot_llm::responses::StateMode::ServerState);
    assert_eq!(
        (t.timeout, t.retry.retries),
        (Some(Duration::from_secs(90)), 5)
    );
}

#[test]
fn the_database_url_encodes_credentials_and_carries_the_ssl_mode() {
    let c = load(&scratch(), &MapEnv::default()).unwrap().config;
    assert_eq!(
        c.database_url("p@ss:w/rd#?"),
        "postgres://qbot:p%40ss%3Aw%2Frd%23%3F@postgres:5432/qbot?sslmode=prefer"
    );
    assert_eq!(
        c.database_url("plain-Pass_1.~"),
        "postgres://qbot:plain-Pass_1.~@postgres:5432/qbot?sslmode=prefer"
    );
}

#[test]
fn authentication_can_be_turned_off_by_naming_no_secret() {
    let root = scratch();
    write(&root, "secrets/database_password", "a");
    write(&root, "secrets/text_api_key", "b");
    write(&root, "secrets/embedding_api_key", "c");
    let env = MapEnv::new([("QBOT__GATEWAY__ACCESS_TOKEN_SECRET", "")]);
    let loaded = load(&root, &env).unwrap();
    let secrets = loaded
        .config
        .resolve_secrets(&resolver(&root, &MapEnv::default()))
        .unwrap();
    assert!(secrets.onebot_access_token.is_none());
}

#[test]
fn the_bot_account_is_required_to_run_but_not_to_load() {
    let root = scratch();
    let loaded = load(&root, &MapEnv::default()).unwrap();
    let missing = loaded.config.require_deployment().unwrap_err().0;
    assert!(matches!(&missing[0], ConfigError::Invalid { key, .. } if key == "bot.account"));
    let configured = load(&root, &MapEnv::new([("QBOT__BOT__ACCOUNT", "10001")])).unwrap();
    configured.config.require_deployment().unwrap();
    assert_eq!(
        configured.config.bot_account().map(|a| a.get()),
        Some(10001)
    );
}

#[test]
fn gateway_and_bot_settings_are_validated_with_every_problem_reported() {
    let root = scratch();
    let env = MapEnv::new([
        ("QBOT__BOT__TIMEZONE", "Mars/Olympus"),
        ("QBOT__BOT__OWNERS", "[0]"),
        ("QBOT__GATEWAY__LISTEN", "not-an-address"),
        ("QBOT__GATEWAY__PATH", "no-slash"),
        ("QBOT__GATEWAY__MAX_MESSAGE_CHARS", "3"),
        ("QBOT__COMMANDS__TOP_MAX_ROWS", "0"),
    ]);
    let keys: Vec<String> = errors(load(&root, &env))
        .into_iter()
        .filter_map(|e| match e {
            ConfigError::Invalid { key, .. } => Some(key),
            _ => None,
        })
        .collect();
    for key in [
        "bot.timezone",
        "bot.owners",
        "gateway.listen",
        "gateway.path",
        "gateway.max_message_chars",
        "commands.top_max_rows",
    ] {
        assert!(keys.iter().any(|k| k == key), "{key} missing from {keys:?}");
    }
}

#[test]
fn readiness_reports_missing_settings_and_secrets_together() {
    let root = scratch();
    let loaded = load(&root, &MapEnv::default()).unwrap();
    let found = loaded
        .config
        .ready_to_run(&resolver(&root, &MapEnv::default()))
        .unwrap_err()
        .0;
    assert!(
        found
            .iter()
            .any(|e| matches!(e, ConfigError::Invalid { key, .. } if key == "bot.account"))
    );
    assert_eq!(
        found
            .iter()
            .filter(|e| matches!(e, ConfigError::MissingSecret { .. }))
            .count(),
        4,
        "{found:?}"
    );
}

#[test]
fn media_settings_convert_and_are_validated() {
    let root = scratch();
    let loaded = load(&root, &MapEnv::default()).unwrap();
    let media = loaded.config.media_config();
    assert_eq!(
        (
            media.images_per_minute,
            media.clips_per_minute,
            media.max_image_bytes
        ),
        (6, 20, 8 * 1024 * 1024)
    );
    assert_eq!(loaded.config.media_wait(), Duration::from_secs(25));
    assert!(
        !loaded.config.providers.vision.enabled,
        "pictures stay bare until a vision model is chosen"
    );

    let env = MapEnv::new([
        ("QBOT__MEDIA__CAPACITY", "0"),
        ("QBOT__ASR__LANGUAGE", "klingon"),
        ("QBOT__ASR__WORKERS", "0"),
    ]);
    let keys: Vec<String> = errors(load(&root, &env))
        .into_iter()
        .filter_map(|e| match e {
            ConfigError::Invalid { key, .. } => Some(key),
            _ => None,
        })
        .collect();
    for key in ["media.capacity", "asr.language", "asr.workers"] {
        assert!(keys.iter().any(|k| k == key), "{key} missing from {keys:?}");
    }
}

#[test]
fn the_vision_key_is_needed_only_when_vision_is_enabled() {
    let root = scratch();
    write(&root, "secrets/database_password", "a");
    write(&root, "secrets/text_api_key", "b");
    write(&root, "secrets/embedding_api_key", "c");
    write(&root, "secrets/onebot_access_token", "d");
    let off = load(&root, &MapEnv::default()).unwrap();
    assert!(
        off.config
            .resolve_secrets(&resolver(&root, &MapEnv::default()))
            .unwrap()
            .vision_api_key
            .is_none()
    );

    let on = load(
        &root,
        &MapEnv::new([("QBOT__PROVIDERS__VISION__ENABLED", "true")]),
    )
    .unwrap();
    let missing = on
        .config
        .resolve_secrets(&resolver(&root, &MapEnv::default()))
        .unwrap_err()
        .0;
    assert!(
        missing.iter().any(
            |e| matches!(e, ConfigError::MissingSecret { name, .. } if name == "VISION_API_KEY")
        )
    );
    write(&root, "secrets/vision_api_key", "e");
    assert_eq!(
        on.config
            .resolve_secrets(&resolver(&root, &MapEnv::default()))
            .unwrap()
            .vision_api_key
            .unwrap()
            .expose(),
        "e"
    );
}

#[test]
fn web_search_needs_its_key_only_when_enabled_and_converts_every_setting() {
    let root = scratch();
    write(&root, "secrets/database_password", "a");
    write(&root, "secrets/text_api_key", "b");
    write(&root, "secrets/embedding_api_key", "c");
    write(&root, "secrets/onebot_access_token", "d");
    let off = load(&root, &MapEnv::default()).unwrap();
    assert!(
        !off.config.providers.search.enabled,
        "off until a key is provided"
    );
    assert!(
        off.config
            .resolve_secrets(&resolver(&root, &MapEnv::default()))
            .unwrap()
            .search_api_key
            .is_none()
    );

    let env = MapEnv::new([
        ("QBOT__PROVIDERS__SEARCH__ENABLED", "true"),
        ("QBOT__PROVIDERS__SEARCH__DEPTH", "advanced"),
        ("QBOT__PROVIDERS__SEARCH__MAX_RESULTS", "8"),
        ("QBOT__PROVIDERS__SEARCH__PROXY", "http://172.17.0.1:7890"),
    ]);
    let on = load(&root, &env).unwrap();
    let missing = on
        .config
        .resolve_secrets(&resolver(&root, &MapEnv::default()))
        .unwrap_err()
        .0;
    assert!(
        missing.iter().any(
            |e| matches!(e, ConfigError::MissingSecret { name, .. } if name == "SEARCH_API_KEY")
        )
    );
    write(&root, "secrets/search_api_key", "e");
    assert!(
        on.config
            .resolve_secrets(&resolver(&root, &MapEnv::default()))
            .unwrap()
            .search_api_key
            .is_some()
    );
    let c = &on.config;
    assert_eq!(
        c.search_provider(),
        qbot_llm::search::TavilyConfig {
            max_results: 8,
            depth: qbot_llm::search::SearchDepth::Advanced,
            timeout: Duration::from_secs(20),
            retry: qbot_llm::responses::RetryPolicy {
                retries: 2,
                base: Duration::from_millis(500),
                jitter: true,
            },
            extract: qbot_llm::search::ExtractConfig {
                depth: qbot_llm::search::SearchDepth::Basic,
                chunks_per_source: 3,
            },
        }
    );
    assert_eq!(c.search_proxy(), Some("http://172.17.0.1:7890"));
    assert_eq!(off.config.search_proxy(), None, "empty means direct");

    let bad = errors(load(
        &root,
        &MapEnv::new([
            ("QBOT__PROVIDERS__SEARCH__ENABLED", "true"),
            ("QBOT__PROVIDERS__SEARCH__MAX_RESULTS", "50"),
            ("QBOT__PROVIDERS__SEARCH__PROXY", "not a url"),
        ]),
    ));
    let keys: Vec<_> = bad.iter().map(ToString::to_string).collect();
    assert!(
        keys.iter()
            .any(|k| k.contains("providers.search.max_results")),
        "{keys:?}"
    );
    assert!(
        keys.iter().any(|k| k.contains("providers.search.proxy")),
        "{keys:?}"
    );
}

#[test]
fn maintenance_settings_convert_and_validate() {
    let root = scratch();
    let loaded = load(&root, &MapEnv::default()).unwrap();
    // No owners by default, so the nightly pipeline is scheduled and the report is not.
    assert_eq!(
        loaded
            .config
            .recurrences()
            .iter()
            .map(|r| r.name.as_str())
            .collect::<Vec<_>>(),
        ["nightly"]
    );
    let with_owner = load(&root, &MapEnv::new([("QBOT__BOT__OWNERS", "[1]")])).unwrap();
    assert_eq!(
        with_owner
            .config
            .recurrences()
            .iter()
            .map(|r| r.name.as_str())
            .collect::<Vec<_>>(),
        ["nightly", "report"]
    );

    let paths = with_owner.config.paths(&with_owner.layout);
    let ops = with_owner
        .config
        .ops_config(&paths, "pw", jiff::tz::TimeZone::UTC);
    let backup = ops.backup.unwrap();
    assert_eq!(
        (backup.keep, backup.timeout, backup.target.password.as_str()),
        (14, Duration::from_secs(900), "pw")
    );
    assert_eq!(backup.dir, paths.backups_dir);
    assert!(backup.bin_dir.is_none(), "empty means PATH");
    assert_eq!(
        (ops.alias_unused, ops.runs_keep),
        (
            Duration::from_secs(30 * 86_400),
            Some(Duration::from_secs(90 * 86_400))
        )
    );

    let off = load(
        &root,
        &MapEnv::new([
            ("QBOT__MAINTENANCE__BACKUPS_ENABLED", "false"),
            ("QBOT__MAINTENANCE__RUNS_KEEP_DAYS", "0"),
        ]),
    )
    .unwrap();
    let ops = off.config.ops_config(&paths, "pw", jiff::tz::TimeZone::UTC);
    assert!(
        ops.backup.is_none() && ops.runs_keep.is_none(),
        "0 keeps runs forever; backups can be off"
    );

    let keys: Vec<String> = errors(load(
        &root,
        &MapEnv::new([
            ("QBOT__MAINTENANCE__NIGHTLY_CRON", "every night"),
            ("QBOT__MAINTENANCE__BACKUP_KEEP", "0"),
        ]),
    ))
    .into_iter()
    .filter_map(|e| match e {
        ConfigError::Invalid { key, .. } => Some(key),
        _ => None,
    })
    .collect();
    assert!(
        keys.contains(&"maintenance.nightly_cron".to_owned())
            && keys.contains(&"maintenance.backup_keep".to_owned()),
        "{keys:?}"
    );
}
