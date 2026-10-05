#![allow(clippy::unwrap_used, clippy::expect_used)]

use qbot_context::{
    AssistantPart, AssistantTurn, ChatBatch, ChatLine, ErrorKind, Instruction, InstructionRole,
    Item, Outcome, Part, Speaker, ToolCall, ToolResult, Transcript, project,
};
use qbot_core::{AccountId, CallId, MemberNo, MessageId, UnixMillis};
use qbot_llm::{Content, ConvItem, Conversation, PlainRenderer, Role, ToolStatus};
use serde_json::json;

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

fn call(id: &str) -> Item {
    Item::Assistant(AssistantTurn::new(vec![AssistantPart::Call(ToolCall {
        id: CallId::new(id).unwrap(),
        name: "search_history".into(),
        arguments: json!({}),
    })]))
}

fn result(id: &str, outcome: Outcome, text: &str) -> Item {
    Item::ToolResult(ToolResult {
        call_id: CallId::new(id).unwrap(),
        outcome,
        content: vec![Part::Text(text.into())],
    })
}

#[test]
fn a_run_lowers_to_a_valid_provider_conversation() {
    let mut t = Transcript::new();
    t.append(Item::Instruction(Instruction {
        role: InstructionRole::System,
        text: "rules".into(),
        template_hash: "h".into(),
    }))
    .unwrap();
    t.append(Item::Instruction(Instruction {
        role: InstructionRole::Developer,
        text: "persona".into(),
        template_hash: "h".into(),
    }))
    .unwrap();
    t.append(Item::Chat(ChatBatch::new(vec![
        line(1, 1, "hello"),
        line(2, 2, "spam"),
    ])))
    .unwrap();
    t.append(call("a")).unwrap();
    t.append(result("a", Outcome::Ok, "long search output"))
        .unwrap();
    t.append(call("b")).unwrap();
    t.append(result("b", Outcome::Error(ErrorKind::Timeout), ""))
        .unwrap();
    t.append(call("c")).unwrap();
    t.append(result("c", Outcome::Ok, "sent")).unwrap();

    let view = project(&t).unwrap();
    let conv = Conversation::lower(&view, &PlainRenderer);
    conv.validate().unwrap();

    let ConvItem::Message(chat) = &conv.items()[2] else {
        panic!("expected the chat message")
    };
    assert_eq!(chat.role, Role::User);
    let Content::Text(text) = &chat.content[0] else {
        panic!()
    };
    assert!(text.contains("[msg:1] member:1: hello"));
    assert!(text.contains("[msg:2] member:2: spam"));

    let outputs: Vec<_> = conv
        .items()
        .iter()
        .filter_map(|i| {
            if let ConvItem::ToolResult(o) = i {
                Some(o)
            } else {
                None
            }
        })
        .collect();
    assert_eq!(outputs[0].status, ToolStatus::Ok);
    assert_eq!(
        outputs[0].content,
        vec![Content::Text("long search output".into())],
        "history is never rewritten"
    );
    assert_eq!(outputs[1].status, ToolStatus::Error);
    assert_eq!(outputs[2].status, ToolStatus::Ok);
    assert_eq!(outputs[2].content, vec![Content::Text("sent".into())]);
}

#[test]
fn prefix_digests_pin_a_continuation_to_unchanged_history() {
    let build = |old: &str| {
        let mut t = Transcript::new();
        t.append(Item::Chat(ChatBatch::new(vec![line(1, 1, old)])))
            .unwrap();
        t.append(call("a")).unwrap();
        t.append(result("a", Outcome::Ok, "x")).unwrap();
        Conversation::lower(&project(&t).unwrap(), &PlainRenderer)
    };
    let (a, b, c) = (build("one"), build("one"), build("two"));
    assert_eq!(a.digest_prefix(2), b.digest_prefix(2));
    assert_ne!(a.digest_prefix(2), c.digest_prefix(2));
}
