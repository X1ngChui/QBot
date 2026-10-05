//! OneBot v11 frames, parsed into typed values.
//!
//! Parsing is tolerant of the platform's quirks (ids as numbers or numeric strings, segment
//! kinds we do not model) but never guesses: a frame that lacks something required is reported
//! as [`Frame::Ignored`] with the reason, so it can be logged rather than silently dropped.

use qbot_core::{AccountId, GroupId, MessageId, UnixMillis};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mention {
    All,
    Account(AccountId),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Segment {
    Text(String),
    At(Mention),
    Reply(MessageId),
    Image {
        /// The platform's file id for the picture.
        file: Option<String>,
        url: Option<String>,
        size: Option<u64>,
    },
    Voice {
        file: Option<String>,
        url: Option<String>,
    },
    Video,
    File(String),
    Face(u32),
    /// A marketplace sticker: the client's summary (often a placeholder), its image link and
    /// the stable sticker id.
    Sticker {
        summary: String,
        url: Option<String>,
        key: Option<String>,
    },
    /// A platform dice; the result is present only on echoes of sent messages.
    Dice(Option<String>),
    Rps(Option<String>),
    /// A forwarded chat record. The platform delivers its messages inline; `nodes` is `None`
    /// when it did not (the content is then unavailable).
    Forward {
        nodes: Option<Vec<ForwardNode>>,
    },
    Card,
    Markdown(String),
    Other(String),
}

/// One message of a forwarded record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForwardNode {
    /// The display name the record carries for the sender, as given (member-typed).
    pub sender: String,
    pub segments: Vec<Segment>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupMessage {
    pub self_id: AccountId,
    pub group: GroupId,
    pub message: MessageId,
    pub sender: AccountId,
    pub at: UnixMillis,
    /// The bot's own message reported back (`message_sent`, or a sender equal to the bot).
    pub from_bot: bool,
    pub segments: Vec<Segment>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeKind {
    /// A message was recalled; `by_admin` when someone other than its author did it.
    Recall {
        by_admin: bool,
    },
    Joined,
    Left {
        kicked: bool,
    },
    /// Mute duration in seconds; zero lifts a mute.
    Ban {
        seconds: u64,
    },
    Poke {
        target_is_bot: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupNotice {
    pub self_id: AccountId,
    pub group: GroupId,
    /// The member the notice is about.
    pub subject: AccountId,
    pub at: UnixMillis,
    pub kind: NoticeKind,
    /// Distinguishes notices of the same kind at the same second (a recalled message id, a poke
    /// target), so the synthetic archive id is stable under redelivery yet not shared.
    pub discriminator: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionResponse {
    pub echo: String,
    pub ok: bool,
    pub retcode: i64,
    pub message_id: Option<MessageId>,
    /// The action's `data` payload, `Null` when absent.
    pub data: Value,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    Message(GroupMessage),
    Notice(GroupNotice),
    Response(ActionResponse),
    /// A frame that is understood but not for us (heartbeats, private messages, ...), or that
    /// is malformed; the reason is for logs.
    Ignored(&'static str),
}

fn int(value: &Value) -> Option<i64> {
    match value {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn account(value: Option<&Value>) -> Option<AccountId> {
    AccountId::new(int(value?)?).ok()
}

fn text_of(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    }
}

fn nonempty(value: Option<&Value>) -> Option<String> {
    Some(text_of(value)).filter(|s| !s.is_empty())
}

fn millis(frame: &Value) -> UnixMillis {
    UnixMillis::new(
        frame
            .get("time")
            .and_then(int)
            .unwrap_or(0)
            .saturating_mul(1000),
    )
}

/// One message of an inline forwarded record. Its segments may be an array (`message` or
/// `content`) or, when the platform is set to string format, only `raw_message`.
fn forward_node(raw: &Value) -> ForwardNode {
    let sender = raw.get("sender");
    let name = [
        sender.and_then(|s| s.get("card")),
        sender.and_then(|s| s.get("nickname")),
        raw.get("nickname"),
    ]
    .into_iter()
    .map(text_of)
    .find(|n| !n.trim().is_empty())
    .unwrap_or_default();
    let segments = match raw.get("message").or_else(|| raw.get("content")) {
        Some(Value::Array(items)) => items.iter().map(segment).collect(),
        _ => match raw.get("raw_message") {
            Some(Value::String(text)) => vec![Segment::Text(text.clone())],
            _ => Vec::new(),
        },
    };
    ForwardNode {
        sender: name,
        segments,
    }
}

fn segment(raw: &Value) -> Segment {
    let kind = raw.get("type").and_then(Value::as_str).unwrap_or("");
    let data = raw.get("data").unwrap_or(&Value::Null);
    let field = |name: &str| data.get(name);
    match kind {
        "text" => Segment::Text(text_of(field("text"))),
        "at" => match field("qq") {
            Some(Value::String(s)) if s == "all" => Segment::At(Mention::All),
            other => account(other).map_or_else(
                || Segment::Other("at".into()),
                |a| Segment::At(Mention::Account(a)),
            ),
        },
        "reply" => field("id")
            .and_then(int)
            .and_then(|id| MessageId::new(id).ok())
            .map_or_else(|| Segment::Other("reply".into()), Segment::Reply),
        "image" => Segment::Image {
            file: nonempty(field("file")),
            url: nonempty(field("url")),
            size: field("file_size")
                .and_then(int)
                .and_then(|s| u64::try_from(s).ok()),
        },
        "record" => Segment::Voice {
            file: nonempty(field("file")),
            url: nonempty(field("url")),
        },
        "video" => Segment::Video,
        "file" => Segment::File(
            Some(text_of(field("name")))
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| text_of(field("file"))),
        ),
        "face" => field("id")
            .and_then(int)
            .and_then(|id| u32::try_from(id).ok())
            .map_or_else(|| Segment::Other("face".into()), Segment::Face),
        "mface" => Segment::Sticker {
            summary: text_of(field("summary")),
            url: nonempty(field("url")),
            key: nonempty(field("emoji_id")).or_else(|| nonempty(field("key"))),
        },
        "dice" => Segment::Dice(
            field("result")
                .map(|v| text_of(Some(v)))
                .filter(|s| !s.is_empty()),
        ),
        "rps" => Segment::Rps(
            field("result")
                .map(|v| text_of(Some(v)))
                .filter(|s| !s.is_empty()),
        ),
        "forward" | "node" => Segment::Forward {
            nodes: field("content")
                .and_then(Value::as_array)
                .map(|nodes| nodes.iter().map(forward_node).collect()),
        },
        "json" | "xml" => Segment::Card,
        "markdown" => Segment::Markdown(text_of(field("content").or_else(|| field("data")))),
        other => Segment::Other(other.to_owned()),
    }
}

fn segments(frame: &Value) -> Vec<Segment> {
    match frame.get("message") {
        Some(Value::Array(items)) => items.iter().map(segment).collect(),
        // A plain string message (array format not enabled) is one text segment.
        Some(Value::String(text)) => vec![Segment::Text(text.clone())],
        _ => Vec::new(),
    }
}

/// Parse one text frame from the platform.
pub fn parse_frame(raw: &str) -> Frame {
    let Ok(frame) = serde_json::from_str::<Value>(raw) else {
        return Frame::Ignored("not json");
    };
    if let Some(echo) = frame.get("echo").and_then(Value::as_str) {
        let retcode = frame.get("retcode").and_then(int).unwrap_or(-1);
        let ok = frame.get("status").and_then(Value::as_str) == Some("ok") && retcode == 0;
        let message_id = frame
            .get("data")
            .and_then(|d| d.get("message_id"))
            .and_then(int)
            .and_then(|id| MessageId::new(id).ok());
        let detail = ["wording", "message", "msg"]
            .iter()
            .map(|k| text_of(frame.get(*k)))
            .find(|s| !s.is_empty())
            .unwrap_or_default();
        return Frame::Response(ActionResponse {
            echo: echo.to_owned(),
            ok,
            retcode,
            message_id,
            data: frame.get("data").cloned().unwrap_or(Value::Null),
            detail,
        });
    }
    let Some(self_id) = account(frame.get("self_id")) else {
        return Frame::Ignored("no self_id");
    };
    match frame.get("post_type").and_then(Value::as_str) {
        Some(post @ ("message" | "message_sent")) => {
            if frame.get("message_type").and_then(Value::as_str) != Some("group") {
                return Frame::Ignored("not a group message");
            }
            let (Some(group), Some(message), Some(sender)) = (
                frame
                    .get("group_id")
                    .and_then(int)
                    .and_then(|g| GroupId::new(g).ok()),
                frame
                    .get("message_id")
                    .and_then(int)
                    .and_then(|m| MessageId::new(m).ok()),
                account(frame.get("user_id")),
            ) else {
                return Frame::Ignored("group message without usable ids");
            };
            Frame::Message(GroupMessage {
                self_id,
                group,
                message,
                sender,
                at: millis(&frame),
                from_bot: post == "message_sent" || sender == self_id,
                segments: segments(&frame),
            })
        }
        Some("notice") => parse_notice(&frame, self_id),
        _ => Frame::Ignored("not a message or notice"),
    }
}

fn parse_notice(frame: &Value, self_id: AccountId) -> Frame {
    let Some(group) = frame
        .get("group_id")
        .and_then(int)
        .and_then(|g| GroupId::new(g).ok())
    else {
        return Frame::Ignored("notice without a group");
    };
    let Some(subject) = account(frame.get("user_id")) else {
        return Frame::Ignored("notice without a subject");
    };
    let sub = frame.get("sub_type").and_then(Value::as_str).unwrap_or("");
    let (kind, discriminator) = match frame.get("notice_type").and_then(Value::as_str) {
        Some("group_recall") => {
            let operator = account(frame.get("operator_id"));
            (
                NoticeKind::Recall {
                    by_admin: operator.is_some_and(|o| o != subject),
                },
                text_of(frame.get("message_id")),
            )
        }
        Some("group_increase") => (NoticeKind::Joined, String::new()),
        Some("group_decrease") => (
            NoticeKind::Left {
                kicked: sub == "kick" || sub == "kick_me",
            },
            String::new(),
        ),
        Some("group_ban") => {
            let duration = frame.get("duration").and_then(int).unwrap_or(0);
            let seconds = if sub == "lift_ban" {
                0
            } else {
                u64::try_from(duration).unwrap_or(0)
            };
            (NoticeKind::Ban { seconds }, String::new())
        }
        Some("notify") if sub == "poke" => {
            let target = account(frame.get("target_id"));
            (
                NoticeKind::Poke {
                    target_is_bot: target == Some(self_id),
                },
                text_of(frame.get("target_id")),
            )
        }
        _ => return Frame::Ignored("unsupported notice"),
    };
    Frame::Notice(GroupNotice {
        self_id,
        group,
        subject,
        at: millis(frame),
        kind,
        discriminator,
    })
}
