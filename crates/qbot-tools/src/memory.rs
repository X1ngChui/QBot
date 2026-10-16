//! Tools over long-term memory: find episodes by meaning, then read one in full.

use std::sync::Arc;

use async_trait::async_trait;
use qbot_agent::{Effect, Tool, ToolCx, ToolError, ToolOutput, ToolSet, ToolSetError};
use qbot_context::{ChatBatch, ChatLine, RefusalReason, Speaker};
use qbot_core::MemberNo;
use qbot_memory::{Episode, EpisodeId, EpisodeStore, Recall, SliceLine};
use qbot_wording::{Text, say};
use schemars::JsonSchema;
use serde::Deserialize;

fn describe(cx: &ToolCx<'_>, episode: &Episode) -> String {
    let e = &episode.episode;
    say(Text::RecallEpisodesEpisode {
        id: episode.id.get().to_string(),
        start: cx.renderer.time(e.started),
        end: cx.renderer.time(e.ended),
        count: e.line_count.to_string(),
        title: e.title.clone(),
        summary: e.summary.clone(),
    })
}

/// A line of an episode as the chat shows it. A line whose author has no member number (an
/// account the archive could not number) keeps the account.
fn render(cx: &ToolCx<'_>, line: &SliceLine) -> String {
    let speaker = match (line.speaker, line.member_no) {
        (None, _) => Some(Speaker::Bot),
        (Some(account), Some(n)) => Some(Speaker::Member {
            account,
            number: MemberNo::new(n),
        }),
        (Some(_), None) => None,
    };
    match speaker {
        Some(speaker) => cx.renderer.chat(&ChatBatch::new(vec![ChatLine {
            message: line.message,
            speaker,
            at: line.at,
            text: line.text.clone(),
        }])),
        None => format!(
            "[msg:{}] {} account:{}: {}",
            line.message.get(),
            cx.renderer.time(line.at),
            line.speaker.map_or(0, |a| a.get()),
            line.text
        ),
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RecallArgs {
    pub question: String,
}

#[derive(Clone)]
pub struct RecallEpisodes(pub Arc<Recall>);

impl std::fmt::Debug for RecallEpisodes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RecallEpisodes").finish_non_exhaustive()
    }
}

#[async_trait]
impl Tool for RecallEpisodes {
    type Args = RecallArgs;
    const NAME: &'static str = "recall_episodes";

    fn description(&self) -> String {
        say(Text::RecallEpisodesDescription {})
    }

    fn parameters(&self) -> Vec<(&'static str, String)> {
        vec![("question", say(Text::RecallEpisodesParamQuestion {}))]
    }

    fn effect(&self) -> Effect {
        Effect::Read
    }

    async fn call(&self, cx: &ToolCx<'_>, args: RecallArgs) -> Result<ToolOutput, ToolError> {
        if args.question.trim().is_empty() {
            return Err(ToolError::InvalidArguments(say(
                Text::RecallEpisodesEmptyQuestion {},
            )));
        }
        let hits = self
            .0
            .recall(cx.group, args.question.trim())
            .await
            .map_err(|e| ToolError::Unavailable(e.to_string()))?;
        if hits.is_empty() {
            return Ok(ToolOutput::text(say(Text::RecallEpisodesNoMatches {})));
        }
        Ok(ToolOutput::text(
            hits.iter()
                .map(|h| describe(cx, &h.episode))
                .collect::<Vec<_>>()
                .join("\n"),
        ))
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReadEpisodeArgs {
    pub id: i64,
}

#[derive(Clone)]
pub struct ReadEpisode(pub Arc<dyn EpisodeStore>);

impl std::fmt::Debug for ReadEpisode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReadEpisode").finish_non_exhaustive()
    }
}

#[async_trait]
impl Tool for ReadEpisode {
    type Args = ReadEpisodeArgs;
    const NAME: &'static str = "read_episode";

    fn description(&self) -> String {
        say(Text::ReadEpisodeDescription {})
    }

    fn parameters(&self) -> Vec<(&'static str, String)> {
        vec![("id", say(Text::ReadEpisodeParamId {}))]
    }

    fn effect(&self) -> Effect {
        Effect::Read
    }

    async fn call(&self, cx: &ToolCx<'_>, args: ReadEpisodeArgs) -> Result<ToolOutput, ToolError> {
        let unavailable = |e: qbot_memory::MemoryError| ToolError::Unavailable(e.to_string());
        let episode = self
            .0
            .get(cx.group, EpisodeId::new(args.id))
            .await
            .map_err(unavailable)?
            .ok_or_else(|| {
                ToolError::refused(
                    RefusalReason::NotAllowed,
                    say(Text::ReadEpisodeNoSuch {
                        id: args.id.to_string(),
                    }),
                )
            })?;
        let e = &episode.episode;
        let lines = self
            .0
            .lines(cx.group, e.first_ordinal, e.last_ordinal)
            .await
            .map_err(unavailable)?;
        let mut out = vec![describe(cx, &episode)];
        out.extend(lines.iter().map(|line| render(cx, line)));
        Ok(ToolOutput::text(out.join("\n")))
    }
}

/// Add the memory tools to a tool set.
pub fn add_memory_tools(
    set: ToolSet,
    recall: Arc<Recall>,
    store: Arc<dyn EpisodeStore>,
) -> Result<ToolSet, ToolSetError> {
    set.with(RecallEpisodes(recall))?.with(ReadEpisode(store))
}
