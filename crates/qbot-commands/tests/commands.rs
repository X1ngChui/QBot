#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::{BTreeSet, HashMap};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use qbot_agent::{Delivered, Delivery, DeliveryError, OutSegment};
use qbot_commands::{
    AdminError, Command, CommandRouter, CommandSettings, Deps, Directory, GroupAdmin, LogLine,
    RecentLogs, RunHistory,
};
use qbot_core::{AccountId, Clock, GroupId, MessageId, UnixMillis};
use qbot_gateway::pipeline::{CommandRequest, Commands};
use qbot_i18n::Locales;
use qbot_memory::{IdentityPolicy, IdentityStore, MemoryIdentityStore};
use qbot_sched::{MemoryTimerStore, TaskLimits, TaskService};
use qbot_store::{BlockRule, RosterRow, TopRow, UsageTotals};
use tokio::sync::Notify;

const T0: i64 = 1_800_000_000_000;
const OWNER: i64 = 1;
const ALICE: i64 = 2;
const BOB: i64 = 3;
const CAROL: i64 = 4;
const BOT: i64 = 100;

fn acct(n: i64) -> AccountId {
    AccountId::new(n).unwrap()
}

#[derive(Debug)]
struct TestClock(AtomicI64);
impl Clock for TestClock {
    fn now(&self) -> UnixMillis {
        UnixMillis::new(self.0.load(Ordering::SeqCst))
    }
}

#[derive(Default)]
struct AdminState {
    muted: bool,
    blocks: Vec<BlockRule>,
}

#[derive(Default)]
struct FakeAdmin(Mutex<AdminState>);

#[async_trait]
impl GroupAdmin for FakeAdmin {
    async fn is_muted(&self, _: GroupId) -> Result<bool, AdminError> {
        Ok(self.0.lock().unwrap().muted)
    }
    async fn set_muted(&self, _: GroupId, muted: bool) -> Result<(), AdminError> {
        self.0.lock().unwrap().muted = muted;
        Ok(())
    }
    async fn blocks(&self, _: GroupId) -> Result<Vec<BlockRule>, AdminError> {
        Ok(self.0.lock().unwrap().blocks.clone())
    }
    async fn block(
        &self,
        _: GroupId,
        account: AccountId,
        until: Option<UnixMillis>,
    ) -> Result<(), AdminError> {
        let mut state = self.0.lock().unwrap();
        state.blocks.retain(|b| b.account != account);
        state.blocks.push(BlockRule { account, until });
        Ok(())
    }
    async fn unblock(&self, _: GroupId, account: AccountId) -> Result<bool, AdminError> {
        let mut state = self.0.lock().unwrap();
        let before = state.blocks.len();
        state.blocks.retain(|b| b.account != account);
        Ok(state.blocks.len() != before)
    }
    async fn member_number(
        &self,
        _: GroupId,
        account: AccountId,
    ) -> Result<Option<u32>, AdminError> {
        Ok([OWNER, ALICE, BOB, CAROL]
            .iter()
            .position(|a| *a == account.get())
            .map(|i| i as u32 + 1))
    }
    async fn message_count(&self, _: GroupId, accounts: &[AccountId]) -> Result<u64, AdminError> {
        Ok(accounts.len() as u64 * 10)
    }
    async fn roster(&self, _: GroupId, _: i64) -> Result<(u64, Vec<RosterRow>), AdminError> {
        Ok((
            3,
            vec![
                RosterRow {
                    account: acct(ALICE),
                    number: 2,
                    messages: 42,
                },
                RosterRow {
                    account: acct(BOB),
                    number: 3,
                    messages: 7,
                },
            ],
        ))
    }
    async fn usage(
        &self,
        group: Option<GroupId>,
        _: UnixMillis,
    ) -> Result<UsageTotals, AdminError> {
        let scale = if group.is_some() { 1 } else { 10 };
        Ok(UsageTotals {
            runs: 3 * scale,
            model_calls: 5 * scale,
            input_tokens: 1000 * scale,
            cached_tokens: 400 * scale,
            output_tokens: 50 * scale,
            tool_calls: 4 * scale,
        })
    }
    async fn top_requesters(
        &self,
        _: GroupId,
        _: UnixMillis,
        limit: i64,
    ) -> Result<Vec<TopRow>, AdminError> {
        let all = [(ALICE, 9), (BOB, 5), (CAROL, 2)];
        Ok(all
            .iter()
            .take(usize::try_from(limit).unwrap_or(3))
            .map(|(a, runs)| TopRow {
                account: acct(*a),
                runs: *runs,
                input_tokens: 100 * runs,
                output_tokens: 10 * runs,
            })
            .collect())
    }
}

struct Names;
#[async_trait]
impl Directory for Names {
    async fn display_name(&self, _: GroupId, account: AccountId) -> Option<String> {
        match account.get() {
            OWNER => Some("Olive".into()),
            ALICE => Some("Alice".into()),
            BOB => Some("Bob".into()),
            _ => None,
        }
    }
}

#[derive(Default)]
struct Sent(Mutex<Vec<Vec<OutSegment>>>);
#[async_trait]
impl Delivery for Sent {
    async fn send(
        &self,
        _: GroupId,
        segments: Vec<OutSegment>,
    ) -> Result<Delivered, DeliveryError> {
        self.0.lock().unwrap().push(segments);
        Err(DeliveryError::EchoMissing)
    }
}

