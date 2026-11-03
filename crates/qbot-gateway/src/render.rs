//! Turning a platform message into the text that is archived and shown to the model.
//!
//! Markers are ASCII and bracketed: `[at:123]`, `[reply:456]`, `[image]`, `[face:14]`. A member
//! cannot forge one, because brackets typed by members are replaced with their fullwidth forms
//! before they reach the archive. The archive rewrites `[at:ACCOUNT]` to the member number as it
//! stores the line.

use std::collections::{HashMap, VecDeque};

use qbot_core::{AccountId, GameResult};

use crate::card::marker;
use crate::wire::{Card, ForwardNode, Mention, Segment};

use qbot_core::marker::{escape_markers, marker_value};

/// A platform-supplied label (a file name, a segment type) reduced to a marker-safe token.
fn label(text: &str) -> String {
    let cleaned: String = text
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        .take(48)
        .collect();
    if cleaned.is_empty() {
        "unnamed".to_owned()
    } else {
        cleaned
    }
}

/// Messages of forwarded records shown in one line, the records inside them included; the rest
/// are counted. A record can hold hundreds of messages, and every prompt that shows the line
/// would carry them all.
pub const FORWARD_MAX_LINES: usize = 40;

/// How deep records inside records are shown: the forwarded record itself is depth 1. A record
/// deeper than this is shown only as its marker.
pub const FORWARD_MAX_DEPTH: usize = 3;

/// How many records inside forwarded records one line shows (the most seen in one real line is
/// eight); each further one is shown only as its marker.
pub const FORWARD_MAX_NESTED: usize = 8;

/// What rendering needs besides the segments.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RenderContext {
    pub bot: AccountId,
    /// Messages of forwarded records shown in the line, every level together; the rest are
    /// counted. A record can hold hundreds of messages, and all of them would enter every prompt
    /// that shows the line.
    pub forward_max_lines: usize,
}

/// Archive text for a message. A mention of the bot is `[at:bot]`; a mention of anyone else is
/// written `[at:ACCOUNT]` and rewritten to the member number by the archive (see
/// [`mentioned`]). A forwarded record is shown under its marker, one indented line per message;
/// a record inside it is numbered where it appears (`[forward:N #K]`) and shown after it under
/// its own heading (`[forward:#K depth D]`), breadth first ([`Forwards`]).
pub fn render(segments: &[Segment], ctx: &RenderContext) -> String {
    let mut out = String::new();
    let mut forwards = Forwards::new(ctx.forward_max_lines);
    render_into(&mut out, segments, ctx, false, &mut forwards);
    out.trim().to_owned()
}

/// One forwarded record as a line shows it: its number (0 for a record the message itself
/// forwarded, else its `#K`), its depth, how many messages it has, and the ones shown.
pub(crate) struct Shown<'a> {
    pub number: usize,
    pub depth: usize,
    pub total: usize,
    pub nodes: &'a [ForwardNode],
}

/// The forwarded records of one message, walked breadth first: a record's shown messages, then
/// the records found in them, level by level, so a record always comes after the line that
/// names it. One budget of lines covers every level ([`FORWARD_MAX_LINES`]); depth and the
/// number of records inside records are bounded ([`FORWARD_MAX_DEPTH`],
/// [`FORWARD_MAX_NESTED`]), so the walk ends however the record is built. A record the platform
/// gives the same id as one already numbered is the same record: it is named by that number
/// and not shown twice. Rendering and the media walk share the plan, so the k-th picture marker
/// of the line is the k-th picture they find.
pub(crate) struct Forwards {
    budget: usize,
    nested: usize,
    by_id: HashMap<String, usize>,
    /// The number given to a record inside a record, by the address of its segment.
    numbers: HashMap<usize, usize>,
}

impl Forwards {
    pub(crate) fn new(budget: usize) -> Self {
        Self {
            budget,
            nested: 0,
            by_id: HashMap::new(),
            numbers: HashMap::new(),
        }
    }

