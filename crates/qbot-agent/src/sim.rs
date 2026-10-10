//! A simulated group chat for tests: archive, delivery with platform echo (including random
//! dice and rock-paper-scissors results), mute and block policy, run log and usage sink.
//! Together with `qbot_llm::fake::FakeProvider` it lets the whole runtime run with no network.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use qbot_context::{ChatLine, Instruction, InstructionRole, Item, Speaker};
use qbot_core::{
    AccountId, GameResult, GroupId, ItemSeq, MemberNo, MessageId, RpsHand, RunId, UnixMillis,
};
use qbot_llm::Renderer;

use crate::env::{
    Archive, ArchiveCursor, ArchivedLine, ContextSource, Delivered, Delivery, DeliveryError,
    EnvError, GroupPolicy, HistoryQuery, OpenedContext, OutSegment, RunLog, RunSummary, Trigger,
    UsageEvent, UsageSink,
};

#[derive(Default)]
struct World {
    lines: Vec<ArchivedLine>,
    next_message: i64,
    muted: bool,
    blocked: HashSet<AccountId>,
    runs: HashMap<RunId, Vec<(ItemSeq, Item)>>,
    next_run: u64,
    summaries: HashMap<RunId, RunSummary>,
    sent: Vec<Vec<OutSegment>>,
    fail_next_send: Option<DeliveryError>,
    fail_log: bool,
    fail_begin: bool,
    echo_delay: Duration,
    pending_chatter: Vec<(AccountId, MemberNo, String)>,
    dice: u32,
    recaps: Vec<crate::env::Recap>,
    undelivered_note: Option<Instruction>,
    /// Timestamp for the next line, advancing a second per line; 0 uses the message number.
    time: i64,
}

pub struct SimWorld {
    group: GroupId,
    inner: Mutex<World>,
}

impl std::fmt::Debug for SimWorld {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SimWorld")
            .field("group", &self.group)
            .finish_non_exhaustive()
    }
}

impl SimWorld {
    pub fn new(group: GroupId) -> Arc<Self> {
        Arc::new(Self {
            group,
            inner: Mutex::new(World {
                next_message: 100,
                ..World::default()
            }),
        })
    }

    pub fn group(&self) -> GroupId {
        self.group
    }