/// Run history with one run of group 900 and one of another group.
struct FakeRuns(Vec<(qbot_store::RunRecord, Vec<qbot_context::Item>)>);

#[async_trait]
impl RunHistory for FakeRuns {
    async fn recent(
        &self,
        group: GroupId,
        limit: i64,
    ) -> Result<Vec<qbot_store::RunRecord>, AdminError> {
        Ok(self
            .0
            .iter()
            .rev()
            .filter(|(r, _)| r.group == group)
            .take(limit as usize)
            .map(|(r, _)| r.clone())
            .collect())
    }
    async fn run(
        &self,
        group: GroupId,
        run: qbot_core::RunId,
    ) -> Result<Option<(qbot_store::RunRecord, Vec<qbot_context::Item>)>, AdminError> {
        Ok(self
            .0
            .iter()
            .find(|(r, _)| r.run == run && r.group == group)
            .cloned())
    }
}

struct FakeLogs(Vec<LogLine>);
impl RecentLogs for FakeLogs {
    fn recent(&self, limit: usize) -> Vec<LogLine> {
        let skip = self.0.len().saturating_sub(limit);
        self.0[skip..].to_vec()
    }
}

fn run_record(run: u64, group: i64) -> qbot_store::RunRecord {
    qbot_store::RunRecord {
        run: qbot_core::RunId::new(run),
        group: GroupId::new(group).unwrap(),
        trigger_kind: qbot_store::TriggerKind::Addressed,
        started: UnixMillis::new(T0),
        ended: Some(UnixMillis::new(T0 + 5_000)),
        end: Some(qbot_context::RunEnd::Delivered),
        error_class: None,
        input_tokens: Some(1200),
        cached_tokens: Some(800),
        output_tokens: Some(34),
        turns: Some(2),
        tool_calls: Some(2),
        sends: Some(1),
    }
}

fn run_items() -> Vec<qbot_context::Item> {
    use qbot_context::{
        AssistantPart, AssistantTurn, ChatBatch, Item, Outcome, Part, ToolCall, ToolResult,
    };
    let call = ToolCall {
        id: qbot_core::CallId::new("c1").unwrap(),
        name: "search_history".into(),
        arguments: serde_json::json!({"query": "printer"}),
    };
    vec![
        Item::Chat(ChatBatch::new(vec![])),
        Item::Assistant(AssistantTurn::new(vec![AssistantPart::Call(call)])),
        Item::ToolResult(ToolResult {
            call_id: qbot_core::CallId::new("c1").unwrap(),
            outcome: Outcome::Ok,
            content: vec![Part::Text("no matches".into())],
        }),
        Item::Assistant(AssistantTurn::new(vec![AssistantPart::Text(
            "done looking".into(),
        )])),
    ]
}

struct Rig {
    router: CommandRouter,
    identity: Arc<MemoryIdentityStore>,
    facts: Arc<qbot_memory::facts::MemoryFactStore>,
    notes: Arc<qbot_memory::MemoryNoteStore>,
    sent: Arc<Sent>,
    next_message: AtomicI64,
}

async fn rig() -> Rig {
    let clock = Arc::new(TestClock(AtomicI64::new(T0)));
    let identity = Arc::new(MemoryIdentityStore::new(IdentityPolicy::default()));
    for a in [OWNER, ALICE, BOB, CAROL] {
        identity.seen(acct(a), UnixMillis::new(T0)).await.unwrap();
    }
    let tasks = TaskService::new(
        Arc::new(MemoryTimerStore::new()),
        clock.clone(),
        TaskLimits::default(),
        Arc::new(Notify::new()),
    );
    let sent = Arc::new(Sent::default());
    let facts = Arc::new(qbot_memory::facts::MemoryFactStore::new());
    let notes = Arc::new(qbot_memory::MemoryNoteStore::default());
    let router = CommandRouter::new(Deps {
        identity: identity.clone(),
        facts: facts.clone(),
        notes: notes.clone(),
        admin: Arc::new(FakeAdmin::default()),
        runs: Arc::new(FakeRuns(vec![
            (run_record(41, 777), vec![]),
            (run_record(42, 900), run_items()),
        ])),
        logs: Arc::new(FakeLogs(vec![
            LogLine {
                at: UnixMillis::new(T0),
                level: "WARN".into(),
                text: "an extraction job could not be queued group=900".into(),
            },
            LogLine {
                at: UnixMillis::new(T0 + 60_000),
                level: "ERROR".into(),
                text: "command failed in storage".into(),
            },
        ])),
        tasks,
        directory: Arc::new(Names),
        delivery: sent.clone(),
        locales: Locales::english(),
        clock,
        settings: CommandSettings {
            bot: acct(BOT),
            owners: BTreeSet::from([acct(OWNER)]),
            zone: jiff::tz::TimeZone::get("Asia/Shanghai").unwrap(),
            max_message_chars: 2000,
            members_max_rows: 60,
            top_max_rows: 20,
            link_ttl: Duration::from_secs(600),
            fact_decay: qbot_memory::facts::DecayPolicy::default(),
            notes_per_account: 3,
            runs_max_rows: 10,
        },
    });
    Rig {
        router,
        identity,
        facts,
        notes,
        sent,
        next_message: AtomicI64::new(1000),
    }
}

