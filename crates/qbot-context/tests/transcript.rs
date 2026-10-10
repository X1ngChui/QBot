#![allow(clippy::unwrap_used, clippy::expect_used)]

use qbot_context::*;
use qbot_core::{AccountId, CallId, ItemSeq, MemberNo, MessageId, UnixMillis};
use serde_json::json;

fn cid(s: &str) -> CallId {
    CallId::new(s).unwrap()
}

fn call(id: &str) -> AssistantPart {
    AssistantPart::Call(ToolCall {
        id: cid(id),
        name: "tool".into(),
        arguments: json!({}),
    })
}

fn turn(ids: &[&str]) -> Item {
    Item::Assistant(AssistantTurn::new(ids.iter().map(|id| call(id)).collect()))
}

fn result(id: &str, text: &str) -> Item {
    Item::ToolResult(ToolResult {
        call_id: cid(id),
        outcome: Outcome::Ok,
        content: vec![Part::Text(text.into())],
    })
}

fn instruction() -> Item {
    Item::Instruction(Instruction {
        role: InstructionRole::System,
        text: "rules".into(),
        template_hash: "h".into(),
    })
}

fn line(msg: i64, account: i64, text: &str) -> ChatLine {
    ChatLine {
        message: MessageId::new(msg).unwrap(),
        speaker: Speaker::Member {
            account: AccountId::new(account).unwrap(),
            number: MemberNo::new(account as u32),
        },
        at: UnixMillis::new(msg),
        text: text.into(),
    }
}

#[test]
fn a_stored_line_with_the_former_standing_field_still_reads() {
    // Run transcripts written before block state moved out of chat lines carry `standing`.
    let stored = json!({
        "message": 1,
        "speaker": {"member": {"account": 7, "number": 3, "standing": "blocked"}},
        "at": 5,
        "text": "hi"
    });
    let line: ChatLine = serde_json::from_value(stored).unwrap();
    assert_eq!(
        line.speaker,
        Speaker::Member {
            account: AccountId::new(7).unwrap(),
            number: MemberNo::new(3),
        }
    );
}

#[test]
fn new_chat_may_arrive_between_turns_but_not_while_calls_are_pending() {
    let mut t = Transcript::new();
    t.append(Item::Chat(ChatBatch::new(vec![line(1, 1, "window")])))
        .unwrap();
    t.append(turn(&["a"])).unwrap();
    assert!(matches!(
        t.append(Item::Chat(ChatBatch::new(vec![line(2, 1, "early")]))),
        Err(AppendError::ItemsWhilePending { .. })
    ));
    t.append(result("a", "r")).unwrap();
    t.append(Item::Chat(ChatBatch::new(vec![line(3, 1, "echo")])))
        .unwrap();
    let view = project(&t).unwrap();
    assert_eq!(view.items.len(), 4);
}

#[test]
fn a_batch_keeps_every_line_whoever_wrote_it() {
    let bot = ChatLine {
        speaker: Speaker::Bot,
        ..line(3, 1, "from bot")
    };
    let batch = ChatBatch::new(vec![line(1, 1, "a"), line(2, 2, "other"), bot]);
    let texts: Vec<_> = batch.lines().iter().map(|l| l.text.as_str()).collect();
    assert_eq!(texts, ["a", "other", "from bot"]);
}

#[test]
fn nothing_but_results_may_follow_pending_calls() {
    let mut t = Transcript::new();
    t.append(turn(&["a", "b"])).unwrap();
    assert_eq!(
        t.append(turn(&["c"])),
        Err(AppendError::ItemsWhilePending { pending: 2 })
    );
    assert!(matches!(
        t.append(Item::Chat(ChatBatch::new(vec![]))),
        Err(AppendError::ItemsWhilePending { .. })
    ));
    t.append(result("b", "rb")).unwrap();
    t.append(result("a", "ra")).unwrap();
    assert!(t.is_quiescent());
}

#[test]
fn results_must_match_an_open_call_exactly_once() {
    let mut t = Transcript::new();
    assert_eq!(
        t.append(result("x", "")),
        Err(AppendError::UnknownCall(cid("x")))
    );
    t.append(turn(&["a"])).unwrap();
    t.append(result("a", "")).unwrap();
    assert_eq!(
        t.append(result("a", "")),
        Err(AppendError::AlreadyResolved(cid("a")))
    );
}

#[test]
fn call_ids_are_unique_across_the_transcript() {
    let mut t = Transcript::new();
    assert_eq!(
        t.append(turn(&["a", "a"])),
        Err(AppendError::DuplicateCallId(cid("a")))
    );
    t.append(turn(&["a"])).unwrap();
    t.append(result("a", "")).unwrap();
    assert_eq!(
        t.append(turn(&["a"])),
        Err(AppendError::DuplicateCallId(cid("a")))
    );
    assert_eq!(
        t.append(Item::Assistant(AssistantTurn::new(vec![]))),
        Err(AppendError::EmptyTurn)
    );
}

