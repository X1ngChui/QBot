//! From one platform frame to its consequences: archive first, then command or run.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use qbot_agent::{Rejected, Supervisor, Trigger, TriggerRequest};
use qbot_core::{AccountId, GroupId, MessageId, UnixMillis};
use qbot_media::{MediaItem, MediaJob, MediaService};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use qbot_store::MediaRefRow;

use crate::echo::EchoBoard;
use crate::intake::{Incoming, Intake, Stored};
use crate::render::RenderContext;
use crate::trigger::{self, Nicknames};
use crate::wire::{Frame, GroupMessage, GroupNotice, Mention, NoticeKind, Segment};
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

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PipelineConfig {
    pub bot: AccountId,
    /// Chance, from 0 to 1, that a member's message that did not trigger the bot starts a
    /// spontaneous run.
    pub spontaneous_chance: f64,
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
            spontaneous_chance: 0.0,
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
    /// Ends waits for media on shutdown: a run they would start is refused then anyway.
    stop: CancellationToken,
    /// Each group's addressed triggers, submitted in order by one task per group.
    queues: Mutex<HashMap<GroupId, mpsc::UnboundedSender<TriggerRequest>>>,
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
            stop: CancellationToken::new(),
            queues: Mutex::default(),
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

    /// Stop accepting commands and wait for those in flight; a reply still waiting for media is
    /// dropped.
    pub async fn finish(&self) {
        self.stop.cancel();
        self.tasks.close();
        self.tasks.wait().await;
    }

    /// Wait (bounded) for the group's media in flight, unless the pipeline is finishing. True
    /// when the run may go ahead.
    fn settled(&self, group: GroupId) -> impl Future<Output = bool> + Send + 'static {
        let media = self
            .media
            .as_ref()
            .filter(|hook| hook.service.busy(group))
            .map(|hook| (Arc::clone(&hook.service), hook.wait));
        let stop = self.stop.clone();
        async move {
            let Some((service, wait)) = media else {
                return true;
            };
            tokio::select! {
                () = stop.cancelled() => false,
                done = service.settle(group, wait) => {
                    if !done {
                        tracing::info!(
                            group = group.get(),
                            "media still pending; going ahead with what is there"
                        );
                    }
                    true
                }
            }
        }
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
        let Some((incoming, items)) = archive_form(&m, &ctx) else {
            return;
        };
        let (line, ordinal) = match self.intake.append(incoming).await {
            Ok(Stored::New { line, ordinal }) => {
                self.archived(m.group, ordinal);
                (line, ordinal)
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
            && !items.is_empty()
        {
            hook.service.admit(MediaJob {
                group: m.group,
                message: m.message,
                ordinal,
                items,
            });
        }

        let typed = render::typed_text(&m.segments);
        if let Some(mut request) = self.command(&m, &typed) {
            // A command sent as a reply carries the @ QQ inserts for the quoted author; it is
            // metadata of the reply, not a target the sender chose.
            if let Some((quoted, account)) = reply_mention(&m.segments)
                && self.quoted_author(m.group, quoted).await == Some(Some(account))
                && let Some(at) = request.mentions.iter().position(|a| *a == account)
            {
                request.mentions.remove(at);
            }
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
        let Some(why) = why else {
            self.maybe_spontaneous(m.group, m.sender);
            return;
        };
        tracing::debug!(group = m.group.get(), ?why, "addressed");
        let request = TriggerRequest {
            group: m.group,
            trigger: Trigger::Addressed {
                message: m.message,
                sender: m.sender,
            },
        };
        self.submit_in_order(request);
    }

    /// Hand an addressed trigger to its group's queue, which submits the group's triggers one at
    /// a time in the order they arrived, each after the group's pictures and clips in flight are
    /// worked on (bounded; perhaps the one the message asks about). Off the reader, so other
    /// frames keep flowing; in order, so a member's quick messages reach the supervisor as they
    /// were sent, the newest last.
    fn submit_in_order(&self, request: TriggerRequest) {
        let group = request.group;
        let mut queues = self.queues.lock().unwrap_or_else(PoisonError::into_inner);
        let queue = queues.entry(group).or_insert_with(|| {
            let (tx, mut rx) = mpsc::unbounded_channel::<TriggerRequest>();
            let supervisor = Arc::clone(&self.supervisor);
            let media = self
                .media
                .as_ref()
                .map(|hook| (Arc::clone(&hook.service), hook.wait));
            let stop = self.stop.clone();
            self.tasks.spawn(async move {
                loop {
                    let first = tokio::select! {
                        () = stop.cancelled() => return,
                        next = rx.recv() => match next {
                            Some(request) => request,
                            None => return,
                        },
                    };
                    // Everything queued meanwhile shares one wait for the group's media, so a
                    // burst is not held up once per message.
                    let mut batch = vec![first];
                    while let Ok(next) = rx.try_recv() {
                        batch.push(next);
                    }
                    if let Some((service, wait)) = &media
                        && service.busy(group)
                    {
                        tokio::select! {
                            () = stop.cancelled() => return,
                            done = service.settle(group, *wait) => if !done {
                                tracing::info!(
                                    group = group.get(),
                                    "media still pending; going ahead with what is there"
                                );
                            },
                        }
                    }
                    for request in batch {
                        submit(&supervisor, request).await;
                    }
                }
            });
            tx
        });
        if queue.send(request).is_err() {
            tracing::debug!(
                group = group.get(),
                "the pipeline is finishing; trigger dropped"
            );
        }
    }

    /// Roll `spontaneous_chance` for a member's message that did not trigger the bot (no @ of the
    /// bot, no nickname, no quote of a bot line, not a command; it may well address another
    /// member), and on a hit start a spontaneous run once the group's media in flight is
    /// described, as a reply would. The supervisor admits at most one per group at a time.
    fn maybe_spontaneous(&self, group: GroupId, author: AccountId) {
        let chance = self.cfg.spontaneous_chance;
        if chance <= 0.0 || rand::random::<f64>() >= chance {
            return;
        }
        let (supervisor, settled) = (Arc::clone(&self.supervisor), self.settled(group));
        self.tasks.spawn(async move {
            if !settled.await {
                return;
            }
            match supervisor.submit_spontaneous(group, author).await {
                Ok(_handle) => {}
                Err(Rejected::Environment(error)) => {
                    tracing::error!(group = group.get(), %error, "admission failed")
                }
                Err(rejected) => {
                    tracing::debug!(
                        group = group.get(),
                        ?rejected,
                        "spontaneous run not admitted"
                    )
                }
            }
        });
    }

    /// Who wrote the quoted line (`Some(None)` for the bot); `None` when unknown.
    async fn quoted_author(&self, group: GroupId, quoted: MessageId) -> Option<Option<AccountId>> {
        self.intake
            .quoted_author(group, quoted)
            .await
            .unwrap_or_else(|error| {
                tracing::warn!(
                    group = group.get(),
                    %error,
                    "could not tell who wrote the quoted message"
                );
                None
            })
    }

    async fn quotes_bot(&self, group: GroupId, quoted: MessageId) -> bool {
        self.quoted_author(group, quoted).await == Some(None)
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
        if n.self_id != self.cfg.bot {
            return;
        }
        // A recalled message is gone for the group, so it is gone for the bot too: whoever wrote
        // it (the bot's own lines included, which an admin may recall).
        if let NoticeKind::Recall { .. } = n.kind
            && let Some(message) = n
                .discriminator
                .trim()
                .parse()
                .ok()
                .and_then(|id| MessageId::new(id).ok())
        {
            let author = (n.subject != self.cfg.bot).then_some(n.subject);
            match self.intake.recall(n.group, message, author).await {
                Ok(true) => {}
                Ok(false) => tracing::debug!(
                    group = n.group.get(),
                    "a recalled message was not in the archive"
                ),
                Err(error) => tracing::error!(
                    group = n.group.get(),
                    %error,
                    "a recalled message could not be removed from the archive"
                ),
            }
        }
        if n.subject == self.cfg.bot {
            return;
        }
        let incoming = Incoming {
            group: n.group,
            message: notice::archive_id(&n),
            platform_seq: None,
            author: Some(n.subject),
            at: n.at,
            text: notice::archive_text(&n),
            mentions: Vec::new(),
            media: Vec::new(),
        };
        match self.intake.append(incoming).await {
            Ok(Stored::New { ordinal, .. }) => self.archived(n.group, ordinal),
            Ok(Stored::Duplicate) => {}
            Err(error) => {
                tracing::error!(%error, group = n.group.get(), "archiving a notice failed")
            }
        }
    }

    /// A line became the group's `ordinal`-th: report a batch it fills, whatever kind of line
    /// it is.
    fn archived(&self, group: GroupId, ordinal: u64) {
        if let Some(hook) = self
            .batches
            .as_ref()
            .filter(|h| ordinal.is_multiple_of(h.lines))
        {
            let sink = Arc::clone(&hook.sink);
            self.tasks.spawn(async move { sink.filled(group).await });
        }
    }
}

/// The quote a message opens with and the account @-mentioned right after it (only whitespace
/// between), if any. When someone replies, QQ inserts an @ of the quoted message's author in
/// exactly that place; whether this one is that @ the caller decides by the quoted line's author.
fn reply_mention(segments: &[Segment]) -> Option<(MessageId, AccountId)> {
    let at = segments
        .iter()
        .position(|s| matches!(s, Segment::Reply(_)))?;
    let Segment::Reply(quoted) = segments[at] else {
        return None;
    };
    segments[at + 1..]
        .iter()
        .find(|s| !matches!(s, Segment::Text(t) if t.trim().is_empty()))
        .and_then(|s| match s {
            Segment::At(Mention::Account(account)) => Some((quoted, *account)),
            _ => None,
        })
}

/// What a message is archived as: its line, and the media work it carries (each picture, sticker
/// and clip with its reference and its position in the line). `None` when the message renders to
/// nothing.
fn archive_form(m: &GroupMessage, ctx: &RenderContext) -> Option<(Incoming, Vec<MediaItem>)> {
    let text = render::render(&m.segments, ctx);
    if text.is_empty() {
        return None;
    }
    let items = crate::media::items_for(m, ctx);
    let incoming = Incoming {
        group: m.group,
        message: m.message,
        platform_seq: m.platform_seq,
        author: (!m.from_bot).then_some(m.sender),
        at: m.at,
        text,
        mentions: render::mentioned(&m.segments, ctx.bot),
        media: items
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
    Some((incoming, items))
}

async fn submit(supervisor: &Supervisor, request: TriggerRequest) {
    let group = request.group;
    match supervisor.submit(request).await {
        Ok(_handle) => {}
        Err(Rejected::Environment(error)) => {
            tracing::error!(group = group.get(), %error, "admission failed")
        }
        // A full queue drops a reply someone is waiting for: worth seeing in /logs.
        Err(Rejected::Overloaded) => {
            tracing::warn!(
                group = group.get(),
                "the reply queue is full; trigger dropped"
            )
        }
        // The member's waiting run answers it.
        Err(Rejected::Folded) => {
            tracing::debug!(group = group.get(), "folded into the member's waiting run")
        }
        Err(rejected) => tracing::info!(group = group.get(), ?rejected, "trigger not admitted"),
    }
}
