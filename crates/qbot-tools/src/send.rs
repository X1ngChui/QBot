//! `send_message`: the only way the model speaks. The model writes the same markers it reads in
//! the chat; this module turns them into message segments, strictly, and explains any mistake.

use std::sync::Arc;

use async_trait::async_trait;
use qbot_agent::{
    Delivery, DeliveryError, Effect, OutSegment, Tool, ToolCx, ToolError, ToolOutput,
};
use qbot_context::RefusalReason;
use qbot_core::{MemberNo, MessageId};
use qbot_wording::{Text, say};
use schemars::JsonSchema;
use serde::Deserialize;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SendArgs {
    // The message text with chat markers.
    pub text: String,
    // Whether the run ends after this message (default true).
    pub end_turn: Option<bool>,
}

#[derive(Clone)]
pub struct SendMessage {
    delivery: Arc<dyn Delivery>,
    /// Messages one run may send. A product rule against flooding the group.
    max_sends: u32,
}

impl std::fmt::Debug for SendMessage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SendMessage")
            .field("max_sends", &self.max_sends)
            .finish_non_exhaustive()
    }
}

impl SendMessage {
    pub fn new(delivery: Arc<dyn Delivery>, max_sends: u32) -> Self {
        Self {
            delivery,
            max_sends,
        }
    }
}

fn refused(message: impl Into<String>) -> ToolError {
    ToolError::refused(RefusalReason::NotAllowed, message)
}

/// What a marker the model wrote stands for.
enum Marker {
    At(u32),
    Reply(i64),
    Face(u32),
    Contact(u32),
    Dice,
    Rps,
}

/// The marker in `[inner]`: `None` when it is not one of the send markers (ordinary bracketed
/// text), an error when it names one but is malformed.
fn marker(inner: &str) -> Option<Result<Marker, String>> {
    let (name, value) = match inner.split_once(':') {
        Some((name, value)) => (name, Some(value)),
        None => (inner, None),
    };
    let number = |needs: fn(String) -> Text| {
        value
            .and_then(|v| v.trim().parse::<u64>().ok())
            .ok_or_else(|| say(needs(name.to_owned())))
    };
    let too_large = || {
        say(Text::SendMessageTooLarge {
            name: name.to_owned(),
        })
    };
    let small = |n: u64| u32::try_from(n).map_err(|_| too_large());
    let member: fn(String) -> Text = |name| Text::SendMessageNeedsMember { name };
    Some(match (name, value) {
        ("at", Some("all")) => Err(say(Text::SendMessageAtAll {})),
        ("at", _) => number(member).and_then(small).map(Marker::At),
        ("contact", _) => number(member).and_then(small).map(Marker::Contact),
        // The chat shows a face with its name (`[face:14:smile]`); the name is only a label.
        ("face", Some(value)) => value
            .split(':')
            .next()
            .and_then(|id| id.trim().parse::<u64>().ok())
            .ok_or_else(|| {
                say(Text::SendMessageNeedsFace {
                    name: name.to_owned(),
                })
            })
            .and_then(small)
            .map(Marker::Face),
        ("face", None) => Err(say(Text::SendMessageNeedsFace {
            name: name.to_owned(),
        })),
        ("reply", _) => number(|name| Text::SendMessageNeedsMessage { name })
            .and_then(|n| i64::try_from(n).map_err(|_| too_large()))
            .map(Marker::Reply),
        ("dice", None) => Ok(Marker::Dice),
        ("rps", None) => Ok(Marker::Rps),
        // A game's result is the platform's to decide and only ever comes back in the echo; a
        // marker carrying one would otherwise go out as text that looks like a real result.
        ("dice" | "dice result", _) => Err(say(Text::SendMessageChosenResult {
            name: "dice".to_owned(),
        })),
        ("rps" | "rps result", _) => Err(say(Text::SendMessageChosenResult {
            name: "rps".to_owned(),
        })),
        _ => return None,
    })
}

/// A piece of the message the model wrote.
enum Piece {
    Text(String),
    Marker(Marker),
}

/// Split the text into text and markers, in order. Brackets that are not send markers stay text;
/// a malformed send marker is an error saying how to write it.
fn parse(text: &str) -> Result<Vec<Piece>, String> {
    let mut out: Vec<Piece> = Vec::new();
    let push_text = |out: &mut Vec<Piece>, text: &str| match out.last_mut() {
        Some(Piece::Text(t)) => t.push_str(text),
        _ if text.is_empty() => {}
        _ => out.push(Piece::Text(text.to_owned())),
    };
    let mut rest = text;
    while let Some(open) = rest.find('[') {
        let Some(close) = rest[open..].find(']').map(|c| open + c) else {
            break;
        };
        match marker(&rest[open + 1..close]) {
            Some(found) => {
                push_text(&mut out, &rest[..open]);
                out.push(Piece::Marker(found?));
                rest = &rest[close + 1..];
            }
            None => {
                push_text(&mut out, &rest[..=open]);
                rest = &rest[open + 1..];
            }
        }
    }
    push_text(&mut out, rest);
    Ok(out)
}