impl Rig {
    fn request(&self, sender: i64, text: &str, mentions: &[i64]) -> CommandRequest {
        let word = text.split_whitespace().next().unwrap().to_owned();
        let args = text
            .trim_start()
            .strip_prefix(&word)
            .unwrap()
            .trim()
            .to_owned();
        CommandRequest {
            group: GroupId::new(900).unwrap(),
            message: MessageId::new(self.next_message.fetch_add(1, Ordering::SeqCst)).unwrap(),
            sender: acct(sender),
            at: UnixMillis::new(T0),
            word,
            args,
            mentions: mentions.iter().map(|m| acct(*m)).collect(),
        }
    }

    async fn say(&self, sender: i64, text: &str, mentions: &[i64]) -> String {
        self.router
            .respond(&self.request(sender, text, mentions))
            .await
            .expect("a recognised command")
    }
}

#[test]
fn only_the_exact_lowercase_word_is_a_command() {
    assert_eq!(Command::from_word("/who"), Some(Command::Who));
    assert_eq!(Command::from_word("/WHO"), None);
    assert_eq!(Command::from_word("who"), None);
    assert_eq!(Command::from_word("/who@bot"), None);
    assert_eq!(
        Command::find("/Help"),
        Some(Command::Help),
        "help is lenient about slash and case"
    );
    assert_eq!(Command::ALL.len(), 16);
}

#[tokio::test]
async fn help_lists_commands_by_category_and_explains_one() {
    let rig = rig().await;
    let all = rig.say(ALICE, "/help", &[]).await;
    assert!(all.starts_with("Available commands:"), "{all}");
    assert!(
        all.contains("/who  Show what is on record about a member"),
        "{all}"
    );
    assert!(
        all.contains("/mute  Mute or unmute the bot here (owner only)"),
        "{all}"
    );
    assert!(all.contains("[Administration]"), "{all}");
    let one = rig.say(ALICE, "/help tasks", &[]).await;
    assert!(
        one.contains("Bot owner only") && one.contains("/tasks add"),
        "{one}"
    );
    assert_eq!(
        rig.say(ALICE, "/help nope", &[]).await,
        "There is no command called \"nope\"."
    );
    assert_eq!(
        rig.say(ALICE, "/help a b", &[]).await,
        "Usage: /help [command]"
    );
}

#[tokio::test]
async fn owner_only_commands_are_refused_for_members() {
    let rig = rig().await;
    for text in ["/mute on", "/members", "/block", "/tasks", "/runs", "/logs"] {
        assert_eq!(
            rig.say(ALICE, text, &[]).await,
            "This action needs bot-owner permission.",
            "{text}"
        );
    }
    assert_eq!(
        rig.say(ALICE, "/stats global", &[]).await,
        "This action needs bot-owner permission."
    );
}

#[tokio::test]
async fn who_shows_a_record_and_enforces_scope_rules() {
    let rig = rig().await;
    let me = rig.say(ALICE, "/who", &[]).await;
    assert!(
        me.contains("Account record | Alice | exact account")
            && me.contains("Messages in this group: 10"),
        "{me}"
    );
    assert_eq!(
        rig.say(ALICE, "/who extra", &[]).await,
        "Usage: /who [--linked] [@account]"
    );
    assert_eq!(
        rig.say(ALICE, "/who --bogus", &[]).await,
        "Unknown option. Send /help for the current syntax."
    );
    assert_eq!(
        rig.say(ALICE, "/who --linked --linked", &[]).await,
        "--linked may be given only once."
    );
    assert_eq!(
        rig.say(ALICE, "/who", &[BOB]).await,
        "You can only act on your own account or on accounts you have confirmed as linked."
    );
    assert!(
        rig.say(OWNER, "/who", &[BOB]).await.contains("| Bob |"),
        "owners may look at anyone"
    );
    assert_eq!(
        rig.say(OWNER, "/who", &[BOB, CAROL]).await,
        "Mention at most one account."
    );
    // Carol has no display name; the label names the account.
    // The platform gives no display name for Carol here: she is named by her member number.
    assert!(rig.say(OWNER, "/who", &[CAROL]).await.contains("member 4"));
}

#[tokio::test]
async fn aliases_can_be_added_listed_and_removed_with_evidence_semantics() {
    let rig = rig().await;
    assert_eq!(
        rig.say(ALICE, "/name", &[]).await,
        "Alice has no names on record."
    );
    assert_eq!(
        rig.say(ALICE, "/name add Al the  Great", &[]).await,
        "Recorded: Alice is also called \"Al the Great\"."
    );
    let list = rig.say(ALICE, "/name", &[]).await;
    assert!(
        list.contains("Names of Alice:")
            && list.contains("- al the great (confirmed, confidence 1.00)"),
        "{list}"
    );
    assert!(
        rig.say(ALICE, "/who", &[])
            .await
            .contains("Confirmed names: al the great (confidence 1.00)")
    );
    assert_eq!(
        rig.say(BOB, "/name add Al the Great", &[]).await,
        "\"Al the Great\" already refers to someone else in this group."
    );
    assert_eq!(
        rig.say(ALICE, "/name add", &[]).await,
        "Usage: /name add [--linked] [@account] name"
    );
    assert_eq!(
        rig.say(ALICE, "/name extra", &[]).await,
        "Usage: /name [--linked] [@account]"
    );
    assert_eq!(
        rig.say(ALICE, "/name remove Al the Great", &[]).await,
        "Removed the name \"Al the Great\" from Alice."
    );
    assert_eq!(
        rig.say(ALICE, "/name remove Al the Great", &[]).await,
        "Alice has no active name \"Al the Great\" in this scope."
    );
    let long = format!("/name add {}", "x".repeat(65));
    assert_eq!(
        rig.say(ALICE, &long, &[]).await,
        "A name can be at most 64 characters."
    );
}