    fn lock(&self) -> MutexGuard<'_, World> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn push(world: &mut World, speaker: Speaker, text: String) -> ChatLine {
        world.next_message += 1;
        let line = ChatLine {
            message: MessageId::new(world.next_message)
                .unwrap_or_else(|_| unreachable!("positive")),
            speaker,
            at: UnixMillis::new(if world.time > 0 {
                world.time
            } else {
                world.next_message
            }),
            text,
        };
        if world.time > 0 {
            world.time += 1000;
        }
        let seq = ArchiveCursor(world.lines.len() as u64 + 1);
        world.lines.push(ArchivedLine {
            seq,
            line: line.clone(),
        });
        line
    }

    /// A member says something.
    pub fn say(&self, account: i64, number: u32, text: &str) -> ChatLine {
        let account = AccountId::new(account).unwrap_or_else(|_| unreachable!("positive account"));
        let mut world = self.lock();
        Self::push(
            &mut world,
            Speaker::Member {
                account,
                number: MemberNo::new(number),
            },
            text.to_owned(),
        )
    }

    /// One of the bot's own earlier lines, as an echo would have archived it.
    pub fn bot_says(&self, text: &str) -> ChatLine {
        let mut world = self.lock();
        Self::push(&mut world, Speaker::Bot, text.to_owned())
    }

    /// Stamp the following lines from `at` on, a second apart, instead of with the message
    /// number. For runs whose prompt shows times.
    pub fn set_time(&self, at: UnixMillis) {
        self.lock().time = at.get();
    }

    /// The note runs give the model when it writes text without calling a tool.
    pub fn set_undelivered_note(&self, note: &str) {
        self.lock().undelivered_note = Some(Instruction {
            role: InstructionRole::Developer,
            text: note.to_owned(),
            template_hash: "sim".into(),
        });
    }

    /// Recaps offered to runs, as the prompt layer would from episodes.
    pub fn set_recaps(&self, recaps: Vec<crate::env::Recap>) {
        self.lock().recaps = recaps;
    }

    pub fn set_muted(&self, muted: bool) {
        self.lock().muted = muted;
    }

    pub fn block(&self, account: i64) {
        if let Ok(account) = AccountId::new(account) {
            self.lock().blocked.insert(account);
        }
    }

    /// Make the next send fail with `error`.
    pub fn fail_next_send(&self, error: DeliveryError) {
        self.lock().fail_next_send = Some(error);
    }

    /// Make appending items and finishing runs fail (a run that already began).
    pub fn fail_run_log(&self, fail: bool) {
        self.lock().fail_log = fail;
    }

    /// Make recording the start of a run fail.
    pub fn fail_run_begin(&self, fail: bool) {
        self.lock().fail_begin = fail;
    }

    /// Simulate the platform taking this long to echo a sent message.
    pub fn set_echo_delay(&self, delay: Duration) {
        self.lock().echo_delay = delay;
    }

    /// Someone speaks right after the bot's next send (a message arriving mid-run).
    pub fn chatter_after_next_send(&self, account: i64, number: u32, text: &str) {
        if let Ok(account) = AccountId::new(account) {
            self.lock()
                .pending_chatter
                .push((account, MemberNo::new(number), text.to_owned()));
        }
    }

    pub fn lines(&self) -> Vec<ChatLine> {
        self.lock().lines.iter().map(|a| a.line.clone()).collect()
    }

    pub fn sent(&self) -> Vec<Vec<OutSegment>> {
        self.lock().sent.clone()
    }

    pub fn logged(&self, run: RunId) -> Vec<(ItemSeq, Item)> {
        self.lock().runs.get(&run).cloned().unwrap_or_default()
    }

    pub fn summary(&self, run: RunId) -> Option<RunSummary> {
        self.lock().summaries.get(&run).cloned()
    }

    pub fn logged_runs(&self) -> Vec<RunId> {
        let mut runs: Vec<_> = self.lock().runs.keys().copied().collect();
        runs.sort();
        runs
    }
}

fn render_segments(world: &mut World, segments: &[OutSegment]) -> String {
    segments
        .iter()
        .map(|segment| match segment {
            OutSegment::Text(text) => text.clone(),
            OutSegment::At(account) => format!("@{}", account.get()),
            OutSegment::Reply(message) => format!("[reply:{}]", message.get()),
            OutSegment::Face(id) => format!("[face:{id}]"),
            OutSegment::Contact(account) => format!("[contact:{}]", account.get()),
            // The simulated platform decides, as the real one does; the request had no result.
            OutSegment::Dice => {
                world.dice += 1;
                GameResult::Dice((world.dice % 6 + 1) as u8).marker()
            }
            OutSegment::Rps => {
                world.dice += 1;
                let hand =
                    [RpsHand::Rock, RpsHand::Paper, RpsHand::Scissors][(world.dice % 3) as usize];
                GameResult::Rps(hand).marker()
            }
        })
        .collect::<Vec<_>>()
        .join("")
}

#[async_trait]
impl Archive for SimWorld {
    async fn since(
        &self,
        _group: GroupId,
        cursor: ArchiveCursor,
    ) -> Result<Vec<ArchivedLine>, EnvError> {
        Ok(self
            .lock()
            .lines
            .iter()
            .filter(|a| a.seq > cursor)
            .cloned()
            .collect())
    }

    async fn search(
        &self,
        _group: GroupId,
        query: &HistoryQuery,
        limit: usize,
    ) -> Result<Vec<ChatLine>, EnvError> {
        Ok(self
            .lock()
            .lines
            .iter()
            .rev()
            .filter(|a| query.text.matches(&a.line.text))
            .filter(|a| match (query.speaker, a.line.speaker) {
                (None, _) => true,
                (Some(wanted), Speaker::Member { number, .. }) => number == wanted,
                (Some(_), Speaker::Bot) => false,
            })
            .take(limit)
            .map(|a| a.line.clone())
            .collect())
    }
}

