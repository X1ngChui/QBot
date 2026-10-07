//! From one platform frame to its consequences: archive first, then command or run.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use qbot_agent::{Rejected, Supervisor, Trigger, TriggerRequest};
use qbot_core::{AccountId, GroupId, MessageId, UnixMillis};
use qbot_media::{MediaJob, MediaService};
use tokio_util::task::TaskTracker;

use qbot_store::MediaRefRow;

use crate::echo::EchoBoard;
use crate::intake::{Incoming, Intake, Stored};
use crate::render::RenderContext;
use crate::trigger::{self, Nicknames};
use crate::wire::{Frame, GroupMessage, GroupNotice, Mention, Segment};
use crate::{notice, render};

/// A recognised command, with everything a handler needs and nothing it must re-parse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandRequest {
    pub group: GroupId,
    pub message: MessageId,
    pub sender: AccountId,
    pub at: UnixMillis,
    /// The command word including its slash, exactly as typed.
    pub word: String,
    /// The typed text after the command word.
    pub args: String,
    /// Accounts @-mentioned in the message, in order, excluding the bot and "@all".
    pub mentions: Vec<AccountId>,
}

#[async_trait]
pub trait Commands: Send + Sync {
    /// Whether `word` (with its leading slash) names a command. Matching is exact.
    fn recognizes(&self, word: &str) -> bool;
    async fn run(&self, request: CommandRequest);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PipelineConfig {
    pub bot: AccountId,
    /// How long an unclaimed echo is kept; the delivery's echo timeout.
    pub echo_keep: Duration,
    /// Messages of a forwarded record shown in its line.
    pub forward_max_lines: usize,
}

impl PipelineConfig {
    /// The settings for `bot`: an echo is kept as long as a delivery waits for one.
    pub fn new(bot: AccountId) -> Self {
        Self {
            bot,
            echo_keep: crate::delivery::DeliveryTimeouts::default().echo,
            forward_max_lines: crate::render::FORWARD_MAX_LINES,
        }
    }
}

/// Told when a group's batch fills: older chat is about to leave the verbatim tier, so its
/// episode should be written.
#[async_trait]
pub trait BatchFilled: Send + Sync {
    async fn filled(&self, group: GroupId);
}

struct BatchHook {
    sink: Arc<dyn BatchFilled>,
    lines: u64,
}

struct MediaHook {
    service: Arc<MediaService>,
    /// How long a reply waits for the group's pictures and clips in flight to be described.
    wait: Duration,
}

pub struct Pipeline {
    media: Option<MediaHook>,
    batches: Option<BatchHook>,
    cfg: PipelineConfig,
    intake: Arc<dyn Intake>,
    commands: Arc<dyn Commands>,
    supervisor: Arc<Supervisor>,
    nicknames: Nicknames,
    echoes: Arc<EchoBoard>,
    tasks: TaskTracker,
}

impl std::fmt::Debug for Pipeline {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pipeline")
            .field("bot", &self.cfg.bot)
            .finish_non_exhaustive()
    }
}

impl Pipeline {
    pub fn new(
        cfg: PipelineConfig,
        intake: Arc<dyn Intake>,
        commands: Arc<dyn Commands>,
        supervisor: Arc<Supervisor>,
        nicknames: Nicknames,
        echoes: Arc<EchoBoard>,
    ) -> Self {
        Self {
            media: None,
            batches: None,
            cfg,
            intake,
            commands,
            supervisor,
            nicknames,
            echoes,
            tasks: TaskTracker::new(),
        }
    }

    /// Report every filled batch of `batch_lines` lines.
    pub fn with_batches(mut self, sink: Arc<dyn BatchFilled>, batch_lines: u32) -> Self {
        self.batches = Some(BatchHook {
            sink,
            lines: u64::from(batch_lines.max(1)),
        });
        self
    }

    /// Describe pictures and transcribe clips as messages arrive. A reply waits up to `wait` for
    /// the group's media in flight so the model sees words, not bare markers.
    pub fn with_media(mut self, service: Arc<MediaService>, wait: Duration) -> Self {
        self.media = Some(MediaHook { service, wait });
        self
    }

    /// Handle one parsed frame. Frames of one connection are handled in order; commands run
    /// concurrently so a slow one never delays the archive.
    pub async fn handle(&self, frame: Frame) {
        match frame {
            Frame::Message(message) => self.message(message).await,
            Frame::Notice(notice) => self.notice(notice).await,
            Frame::Response(_) | Frame::Ignored(_) => {}
        }
    }

    /// Stop accepting commands and wait for those in flight.
    pub async fn finish(&self) {
        self.tasks.close();
        self.tasks.wait().await;
    }