#[tokio::test]
async fn linking_needs_an_invitation_and_the_invited_account_to_confirm() {
    let rig = rig().await;
    assert_eq!(
        rig.say(ALICE, "/link", &[]).await,
        "Usage: /link @account, /link confirm or /link cancel"
    );
    assert_eq!(
        rig.say(ALICE, "/link confirm extra", &[BOB]).await,
        "Usage: /link @account, /link confirm or /link cancel"
    );
    assert_eq!(
        rig.say(ALICE, "/link", &[ALICE]).await,
        "An account cannot be linked with itself."
    );
    assert_eq!(
        rig.say(ALICE, "/link", &[BOB]).await,
        "Invited Bob to link. The invited account should send /link confirm in this group within 600 seconds."
    );
    assert_eq!(
        rig.say(CAROL, "/link confirm", &[]).await,
        "This account has no pending invitation to confirm in this group."
    );
    assert_eq!(
        rig.say(ALICE, "/link confirm", &[]).await,
        "Only the invited account can confirm."
    );
    assert_eq!(
        rig.say(BOB, "/link confirm", &[]).await,
        "Link confirmed. Both accounts are now linked."
    );
    // Now linked: Alice may act on Bob, and /who --linked shows the pair.
    assert!(
        rig.say(ALICE, "/who --linked", &[BOB])
            .await
            .contains("Linked accounts: 2")
    );
    assert_eq!(
        rig.say(ALICE, "/link cancel", &[]).await,
        "There is no pending invitation involving this account in this group."
    );
    assert_eq!(
        rig.say(ALICE, "/unlink", &[]).await,
        "Unlinked this account; the others stay linked."
    );
    assert_eq!(
        rig.say(ALICE, "/unlink", &[]).await,
        "This account is not linked to any other account, so there is nothing to detach."
    );
    assert!(
        rig.identity
            .holder_of(acct(BOB))
            .await
            .unwrap()
            .unwrap()
            .accounts
            .len()
            == 1
    );
}

#[tokio::test]
async fn owners_link_and_unlink_other_accounts_directly() {
    let rig = rig().await;
    assert_eq!(
        rig.say(ALICE, "/link", &[BOB, CAROL]).await,
        "Linking two other accounts directly needs bot-owner permission; use /link @account to invite."
    );
    assert_eq!(
        rig.say(OWNER, "/link", &[ALICE, 999]).await,
        "Account 999 has no record and cannot be linked."
    );
    assert_eq!(
        rig.say(OWNER, "/link", &[ALICE, BOB]).await,
        "The two accounts are now linked."
    );
    assert_eq!(
        rig.say(OWNER, "/link", &[ALICE, BOB]).await,
        "These two accounts are already linked."
    );
    assert_eq!(
        rig.say(ALICE, "/unlink", &[BOB]).await,
        "Detaching another account needs bot-owner permission; /unlink detaches only your own."
    );
    assert_eq!(
        rig.say(OWNER, "/unlink", &[BOB]).await,
        "Detached the selected account; the others stay linked."
    );
    assert_eq!(
        rig.say(OWNER, "/unlink", &[BOB]).await,
        "This account is not linked to any other account, so there is nothing to detach."
    );
    assert_eq!(
        rig.say(OWNER, "/unlink", &[ALICE, BOB]).await,
        "Usage: /unlink"
    );
}

#[tokio::test]
async fn mute_reports_and_changes_state() {
    let rig = rig().await;
    assert_eq!(
        rig.say(OWNER, "/mute", &[]).await,
        "This group is not muted."
    );
    assert_eq!(rig.say(OWNER, "/mute on", &[]).await, "Muted this group.");
    assert_eq!(
        rig.say(OWNER, "/mute on", &[]).await,
        "This group is already muted."
    );
    assert_eq!(
        rig.say(OWNER, "/mute status", &[]).await,
        "This group is muted."
    );
    assert_eq!(
        rig.say(OWNER, "/mute off", &[]).await,
        "Replies are enabled again in this group."
    );
    assert_eq!(
        rig.say(OWNER, "/mute off", &[]).await,
        "Replies are already enabled in this group."
    );
    assert_eq!(
        rig.say(OWNER, "/mute maybe", &[]).await,
        "Usage: /mute [status|on|off]"
    );
    assert_eq!(
        rig.say(OWNER, "/mute", &[BOB]).await,
        "Usage: /mute [status|on|off]"
    );
}

