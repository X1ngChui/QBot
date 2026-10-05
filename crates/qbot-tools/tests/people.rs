#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use qbot_agent::{ChatView, RunState, Tool, ToolCx, ToolError, Trigger};
use qbot_context::{ChatLine, Speaker};
use qbot_core::{AccountId, Clock, GroupId, MemberNo, MessageId, RunId, UnixMillis};
use qbot_memory::episode::EpisodeId;
use qbot_memory::facts::{DecayPolicy, FactStore, MemoryFactStore, Observation};
use qbot_memory::identity::{AliasTarget, IdentityPolicy};
use qbot_memory::predicates::DecayClass;
use qbot_memory::{IdentityStore, MemoryIdentityStore};
use qbot_tools::{LookupArgs, LookupMember};

const DAY: i64 = 86_400_000;

/// The platform's current group name: account 11 shows as "Kitty".
struct Shown;
#[async_trait::async_trait]
impl qbot_agent::Directory for Shown {
    async fn display_name(&self, _: GroupId, account: AccountId) -> Option<String> {
        (account.get() == 11).then(|| "Kitty".to_owned())
    }
}

struct At(i64);
impl Clock for At {
    fn now(&self) -> UnixMillis {
        UnixMillis::new(self.0)
    }
}

fn account(n: i64) -> AccountId {
    AccountId::new(n).unwrap()
}

fn observation(subject: i64, predicate: &str, object: &str, episode: i64, day: i64) -> Observation {
    Observation {
        group: GroupId::new(1).unwrap(),
        subject: Some(account(subject)),
        predicate: predicate.into(),
        key: object.to_lowercase(),
        object: object.into(),
        label: None,
        opposite: None,
        decay: DecayClass::Default,
        episode: EpisodeId::new(episode),
        message: MessageId::new(episode).unwrap(),
        quote: "q".into(),
        at: UnixMillis::new(day * DAY),
    }
}

#[tokio::test]
async fn a_member_lookup_lists_names_and_current_facts_of_the_whole_person() {
    let group = GroupId::new(1).unwrap();
    let identity = Arc::new(MemoryIdentityStore::new(IdentityPolicy::default()));
    for a in [11, 12, 13] {
        identity.seen(account(a), UnixMillis::new(0)).await.unwrap();
    }
    identity.merge(account(11), account(12)).await.unwrap();
    identity
        .set_name(
            group,
            "Kit",
            AliasTarget::Account(account(11)),
            UnixMillis::new(0),
        )
        .await
        .unwrap();
    let facts = Arc::new(MemoryFactStore::new());
    facts
        .observe(&observation(11, "lives_in", "Hangzhou", 1, 0))
        .await
        .unwrap();
    facts
        .observe(&observation(12, "likes", "cats", 2, 10))
        .await
        .unwrap();
    facts
        .observe(&observation(12, "likes", "cats", 3, 30))
        .await
        .unwrap();
    facts
        .observe(&observation(13, "likes", "tea", 4, 0))
        .await
        .unwrap();

    let notes = Arc::new(qbot_memory::MemoryNoteStore::default());
    {
        use qbot_memory::NoteStore;
        notes
            .add(
                group,
                account(12),
                "runs the Friday game night",
                account(1),
                UnixMillis::new(20 * DAY),
            )
            .await
            .unwrap();
        notes
            .add(
                group,
                account(13),
                "someone else's note",
                account(1),
                UnixMillis::new(0),
            )
            .await
            .unwrap();
    }
    let tool = LookupMember {
        directory: Arc::new(Shown),
        identity,
        facts,
        notes,
        decay: DecayPolicy::default(),
    };
    let mut view = ChatView::default();
    view.absorb(&[ChatLine {
        message: MessageId::new(5).unwrap(),
        speaker: Speaker::Member {
            account: account(11),
            number: MemberNo::new(2),
        },
        at: UnixMillis::new(0),
        text: "hi".into(),
    }]);
    let state = RunState::default();
    let trigger = Trigger::Addressed {
        message: MessageId::new(5).unwrap(),
        sender: account(11),
    };
    let clock = At(30 * DAY);
    let cx = ToolCx {
        group,
        run: RunId::new(1),
        trigger: &trigger,
        view: &view,
        state: &state,
        clock: &clock,
    };

    let out = tool.call(&cx, LookupArgs { member: 2 }).await.unwrap();
    let qbot_context::Part::Text(text) = &out.content[0] else {
        panic!()
    };
    assert_eq!(
        text,
        "member:2\n\
         shown in this group as: Kitty\n\
         other names: kit (confirmed)\n\
         notes written by members (their words, not checked):\n\
         - runs the Friday game night (written 10 days ago)\n\
         learned from chat (inferred automatically):\n\
         - likes: cats (confidence 0.43, last confirmed today)\n\
         - lives_in: Hangzhou (confidence 0.13, last confirmed 30 days ago)",
        "notes and facts of linked accounts are the person's, listed apart; another member's are not shown"
    );
    assert!(matches!(
        tool.call(&cx, LookupArgs { member: 9 }).await,
        Err(ToolError::InvalidArguments(_))
    ));
}
