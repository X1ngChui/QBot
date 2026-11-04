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
        // What the episode showed can be quoted and its authors mentioned.
        for line in &lines {
            let author = line
                .speaker
                .zip(line.member_no)
                .map(|(account, n)| (MemberNo::new(n), account));
            cx.state.note_shown(line.message, author);
        }
        let mut out = vec![describe(cx, &episode)];
        out.extend(lines.iter().map(|line| render(cx, line)));
        Ok(ToolOutput::text(out.join("\n")))
    }
}

/// What is kept about one member of a group, as the model reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberRecord {
    pub account: qbot_core::AccountId,
    pub text: String,
}

/// Where `about_member` reads from: the names, notes and memories kept about a member.
#[async_trait]
pub trait MemberRecords: Send + Sync {
    /// Everything kept about member `member` of `group`, rendered; `None` when the group has no
    /// such member.
    async fn describe(
        &self,
        group: qbot_core::GroupId,
        member: MemberNo,
    ) -> Result<Option<MemberRecord>, String>;
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AboutMemberArgs {
    pub member: u32,
}

#[derive(Clone)]
pub struct AboutMember(pub Arc<dyn MemberRecords>);

impl std::fmt::Debug for AboutMember {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AboutMember").finish_non_exhaustive()
    }
}

#[async_trait]
impl Tool for AboutMember {
    type Args = AboutMemberArgs;
    const NAME: &'static str = "about_member";

    fn description(&self) -> String {
        say(Text::AboutMemberDescription {})
    }

    fn parameters(&self) -> Vec<(&'static str, String)> {
        vec![("member", say(Text::AboutMemberParamMember {}))]
    }

    fn effect(&self) -> Effect {
        Effect::Read
    }

    async fn call(&self, cx: &ToolCx<'_>, args: AboutMemberArgs) -> Result<ToolOutput, ToolError> {
        let number = MemberNo::new(args.member);
        let record = self
            .0
            .describe(cx.group, number)
            .await
            .map_err(ToolError::Unavailable)?
            .ok_or_else(|| {
                ToolError::refused(
                    RefusalReason::NotAllowed,
                    say(Text::AboutMemberNoSuch {
                        member: args.member.to_string(),
                    }),
                )
            })?;
        // The model was told about them, so it may mention them.
        cx.state.note_member(number, record.account);
        Ok(ToolOutput::text(record.text))
    }
}

/// Add the memory tools to a tool set; `about_member` when there are member records to read.
pub fn add_memory_tools(
    set: ToolSet,
    recall: Arc<Recall>,
    store: Arc<dyn EpisodeStore>,
    members: Option<Arc<dyn MemberRecords>>,
) -> Result<ToolSet, ToolSetError> {
    let set = set.with(RecallEpisodes(recall))?.with(ReadEpisode(store))?;
    match members {
        Some(records) => set.with(AboutMember(records)),
        None => Ok(set),
    }
}