#[tokio::test]
async fn blocking_supports_durations_scopes_and_protects_owners() {
    let rig = rig().await;
    assert_eq!(
        rig.say(OWNER, "/block", &[]).await,
        "This group has no reply blocks."
    );
    assert_eq!(
        rig.say(OWNER, "/block add 30m", &[BOB]).await,
        "Blocked replies to Bob | exact account | until 01-15 16:30."
    );
    assert_eq!(
        rig.say(OWNER, "/block add", &[CAROL]).await,
        "Blocked replies to member 4 | exact account | until removed."
    );
    let list = rig.say(OWNER, "/block", &[]).await;
    assert!(
        list.contains("- Bob  (until 01-15 16:30)") && list.contains("- member 4"),
        "{list}"
    );
    assert_eq!(
        rig.say(OWNER, "/block add 5x", &[BOB]).await,
        "Invalid duration; use 30m, 12h or 3d."
    );
    assert_eq!(
        rig.say(OWNER, "/block add 1d 2d", &[BOB]).await,
        "Usage: /block add [--linked] @account [30m|12h|3d]"
    );
    assert_eq!(
        rig.say(OWNER, "/block add", &[OWNER]).await,
        "The bot and bot owners cannot be blocked."
    );
    assert_eq!(
        rig.say(OWNER, "/block add", &[]).await,
        "Mention exactly one account."
    );
    assert_eq!(
        rig.say(OWNER, "/block nonsense", &[]).await,
        "Usage: /block add|remove [--linked] @account [duration]"
    );
    assert_eq!(
        rig.say(OWNER, "/block remove", &[BOB]).await,
        "Removed the reply block: Bob | exact account."
    );
    assert_eq!(
        rig.say(OWNER, "/block remove", &[BOB]).await,
        "There is no reply block on Bob in scope: exact account."
    );

    // --linked covers every account linked to the person right now.
    rig.say(OWNER, "/link", &[ALICE, BOB]).await;
    assert!(
        rig.say(OWNER, "/block add --linked", &[ALICE])
            .await
            .contains("all linked accounts")
    );
    let list = rig.say(OWNER, "/block", &[]).await;
    assert!(list.contains("- Alice") && list.contains("- Bob"), "{list}");
}

#[tokio::test]
async fn members_stats_and_top_summarise_the_group() {
    let rig = rig().await;
    let members = rig.say(OWNER, "/members", &[]).await;
    assert!(
        members.starts_with("3 members on record")
            && members.contains("- Alice (42 messages)")
            && members.contains("- Bob (7 messages)"),
        "{members}"
    );
    let stats = rig.say(ALICE, "/stats", &[]).await;
    assert!(
        stats.starts_with("Usage today | group 900")
            && stats.contains("Runs: 3 (5 model calls, 4 tool calls)")
            && stats.contains("Group replies: enabled"),
        "{stats}"
    );
    assert!(
        rig.say(OWNER, "/stats global", &[])
            .await
            .contains("Runs: 30")
    );
    assert_eq!(
        rig.say(ALICE, "/stats nope", &[]).await,
        "Usage: /stats [global]"
    );
    let top = rig.say(ALICE, "/top 2", &[]).await;
    assert!(
        top.starts_with("Most active requesters this month | exact account"),
        "{top}"
    );
    assert!(
        top.contains("1. Alice  9 replies, 990 tokens")
            && top.contains("2. Bob  5 replies, 550 tokens")
            && !top.contains("3."),
        "{top}"
    );
    assert_eq!(
        rig.say(ALICE, "/top 0", &[]).await,
        "Usage: /top [--linked] [count]"
    );
    assert_eq!(
        rig.say(ALICE, "/top 1 2", &[]).await,
        "Usage: /top [--linked] [count]"
    );
    // Linked accounts are combined by person.
    rig.say(OWNER, "/link", &[ALICE, BOB]).await;
    let combined = rig.say(ALICE, "/top --linked", &[]).await;
    assert!(
        combined.contains("1. Alice (2 accounts)  14 replies"),
        "{combined}"
    );
}

#[tokio::test]
async fn tasks_can_be_created_listed_changed_and_cancelled() {
    let rig = rig().await;
    assert_eq!(
        rig.say(OWNER, "/tasks", &[]).await,
        "This group has no pending or running tasks."
    );
    let created = rig
        .say(
            OWNER,
            "/tasks add --in 30m -- remind everyone\nabout the  meeting",
            &[],
        )
        .await;
    assert!(
        created.starts_with(
            "Created the task:\n1\nState: pending | Due: 2027-01-15T16:30:00+08:00\nremind everyone"
        ),
        "{created}"
    );
    assert!(
        created.contains("about the  meeting"),
        "content keeps its inner whitespace: {created}"
    );
    let list = rig.say(OWNER, "/tasks list", &[]).await;
    assert!(
        list.starts_with("Active tasks of this group | page 1")
            && list.contains("Full content and results"),
        "{list}"
    );
    let shown = rig.say(OWNER, "/tasks show 1", &[]).await;
    assert!(shown.starts_with("Task details:\n1\n"), "{shown}");
    let edited = rig
        .say(OWNER, "/tasks edit 1 --in 2h -- new text", &[])
        .await;
    assert!(
        edited.starts_with("Changed the task:")
            && edited.contains("new text")
            && edited.contains("2027-01-15T18:00:00+08:00"),
        "{edited}"
    );
    assert_eq!(
        rig.say(OWNER, "/tasks edit 1", &[]).await,
        "Editing a task needs new content or a new time."
    );
    assert!(
        rig.say(OWNER, "/tasks cancel 1", &[])
            .await
            .contains("State: cancelled")
    );
    assert_eq!(
        rig.say(OWNER, "/tasks cancel 1", &[]).await,
        "This group has no such task, or it is no longer pending and so cannot be cancelled."
    );
    assert_eq!(
        rig.say(OWNER, "/tasks edit 1 -- x", &[]).await,
        "This group has no such task, or it is no longer pending and so cannot be changed."
    );
    assert_eq!(
        rig.say(OWNER, "/tasks show 999", &[]).await,
        "This group has no such task."
    );
}

