//! `qbot import-history`: the Python bot's chat history, from a read-only export
//! (`deploy/export_python_history.sql`), into the archive.
//!
//! Each exported message is rebuilt as the OneBot frame the platform sent and goes through the
//! gateway's own parser and renderer, so imported lines read exactly like live ones. Only what
//! the Python adapter took out of a message is put back, and only where the export proves it:
//!
//! - a quote it moved into `reply_to` becomes the leading `reply` segment again;
//! - a mention of the bot it stripped (the message is marked `to_me`, has no mention of the bot,
//!   and does not quote one of the bot's messages, the other way to be `to_me`) becomes a
//!   leading mention again. When the quoted message is not in the export, which of the two it
//!   was is unknown and nothing is added.
//!
//! Picture descriptions the Python bot wrote fill the pictures' markers, as the media service
//! would have. Notices are skipped: only the Python bot's wording of them survives. Everything
//! derived (episodes, facts, group knowledge) is rebuilt afterwards by the normal extraction.
//!
//! The whole export is read and checked before anything is written. Writing goes through the
//! archive's normal append, whose message-id dedup makes a rerun skip what is already there.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use qbot_config::{ConfigErrors, Env, Loaded, SecretResolver, prepare_directories};
use qbot_core::marker::fill;
use qbot_core::{AccountId, GroupId, MessageId, UnixMillis};
use qbot_gateway::intake::Incoming;
use qbot_gateway::pipeline::archive_form;
use qbot_gateway::render::RenderContext;
use qbot_gateway::wire::{Frame, parse_frame};
use qbot_media::{Kind, clean_description};
use qbot_store::{
    Appended, DEFAULT_KEY, LeaseError, NewLine, NewSpeaker, PgArchive, RuntimeLease, Store,
    StoreError,
};
use serde::Deserialize;
use serde_json::{Value, json};

/// One line of the export.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Record {
    /// A picture description, keyed by the picture's file hash (the stem of its file name).
    Description {
        key: String,
        text: String,
    },
    Message(Exported),
}

/// One archived event of the Python bot.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Exported {
    pub group_id: i64,
    pub user_id: String,
    pub at_ms: i64,
    pub payload: Payload,
}

