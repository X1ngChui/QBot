use jiff::Timestamp;
use jiff::tz::TimeZone;
use qbot_context::{ChatBatch, ChatLine, ErrorKind, Outcome, RefusalReason, Speaker};
use qbot_core::UnixMillis;
use qbot_llm::Renderer;
use qbot_wording::{Text, say};

/// Chat lines and tool-outcome notes in the wording the reading guide describes.
#[derive(Debug, Clone)]
pub struct PromptRenderer {
    zone: TimeZone,
}

/// `MM-DD HH:MM` in `zone`.
pub fn clock_label(zone: &TimeZone, at: UnixMillis) -> String {
    Timestamp::from_millisecond(at.get())
        .map(|t| t.to_zoned(zone.clone()).strftime("%m-%d %H:%M").to_string())
        .unwrap_or_else(|_| "??-?? ??:??".to_owned())
}

impl PromptRenderer {
    pub fn new(zone: TimeZone) -> Self {
        Self { zone }
    }

    pub fn line(&self, line: &ChatLine) -> String {
        let who = match line.speaker {
            Speaker::Bot => "you".to_owned(),
            Speaker::Member { number, .. } => format!("member:{}", number.get()),
        };
        format!(
            "[msg:{}] {} {}: {}",
            line.message.get(),
            clock_label(&self.zone, line.at),
            who,
            line.text
        )
    }
}

impl Renderer for PromptRenderer {
    fn time(&self, at: UnixMillis) -> String {
        Timestamp::from_millisecond(at.get())
            .map(|t| {
                t.to_zoned(self.zone.clone())
                    .strftime("%Y-%m-%d %H:%M %:z")
                    .to_string()
            })
            .unwrap_or_else(|_| format!("{}ms", at.get()))
    }

    fn chat(&self, batch: &ChatBatch) -> String {
        batch
            .lines()
            .iter()
            .map(|l| self.line(l))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn summary(&self, text: &str) -> String {
        format!("[summary of earlier conversation]\n{text}")
    }

    fn outcome_note(&self, outcome: &Outcome) -> Option<String> {
        match outcome {
            Outcome::Ok => None,
            // Nothing ran: unlike a failure, the call can simply be corrected and made again.
            Outcome::Error(ErrorKind::InvalidArguments) => Some(say(Text::OutcomeNotRun {})),
            Outcome::Error(kind) => Some(say(Text::OutcomeFailed {
                why: say(match kind {
                    ErrorKind::InvalidArguments => Text::OutcomeInvalidArguments {},
                    ErrorKind::Execution => Text::OutcomeExecution {},
                    ErrorKind::Unavailable => Text::OutcomeUnavailable {},
                    ErrorKind::Timeout => Text::OutcomeTimeout {},
                }),
            })),
            Outcome::Refused(reason) => Some(say(Text::OutcomeRefused {
                why: say(match reason {
                    RefusalReason::LimitReached => Text::OutcomeLimitReached {},
                    RefusalReason::NotAllowed => Text::OutcomeNotAllowed {},
                    RefusalReason::Conflict => Text::OutcomeConflict {},
                }),
            })),
            Outcome::Interrupted => Some(say(Text::OutcomeInterrupted {})),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn tool_times_are_local_with_their_offset() {
        let renderer = PromptRenderer::new(TimeZone::get("Asia/Shanghai").unwrap());
        // 2026-10-14T10:30:00Z
        assert_eq!(
            renderer.time(UnixMillis::new(1_791_973_800_000)),
            "2026-10-14 18:30 +08:00"
        );
    }
}
