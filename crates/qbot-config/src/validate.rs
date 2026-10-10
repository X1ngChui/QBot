//! Startup validation. Every problem is collected and reported together, each naming its key.

use crate::error::{ConfigError, ConfigErrors};
use crate::model::Config;

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
        c.that(
            b.owners.iter().all(|owner| *owner >= 1),
            "bot.owners",
            "each owner must be a QQ account number",
        );
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
        if !g.access_token_secret.is_empty() {
            c.secret_name(&g.access_token_secret, "gateway.access_token_secret");
        }

        let d = &self.database;
        c.text(&d.host, "database.host");
        c.text(&d.name, "database.name");
        c.text(&d.user, "database.user");
        c.that(d.port >= 1, "database.port", "must be at least 1");
        c.secret_name(&d.password_secret, "database.password_secret");

        let r = &self.replies;
        c.that(
            r.concurrency >= 1,
            "replies.concurrency",
            "must be at least 1",
        );
        c.that(
            r.deadline_secs >= 1,
            "replies.deadline_secs",
            "must be at least 1",
        );
        c.that(
            r.max_messages >= 1,
            "replies.max_messages",
            "must be at least 1",
        );
        c.that(
            (0.0..=1.0).contains(&r.spontaneous_chance),
            "replies.spontaneous_chance",
            "must be from 0 (never) to 1 (every line)",
        );

        let h = &self.history;
        c.that(
            h.batch_lines >= 1,
            "history.batch_lines",
            "must be at least 1",
        );
        c.that(
            h.raw_batches >= 1,
            "history.raw_batches",
            "must be at least 1: the batch being filled is always shown",
        );

        let m = &self.memory;
        c.that(
            m.slice_batches >= 1,
            "memory.slice_batches",
            "must be at least 1",
        );
        c.that(
            m.slice_batches <= h.raw_batches,
            "memory.slice_batches",
            "must be at most history.raw_batches: an episode must exist before its chat leaves the verbatim tier",
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

        let md = &self.media;
        c.that(
            md.reply_wait_secs < r.deadline_secs,
            "media.reply_wait_secs",
            "must be less than replies.deadline_secs: the wait counts against a reply's deadline",
        );
        c.that(
            md.images_per_minute >= 1,
            "media.images_per_minute",
            "must be at least 1 (switch pictures off with providers.vision.enabled)",
        );
        c.that(
            md.clips_per_minute >= 1,
            "media.clips_per_minute",
            "must be at least 1 (switch voice off with media.transcribe_voice)",
        );

        let t = &self.providers.text;
        c.endpoint(&t.endpoint, "providers.text.endpoint");
        c.text(&t.model, "providers.text.model");
        c.secret_name(&t.api_key_secret, "providers.text.api_key_secret");

        let v = &self.providers.vision;
        if v.enabled {
            c.endpoint(&v.endpoint, "providers.vision.endpoint");
            c.text(&v.model, "providers.vision.model");
            c.secret_name(&v.api_key_secret, "providers.vision.api_key_secret");
        }

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

        let s = &self.providers.search;
        if s.enabled {
            c.secret_name(&s.api_key_secret, "providers.search.api_key_secret");
        }
        if !self.network.proxy.is_empty() {
            c.endpoint(&self.network.proxy, "network.proxy");
        }

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

        if c.0.is_empty() {
            Ok(())
        } else {
            Err(ConfigErrors(c.0))
        }
    }
}