/// The Python bot's normalized form of a message: the platform's segments after its adapter
/// took out quotes and leading or trailing mentions of the bot, plus what it took out.
#[derive(Debug, Deserialize)]
pub struct Payload {
    pub message_id: Value,
    #[serde(default)]
    pub sub_type: String,
    pub segments: Vec<Value>,
    #[serde(default)]
    pub reply_to: Option<Value>,
    #[serde(default)]
    pub to_me: bool,
    pub author_kind: String,
    #[serde(default)]
    pub self_id: Value,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImportStats {
    pub messages: usize,
    pub bot_messages: usize,
    pub groups: usize,
    pub notices_skipped: usize,
    pub empty_skipped: usize,
    pub quotes_restored: usize,
    pub bot_mentions_restored: usize,
    /// `to_me` messages whose quote is not in the export, so it is unknown whether a mention of
    /// the bot was taken out.
    pub bot_mentions_unknown: usize,
    pub pictures: usize,
    pub pictures_described: usize,
    pub stored: usize,
    pub already_there: usize,
}

impl std::fmt::Display for ImportStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(
            f,
            "messages:              {} ({} by the bot) in {} groups",
            self.messages, self.bot_messages, self.groups
        )?;
        writeln!(f, "notices skipped:       {}", self.notices_skipped)?;
        writeln!(f, "empty skipped:         {}", self.empty_skipped)?;
        writeln!(f, "quotes restored:       {}", self.quotes_restored)?;
        writeln!(
            f,
            "bot mentions restored: {} ({} undecidable, left as they were)",
            self.bot_mentions_restored, self.bot_mentions_unknown
        )?;
        writeln!(
            f,
            "pictures described:    {} of {}",
            self.pictures_described, self.pictures
        )?;
        write!(
            f,
            "written:               {} new, {} already archived",
            self.stored, self.already_there
        )
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ImportError {
    #[error("{0}")]
    Config(#[from] ConfigErrors),
    #[error("cannot read the export: {0}")]
    Read(#[from] std::io::Error),
    #[error("export line {line}: {problem}")]
    Line { line: usize, problem: String },
    #[error(
        "the export was made by bot account {found}, but this deployment is account {expected}"
    )]
    OtherBot { found: String, expected: i64 },
    #[error(
        "group {group} already has {count} archived lines that are not in this export; importing \
         now would put older history after them"
    )]
    Occupied { group: i64, count: usize },
    #[error("database: {0}")]
    Store(#[from] StoreError),
    #[error("{0}")]
    Lease(#[from] LeaseError),
}

/// A message id as the export writes it (a JSON number or a numeric string).
fn id_text(value: &Value) -> Option<String> {
    match value {
        Value::Number(n) => Some(n.to_string()),
        Value::String(s) if !s.trim().is_empty() => Some(s.trim().to_owned()),
        _ => None,
    }
}

fn is_bot_mention(segment: &Value, bot: &str) -> bool {
    segment.get("type").and_then(Value::as_str) == Some("at")
        && segment
            .get("data")
            .and_then(|d| d.get("qq"))
            .and_then(id_text)
            .as_deref()
            == Some(bot)
}

/// The export read and checked, with every line ready to be written.
#[derive(Debug)]
pub struct Prepared {
    pub lines: Vec<Incoming>,
    pub stats: ImportStats,
}

/// Read the export and build the lines. Nothing is written; any problem stops the import with
/// the line it is on.
pub fn prepare(export: &str, bot: AccountId, ctx: &RenderContext) -> Result<Prepared, ImportError> {
    let bot_text = bot.get().to_string();
    let mut descriptions: HashMap<String, String> = HashMap::new();
    let mut messages: Vec<(usize, Exported)> = Vec::new();
    for (index, raw) in export.lines().enumerate() {
        let line = index + 1;
        if raw.trim().is_empty() {
            continue;
        }
        let record: Record = serde_json::from_str(raw).map_err(|e| ImportError::Line {
            line,
            problem: e.to_string(),
        })?;
        match record {
            Record::Description { key, text } => {
                let text = clean_description(&text);
                if !text.is_empty() {
                    descriptions.insert(key.to_lowercase(), text);
                }
            }
            Record::Message(message) => messages.push((line, message)),
        }
    }

    // Who wrote each message, to tell a quote of the bot from a quote of a member.
    let mut authors: HashMap<(i64, String), bool> = HashMap::new();
    for (line, message) in &messages {
        if let Some(found) = id_text(&message.payload.self_id)
            && found != bot_text
        {
            return Err(ImportError::OtherBot {
                found,
                expected: bot.get(),
            });
        }
        let id = id_text(&message.payload.message_id).ok_or_else(|| ImportError::Line {
            line: *line,
            problem: "no message id".into(),
        })?;
        authors.insert((message.group_id, id), message.payload.author_kind == "bot");
    }

    let mut stats = ImportStats::default();
    let mut groups = HashSet::new();
    let mut lines = Vec::with_capacity(messages.len());
    for (line, message) in messages {
        let fail = |problem: String| ImportError::Line { line, problem };
        let payload = &message.payload;
        if payload.sub_type == "notice" {
            stats.notices_skipped += 1;
            continue;
        }
        let from_bot = match payload.author_kind.as_str() {
            "bot" => true,
            "member" => false,
            other => return Err(fail(format!("unknown author kind {other:?}"))),
        };
        let mut segments = payload.segments.clone();
        let quoted = payload.reply_to.as_ref().and_then(id_text);
        let has_quote = segments
            .iter()
            .any(|s| s.get("type").and_then(Value::as_str) == Some("reply"));
        if let Some(quoted) = &quoted
            && !has_quote
        {
            segments.insert(0, json!({ "type": "reply", "data": { "id": quoted } }));
            stats.quotes_restored += 1;
        }
        if !from_bot && payload.to_me && !segments.iter().any(|s| is_bot_mention(s, &bot_text)) {
            match quoted
                .as_ref()
                .map(|q| authors.get(&(message.group_id, q.clone())))
            {
                // Addressed by quoting the bot: nothing was taken out.
                Some(Some(true)) => {}
                Some(None) => stats.bot_mentions_unknown += 1,
                None | Some(Some(false)) => {
                    let at = usize::from(quoted.is_some());
                    segments.insert(at, json!({ "type": "at", "data": { "qq": bot_text } }));
                    stats.bot_mentions_restored += 1;
                }
            }
        }
        let frame = json!({
            "post_type": if from_bot { "message_sent" } else { "message" },
            "message_type": "group",
            "self_id": bot.get(),
            "group_id": message.group_id,
            "user_id": if from_bot { bot_text.clone() } else { message.user_id.clone() },
            "message_id": payload.message_id,
            "message": segments,
        });
        let mut parsed = match parse_frame(&frame.to_string()) {
            Frame::Message(parsed) => parsed,
            Frame::Ignored(why) => return Err(fail(format!("not a usable message: {why}"))),
            other => return Err(fail(format!("unexpected frame {other:?}"))),
        };
        parsed.at = UnixMillis::new(message.at_ms);
        let Some((mut incoming, job)) = archive_form(&parsed, ctx) else {
            stats.empty_skipped += 1;
            continue;
        };
        for item in job.items.iter().filter(|i| i.kind == Kind::Image) {
            stats.pictures += 1;
            let found = item
                .reference
                .file
                .as_deref()
                .and_then(|file| file.split('.').next())
                .and_then(|stem| descriptions.get(&stem.to_lowercase()));
            if let Some(text) = found
                && let Some(filled) = fill(
                    &incoming.text,
                    Kind::Image.marker(),
                    item.index,
                    &format!("[image:{text}]"),
                )
            {
                incoming.text = filled;
                stats.pictures_described += 1;
            }
        }
        stats.messages += 1;
        if from_bot {
            stats.bot_messages += 1;
        }
        groups.insert(incoming.group);
        lines.push(incoming);
    }
    stats.groups = groups.len();
    Ok(Prepared { lines, stats })
}

/// Refuse when a group already has lines this import does not bring: older history appended
/// after them would be out of order. Lines from an earlier run of the same import are fine.
fn check_target(existing: &[(GroupId, MessageId)], lines: &[Incoming]) -> Result<(), ImportError> {
    let incoming: HashSet<(GroupId, MessageId)> =
        lines.iter().map(|l| (l.group, l.message)).collect();
    let mut foreign: HashMap<GroupId, usize> = HashMap::new();
    for key in existing {
        if !incoming.contains(key) {
            *foreign.entry(key.0).or_default() += 1;
        }
    }
    match foreign.into_iter().min_by_key(|(group, _)| group.get()) {
        Some((group, count)) => Err(ImportError::Occupied {
            group: group.get(),
            count,
        }),
        None => Ok(()),
    }
}

/// Run the import against the configured database. `dry_run` reads and checks the export and
/// the database without writing.
pub async fn import_history(
    loaded: Loaded,
    env: std::sync::Arc<dyn Env>,
    export: &Path,
    dry_run: bool,
) -> Result<ImportStats, ImportError> {
    let config = loaded.config;
    let paths = config.paths(&loaded.layout);
    prepare_directories(&paths)?;
    let resolver = SecretResolver::new(env, loaded.layout.secrets_dir.clone());
    let secrets = config.ready_to_run(&resolver)?;
    let Some(bot) = config.bot_account() else {
        return Err(ImportError::Config(ConfigErrors(Vec::new())));
    };
    let ctx = RenderContext {
        bot,
        forward_max_lines: qbot_gateway::render::FORWARD_MAX_LINES,
    };
    let text = std::fs::read_to_string(export)?;
    let Prepared { lines, mut stats } = prepare(&text, bot, &ctx)?;

    let url = config.database_url(secrets.database_password.expose());
    let store = Store::connect(&url).await?;
    store.migrate().await?;
    // The bot must not be running: its live lines would interleave with the history.
    let lease = RuntimeLease::acquire(&url, DEFAULT_KEY).await?;
    let clock = std::sync::Arc::new(qbot_core::SystemClock);
    let archive = PgArchive::new(store.pool().clone(), clock);
    check_target(&archive.message_ids().await?, &lines)?;
    if !dry_run {
        for (n, line) in lines.into_iter().enumerate() {
            let speaker = line.author.map_or(NewSpeaker::Bot, NewSpeaker::Member);
            let appended = archive
                .append_line(
                    NewLine {
                        group: line.group,
                        message: line.message,
                        speaker,
                        at: line.at,
                        text: line.text,
                    },
                    &line.mentions,
                    &line.media,
                )
                .await?;
            match appended {
                Appended::Stored { .. } => stats.stored += 1,
                Appended::Duplicate => stats.already_there += 1,
            }
            if (n + 1) % 5000 == 0 {
                tracing::info!(written = n + 1, "importing");
            }
        }
    }
    drop(lease);
    Ok(stats)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    const BOT: i64 = 900;

    fn ctx() -> RenderContext {
        RenderContext {
            bot: AccountId::new(BOT).unwrap(),
            forward_max_lines: 5,
        }
    }

    fn message(id: i64, author: &str, user: i64, extra: Value) -> String {
        let mut payload = json!({
            "message_id": id.to_string(),
            "sub_type": "normal",
            "segments": [{ "type": "text", "data": { "text": format!("line {id}") } }],
            "to_me": false,
            "author_kind": author,
            "self_id": BOT.to_string(),
        });
        if let (Some(p), Some(e)) = (payload.as_object_mut(), extra.as_object()) {
            for (k, v) in e {
                p.insert(k.clone(), v.clone());
            }
        }
        json!({ "message": {
            "group_id": 4242, "user_id": user.to_string(), "at_ms": 1_000 * id, "payload": payload
        }})
        .to_string()
    }

    fn run(lines: &[String]) -> Prepared {
        prepare(&lines.join("\n"), AccountId::new(BOT).unwrap(), &ctx()).unwrap()
    }

    #[test]
    fn quotes_come_back_and_a_stripped_bot_mention_only_where_the_export_proves_it() {
        let prepared = run(&[
            message(1, "bot", BOT, json!({})),
            message(2, "member", 11, json!({})),
            // Quotes the bot: addressed by the quote, no mention was taken out.
            message(3, "member", 11, json!({ "reply_to": "1", "to_me": true })),
            // Quotes a member and is to_me: the mention was taken out.
            message(4, "member", 12, json!({ "reply_to": "2", "to_me": true })),
            // No quote and to_me.
            message(5, "member", 12, json!({ "to_me": true })),
            // Quotes something not exported: undecidable.
            message(6, "member", 12, json!({ "reply_to": "77", "to_me": true })),
            // Already carries the mention (restored by the Python bot itself).
            message(
                7,
                "member",
                12,
                json!({ "to_me": true, "segments": [
                    { "type": "at", "data": { "qq": BOT.to_string(), "name": "bot" } },
                    { "type": "text", "data": { "text": "hi" } }
                ] }),
            ),
        ]);
        let texts: Vec<&str> = prepared.lines.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(
            texts,
            [
                "line 1",
                "line 2",
                "[reply:1]line 3",
                "[reply:2][at:bot]line 4",
                "[at:bot]line 5",
                "[reply:77]line 6",
                "[at:bot]hi",
            ]
        );
        assert_eq!(prepared.lines[0].author, None, "the bot's own line");
        assert_eq!(prepared.stats.quotes_restored, 3);
        assert_eq!(prepared.stats.bot_mentions_restored, 2);
        assert_eq!(prepared.stats.bot_mentions_unknown, 1);
        assert_eq!(prepared.lines[4].at, UnixMillis::new(5_000));
    }

    #[test]
    fn descriptions_fill_pictures_by_file_hash_and_notices_are_skipped() {
        let picture =
            |file: &str| json!({ "type": "image", "data": { "file": file, "url": "https://x/y" } });
        let prepared = run(&[
            json!({ "description": { "key": "abcdef0123", "text": "a cat [asleep]\non a mat" } })
                .to_string(),
            message(
                1,
                "member",
                11,
                json!({ "segments": [picture("ABCDEF0123.jpg"), picture("FFFF.png")] }),
            ),
            message(2, "member", 11, json!({ "sub_type": "notice" })),
        ]);
        assert_eq!(prepared.lines.len(), 1);
        assert_eq!(
            prepared.lines[0].text,
            "[image:a cat \u{FF3B}asleep\u{FF3D} on a mat][image]"
        );
        assert_eq!(prepared.lines[0].media.len(), 2, "references kept for both");
        assert_eq!(prepared.stats.pictures, 2);
        assert_eq!(prepared.stats.pictures_described, 1);
        assert_eq!(prepared.stats.notices_skipped, 1);
    }

    #[test]
    fn an_export_from_another_bot_account_or_a_broken_line_stops_everything() {
        let other = message(1, "member", 11, json!({ "self_id": "901" }));
        let error = prepare(&other, AccountId::new(BOT).unwrap(), &ctx()).unwrap_err();
        assert!(matches!(error, ImportError::OtherBot { .. }), "{error}");
        let error = prepare("{\"message\": {}}", AccountId::new(BOT).unwrap(), &ctx()).unwrap_err();
        assert!(
            matches!(error, ImportError::Line { line: 1, .. }),
            "{error}"
        );
    }

    #[test]
    fn a_group_with_lines_the_export_does_not_have_is_refused() {
        let prepared = run(&[message(1, "member", 11, json!({}))]);
        let group = GroupId::new(4242).unwrap();
        let mine = (group, MessageId::new(1).unwrap());
        let live = (group, MessageId::new(99).unwrap());
        assert!(check_target(&[mine], &prepared.lines).is_ok(), "a rerun");
        assert!(matches!(
            check_target(&[mine, live], &prepared.lines),
            Err(ImportError::Occupied {
                group: 4242,
                count: 1
            })
        ));
    }
}
