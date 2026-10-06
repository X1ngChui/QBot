//! Turning a platform message into the text that is archived and shown to the model.
//!
//! Markers are ASCII and bracketed: `[at:123]`, `[reply:456]`, `[image]`, `[face:14]`. A member
//! cannot forge one, because brackets typed by members are replaced with their fullwidth forms
//! before they reach the archive. The prompt layer later maps `[at:ID]` to member numbers.

use qbot_core::{AccountId, GameResult};

use crate::wire::{Mention, Segment};

pub use qbot_core::marker::neutralize;

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

/// Messages of a forwarded record shown in its line; the rest are counted. A record can hold
/// hundreds of messages, and every prompt that shows the line would carry them all.
pub const FORWARD_MAX_LINES: usize = 30;

/// What rendering needs besides the segments.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RenderContext {
    pub bot: AccountId,
    /// Messages of a forwarded record shown in the line; the rest are counted. A record can hold
    /// hundreds of messages, and all of them would enter every prompt that shows the line.
    pub forward_max_lines: usize,
}

/// Archive text for a message. A mention of the bot is `[at:bot]`; a mention of anyone else is
/// written `[at:ACCOUNT]` and rewritten to the member number by the archive (see
/// [`mentioned`]). A forwarded record is shown under its marker, one indented line per message.
pub fn render(segments: &[Segment], ctx: &RenderContext) -> String {
    let mut out = String::new();
    render_into(&mut out, segments, ctx, false);
    out.trim().to_owned()
}

fn sender_name(raw: &str) -> String {
    let name = neutralize(&raw.replace(['\n', '\r'], " "));
    let name = name.trim();
    if name.is_empty() {
        "someone".to_owned()
    } else {
        name.to_owned()
    }
}

/// `nested` is true inside a forwarded record: mentions there are not addressed to anyone here
/// (so no account numbers), quotes point at messages that are not on screen, and a record inside
/// a record is not expanded.
fn render_into(out: &mut String, segments: &[Segment], ctx: &RenderContext, nested: bool) {
    for segment in segments {
        match segment {
            Segment::Text(text) | Segment::Markdown(text) => out.push_str(&neutralize(text)),
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
            Segment::File(name) => out.push_str(&format!("[file:{}]", label(name))),
            Segment::Face(id) => out.push_str(&format!("[face:{id}]")),
            Segment::Sticker { summary, .. } => {
                // The client's summary stands in until the sticker is described.
                let summary = neutralize(summary.trim());
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
            Segment::Forward { nodes: None } => out.push_str("[forward]"),
            Segment::Forward { nodes: Some(nodes) } if nested => {
                out.push_str(&format!("[forward:{}]", nodes.len()))
            }
            Segment::Forward { nodes: Some(nodes) } => {
                out.push_str(&format!("[forward:{}]", nodes.len()));
                for node in nodes.iter().take(ctx.forward_max_lines) {
                    let mut line = String::new();
                    render_into(&mut line, &node.segments, ctx, true);
                    let line = line.replace(['\n', '\r'], " ");
                    out.push_str(&format!(
                        "\n  | {}: {}",
                        sender_name(&node.sender),
                        line.trim()
                    ));
                }
                let hidden = nodes.len().saturating_sub(ctx.forward_max_lines);
                if hidden > 0 {
                    out.push_str(&format!("\n  | [forward_more:{hidden}]"));
                }
            }
            Segment::Card => out.push_str("[card]"),
            Segment::Other(kind) => out.push_str(&format!("[unsupported:{}]", label(kind))),
        }
    }
}

/// What the sender typed: top-level text segments only, each trimmed, joined by one space.
/// Mentions, images and quotes contribute nothing. This is the text commands and nickname
/// matching look at.
pub fn typed_text(segments: &[Segment]) -> String {
    segments
        .iter()
        .filter_map(|s| {
            if let Segment::Text(t) = s {
                Some(neutralize(t))
            } else {
                None
            }
        })
        .map(|t| t.trim().to_owned())
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Accounts mentioned other than the bot, in order, without repeats.
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