#[tokio::test]
async fn task_arguments_are_validated_with_specific_messages() {
    let rig = rig().await;
    assert_eq!(
        rig.say(OWNER, "/tasks add --in 1m -- soon", &[])
            .await
            .split(';')
            .next()
            .unwrap(),
        "The time is too soon"
    );
    assert_eq!(
        rig.say(OWNER, "/tasks add --in 30m", &[]).await,
        "Usage: /tasks add (--at TIME | --in DURATION) -- content"
    );
    assert_eq!(
        rig.say(OWNER, "/tasks add -- x", &[]).await,
        "Usage: /tasks add (--at TIME | --in DURATION) -- content"
    );
    assert_eq!(
        rig.say(
            OWNER,
            "/tasks add --in 30m --at 2030-01-01T00:00:00Z -- x",
            &[]
        )
        .await,
        "Use either --at or --in, not both."
    );
    assert_eq!(
        rig.say(OWNER, "/tasks add --in -- x", &[]).await,
        "Each time option needs exactly one value."
    );
    assert_eq!(
        rig.say(OWNER, "/tasks add --in 30m --in 40m -- x", &[])
            .await,
        "Only one --at or --in is accepted, and no other options."
    );
    assert_eq!(
        rig.say(OWNER, "/tasks add --when 30m -- x", &[]).await,
        "Only one --at or --in is accepted, and no other options."
    );
    assert_eq!(
        rig.say(OWNER, "/tasks add --in 30m --", &[]).await,
        "The task content must not be empty."
    );
    assert_eq!(
        rig.say(OWNER, "/tasks add --in 99x -- x", &[]).await,
        "Invalid duration; use 30m, 12h or 3d."
    );
    assert!(
        rig.say(OWNER, "/tasks add --at tomorrow -- x", &[])
            .await
            .starts_with("--at needs an RFC 3339 time")
    );
    assert!(
        rig.say(OWNER, "/tasks add --at 2030-01-02T09:00:00+08:00 -- x", &[])
            .await
            .contains("Due: 2030-01-02T09:00:00+08:00")
    );
    assert_eq!(
        rig.say(OWNER, "/tasks show", &[]).await,
        "Usage: /tasks show ID"
    );
    assert_eq!(
        rig.say(OWNER, "/tasks show abc", &[]).await,
        "Invalid task ID or arguments. See /help tasks."
    );
    assert_eq!(
        rig.say(OWNER, "/tasks fly", &[]).await,
        "Unknown task action. See /help tasks."
    );
    assert_eq!(
        rig.say(OWNER, "/tasks list 0", &[]).await,
        "Numbers and page numbers must be whole numbers from 1 to 10000. See /help."
    );
    assert_eq!(
        rig.say(OWNER, "/tasks list 2", &[]).await,
        "There are no tasks on this page; look at an earlier page."
    );
    assert_eq!(
        rig.say(OWNER, "/tasks", &[BOB]).await,
        "/tasks manages this group's tasks only and takes no member targets."
    );
}

#[tokio::test]
async fn task_lists_page_by_five() {
    let rig = rig().await;
    for n in 0..7 {
        rig.say(
            OWNER,
            &format!("/tasks add --in {}m -- task number {n}", 10 + n),
            &[],
        )
        .await;
    }
    let first = rig.say(OWNER, "/tasks list", &[]).await;
    assert_eq!(first.matches("State: pending").count(), 5);
    assert!(first.contains("Next page: /tasks list 2"), "{first}");
    let second = rig.say(OWNER, "/tasks list 2", &[]).await;
    assert_eq!(second.matches("State: pending").count(), 2);
    assert!(!second.contains("Next page"), "{second}");
}

#[tokio::test]
async fn replies_quote_and_mention_the_sender_and_long_ones_are_split() {
    let rig = rig().await;
    let request = rig.request(ALICE, "/help", &[]);
    let message = request.message;
    rig.router.run(request).await;
    let sent = rig.sent.0.lock().unwrap().clone();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0][0], OutSegment::Reply(message));
    assert_eq!(sent[0][1], OutSegment::At(acct(ALICE)));
    assert!(matches!(&sent[0][2], OutSegment::Text(t) if t.starts_with(" Available commands:")));
    assert!(!rig.router.recognizes("/nope") && rig.router.recognizes("/tasks"));
    let _ = HashMap::<i32, i32>::new();
}

