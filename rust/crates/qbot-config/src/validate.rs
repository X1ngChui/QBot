//! Startup validation. Every problem is collected and reported together, each naming its key.

use crate::error::{ConfigError, ConfigErrors};
use crate::model::{Config, State, TextKind};

struct Check(Vec<ConfigError>);

impl Check {
    fn that(&mut self, ok: bool, key: &str, reason: &str) {
        if !ok {
            self.0.push(ConfigError::Invalid {
                key: key.to_owned(),
                reason: reason.to_owned(),
            });
        }
    }

    fn text(&mut self, value: &str, key: &str) {
        self.that(!value.trim().is_empty(), key, "must not be empty");
    }

    fn secret_name(&mut self, value: &str, key: &str) {
        let valid = value.chars().next().is_some_and(|c| c.is_ascii_uppercase())
            && value
                .chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
        self.that(valid, key, "must be the NAME of a secret (capital letters, digits and underscores), never the secret itself");
    }

    fn endpoint(&mut self, value: &str, key: &str) {
        let rest = value
            .strip_prefix("https://")
            .or_else(|| value.strip_prefix("http://"));
        let ok = rest.is_some_and(|r| {
            let authority = r.split('/').next().unwrap_or_default();
            !authority.is_empty()
                && !authority.contains('@')
                && !value.chars().any(char::is_whitespace)
        });
        self.that(
            ok,
            key,
            "must be an http:// or https:// URL without embedded credentials",
        );
    }
}

impl Config {
    /// Settings with no usable default that a deployment must provide before the bot can run.
    /// Separate from [`validate`](Self::validate) because the defaults alone are a valid
    /// configuration (for tooling) but not a runnable one.
    pub fn require_deployment(&self) -> Result<(), ConfigErrors> {
        if self.bot.account >= 1 {
            return Ok(());
        }
        Err(ConfigErrors(vec![ConfigError::Invalid {
            key: "bot.account".to_owned(),
            reason: "is not set; put the bot's QQ account number in config.toml".to_owned(),
        }]))
    }