#[async_trait]
impl ContextSource for SimWorld {
    async fn open(&self, _group: GroupId, trigger: &Trigger) -> Result<OpenedContext, EnvError> {
        let world = self.lock();
        let window: Vec<ChatLine> = world.lines.iter().map(|a| a.line.clone()).collect();
        let cursor = world.lines.last().map_or(ArchiveCursor(0), |a| a.seq);
        let trigger_note = match trigger {
            Trigger::Wake { timer, intent, .. } => Some(Instruction {
                role: InstructionRole::Trigger,
                text: format!(
                    "[scheduled task {} woke up; its stored intent: {intent}]",
                    timer.get()
                ),
                template_hash: "sim".into(),
            }),
            Trigger::Spontaneous => Some(Instruction {
                role: InstructionRole::Trigger,
                text: "[you looked at the chat on your own; nobody asked]".into(),
                template_hash: "sim".into(),
            }),
            Trigger::Addressed { .. } => None,
        };
        Ok(OpenedContext {
            instructions: vec![
                Instruction {
                    role: InstructionRole::System,
                    text: "You are a group member.".into(),
                    template_hash: "sim".into(),
                },
                Instruction {
                    role: InstructionRole::Developer,
                    text: "Persona: friendly.".into(),
                    template_hash: "sim".into(),
                },
            ],
            window,
            members: Vec::new(),
            trigger_note,
            cursor,
            recaps: world.recaps.clone(),
            undelivered_note: world.undelivered_note.clone(),
        })
    }
}

#[async_trait]
impl Delivery for SimWorld {
    async fn send(
        &self,
        _group: GroupId,
        segments: Vec<OutSegment>,
    ) -> Result<Delivered, DeliveryError> {
        let delay = {
            let mut world = self.lock();
            if let Some(error) = world.fail_next_send.take() {
                return Err(error);
            }
            world.echo_delay
        };
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        let mut world = self.lock();
        world.sent.push(segments.clone());
        let text = render_segments(&mut world, &segments);
        let echo = Self::push(&mut world, Speaker::Bot, text);
        for (account, number, text) in std::mem::take(&mut world.pending_chatter) {
            Self::push(&mut world, Speaker::Member { account, number }, text);
        }
        Ok(Delivered { echo })
    }
}

#[async_trait]
impl GroupPolicy for SimWorld {
    async fn is_muted(&self, _group: GroupId) -> Result<bool, EnvError> {
        Ok(self.lock().muted)
    }

    async fn is_blocked(&self, _group: GroupId, account: AccountId) -> Result<bool, EnvError> {
        Ok(self.lock().blocked.contains(&account))
    }
}

#[async_trait]
impl RunLog for SimWorld {
    async fn begin(&self, _group: GroupId, _trigger: &Trigger) -> Result<RunId, EnvError> {
        let mut world = self.lock();
        if world.fail_begin {
            return Err(EnvError("run log unavailable".into()));
        }
        world.next_run += 1;
        let run = RunId::new(world.next_run);
        world.runs.entry(run).or_default();
        Ok(run)
    }

    async fn append(
        &self,
        _group: GroupId,
        run: RunId,
        seq: ItemSeq,
        item: &Item,
    ) -> Result<(), EnvError> {
        let mut world = self.lock();
        if world.fail_log {
            return Err(EnvError("run log unavailable".into()));
        }
        world.runs.entry(run).or_default().push((seq, item.clone()));
        Ok(())
    }

    async fn finish(
        &self,
        _group: GroupId,
        run: RunId,
        summary: &RunSummary,
    ) -> Result<(), EnvError> {
        let mut world = self.lock();
        if world.fail_log {
            return Err(EnvError("run log unavailable".into()));
        }
        world.summaries.insert(run, summary.clone());
        Ok(())
    }
}

/// Collects usage events for assertions.
#[derive(Debug, Default)]
pub struct MemorySink {
    events: Mutex<Vec<UsageEvent>>,
}

impl MemorySink {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn events(&self) -> Vec<UsageEvent> {
        self.events
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl UsageSink for MemorySink {
    fn record(&self, event: UsageEvent) {
        self.events
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(event);
    }
}

/// The plain renderer, re-exported so tests need one import.
pub fn renderer() -> Arc<dyn Renderer> {
    Arc::new(qbot_llm::PlainRenderer)
}