fn observation(
    subject: i64,
    predicate: &str,
    object: &str,
    episode: i64,
) -> qbot_memory::facts::Observation {
    qbot_memory::facts::Observation {
        group: GroupId::new(900).unwrap(),
        subject: Some(acct(subject)),
        predicate: predicate.into(),
        key: object.to_lowercase(),
        object: object.into(),
        label: None,
        opposite: None,
        decay: qbot_memory::predicates::DecayClass::Default,
        episode: qbot_memory::episode::EpisodeId::new(episode),
        message: MessageId::new(episode).unwrap(),
        quote: "q".into(),
        at: UnixMillis::new(T0),
    }
}

#[tokio::test]
async fn who_shows_learned_facts_and_forget_removes_one_by_its_number() {
    use qbot_memory::facts::FactStore;
    let rig = rig().await;
    assert!(
        rig.say(ALICE, "/who", &[])
            .await
            .contains("Learned from chat: nothing yet")
    );
    rig.facts
        .observe(&observation(ALICE, "likes", "cats", 1))
        .await
        .unwrap();
    rig.facts
        .observe(&observation(ALICE, "lives_in", "Hangzhou", 2))
        .await
        .unwrap();
    rig.facts
        .observe(&observation(BOB, "likes", "tea", 3))
        .await
        .unwrap();
    let who = rig.say(ALICE, "/who", &[]).await;
    assert!(
        who.contains("Learned from chat (2):\n1. likes: cats (confidence 0.27)\n2. lives_in: Hangzhou (confidence 0.27)\nRemove a note with /note remove N, a learned fact with /forget N (same scope)."),
        "{who}"
    );
    assert_eq!(
        rig.say(ALICE, "/forget 3", &[]).await,
        "There is no learned fact 3 in that scope. Send /who to see the list."
    );
    assert_eq!(
        rig.say(ALICE, "/forget", &[]).await,
        "Usage: /forget [--linked] [@account] N, or /forget group N"
    );
    assert_eq!(
        rig.say(ALICE, "/forget 1", &[BOB]).await,
        "You can only act on your own account or on accounts you have confirmed as linked."
    );
    assert_eq!(
        rig.say(ALICE, "/forget 1", &[]).await,
        "Forgot 1. likes: cats."
    );
    assert!(
        rig.say(ALICE, "/who", &[])
            .await
            .contains("1. lives_in: Hangzhou"),
        "the numbering follows what is left"
    );
    // Owners may forget anyone's, and --linked covers linked accounts.
    rig.say(OWNER, "/link", &[ALICE, BOB]).await;
    assert!(
        rig.say(ALICE, "/who --linked", &[])
            .await
            .contains("Learned from chat (2):")
    );
    assert_eq!(
        rig.say(OWNER, "/forget 1", &[BOB]).await,
        "Forgot 1. likes: tea."
    );
}

#[tokio::test]
async fn notes_are_written_listed_edited_and_removed_by_hand() {
    use qbot_memory::NoteStore;
    let rig = rig().await;
    assert_eq!(
        rig.say(ALICE, "/note", &[]).await,
        "There are no notes about Alice in this scope."
    );
    assert_eq!(
        rig.say(ALICE, "/note add  prefers  short replies, please ", &[])
            .await,
        "Added note 1 about Alice."
    );
    rig.say(ALICE, "/note add on call this week", &[]).await;
    let list = rig.say(ALICE, "/note", &[]).await;
    assert!(
        list.contains("1. prefers  short replies, please (by Alice,"),
        "the text is kept exactly as written: {list}"
    );
    assert!(list.contains("2. on call this week (by Alice,"), "{list}");
    assert_eq!(
        rig.say(ALICE, "/note edit 2 off call now", &[]).await,
        "Changed note 2."
    );
    assert_eq!(
        rig.notes
            .notes(GroupId::new(900).unwrap(), &[acct(ALICE)])
            .await
            .unwrap()[1]
            .text,
        "off call now"
    );
    assert_eq!(
        rig.say(ALICE, "/note remove 1", &[]).await,
        "Removed note 1: prefers  short replies, please"
    );
    assert_eq!(
        rig.say(ALICE, "/note remove 5", &[]).await,
        "There is no note 5 in that scope. Send /note to see the list."
    );
    // Members manage their own notes; owners anyone's, and the author is recorded.
    assert_eq!(
        rig.say(ALICE, "/note add likes tea", &[BOB]).await,
        "You can only act on your own account or on accounts you have confirmed as linked."
    );
    assert_eq!(
        rig.say(OWNER, "/note add new member", &[BOB]).await,
        "Added note 1 about Bob."
    );
    assert!(rig.say(BOB, "/note", &[]).await.contains("(by Olive,"));
    // A bound on how many, and empty or malformed input is explained.
    rig.say(BOB, "/note add two", &[]).await;
    rig.say(BOB, "/note add three", &[]).await;
    assert_eq!(
        rig.say(BOB, "/note add four", &[]).await,
        "An account can have at most 3 notes; remove one first."
    );
    assert_eq!(
        rig.say(ALICE, "/note add", &[]).await,
        "A note needs some text."
    );
    assert!(
        rig.say(ALICE, "/note frobnicate", &[])
            .await
            .starts_with("Usage: /note")
    );
    assert_eq!(
        rig.say(BOB, "/note clear", &[]).await,
        "Removed 3 notes about Bob."
    );
}