#[test]
fn projection_orders_results_by_call_order_and_requires_quiescence() {
    let mut t = Transcript::new();
    t.append(turn(&["a", "b"])).unwrap();
    assert_eq!(
        project(&t).unwrap_err(),
        ProjectError::PendingCalls { pending: 2 }
    );
    t.append(result("b", "rb")).unwrap();
    t.append(result("a", "ra")).unwrap();
    let view = project(&t).unwrap();
    let ids: Vec<_> = view
        .items
        .iter()
        .filter_map(|i| match i {
            ViewItem::ToolResult(r) => Some(r.call_id.as_str().to_owned()),
            _ => None,
        })
        .collect();
    assert_eq!(ids, ["a", "b"]);
}

#[test]
fn summary_replaces_a_closed_range_and_originals_remain() {
    let mut t = Transcript::new();
    t.append(instruction()).unwrap(); // 0
    t.append(turn(&["a"])).unwrap(); // 1
    t.append(result("a", "x")).unwrap(); // 2
    t.append(turn(&["b"])).unwrap(); // 3
    t.append(result("b", "y")).unwrap(); // 4
    let summary = |from, to| {
        Item::Summary(Summary {
            from: ItemSeq::new(from),
            to: ItemSeq::new(to),
            text: "s".into(),
        })
    };

    assert_eq!(
        t.append(summary(0, 3)),
        Err(AppendError::SummaryCoversInstruction)
    );
    assert_eq!(
        t.append(summary(1, 2)),
        Err(AppendError::SummarySplitsCall(cid("a")))
    );
    assert_eq!(
        t.append(summary(2, 3)),
        Err(AppendError::SummarySplitsCall(cid("a")))
    );
    assert!(matches!(
        t.append(summary(1, 99)),
        Err(AppendError::BadSummaryRange { .. })
    ));
    t.append(summary(1, 3)).unwrap();
    assert_eq!(
        t.append(summary(1, 3)),
        Err(AppendError::SummaryAlreadyCovered)
    );
    assert_eq!(
        t.append(summary(2, 5)),
        Err(AppendError::SummarySplitsCall(cid("a")))
    );

    let view = project(&t).unwrap();
    assert!(matches!(view.items[0], ViewItem::Instruction(_)));
    assert!(matches!(&view.items[1], ViewItem::Summary(s) if s == "s"));
    assert_eq!(view.items.len(), 4); // instruction, summary, call b, result b
    assert_eq!(t.items().len(), 6); // originals plus the summary item remain stored
}

#[test]
fn a_wider_summary_supersedes_a_narrower_one() {
    let mut t = Transcript::new();
    for id in ["a", "b"] {
        t.append(turn(&[id])).unwrap();
        t.append(result(id, "r")).unwrap();
    }
    let s = |from, to, text: &str| {
        Item::Summary(Summary {
            from: ItemSeq::new(from),
            to: ItemSeq::new(to),
            text: text.into(),
        })
    };
    t.append(s(0, 2, "narrow")).unwrap();
    t.append(s(0, 4, "wide")).unwrap();
    let view = project(&t).unwrap();
    assert_eq!(view.items, vec![ViewItem::Summary("wide".into())]);
}

#[test]
fn resume_closes_pending_calls_as_interrupted() {
    let mut t = Transcript::new();
    t.append(turn(&["a", "b"])).unwrap();
    t.append(result("a", "done")).unwrap();
    assert_eq!(t.close_interrupted(), 1);
    assert!(t.is_quiescent());
    assert_eq!(t.close_interrupted(), 0);
    let view = project(&t).unwrap();
    assert!(matches!(
        &view.items[2],
        ViewItem::ToolResult(r) if r.call_id == cid("b") && r.outcome == Outcome::Interrupted
    ));
    assert!(matches!(
        t.items().last(),
        Some(Item::Meta(Meta::CallsInterrupted {
            interrupted_calls: 1
        }))
    ));
}

#[test]
fn storage_round_trip_is_lossless_and_revalidated() {
    let mut t = Transcript::new();
    t.append(instruction()).unwrap();
    t.append(turn(&["a"])).unwrap();
    t.append(result("a", "r")).unwrap();
    let json = serde_json::to_string(&t).unwrap();
    let back: Transcript = serde_json::from_str(&json).unwrap();
    assert_eq!(back, t);
    assert_eq!(
        project(&back).unwrap().digest(),
        project(&t).unwrap().digest()
    );

    // A stored log with an orphan result must be rejected on load.
    let orphan = serde_json::to_string(&vec![result("zzz", "")]).unwrap();
    assert!(serde_json::from_str::<Transcript>(&orphan).is_err());
}

#[test]
fn digest_changes_when_the_view_changes() {
    let mut t = Transcript::new();
    t.append(turn(&["a"])).unwrap();
    t.append(result("a", "r")).unwrap();
    let before = project(&t).unwrap().digest();
    t.append(turn(&["b"])).unwrap();
    t.append(result("b", "r")).unwrap();
    assert_ne!(before, project(&t).unwrap().digest());
}

#[test]
fn an_interruption_stored_under_its_earlier_name_still_loads() {
    let stored = serde_json::json!({"kind": "meta", "data": {"kind": "resumed", "data": {"interrupted_calls": 2}}});
    let item: qbot_context::Item = serde_json::from_value(stored).unwrap();
    assert_eq!(
        item,
        qbot_context::Item::Meta(qbot_context::Meta::CallsInterrupted {
            interrupted_calls: 2
        })
    );
}