    pub fn validate(&self) -> Result<(), ConfigErrors> {
        let mut c = Check(Vec::new());

        let b = &self.bot;
        c.that(b.account >= 0, "bot.account", "must be a QQ account number");
        for owner in &b.owners {
            c.that(
                *owner >= 1,
                "bot.owners",
                "each owner must be a QQ account number",
            );
        }
        c.that(
            b.nicknames.iter().all(|n| !n.trim().is_empty()),
            "bot.nicknames",
            "must not contain an empty name",
        );
        c.that(
            jiff::tz::TimeZone::get(&b.timezone).is_ok(),
            "bot.timezone",
            "must be an IANA time zone name such as Asia/Shanghai or UTC",
        );
        c.text(&b.locale, "bot.locale");

        let g = &self.gateway;
        c.that(
            g.listen.parse::<std::net::SocketAddr>().is_ok(),
            "gateway.listen",
            "must be an address and port such as 0.0.0.0:6199",
        );
        c.that(
            g.path.starts_with('/') && !g.path.contains(char::is_whitespace),
            "gateway.path",
            "must start with / and contain no whitespace",
        );
        if !g.access_token_secret.is_empty() {
            c.secret_name(&g.access_token_secret, "gateway.access_token_secret");
        }
        c.that(
            g.action_timeout_secs >= 1,
            "gateway.action_timeout_secs",
            "must be at least 1",
        );
        c.that(
            g.echo_timeout_secs >= 1,
            "gateway.echo_timeout_secs",
            "must be at least 1",
        );
        c.that(
            g.max_message_chars >= 16,
            "gateway.max_message_chars",
            "must leave room for the quote and mention that start every reply",
        );
        c.that(
            self.commands.members_max_rows >= 1,
            "commands.members_max_rows",
            "must be at least 1",
        );
        c.that(
            self.commands.top_max_rows >= 1,
            "commands.top_max_rows",
            "must be at least 1",
        );
        c.that(
            self.commands.runs_max_rows >= 1,
            "commands.runs_max_rows",
            "must be at least 1",
        );
        c.that(
            self.commands.notes_per_account >= 1,
            "commands.notes_per_account",
            "must be at least 1",
        );

        let d = &self.database;
        c.text(&d.host, "database.host");
        c.text(&d.name, "database.name");
        c.text(&d.user, "database.user");
        c.that(d.port >= 1, "database.port", "must be at least 1");
        c.that(
            d.max_connections >= 1,
            "database.max_connections",
            "must be at least 1",
        );
        c.that(
            d.connect_timeout_secs >= 1,
            "database.connect_timeout_secs",
            "must be at least 1",
        );
        c.secret_name(&d.password_secret, "database.password_secret");

        let r = &self.runtime;
        c.that(
            r.reply_capacity >= 1,
            "runtime.reply_capacity",
            "must be at least 1",
        );
        c.that(
            r.reply_concurrency >= 1,
            "runtime.reply_concurrency",
            "must be at least 1",
        );
        c.that(
            r.reply_concurrency <= r.reply_capacity,
            "runtime.reply_concurrency",
            "must not exceed runtime.reply_capacity",
        );
        c.that(
            r.reply_deadline_secs >= 1,
            "runtime.reply_deadline_secs",
            "must be at least 1",
        );

        let a = &self.agent;
        c.that(a.max_turns >= 1, "agent.max_turns", "must be at least 1");
        c.that(
            a.max_sends_per_run >= 1,
            "agent.max_sends_per_run",
            "must be at least 1",
        );
        c.that(
            a.max_output_tokens >= 1,
            "agent.max_output_tokens",
            "must be at least 1",
        );
        c.that(
            self.history.batch_lines >= 1,
            "history.batch_lines",
            "must be at least 1",
        );
        c.that(
            self.history.raw_batches >= 1,
            "history.raw_batches",
            "must be at least 1: the batch being filled is always shown",
        );

        let t = &self.providers.text;
        c.endpoint(&t.endpoint, "providers.text.endpoint");
        c.text(&t.model, "providers.text.model");
        c.secret_name(&t.api_key_secret, "providers.text.api_key_secret");
        c.that(
            !(t.kind == TextKind::Deepseek && t.state == State::ServerState),
            "providers.text.state",
            "the deepseek kind keeps no server-side state; use stateless",
        );
        c.that(
            t.request_timeout_secs >= 1,
            "providers.text.request_timeout_secs",
            "must be at least 1",
        );
        c.that(
            t.connect_timeout_secs >= 1,
            "providers.text.connect_timeout_secs",
            "must be at least 1",
        );

        let vision = &self.providers.vision;
        if vision.enabled {
            c.endpoint(&vision.endpoint, "providers.vision.endpoint");
            c.text(&vision.model, "providers.vision.model");
            c.secret_name(&vision.api_key_secret, "providers.vision.api_key_secret");
            c.that(
                vision.max_output_tokens >= 1,
                "providers.vision.max_output_tokens",
                "must be at least 1",
            );
            c.that(
                vision.request_timeout_secs >= 1,
                "providers.vision.request_timeout_secs",
                "must be at least 1",
            );
            c.that(
                vision.connect_timeout_secs >= 1,
                "providers.vision.connect_timeout_secs",
                "must be at least 1",
            );
        }

        let search = &self.providers.search;
        if search.enabled {
            c.endpoint(&search.endpoint, "providers.search.endpoint");
            c.secret_name(&search.api_key_secret, "providers.search.api_key_secret");
            c.that(
                (1..=20).contains(&search.max_results),
                "providers.search.max_results",
                "must be between 1 and 20 (what the provider accepts)",
            );
            c.that(
                (1..=5).contains(&search.chunks_per_source),
                "providers.search.chunks_per_source",
                "must be between 1 and 5 (what the provider accepts)",
            );
            if !search.proxy.is_empty() {
                c.endpoint(&search.proxy, "providers.search.proxy");
            }
            c.that(
                search.request_timeout_secs >= 1,
                "providers.search.request_timeout_secs",
                "must be at least 1",
            );
            c.that(
                search.connect_timeout_secs >= 1,
                "providers.search.connect_timeout_secs",
                "must be at least 1",
            );
        }

        let md = &self.media;
        for (key, value) in [
            ("media.images_per_minute", u64::from(md.images_per_minute)),
            ("media.clips_per_minute", u64::from(md.clips_per_minute)),
            ("media.max_image_mb", md.max_image_mb),
            ("media.max_audio_secs", md.max_audio_secs),
            ("media.concurrency", md.concurrency as u64),
            ("media.capacity", md.capacity as u64),
            ("media.http_timeout_secs", md.http_timeout_secs),
            ("media.protocol_timeout_secs", md.protocol_timeout_secs),
            ("media.unreadable_hold_secs", md.unreadable_hold_secs),
            ("media.forward_max_lines", md.forward_max_lines as u64),
        ] {
            c.that(value >= 1, key, "must be at least 1");
        }
        c.text(&md.description_language, "media.description_language");

        let asr = &self.asr;
        c.that(
            ["auto", "zh", "en", "ja", "ko", "yue"].contains(&asr.language.as_str()),
            "asr.language",
            "must be one of auto, zh, en, ja, ko, yue",
        );
        c.that(asr.threads >= 1, "asr.threads", "must be at least 1");
        c.that(asr.workers >= 1, "asr.workers", "must be at least 1");
        c.that(
            !asr.model_dir.as_os_str().is_empty(),
            "asr.model_dir",
            "must not be empty",
        );

        let mt = &self.maintenance;
        for (key, expression) in [
            ("maintenance.nightly_cron", &mt.nightly_cron),
            ("maintenance.report_cron", &mt.report_cron),
        ] {
            if let Err(error) =
                qbot_sched::Recurrence::new("check", expression, qbot_sched::JobKind::Nightly)
            {
                c.that(false, key, &error.to_string());
            }
        }
        c.that(
            mt.backup_keep >= 1,
            "maintenance.backup_keep",
            "must be at least 1",
        );
        c.that(
            mt.backup_timeout_secs >= 1,
            "maintenance.backup_timeout_secs",
            "must be at least 1",
        );
        c.that(
            mt.alias_unused_days >= 1,
            "maintenance.alias_unused_days",
            "must be at least 1",
        );
        c.that(
            mt.timers_keep_days >= 1,
            "maintenance.timers_keep_days",
            "must be at least 1",
        );

        let e = &self.providers.embedding;
        c.endpoint(&e.endpoint, "providers.embedding.endpoint");
        c.text(&e.model, "providers.embedding.model");
        c.secret_name(&e.api_key_secret, "providers.embedding.api_key_secret");
        c.that(
            e.dims >= 1,
            "providers.embedding.dims",
            "must be at least 1",
        );
        c.that(
            e.max_batch >= 1,
            "providers.embedding.max_batch",
            "must be at least 1",
        );
        c.that(
            e.request_timeout_secs >= 1,
            "providers.embedding.request_timeout_secs",
            "must be at least 1",
        );
        c.that(
            e.connect_timeout_secs >= 1,
            "providers.embedding.connect_timeout_secs",
            "must be at least 1",
        );

        let k = &self.tasks;
        c.that(
            k.min_delay_secs >= 1,
            "tasks.min_delay_secs",
            "must be at least 1 (it is what stops a task waking itself in a tight loop)",
        );
        c.that(
            k.max_pending_per_group >= 1,
            "tasks.max_pending_per_group",
            "must be at least 1",
        );
        c.that(
            k.max_chain_depth >= 1,
            "tasks.max_chain_depth",
            "must be at least 1",
        );

        let s = &self.scheduler;
        c.that(
            s.job_lease_secs >= 1,
            "scheduler.job_lease_secs",
            "must be at least 1",
        );
        c.that(
            s.max_job_attempts >= 1,
            "scheduler.max_job_attempts",
            "must be at least 1",
        );
        c.that(
            !s.job_backoff_secs.is_empty() && s.job_backoff_secs.iter().all(|d| *d >= 1),
            "scheduler.job_backoff_secs",
            "must list at least one delay, each at least 1",
        );
        c.that(
            s.max_concurrent_jobs >= 1,
            "scheduler.max_concurrent_jobs",
            "must be at least 1",
        );

        c.that(
            self.tools.read_url.max_chars >= 1,
            "tools.read_url.max_chars",
            "must be at least 1",
        );
        let h = &self.tools.search_history;
        c.that(
            h.default_limit >= 1,
            "tools.search_history.default_limit",
            "must be at least 1",
        );
        c.that(
            h.max_limit >= h.default_limit,
            "tools.search_history.max_limit",
            "must not be below default_limit",
        );

        let m = &self.memory;
        c.text(&m.language, "memory.language");
        c.that(
            m.slice_batches >= 1,
            "memory.slice_batches",
            "must be at least 1",
        );
        c.that(
            m.extraction.max_output_tokens >= 1,
            "memory.extraction.max_output_tokens",
            "must be at least 1",
        );
        c.that(
            m.extraction.max_attempts >= 1,
            "memory.extraction.max_attempts",
            "must be at least 1",
        );
        c.that(
            m.recall.limit >= 1,
            "memory.recall.limit",
            "must be at least 1",
        );
        c.that(
            m.recall.max_distance > 0.0 && m.recall.max_distance < 1.0,
            "memory.recall.max_distance",
            "must be in (0, 1): a candidate must be similar, not unrelated or opposite",
        );
        let half = &m.facts.half_life_days;
        c.that(
            half.stable >= 1 && half.default >= 1 && half.fast >= 1,
            "memory.facts.half_life_days",
            "every half-life must be at least 1 day",
        );
        c.that(
            m.facts.forget_below > 0.0 && m.facts.forget_below < 1.0,
            "memory.facts.forget_below",
            "must be between 0 and 1",
        );

        let i = &self.identity;
        c.that(
            i.confirm_at > 0.0 && i.confirm_at <= 1.0,
            "identity.confirm_at",
            "must be in (0, 1]",
        );
        c.that(
            i.invitation_ttl_secs >= 1,
            "identity.invitation_ttl_secs",
            "must be at least 1",
        );

        for (key, path) in [
            ("paths.models_dir", &self.paths.models_dir),
            ("paths.backups_dir", &self.paths.backups_dir),
            ("paths.personas_dir", &self.paths.personas_dir),
            ("paths.locales_dir", &self.paths.locales_dir),
        ] {
            c.that(!path.as_os_str().is_empty(), key, "must not be empty");
        }

        if c.0.is_empty() {
            Ok(())
        } else {
            Err(ConfigErrors(c.0))
        }
    }
}