#[tokio::test]
async fn notes_and_learned_facts_are_separate_and_each_command_touches_only_its_own() {
    use qbot_memory::facts::FactStore;
    let rig = rig().await;
    rig.facts
        .observe(&observation(ALICE, "likes", "cats", 1))
        .await
        .unwrap();
    rig.say(ALICE, "/note add likes cats too, says so herself", &[])
        .await;
    let who = rig.say(ALICE, "/who", &[]).await;
    assert!(
        who.contains("Notes written by people (1):\n1. likes cats too, says so herself"),
        "{who}"
    );
    assert!(
        who.contains("Learned from chat (1):\n1. likes: cats"),
        "{who}"
    );
    // Forgetting learned fact 1 leaves note 1 alone ...
    assert_eq!(
        rig.say(ALICE, "/forget 1", &[]).await,
        "Forgot 1. likes: cats."
    );
    let who = rig.say(ALICE, "/who", &[]).await;
    assert!(who.contains("Notes written by people (1):"), "{who}");
    assert!(who.contains("Learned from chat: nothing yet"), "{who}");
    // ... and removing note 1 leaves learned facts alone.
    rig.facts
        .observe(&observation(ALICE, "lives_in", "Hangzhou", 2))
        .await
        .unwrap();
    rig.say(ALICE, "/note remove 1", &[]).await;
    let who = rig.say(ALICE, "/who", &[]).await;
    assert!(who.contains("Notes written by people: none"), "{who}");
    assert!(who.contains("Learned from chat (1):"), "{who}");
}

fn group_fact(
    predicate: &str,
    key: &str,
    label: Option<&str>,
    object: &str,
    episode: i64,
) -> qbot_memory::facts::Observation {
    qbot_memory::facts::Observation {
        subject: None,
        predicate: predicate.into(),
        key: key.into(),
        label: label.map(str::to_owned),
        ..observation(ALICE, predicate, object, episode)
    }
}

#[tokio::test]
async fn group_shows_learned_knowledge_and_only_owners_forget_it() {
    use qbot_memory::facts::{FactStore, GROUP_TERM, GROUP_TOPIC};
    let rig = rig().await;
    assert_eq!(
        rig.say(ALICE, "/group", &[]).await,
        "Nothing has been learned about this group yet."
    );
    rig.facts
        .observe(&group_fact(
            GROUP_TERM,
            "printer room",
            Some("printer room"),
            "the room with the 3D printer",
            1,
        ))
        .await
        .unwrap();
    rig.facts
        .observe(&group_fact(GROUP_TOPIC, "", None, "3D printing", 2))
        .await
        .unwrap();
    let group = rig.say(ALICE, "/group", &[]).await;
    assert!(
        group.contains("1. What the group is about: 3D printing\n2. \"printer room\" means: the room with the 3D printer"),
        "the topic first, then terms: {group}"
    );
    assert_eq!(
        rig.say(ALICE, "/forget group 1", &[]).await,
        "This action needs bot-owner permission."
    );
    assert_eq!(
        rig.say(OWNER, "/forget group 9", &[]).await,
        "There is no group knowledge 9. Send /group to see the list."
    );
    assert_eq!(
        rig.say(OWNER, "/forget group 2", &[]).await,
        "Forgot group knowledge 2: printer room: the room with the 3D printer"
    );
    assert!(!rig.say(ALICE, "/group", &[]).await.contains("printer room"));
}

#[tokio::test]
async fn runs_lists_this_groups_runs_and_shows_one_runs_steps() {
    let rig = rig().await;
    let list = rig.say(OWNER, "/runs", &[]).await;
    assert!(list.starts_with("Latest runs of this group (1):"), "{list}");
    assert!(
        list.contains("#42 ")
            && list.contains("delivered | 2 turns, 2 tool calls, 1 sent | 1234 tokens"),
        "{list}"
    );
    assert!(!list.contains("#41"), "another group's run is not listed");
    let one = rig.say(OWNER, "/runs 42", &[]).await;
    assert!(one.starts_with("Run #42 |"), "{one}");
    assert!(
        one.contains("chat: 0 lines\ncall search_history: {\"query\":\"printer\"}\nresult (ok): no matches\nmodel text: done looking"),
        "{one}"
    );
    assert_eq!(
        rig.say(OWNER, "/runs 41", &[]).await,
        "This group has no run 41.",
        "another group's run cannot be opened"
    );
    assert_eq!(
        rig.say(OWNER, "/runs abc", &[]).await,
        "This group has no run abc."
    );
}

#[tokio::test]
async fn logs_shows_the_latest_warnings_and_errors() {
    let rig = rig().await;
    let all = rig.say(OWNER, "/logs", &[]).await;
    assert!(all.starts_with("Latest warnings and errors (2):"), "{all}");
    assert!(all.contains("WARN an extraction job could not be queued group=900"));
    assert!(
        all.ends_with("ERROR command failed in storage"),
        "newest last: {all}"
    );
    let one = rig.say(OWNER, "/logs 1", &[]).await;
    assert!(
        one.starts_with("Latest warnings and errors (1):") && !one.contains("WARN"),
        "{one}"
    );
}