    /// The records to show for a record the message forwarded, in order.
    pub(crate) fn plan<'a>(&mut self, top: &'a [ForwardNode]) -> Vec<Shown<'a>> {
        let mut out = Vec::new();
        let mut queue = VecDeque::from([(0, 1, top)]);
        while let Some((number, depth, nodes)) = queue.pop_front() {
            let shown = nodes.len().min(self.budget);
            self.budget -= shown;
            for node in &nodes[..shown] {
                for segment in &node.segments {
                    let Segment::Forward {
                        id,
                        nodes: Some(inner),
                    } = segment
                    else {
                        continue;
                    };
                    if let Some(known) = id.as_ref().and_then(|id| self.by_id.get(id)) {
                        self.numbers.insert(address(segment), *known);
                        continue;
                    }
                    // No lines left to show it: it stays a marker and leaves its number to none.
                    if depth >= FORWARD_MAX_DEPTH
                        || self.nested >= FORWARD_MAX_NESTED
                        || self.budget == 0
                    {
                        continue;
                    }
                    self.nested += 1;
                    let k = self.nested;
                    if let Some(id) = id {
                        self.by_id.insert(id.clone(), k);
                    }
                    self.numbers.insert(address(segment), k);
                    queue.push_back((k, depth + 1, inner.as_slice()));
                }
            }
            out.push(Shown {
                number,
                depth,
                total: nodes.len(),
                nodes: &nodes[..shown],
            });
        }
        out
    }

    /// The number of a record inside a record, if it is shown.
    fn number(&self, segment: &Segment) -> Option<usize> {
        self.numbers.get(&address(segment)).copied()
    }
}

/// A segment's identity within one parsed message, which outlives the walk.
fn address(segment: &Segment) -> usize {
    std::ptr::from_ref(segment) as usize
}

fn sender_name(raw: &str) -> String {
    let name = escape_markers(&raw.replace(['\n', '\r'], " "));
    let name = name.trim();
    if name.is_empty() {
        "someone".to_owned()
    } else {
        name.to_owned()
    }
}

/// `nested` is true inside a forwarded record: mentions there are not addressed to anyone here
/// (so no account numbers), quotes point at messages that are not on screen, and a record inside
/// a record is named by its number, shown later by the record walk (`forwards`).
fn render_into(
    out: &mut String,
    segments: &[Segment],
    ctx: &RenderContext,
    nested: bool,
    forwards: &mut Forwards,
) {
    // Consecutive text is escaped as one run: two pieces must not join into a marker. A
    // forwarded message is shown on one line, so its line breaks go before escaping: escaping
    // first and joining after could make `[at` and `:all]` from two lines one marker.
    let escape = |text: String| {
        if nested {
            escape_markers(&text.replace(['\n', '\r'], " "))
        } else {
            escape_markers(&no_record_lines(&text))
        }
    };
    let mut typed = String::new();
    for segment in segments {
        if let Segment::Text(text) | Segment::Markdown(text) = segment {
            typed.push_str(text);
            continue;
        }
        out.push_str(&escape(std::mem::take(&mut typed)));
        match segment {
            Segment::Text(_) | Segment::Markdown(_) => {}
            Segment::At(Mention::All) => out.push_str("[at:all]"),
            Segment::At(Mention::Account(_)) if nested => out.push_str("@someone"),
            Segment::At(Mention::Account(account)) if *account == ctx.bot => {
                out.push_str("[at:bot]")
            }
            Segment::At(Mention::Account(account)) => {
                out.push_str(&format!("[at:{}]", account.get()))
            }
            Segment::Reply(_) if nested => {}
            Segment::Reply(message) => out.push_str(&format!("[reply:{}]", message.get())),
            Segment::Image { .. } => out.push_str("[image]"),
            Segment::Voice { .. } => out.push_str("[voice]"),
            Segment::Video => out.push_str("[video]"),
            Segment::File { name, size, folder } => {
                let kind = if *folder { "folder" } else { "file" };
                let name = crate::card::line(name)
                    .map(|n| marker_value(&n))
                    .unwrap_or_else(|| "unnamed".to_owned());
                match size.filter(|_| !*folder) {
                    Some(bytes) => {
                        out.push_str(&format!("[{kind}:{name} ({})]", bytes_label(bytes)))
                    }
                    None => out.push_str(&format!("[{kind}:{name}]")),
                }
            }
            Segment::FileTransfer => out.push_str("[file transfer]"),
            Segment::Face {
                id,
                name: Some(name),
            } => out.push_str(&format!("[face:{id}:{}]", marker_value(name))),
            Segment::Face { id, name: None } => out.push_str(&format!("[face:{id}]")),
            Segment::Poke => out.push_str("[poke]"),
            Segment::Sticker { summary, .. } => {
                // The client's summary stands in until the sticker is described.
                let summary = marker_value(summary.trim());
                if summary.is_empty() {
                    out.push_str("[sticker]");
                } else {
                    out.push_str(&format!("[sticker:{summary}]"));
                }
            }
            // The platform's result, normalized; without a readable one only the action is known.
            Segment::Dice(result) => out.push_str(
                &result
                    .as_deref()
                    .and_then(GameResult::dice)
                    .map_or_else(|| "[dice]".to_owned(), GameResult::marker),
            ),
            Segment::Rps(result) => out.push_str(
                &result
                    .as_deref()
                    .and_then(GameResult::rps)
                    .map_or_else(|| "[rps]".to_owned(), GameResult::marker),
            ),
            Segment::Forward { nodes: None, .. } => out.push_str("[forward]"),
            // A record inside a record: its number if it is shown further down.
            Segment::Forward {
                nodes: Some(nodes), ..
            } if nested => match forwards.number(segment) {
                Some(k) => out.push_str(&format!("[forward:{} #{k}]", nodes.len())),
                None => out.push_str(&format!("[forward:{}]", nodes.len())),
            },
            Segment::Forward {
                nodes: Some(nodes), ..
            } => {
                out.push_str(&format!("[forward:{}]", nodes.len()));
                for record in forwards.plan(nodes) {
                    if record.number > 0 {
                        out.push_str(&format!(
                            "\n[forward:#{} depth {}]",
                            record.number, record.depth
                        ));
                    }
                    for node in record.nodes {
                        let mut line = String::new();
                        render_into(&mut line, &node.segments, ctx, true, forwards);
                        let line = line.replace(['\n', '\r'], " ");
                        let line = line.trim();
                        let gap = if line.is_empty() { "" } else { " " };
                        out.push_str(&format!("\n  | {}:{gap}{line}", sender_name(&node.sender)));
                    }
                    let hidden = record.total - record.nodes.len();
                    if hidden > 0 {
                        out.push_str(&format!("\n  | [forward_more:{hidden}]"));
                    }
                }
                // What the message says after the record is not part of the record's last line.
                out.push('\n');
            }
            Segment::Card(card) => out.push_str(&card_marker(card)),
            Segment::Other(kind) => out.push_str(&format!("[unsupported:{}]", label(kind))),
        }
    }
    out.push_str(&escape(typed));
}

