#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;
use std::sync::Arc;

use async_trait::async_trait;
use qbot_agent::{ArchiveCursor, Chain, ContextSource, EnvError, RecapWhen, Trigger};
use qbot_context::{ChatBatch, ChatLine, InstructionRole, Outcome, Speaker};
use qbot_core::{
    AccountId, ChainId, GroupId, HistoryWindow, MemberNo, MessageId, TimerId, UnixMillis,
};
use qbot_llm::Renderer;
use qbot_prompt::{
    GroupPeople, HistorySource, PeopleSource, Persona, PersonaError, Personas, PromptContext,
    PromptError, PromptRenderer, PromptSettings, Template, render_template,
};

#[test]
fn every_template_uses_exactly_its_declared_slots_and_no_cjk() {
    for template in Template::ALL {
        let declared: BTreeSet<String> = template.slots().iter().map(|s| (*s).to_owned()).collect();
        assert_eq!(template.referenced_slots(), declared, "{template:?}");
        assert!(
            template
                .text()
                .chars()
                .all(|c| !('\u{2E80}'..='\u{9FFF}').contains(&c)
                    && !('\u{FF00}'..='\u{FFEF}').contains(&c)),
            "{template:?} contains CJK or fullwidth text"
        );
        assert_eq!(template.hash().len(), 12);
    }
}

#[test]
fn rendering_demands_the_exact_slot_set_and_never_rescans_values() {
    assert_eq!(
        render_template(Template::PersonaBlock, &[]),
        Err(PromptError::MissingSlot {
            template: Template::PersonaBlock,
            slot: "persona"
        })
    );
    assert!(matches!(
        render_template(Template::Legend, &[("extra", "x")]),
        Err(PromptError::UnknownSlot { .. })
    ));
    let text = render_template(
        Template::PersonaBlock,
        &[("persona", "call me {{persona}}")],
    )
    .unwrap();
    assert!(text.ends_with("call me {{persona}}"), "{text}");
}

fn line(message: i64, account: i64, number: u32, at: i64, text: &str) -> ChatLine {
    ChatLine {
        message: MessageId::new(message).unwrap(),
        speaker: Speaker::Member {
            account: AccountId::new(account).unwrap(),
            number: MemberNo::new(number),
        },
        at: UnixMillis::new(at),
        text: text.into(),
    }
}

#[test]
fn chat_lines_show_id_local_time_and_member_number() {
    // 2023-11-14 22:13:20 UTC is 2023-11-15 06:13 in Shanghai.
    let renderer = PromptRenderer::new(jiff::tz::TimeZone::get("Asia/Shanghai").unwrap());
    let at = 1_700_000_000_000;
    let batch = ChatBatch::new(vec![
        line(5, 1, 3, at, "hello [at:bot]"),
        line(6, 2, 4, at, "rude"),
        ChatLine {
            message: MessageId::new(7).unwrap(),
            speaker: Speaker::Bot,
            at: UnixMillis::new(at),
            text: "ok".into(),
        },
    ]);
    assert_eq!(
        renderer.chat(&batch),
        "[msg:5] 11-15 06:13 member:3: hello [at:bot]\n[msg:6] 11-15 06:13 member:4: rude\n[msg:7] 11-15 06:13 you: ok"
    );
    assert_eq!(renderer.outcome_note(&Outcome::Ok), None);
    assert!(
        renderer
            .outcome_note(&Outcome::Interrupted)
            .unwrap()
            .contains("unknown")
    );
}

fn persona(name: &str, knowledge: &str) -> Persona {
    Persona {
        name: name.into(),
        system_prompt: format!("You are {name}."),
        group_knowledge: knowledge.into(),
    }
}

struct FixedHistory(Vec<ChatLine>);

#[async_trait]
impl HistorySource for FixedHistory {
    async fn last_ordinal(&self, _: GroupId) -> Result<u64, EnvError> {
        Ok(self.0.len() as u64)
    }

    async fn lines_from(
        &self,
        _: GroupId,
        first: u64,
    ) -> Result<(Vec<(u64, ChatLine)>, ArchiveCursor), EnvError> {
        let lines = (1..)
            .zip(self.0.iter().cloned())
            .filter(|(o, _)| *o >= first)
            .collect();
        Ok((lines, ArchiveCursor(self.0.len() as u64)))
    }
}

struct Clock0;
impl qbot_core::Clock for Clock0 {
    fn now(&self) -> UnixMillis {
        UnixMillis::new(1_700_000_000_000)
    }
}