    async fn message(&self, m: GroupMessage) {
        if m.self_id != self.cfg.bot {
            tracing::warn!(
                self_id = m.self_id.get(),
                "message for a different bot account ignored"
            );
            return;
        }
        let ctx = RenderContext {
            bot: self.cfg.bot,
            forward_max_lines: self.cfg.forward_max_lines,
        };
        let Some((incoming, job)) = archive_form(&m, &ctx) else {
            return;
        };
        let line = match self.intake.append(incoming).await {
            Ok(Stored::New { line, ordinal }) => {
                if let Some(hook) = self.batches.as_ref().filter(|h| ordinal % h.lines == 0) {
                    let (sink, group) = (Arc::clone(&hook.sink), m.group);
                    self.tasks.spawn(async move { sink.filled(group).await });
                }
                line
            }
            Ok(Stored::Duplicate) => {
                tracing::debug!(
                    group = m.group.get(),
                    message = m.message.get(),
                    "already archived"
                );
                return;
            }
            Err(error) => {
                tracing::error!(%error, group = m.group.get(), "archiving failed; message dropped");
                return;
            }
        };
        if m.from_bot {
            self.echoes
                .publish(m.group, m.message, line, self.cfg.echo_keep);
            return;
        }

        if let Some(hook) = &self.media
            && !job.items.is_empty()
        {
            hook.service.admit(job);
        }

        let typed = render::typed_text(&m.segments);
        if let Some(request) = self.command(&m, &typed) {
            let commands = Arc::clone(&self.commands);
            self.tasks.spawn(async move { commands.run(request).await });
            return;
        }

        let at_bot = m
            .segments
            .iter()
            .any(|s| matches!(s, Segment::At(Mention::Account(a)) if *a == self.cfg.bot));
        let quoted = m.segments.iter().find_map(|s| {
            if let Segment::Reply(id) = s {
                Some(*id)
            } else {
                None
            }
        });
        let why = match trigger::decide(at_bot, &typed, &self.nicknames) {
            Some(why) => Some(why),
            None => match quoted {
                Some(id) if self.quotes_bot(m.group, id).await => Some(trigger::Why::QuotedBot),
                _ => None,
            },
        };
        let Some(why) = why else { return };
        tracing::debug!(group = m.group.get(), ?why, "addressed");
        let request = TriggerRequest {
            group: m.group,
            trigger: Trigger::Addressed {
                message: m.message,
                sender: m.sender,
            },
        };
        match &self.media {
            Some(hook) if hook.service.busy(m.group) => {
                // Pictures and clips of the group are still being worked on, perhaps the one
                // this message asks about: let them finish (bounded) before the run's window is
                // taken, off the reader so other frames keep flowing.
                let (supervisor, service, wait) = (
                    Arc::clone(&self.supervisor),
                    Arc::clone(&hook.service),
                    hook.wait,
                );
                let group = m.group;
                self.tasks.spawn(async move {
                    if !service.settle(group, wait).await {
                        tracing::info!(
                            group = group.get(),
                            "media still pending; replying with what is there"
                        );
                    }
                    submit(&supervisor, request).await;
                });
            }
            _ => submit(&self.supervisor, request).await,
        }
    }

    async fn quotes_bot(&self, group: GroupId, quoted: MessageId) -> bool {
        self.intake
            .is_bot_message(group, quoted)
            .await
            .unwrap_or_else(|error| {
                tracing::warn!(%error, "could not tell whether the quoted message is the bot's");
                false
            })
    }

    fn command(&self, m: &GroupMessage, typed: &str) -> Option<CommandRequest> {
        let word = typed.split_whitespace().next()?;
        if !word.starts_with('/') || !self.commands.recognizes(word) {
            return None;
        }
        let args = typed
            .trim_start()
            .strip_prefix(word)
            .unwrap_or("")
            .trim()
            .to_owned();
        let mentions = m
            .segments
            .iter()
            .filter_map(|s| match s {
                Segment::At(Mention::Account(a)) if *a != self.cfg.bot => Some(*a),
                _ => None,
            })
            .collect();
        Some(CommandRequest {
            group: m.group,
            message: m.message,
            sender: m.sender,
            at: m.at,
            word: word.to_owned(),
            args,
            mentions,
        })
    }

    async fn notice(&self, n: GroupNotice) {
        if n.self_id != self.cfg.bot || n.subject == self.cfg.bot {
            return;
        }
        let incoming = Incoming {
            group: n.group,
            message: notice::archive_id(&n),
            author: Some(n.subject),
            at: n.at,
            text: notice::archive_text(&n),
            mentions: Vec::new(),
            media: Vec::new(),
        };
        if let Err(error) = self.intake.append(incoming).await {
            tracing::error!(%error, group = n.group.get(), "archiving a notice failed");
        }
    }
}

/// What a message is archived as: its line, and the media work it carries (each picture, sticker
/// and clip with its reference and its position in the line). `None` when the message renders to
/// nothing.
fn archive_form(m: &GroupMessage, ctx: &RenderContext) -> Option<(Incoming, MediaJob)> {
    let text = render::render(&m.segments, ctx);
    if text.is_empty() {
        return None;
    }
    let job = crate::media::job_for(m, ctx);
    let incoming = Incoming {
        group: m.group,
        message: m.message,
        author: (!m.from_bot).then_some(m.sender),
        at: m.at,
        text,
        mentions: render::mentioned(&m.segments, ctx.bot),
        media: job
            .items
            .iter()
            .map(|item| MediaRefRow {
                kind: item.kind,
                index: u32::try_from(item.index).unwrap_or(u32::MAX),
                key: item.reference.key.clone(),
                file: item.reference.file.clone(),
                url: item.reference.url.clone(),
                size: item.reference.size,
            })
            .collect(),
    };
    Some((incoming, job))
}

async fn submit(supervisor: &Supervisor, request: TriggerRequest) {
    let group = request.group;
    match supervisor.submit(request).await {
        Ok(_handle) => {}
        Err(Rejected::Environment(error)) => tracing::error!(%error, "admission failed"),
        Err(rejected) => tracing::info!(group = group.get(), ?rejected, "trigger not admitted"),
    }
}