fn blank(piece: &Piece) -> bool {
    matches!(piece, Piece::Text(t) if t.trim().is_empty())
}

fn lower(cx: &ToolCx<'_>, text: &str) -> Result<Vec<OutSegment>, ToolError> {
    let invalid = ToolError::InvalidArguments;
    let pieces = parse(text).map_err(invalid)?;
    let member = |n: u32| {
        cx.view.account_of(MemberNo::new(n)).ok_or_else(|| {
            refused(say(Text::SendMessageNoMember {
                member: n.to_string(),
            }))
        })
    };
    let meaningful = pieces.iter().filter(|p| !blank(p)).count();
    if meaningful == 0 {
        return Err(invalid(say(Text::SendMessageEmpty {})));
    }
    let lone = pieces.iter().any(|p| {
        matches!(
            p,
            Piece::Marker(Marker::Dice | Marker::Rps | Marker::Contact(_))
        )
    });
    if lone && meaningful != 1 {
        return Err(invalid(say(Text::SendMessageAlone {})));
    }
    let mut out = Vec::with_capacity(pieces.len());
    for (i, piece) in pieces.iter().enumerate() {
        out.push(match piece {
            // Whitespace around a lone special message is not part of it.
            Piece::Text(_) if lone => continue,
            Piece::Text(text) => OutSegment::Text(text.clone()),
            Piece::Marker(Marker::At(n)) => OutSegment::At(member(*n)?),
            Piece::Marker(Marker::Face(id)) => OutSegment::Face(*id),
            Piece::Marker(Marker::Reply(id)) => {
                if !pieces[..i].iter().all(blank) {
                    return Err(invalid(say(Text::SendMessageReplyFirst {})));
                }
                let message = MessageId::new(*id).map_err(|_| {
                    invalid(say(Text::SendMessageNotMessageId { id: id.to_string() }))
                })?;
                if !cx.view.has_message(message) {
                    return Err(refused(say(Text::SendMessageNoMessage {
                        id: id.to_string(),
                    })));
                }
                OutSegment::Reply(message)
            }
            Piece::Marker(Marker::Dice) => OutSegment::Dice,
            Piece::Marker(Marker::Rps) => OutSegment::Rps,
            // Only members seen in this group's chat: the card can never point outside it.
            Piece::Marker(Marker::Contact(n)) => OutSegment::Contact(member(*n)?),
        });
    }
    Ok(out)
}

#[async_trait]
impl Tool for SendMessage {
    type Args = SendArgs;
    const NAME: &'static str = "send_message";

    fn description(&self) -> String {
        say(Text::SendMessageDescription {})
    }

    fn parameters(&self) -> Vec<(&'static str, String)> {
        vec![
            ("text", say(Text::SendMessageParamText {})),
            ("end_turn", say(Text::SendMessageParamEndTurn {})),
        ]
    }

    fn effect(&self) -> Effect {
        Effect::Write
    }

    async fn call(&self, cx: &ToolCx<'_>, args: SendArgs) -> Result<ToolOutput, ToolError> {
        let segments = lower(cx, &args.text)?;
        if !cx.state.try_claim_send(self.max_sends) {
            return Err(ToolError::refused(
                RefusalReason::LimitReached,
                say(Text::SendMessageLimit {
                    max: self.max_sends.to_string(),
                }),
            ));
        }
        let delivered =
            self.delivery
                .send(cx.group, segments)
                .await
                .map_err(|error| match error {
                    DeliveryError::Rejected(reason) => {
                        ToolError::Failed(say(Text::SendMessageRejected { reason }))
                    }
                    DeliveryError::EchoMissing => {
                        ToolError::Failed(say(Text::SendMessageUnconfirmed {}))
                    }
                    DeliveryError::Unavailable(message) => ToolError::Unavailable(message),
                })?;
        cx.state.note_observed(delivered.echo.message);
        Ok(ToolOutput::text(say(Text::SendMessageSent {
            id: delivered.echo.message.get().to_string(),
            text: delivered.echo.text,
        }))
        .ends_run(args.end_turn.unwrap_or(true)))
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct StaySilentArgs {}

/// Ends the run without saying anything: silence as a deliberate choice.
#[derive(Debug, Clone, Copy)]
pub struct StaySilent;

#[async_trait]
impl Tool for StaySilent {
    type Args = StaySilentArgs;
    const NAME: &'static str = "stay_silent";

    fn description(&self) -> String {
        say(Text::StaySilentDescription {})
    }

    fn effect(&self) -> Effect {
        Effect::Read
    }

    async fn call(&self, _cx: &ToolCx<'_>, _args: StaySilentArgs) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput::text(say(Text::StaySilentResult {})).ends_run(true))
    }
}