fn context(personas: Personas) -> PromptContext {
    let history = FixedHistory(vec![line(5, 1, 3, 1_700_000_000_000, "hi")]);
    PromptContext::new(
        Arc::new(history),
        personas,
        Arc::new(Clock0),
        PromptSettings {
            window: HistoryWindow::default(),
            timezone: "Asia/Shanghai".into(),
        },
    )
    .unwrap()
}

#[tokio::test]
async fn instructions_are_stable_per_group_and_the_trigger_note_carries_what_varies() {
    let g = GroupId::new(900).unwrap();
    let other = GroupId::new(901).unwrap();
    let mut personas = Personas::single(persona("Bobo", ""));
    let ctx_default = context(personas.clone());
    let a = ctx_default
        .open(
            g,
            &Trigger::Addressed {
                message: MessageId::new(5).unwrap(),
                sender: AccountId::new(1).unwrap(),
            },
        )
        .await
        .unwrap();
    let b = ctx_default
        .open(
            g,
            &Trigger::Addressed {
                message: MessageId::new(5).unwrap(),
                sender: AccountId::new(1).unwrap(),
            },
        )
        .await
        .unwrap();
    assert_eq!(
        a.instructions, b.instructions,
        "the prefix is identical across runs"
    );
    assert_eq!(
        a.instructions.len(),
        3,
        "rules, reading guide, persona; no knowledge block when empty"
    );
    assert!(
        a.instructions
            .iter()
            .all(|i| i.role == InstructionRole::System)
    );
    assert!(a.instructions[0].text.starts_with("You are Bobo,"));
    let note = a.trigger_note.unwrap();
    assert_eq!(note.role, InstructionRole::Trigger);
    assert!(
        note.text.contains("2023-11-15 06:13 (Asia/Shanghai)"),
        "{}",
        note.text
    );
    assert!(note.text.contains("member:3 in [msg:5]"), "{}", note.text);
    assert_eq!(a.window.len(), 1);

    // A per-group persona replaces the default for that group only, and adds its background.
    let _ = &mut personas;
    let dir = std::env::temp_dir().join(format!("qbot-persona-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("default.toml"),
        "name = \"Bobo\"\nsystem_prompt = \"Be Bobo.\"\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("group_900.toml"),
        "name = \"Cici\"\nsystem_prompt = \"Be Cici.\"\ngroup_knowledge = \"A chess club.\"\n",
    )
    .unwrap();
    let loaded = Personas::load(&dir).unwrap();
    let ctx = context(loaded);
    let special = ctx.instructions(g).unwrap();
    assert_eq!(special.len(), 4);
    assert!(
        special[0].text.starts_with("You are Cici,") && special[3].text.contains("A chess club.")
    );
    assert!(
        ctx.instructions(other).unwrap()[0]
            .text
            .starts_with("You are Bobo,")
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn a_wake_note_states_the_task_and_that_it_is_not_a_message() {
    let ctx = context(Personas::single(persona("Bobo", "")));
    let opened = ctx
        .open(
            GroupId::new(900).unwrap(),
            &Trigger::Wake {
                timer: TimerId::new(12),
                intent: "remind the group about the meeting".into(),
                chain: Chain {
                    id: ChainId::new(3),
                    depth: 2,
                },
            },
        )
        .await
        .unwrap();
    let note = opened.trigger_note.unwrap().text;
    assert!(
        note.contains("Task 12, chain depth 2")
            && note.contains("remind the group about the meeting"),
        "{note}"
    );
    assert!(note.contains("not triggered by a new message"));
}

#[test]
fn persona_loading_reports_every_problem() {
    let dir = std::env::temp_dir().join(format!("qbot-persona-bad-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("group_x.toml"),
        "name = \"A\"\nsystem_prompt = \"B\"\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("group_5.toml"),
        "name = \"\"\nsystem_prompt = \"B\"\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("group_6.toml"),
        "name = \"A\"\nsystem_prompt = \"B\"\nsurprise = 1\n",
    )
    .unwrap();
    let errors = Personas::load(&dir).unwrap_err();
    assert!(
        errors
            .iter()
            .any(|e| matches!(e, PersonaError::NoDefault { .. }))
    );
    assert!(
        errors
            .iter()
            .any(|e| matches!(e, PersonaError::BadGroupFile { .. }))
    );
    assert_eq!(
        errors
            .iter()
            .filter(|e| matches!(e, PersonaError::Invalid { .. }))
            .count(),
        2
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn learned_group_knowledge_joins_the_stable_instructions() {
    use qbot_memory::episode::EpisodeId;
    use qbot_memory::facts::{FactStore, GROUP_TERM, GROUP_TOPIC, MemoryFactStore, Observation};
    use qbot_memory::predicates::DecayClass;
    let g = GroupId::new(900).unwrap();
    let facts = Arc::new(MemoryFactStore::new());
    let observe =
        |predicate: &str, key: &str, label: Option<&str>, text: &str, episode: i64| Observation {
            group: g,
            subject: None,
            predicate: predicate.into(),
            key: key.into(),
            object: text.into(),
            label: label.map(Into::into),
            opposite: None,
            decay: DecayClass::Default,
            episode: EpisodeId::new(episode),
            message: MessageId::new(episode).unwrap(),
            quote: "q".into(),
            at: UnixMillis::new(0),
        };
    let ctx = context(Personas::single(persona("Bobo", "")))
        .with_knowledge(Arc::new(qbot_prompt::FactKnowledge::new(facts.clone(), 40)));
    let trigger = Trigger::Addressed {
        message: MessageId::new(5).unwrap(),
        sender: AccountId::new(1).unwrap(),
    };
    assert_eq!(
        ctx.open(g, &trigger).await.unwrap().instructions.len(),
        3,
        "no knowledge, no block"
    );

    facts
        .observe(&observe(GROUP_TERM, "zz", Some("ZZ"), "sleeping", 1))
        .await
        .unwrap();
    facts
        .observe(&observe(GROUP_TERM, "gg", Some("GG"), "good game", 2))
        .await
        .unwrap();
    facts
        .observe(&observe(GROUP_TOPIC, "", None, "a chess club", 3))
        .await
        .unwrap();
    let opened = ctx.open(g, &trigger).await.unwrap();
    assert_eq!(opened.instructions.len(), 4);
    let block = &opened.instructions[3].text;
    assert!(
        block.starts_with("## What you have learned about this group"),
        "{block}"
    );
    assert!(
        block.ends_with("- The group is about: a chess club\n- \"GG\" means: good game\n- \"ZZ\" means: sleeping"),
        "the topic, then terms in a stable order: {block}"
    );
    assert_eq!(
        ctx.open(g, &trigger).await.unwrap().instructions,
        opened.instructions,
        "unchanged knowledge, unchanged prefix"
    );
}

struct FixedPeople(GroupPeople);

#[async_trait]
impl PeopleSource for FixedPeople {
    async fn people(&self, _: GroupId, _: UnixMillis) -> Result<GroupPeople, EnvError> {
        Ok(self.0.clone())
    }
}

#[tokio::test]
async fn blocked_and_linked_members_are_listed_by_member_number() {
    let g = GroupId::new(900).unwrap();
    let trigger = Trigger::Addressed {
        message: MessageId::new(5).unwrap(),
        sender: AccountId::new(1).unwrap(),
    };
    let with = |people: GroupPeople| {
        context(Personas::single(persona("Bobo", ""))).with_people(Arc::new(FixedPeople(people)))
    };
    let no = MemberNo::new;
    assert_eq!(
        with(GroupPeople::default())
            .open(g, &trigger)
            .await
            .unwrap()
            .instructions
            .len(),
        3,
        "nobody blocked or linked, no block"
    );

    let opened = with(GroupPeople {
        blocked: vec![no(4), no(17)],
        same_person: vec![vec![no(2), no(9)], vec![no(3), no(5), no(8)]],
    })
    .open(g, &trigger)
    .await
    .unwrap();
    assert_eq!(opened.instructions.len(), 4);
    let block = &opened.instructions[3].text;
    assert!(block.starts_with("## People in this group"), "{block}");
    let entries: Vec<&str> = block.lines().filter(|l| l.starts_with("- ")).collect();
    assert_eq!(entries.len(), 3, "{block}");
    assert!(entries[0].starts_with("- Blocked: member:4, member:17."));
    assert!(entries[1].contains("member:2, member:9."));
    assert!(entries[2].contains("member:3, member:5, member:8."));
}

#[tokio::test]
async fn only_the_most_recently_confirmed_terms_are_shown_in_a_stable_order() {
    use qbot_memory::episode::EpisodeId;
    use qbot_memory::facts::{FactStore, GROUP_TERM, GROUP_TOPIC, MemoryFactStore, Observation};
    use qbot_memory::predicates::DecayClass;
    use qbot_prompt::KnowledgeSource;
    let g = GroupId::new(901).unwrap();
    let facts = Arc::new(MemoryFactStore::new());
    let observe = |predicate: &str, key: &str, text: &str, episode: i64, at: i64| Observation {
        group: g,
        subject: None,
        predicate: predicate.into(),
        key: key.into(),
        object: text.into(),
        label: None,
        opposite: None,
        decay: DecayClass::Default,
        episode: EpisodeId::new(episode),
        message: MessageId::new(episode).unwrap(),
        quote: "q".into(),
        at: UnixMillis::new(at),
    };
    // Learned in this order: old "aa", then "zz", then "mm".
    for (n, (key, at)) in [("aa", 100), ("zz", 200), ("mm", 300)]
        .into_iter()
        .enumerate()
    {
        facts
            .observe(&observe(GROUP_TERM, key, key, n as i64 + 1, at))
            .await
            .unwrap();
    }
    facts
        .observe(&observe(GROUP_TOPIC, "", "a chess club", 9, 50))
        .await
        .unwrap();
    let source = qbot_prompt::FactKnowledge::new(facts.clone(), 2);
    let shown = |items: Vec<qbot_prompt::Knowledge>| -> Vec<String> {
        items
            .into_iter()
            .map(|k| k.term.unwrap_or_else(|| "(topic)".into()))
            .collect()
    };
    assert_eq!(
        shown(source.knowledge(g).await.unwrap()),
        ["(topic)", "mm", "zz"],
        "the two most recently confirmed terms, in key order; the old one is left out"
    );
    // Confirming the old term again makes it recent: it is shown, the least recent goes.
    facts
        .observe(&observe(GROUP_TERM, "aa", "aa", 4, 400))
        .await
        .unwrap();
    assert_eq!(
        shown(source.knowledge(g).await.unwrap()),
        ["(topic)", "aa", "mm"]
    );
}

/// A context over `n` lines (ordinal = message id), batches of two: the newest two batches
/// verbatim, the two before them as summaries.
async fn tiered(n: i64, episodes: &[(u64, u64, &str)]) -> PromptContext {
    use qbot_memory::{EpisodeStore, MemoryEpisodeStore, conformance::episode};

    let g = GroupId::new(900).unwrap();
    let lines: Vec<ChatLine> = (1..=n)
        .map(|m| line(m, 1, 3, 1_700_000_000_000, &format!("line {m}")))
        .collect();
    let store = Arc::new(MemoryEpisodeStore::default());
    for (first, last, title) in episodes {
        store
            .insert(&episode(g, *first, *last, title), &[1.0, 0.0], "m")
            .await
            .unwrap();
    }
    PromptContext::new(
        Arc::new(FixedHistory(lines)),
        Personas::single(persona("Bobo", "")),
        Arc::new(Clock0),
        PromptSettings {
            window: HistoryWindow {
                batch_lines: 2,
                raw_batches: 2,
                summary_batches: 2,
            },
            timezone: "UTC".into(),
        },
    )
    .unwrap()
    .with_episodes(store)
}

fn addressed(message: i64) -> Trigger {
    Trigger::Addressed {
        message: MessageId::new(message).unwrap(),
        sender: AccountId::new(1).unwrap(),
    }
}

#[tokio::test]
async fn history_is_raw_then_summarized_then_left_out_by_batch_count() {
    let g = GroupId::new(900).unwrap();
    // 13 lines: batch 6 (13-14) is being filled. Raw: batches 5-6 (lines 11-13). Summaries:
    // batches 3-4 (lines 7-10). Lines 1-6 are left out, except that an episode ending in the
    // summary tier is shown whole, so its lines are loaded from where it begins.
    let ctx = tiered(
        13,
        &[
            (1, 4, "too old"),
            (5, 8, "trip"),
            (9, 10, "lunch"),
            (11, 12, "now"),
        ],
    )
    .await;
    let opened = ctx.open(g, &addressed(13)).await.unwrap();
    let texts: Vec<_> = opened.window.iter().map(|l| l.text.as_str()).collect();
    assert_eq!(texts.first(), Some(&"line 5"), "nothing older is loaded");
    assert_eq!(texts.len(), 9);
    let recaps: Vec<_> = opened
        .recaps
        .iter()
        .map(|r| (r.lines.clone(), r.when))
        .collect();
    assert_eq!(
        recaps,
        vec![
            (0..4, RecapWhen::Open),
            (4..6, RecapWhen::Open),
            (6..8, RecapWhen::Overflow),
        ],
        "summary-tier episodes apply at once; a raw-tier episode only as the fallback"
    );
    assert!(opened.recaps[0].text.contains("summary of trip"));
    assert!(!opened.recaps.iter().any(|r| r.text.contains("too old")));

    // The trigger is never summarized, even by the fallback.
    let opened = ctx.open(g, &addressed(12)).await.unwrap();
    assert_eq!(
        opened.recaps.len(),
        2,
        "the episode holding the trigger stays raw"
    );

    // The same history always gives the same view.
    assert_eq!(ctx.open(g, &addressed(13)).await.unwrap(), {
        ctx.open(g, &addressed(13)).await.unwrap()
    });
}

#[tokio::test]
async fn summary_tier_lines_without_an_episode_yet_stay_verbatim() {
    let g = GroupId::new(900).unwrap();
    let ctx = tiered(13, &[(9, 10, "lunch")]).await;
    let opened = ctx.open(g, &addressed(13)).await.unwrap();
    let texts: Vec<_> = opened.window.iter().map(|l| l.text.as_str()).collect();
    assert_eq!(
        texts.first(),
        Some(&"line 7"),
        "the summary tier starts at batch 3"
    );
    let recaps: Vec<_> = opened.recaps.iter().map(|r| r.lines.clone()).collect();
    assert_eq!(
        recaps,
        vec![2..4],
        "lines 7-8 have no episode yet and are shown as they are"
    );

    // An episode that reaches into the raw tier is not a summary-tier episode.
    let ctx = tiered(13, &[(9, 12, "straddles")]).await;
    let opened = ctx.open(g, &addressed(13)).await.unwrap();
    assert_eq!(
        opened.recaps.iter().map(|r| r.when).collect::<Vec<_>>(),
        vec![RecapWhen::Overflow]
    );

    // A new batch moves every boundary by one batch.
    let ctx = tiered(15, &[]).await;
    let opened = ctx.open(g, &addressed(15)).await.unwrap();
    assert_eq!(
        opened.window.first().map(|l| l.text.as_str()),
        Some("line 9")
    );
}

/// Display names as a platform would report them; accounts not listed have none.
struct Shown(Vec<(i64, &'static str)>);

#[async_trait]
impl qbot_agent::Directory for Shown {
    async fn display_name(&self, _: GroupId, account: AccountId) -> Option<String> {
        self.0
            .iter()
            .find(|(a, _)| *a == account.get())
            .map(|(_, n)| (*n).to_owned())
    }
}

#[tokio::test]
async fn the_trigger_note_names_the_members_in_the_chat_so_numbers_stay_internal() {
    let g = GroupId::new(900).unwrap();
    let at = 1_700_000_000_000;
    let history = || {
        Arc::new(FixedHistory(vec![
            line(1, 22, 7, at, "hello"),
            line(2, 11, 2, at, "[at:bot] who was that?"),
            ChatLine {
                message: MessageId::new(3).unwrap(),
                speaker: Speaker::Bot,
                at: UnixMillis::new(at),
                text: "hi".into(),
            },
            line(4, 22, 7, at, "me again"),
        ]))
    };
    let ctx = |directory: Option<Arc<dyn qbot_agent::Directory>>| {
        let ctx = PromptContext::new(
            history(),
            Personas::single(persona("Bobo", "")),
            Arc::new(Clock0),
            PromptSettings {
                window: HistoryWindow::default(),
                timezone: "Asia/Shanghai".into(),
            },
        )
        .unwrap();
        match directory {
            Some(d) => ctx.with_directory(d),
            None => ctx,
        }
    };
    let trigger = Trigger::Addressed {
        message: MessageId::new(2).unwrap(),
        sender: AccountId::new(11).unwrap(),
    };

    // Account 11 is shown as a name with brackets; account 22 has no name the platform can give.
    let named = ctx(Some(Arc::new(Shown(vec![(11, "Ali[ce]")]))))
        .open(g, &trigger)
        .await
        .unwrap();
    let note = named.trigger_note.unwrap().text;
    let listed: Vec<&str> = note
        .lines()
        .filter(|l| l.starts_with("- member:"))
        .collect();
    assert_eq!(
        listed,
        [
            "- member:2 is Ali\u{ff3b}ce\u{ff3d}",
            "- member:7: no name available; mention them with [at:7], quote their message, or describe them (\"the one who posted the link\"), but do not call them member:7"
        ],
        "each speaker once, in member order, brackets neutralized, the bot left out: {note}"
    );

    // Names vary from run to run, so they live in the trigger note, never in the cached prefix.
    let plain = ctx(None).open(g, &trigger).await.unwrap();
    assert_eq!(named.instructions, plain.instructions);
    let plain_note = plain.trigger_note.unwrap().text;
    assert!(!plain_note.contains("- member:"), "{plain_note}");
    assert!(!plain_note.contains("called in the group"), "{plain_note}");
}
