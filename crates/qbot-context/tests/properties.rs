#![allow(clippy::unwrap_used, clippy::expect_used)]

use proptest::prelude::*;
use qbot_context::*;
use qbot_core::{CallId, ItemSeq};
use serde_json::json;

#[derive(Debug, Clone)]
enum Op {
    Turn(u8),
    Resolve(u8),
    Interrupt,
    Summarize(u8, u8),
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        (0u8..4).prop_map(Op::Turn),
        any::<u8>().prop_map(Op::Resolve),
        Just(Op::Interrupt),
        (any::<u8>(), any::<u8>()).prop_map(|(a, b)| Op::Summarize(a, b)),
    ]
}

fn apply(t: &mut Transcript, ops: &[Op]) {
    let mut next = 0u32;
    for op in ops {
        match op {
            Op::Turn(n) => {
                let parts = (0..*n)
                    .map(|_| {
                        next += 1;
                        AssistantPart::Call(ToolCall {
                            id: CallId::new(format!("c{next}")).unwrap(),
                            name: "t".into(),
                            arguments: json!({ "n": next }),
                        })
                    })
                    .chain(std::iter::once(AssistantPart::Text("x".into())))
                    .collect();
                let _ = t.append(Item::Assistant(AssistantTurn::new(parts)));
            }
            Op::Resolve(pick) => {
                if let Some(id) = t
                    .pending()
                    .get(*pick as usize % t.pending().len().max(1))
                    .cloned()
                {
                    let _ = t.append(Item::ToolResult(ToolResult {
                        call_id: id,
                        outcome: Outcome::Ok,
                        content: vec![Part::Text("r".repeat(*pick as usize))],
                    }));
                }
            }
            Op::Interrupt => {
                t.close_interrupted();
            }
            Op::Summarize(a, b) => {
                let len = t.len().max(1);
                let (from, to) = (*a as usize % len, *b as usize % (len + 1));
                let _ = t.append(Item::Summary(Summary {
                    from: ItemSeq::new(from as u32),
                    to: ItemSeq::new(to as u32),
                    text: "s".into(),
                }));
            }
        }
    }
}

proptest! {
    #[test]
    fn every_reachable_transcript_projects_to_paired_ordered_calls(
        ops in prop::collection::vec(op(), 0..40),
    ) {
        let mut t = Transcript::new();
        apply(&mut t, &ops);
        t.close_interrupted();
        let view = project(&t).unwrap();

        // After each assistant turn, results appear in call order, and nowhere else.
        let mut expected: std::collections::VecDeque<CallId> = Default::default();
        for item in &view.items {
            match item {
                ViewItem::Assistant(turn) => {
                    prop_assert!(expected.is_empty(), "calls left without results");
                    expected.extend(turn.calls().map(|c| c.id.clone()));
                }
                ViewItem::ToolResult(r) => {
                    prop_assert_eq!(expected.pop_front(), Some(r.call_id.clone()));
                }
                _ => prop_assert!(expected.is_empty(), "item between call and result"),
            }
        }
        prop_assert!(expected.is_empty());
    }

    #[test]
    fn reload_and_reprojection_are_deterministic(ops in prop::collection::vec(op(), 0..40)) {
        let mut t = Transcript::new();
        apply(&mut t, &ops);
        t.close_interrupted();
        let json = serde_json::to_string(&t).unwrap();
        let back: Transcript = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back, &t);
        prop_assert_eq!(
            project(&back).unwrap().digest(),
            project(&t).unwrap().digest()
        );
    }

    #[test]
    fn compaction_never_loses_stored_history(ops in prop::collection::vec(op(), 0..40)) {
        let mut t = Transcript::new();
        let mut stored = 0usize;
        for chunk in ops.chunks(1) {
            apply(&mut t, chunk);
            prop_assert!(t.len() >= stored, "items are never removed");
            stored = t.len();
        }
    }
}