/// A card as its marker. Every value is outside text inside a marker.
fn card_marker(card: &Card) -> String {
    let safe = |value: &Option<String>| value.as_deref().map(marker_value);
    let one = |kind: &str, value: &Option<String>| match safe(value) {
        Some(value) => format!("[{kind}:{value}]"),
        None => format!("[{kind}]"),
    };
    match card {
        Card::Contact { name } => one("contact card", name),
        Card::Group { name } => one("group card", name),
        Card::Other { prompt } => one("card", prompt),
        Card::Location { name, address } => marker(
            "location",
            &[("name", &safe(name)), ("address", &safe(address))],
        ),
        Card::Music { title, artist, url } => marker(
            "music",
            &[
                ("title", &safe(title)),
                ("artist", &safe(artist)),
                ("url", &safe(url)),
            ],
        ),
        Card::Link {
            title,
            text,
            source,
            url,
        } => marker(
            "link",
            &[
                ("title", &safe(title)),
                ("text", &safe(text)),
                ("source", &safe(source)),
                ("url", &safe(url)),
            ],
        ),
    }
}

/// A size as people read it: bytes, KB, MB or GB with one decimal past bytes.
fn bytes_label(bytes: u64) -> String {
    const UNITS: [&str; 3] = ["KB", "MB", "GB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

/// What the sender typed, as typed: top-level text segments only, each trimmed, joined by one
/// space. Mentions, images and quotes contribute nothing. This is the text commands and nickname
/// matching look at; it never stands among markers, so only NUL (which the database rejects) is
/// dropped.
pub fn typed_text(segments: &[Segment]) -> String {
    segments
        .iter()
        .filter_map(|s| {
            if let Segment::Text(t) = s {
                Some(t.replace('\0', ""))
            } else {
                None
            }
        })
        .map(|t| t.trim().to_owned())
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Accounts mentioned other than the bot, in order, without repeats: the archive gives each a
/// member number in place of the account. Nothing else a message carries (a contact or group
/// card) names an account or group the bot keeps.
pub fn mentioned(segments: &[Segment], bot: AccountId) -> Vec<AccountId> {
    let mut seen = Vec::new();
    for segment in segments {
        if let Segment::At(Mention::Account(account)) = segment
            && *account != bot
            && !seen.contains(account)
        {
            seen.push(*account);
        }
    }
    seen
}

/// Typed text kept off the form of a forwarded record's lines (`  | name: text`): a line that
/// starts with indentation and `|` gets the fullwidth bar, which reads the same, so what a member
/// types never passes for a message of a record. A `|` anywhere else, a table at the start of a
/// line included, stays as typed.
fn no_record_lines(text: &str) -> String {
    text.split('\n')
        .map(|line| {
            let indent = line.len() - line.trim_start_matches([' ', '\t']).len();
            if indent > 0 && line[indent..].starts_with('|') {
                format!("{}\u{FF5C}{}", &line[..indent], &line[indent + 1..])
            } else {
                line.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}
