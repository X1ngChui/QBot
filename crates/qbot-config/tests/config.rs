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
            c.replies.concurrency,
            c.replies.deadline_secs,
            c.replies.max_messages
        ),
        (3, 180, 4)
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
        "[replies]\nconcurrency = 10\ndeadline_secs = 60\n[providers.text]\nmodel = \"m-one\"\n",
    );
    write(
        &root,
        "etc/conf.d/20-b.toml",
        "[replies]\nconcurrency = 20\n",
    );
    write(
        &root,
        "etc/conf.d/10-a.toml",
        "[replies]\nconcurrency = 15\nmax_messages = 7\n",
    );
    write(
        &root,
        "etc/conf.d/ignored.txt",
        "this is not toml and is not read",
    );
    let env = MapEnv::new([("QBOT__REPLIES__MAX_MESSAGES", "9")]);
    let loaded = load(&root, &env).unwrap();
    let c = &loaded.config;

    assert_eq!(
        c.replies.concurrency, 20,
        "drop-ins apply in file-name order, so 20-b wins over 10-a"
    );
    assert_eq!(
        c.replies.deadline_secs, 60,
        "a key set in config.toml survives later layers that do not set it"
    );
    assert_eq!(
        c.replies.max_messages, 9,
        "the environment beats every file"
    );
    assert_eq!(c.providers.text.model, "m-one");
    assert_eq!(
        c.history.batch_lines, 30,
        "a key nobody sets keeps its default"
    );
    assert_eq!(
        c.providers.text.endpoint, "https://api.deepseek.com",
        "siblings of an overridden key are untouched"
    );

    let described: Vec<String> = loaded.sources.iter().map(|s| format!("{s:?}")).collect();
    assert_eq!(loaded.sources.len(), 5, "{described:?}");
    assert!(matches!(&loaded.sources[0], Source::Defaults));
    assert!(matches!(&loaded.sources[1], Source::File(p) if p.ends_with("config.toml")));
    assert!(matches!(&loaded.sources[2], Source::DropIn(p) if p.ends_with("10-a.toml")));
    assert!(matches!(&loaded.sources[3], Source::DropIn(p) if p.ends_with("20-b.toml")));
    assert!(
        matches!(&loaded.sources[4], Source::Env(n) if n == "QBOT__REPLIES__MAX_MESSAGES"),
        "only the name is recorded, never the value"
    );
}

#[test]
fn arrays_replace_rather_than_merge() {
    let root = scratch();
    write(&root, "etc/config.toml", "[bot]\nowners = [5, 10]\n");
    assert_eq!(
        load(&root, &MapEnv::default()).unwrap().config.bot.owners,
        [5, 10]
    );
    let env = MapEnv::new([("QBOT__BOT__OWNERS", "[1, 2, 3]")]);
    assert_eq!(load(&root, &env).unwrap().config.bot.owners, [1, 2, 3]);
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
        ("QBOT__PROVIDERS__TEXT__MODEL", "m-two"),
        ("QBOT__MEDIA__TRANSCRIBE_VOICE", "false"),
        ("QBOT__MEMORY__RECALL__MAX_DISTANCE", "0.3"),
        ("QBOT__MAINTENANCE__RUNS_KEEP_DAYS", "30"),
        ("QBOT__DATABASE__SSL_MODE", "verify-full"),
    ]);
    let c = load(&root, &ok).unwrap().config;
    assert_eq!(c.providers.text.model, "m-two");
    assert!(!c.media.transcribe_voice);
    assert!((c.memory.recall.max_distance - 0.3).abs() < 1e-6);
    assert_eq!(c.maintenance.runs_keep_days, 30);
    assert_eq!(c.database.ssl_mode.as_str(), "verify-full");

    // A string that looks like a number must be quoted; unquoted it is a number, and a number is
    // not accepted where text belongs (no silent coercion either way).
    let quoted = load(
        &root,
        &MapEnv::new([("QBOT__PROVIDERS__TEXT__MODEL", "\"123\"")]),
    )
    .unwrap()
    .config;
    assert_eq!(quoted.providers.text.model, "123");
    let text = message(&errors(load(
        &root,
        &MapEnv::new([("QBOT__PROVIDERS__TEXT__MODEL", "123")]),
    )));
    assert!(
        text.to_lowercase().contains("model") && text.contains("expected a string"),
        "{text}"
    );
}

#[test]
fn bad_environment_overrides_name_the_key_and_the_variable() {
    let root = scratch();
    for (name, value, key) in [
        ("QBOT__REPLIES__CONCURRENCY", "many", "concurrency"),
        ("QBOT__MEDIA__TRANSCRIBE_VOICE", "maybe", "transcribe_voice"),
        ("QBOT__BOT__OWNERS", "60", "owners"),
        ("QBOT__REPLIES__NO_SUCH", "1", "no_such"),
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
    write(&root, "etc/config.toml", "[replies]\nconcurency = 4\n");
    let text = message(&errors(load(&root, &MapEnv::default())));
    assert!(
        text.contains("concurency") && text.contains("config.toml"),
        "{text}"
    );

    write(
        &root,
        "etc/config.toml",
        "[replies]\ndeadline_secs = \"soon\"\n",
    );
    let text = message(&errors(load(&root, &MapEnv::default())));
    assert!(
        text.contains("deadline_secs") && text.contains("config.toml"),
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
    write(&root, "etc/conf.d/broken.toml", "[replies\nx = ");
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
[replies]
concurrency = 0
max_messages = 0
[providers.text]
endpoint = "https://user:pass@api.example.com"
[providers.embedding]
dims = 0
[media]
images_per_minute = 0
[memory]
slice_batches = 0
[memory.recall]
max_distance = 1.0
[memory.facts]
half_life_days = { stable = 0, default = 30, fast = 14 }
[maintenance]
nightly_cron = "every night"
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
        "replies.concurrency",
        "replies.max_messages",
        "providers.text.endpoint",
        "providers.embedding.dims",
        "media.images_per_minute",
        "memory.slice_batches",
        "memory.recall.max_distance",
        "memory.facts.half_life_days",
        "maintenance.nightly_cron",
    ] {
        assert!(
            keys.contains(&expected.to_owned()),
            "{expected} missing from {keys:?}"
        );
    }
    assert_eq!(keys.len(), 9);
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
fn state_and_configuration_files_have_fixed_places_under_their_directories() {
    let root = scratch();
    let loaded = load(&root, &MapEnv::default()).unwrap();
    let p = loaded.config.paths(&loaded.layout);
    assert_eq!(p.models_dir, root.join("data/models"));
    assert_eq!(p.backups_dir, root.join("data/backups"));
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
    assert_eq!(c.recall_params(), qbot_memory::RecallParams::default());
    assert_eq!(c.decay_policy(), qbot_memory::facts::DecayPolicy::default());
    assert_eq!(c.history_window(), qbot_core::HistoryWindow::default());
    assert_eq!(c.media_config(), qbot_media::MediaConfig::default());
    assert_eq!(c.tool_settings().max_sends_per_run, 4);

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
}

#[test]
fn settings_flow_through_to_the_runtime_structs() {
    let root = scratch();
    write(
        &root,
        "etc/config.toml",
        r#"
[replies]
concurrency = 2
deadline_secs = 45
max_messages = 2
[history]
batch_lines = 20
raw_batches = 3
summary_batches = 0
[memory]
slice_batches = 4
[memory.recall]
half_life_days = 30
[providers.text]
kind = "openai_responses"
reasoning = "high"
"#,
    );
    let c = load(&root, &MapEnv::default()).unwrap().config;
    let s = c.supervisor();
    assert_eq!(
        (s.capacity, s.concurrency, s.reply_deadline),
        (20, 2, Duration::from_secs(45)),
        "the queue is derived from the concurrency"
    );
    assert_eq!(c.tool_settings().max_sends_per_run, 2);
    let grid = c.slice_grid();
    assert_eq!(grid.grid.lines_per_batch, 20);
    assert_eq!(
        (
            grid.slice_batches,
            grid.previous_context_batches,
            grid.next_context_batches
        ),
        (4, 1, 1)
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
    assert_eq!(
        c.recall_params().half_life,
        Duration::from_secs(30 * 86_400)
    );
    let t = c.text_provider();
    assert_eq!(t.flavor, qbot_llm::responses::Flavor::Standard);
    assert_eq!(
        t.state,
        qbot_llm::responses::StateMode::ServerState,
        "an OpenAI-style provider continues from its stored response"
    );
    assert!(t.timeout.is_some(), "every text call has a deadline");
    assert_eq!(c.reply_reasoning(), qbot_llm::ReasoningEffort::High);
    let v = c.vision_provider();
    assert_eq!(
        v.state,
        qbot_llm::responses::StateMode::Stateless,
        "each picture is its own conversation"
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
        ("QBOT__BOT__NICKNAMES", "[\" \"]"),
        ("QBOT__GATEWAY__LISTEN", "not-an-address"),
        ("QBOT__GATEWAY__ACCESS_TOKEN_SECRET", "a-token-pasted-here"),
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
        "bot.nicknames",
        "gateway.listen",
        "gateway.access_token_secret",
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
    assert_eq!((media.images_per_minute, media.clips_per_minute), (6, 20));
    assert!(
        !loaded.config.providers.vision.enabled,
        "pictures stay bare until a vision model is chosen"
    );
    let busy = load(
        &root,
        &MapEnv::new([("QBOT__MEDIA__IMAGES_PER_MINUTE", "12")]),
    )
    .unwrap();
    assert_eq!(busy.config.media_config().images_per_minute, 12);

    let env = MapEnv::new([
        ("QBOT__MEDIA__IMAGES_PER_MINUTE", "0"),
        ("QBOT__MEDIA__CLIPS_PER_MINUTE", "0"),
    ]);
    let keys: Vec<String> = errors(load(&root, &env))
        .into_iter()
        .filter_map(|e| match e {
            ConfigError::Invalid { key, .. } => Some(key),
            _ => None,
        })
        .collect();
    for key in ["media.images_per_minute", "media.clips_per_minute"] {
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
        qbot_llm::search::TavilyConfig::new(
            qbot_llm::search::SearchDepth::Advanced,
            qbot_llm::search::SearchDepth::Basic,
        )
    );
}

#[test]
fn one_proxy_serves_the_services_listed_for_it_and_the_rest_connect_directly() {
    use qbot_config::Service;
    use qbot_llm::net::Route;
    let root = scratch();
    let none = load(&root, &MapEnv::default()).unwrap().config;
    for service in [
        Service::Text,
        Service::Vision,
        Service::Embedding,
        Service::Search,
        Service::Media,
    ] {
        assert_eq!(none.route(service), Route::Direct, "no proxy, all direct");
    }

    let proxy = "http://172.17.0.1:7890";
    let all = load(&root, &MapEnv::new([("QBOT__NETWORK__PROXY", proxy)]))
        .unwrap()
        .config;
    assert_eq!(
        all.route(Service::Media),
        Route::Proxy(proxy.into()),
        "a proxy serves every service by default"
    );

    let some = load(
        &root,
        &MapEnv::new([
            ("QBOT__NETWORK__PROXY", proxy),
            ("QBOT__NETWORK__PROXY_FOR", "[\"search\"]"),
        ]),
    )
    .unwrap()
    .config;
    assert_eq!(some.route(Service::Search), Route::Proxy(proxy.into()));
    assert_eq!(some.route(Service::Text), Route::Direct);
    assert_eq!(some.route(Service::Media), Route::Direct);

    let bad = errors(load(
        &root,
        &MapEnv::new([
            ("QBOT__NETWORK__PROXY", "not a url"),
            ("QBOT__NETWORK__PROXY_FOR", "[\"search\", \"ftp\"]"),
        ]),
    ));
    let text: Vec<_> = bad.iter().map(ToString::to_string).collect();
    assert!(
        text.iter()
            .any(|k| k.contains("proxy_for") || k.contains("ftp")),
        "an unknown service is refused: {text:?}"
    );
    let bad_url = errors(load(
        &root,
        &MapEnv::new([("QBOT__NETWORK__PROXY", "not a url")]),
    ));
    assert!(
        bad_url
            .iter()
            .any(|e| matches!(e, ConfigError::Invalid { key, .. } if key == "network.proxy")),
        "{bad_url:?}"
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
    assert_eq!((backup.keep, backup.target.password.as_str()), (14, "pw"));
    assert_eq!(backup.dir, paths.backups_dir);
    assert_eq!(ops.runs_keep, Some(Duration::from_secs(90 * 86_400)));

    let off = load(
        &root,
        &MapEnv::new([
            ("QBOT__MAINTENANCE__BACKUPS", "0"),
            ("QBOT__MAINTENANCE__RUNS_KEEP_DAYS", "0"),
        ]),
    )
    .unwrap();
    let ops = off.config.ops_config(&paths, "pw", jiff::tz::TimeZone::UTC);
    assert!(
        ops.backup.is_none() && ops.runs_keep.is_none(),
        "0 keeps runs forever; 0 backups makes none"
    );

    let keys: Vec<String> = errors(load(
        &root,
        &MapEnv::new([
            ("QBOT__MAINTENANCE__NIGHTLY_CRON", "every night"),
            ("QBOT__MAINTENANCE__REPORT_CRON", "61 * * * *"),
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
            && keys.contains(&"maintenance.report_cron".to_owned()),
        "{keys:?}"
    );
}
