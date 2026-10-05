#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use qbot_llm::fake::{FakeProvider, FakeReply, Step};
use qbot_llm::{CacheUsage, LlmError, Provider, ReplayPlan, ToolChoice};
use serde_json::json;

#[tokio::test]
async fn the_fake_records_what_it_was_asked() {
    let fake = FakeProvider::new([first_reply().into(), second_reply().into()]);
    let (tools, conv0) = (tools(), base_conversation());
    let first = fake.respond(request(&conv0, &tools, None)).await.unwrap();
    let conv1 = extend(&conv0, &first, "ok");
    fake.respond(request(&conv1, &tools, Some(&first.continuation)))
        .await
        .unwrap();

    let recorded = fake.recorded();
    assert_eq!(recorded.len(), 2);
    assert_eq!(recorded[0].conversation, conv0);
    assert_eq!(recorded[1].conversation, conv1);
    assert_eq!(recorded[0].tool_names, ["send_message"]);
    assert_eq!(
        recorded[1].plan,
        ReplayPlan::Full,
        "a stateless provider always replays in full"
    );
    assert_eq!(fake.remaining_steps(), 0);
}

#[tokio::test]
async fn cache_usage_is_the_common_prefix_of_consecutive_requests() {
    let fake = FakeProvider::new([first_reply().into(), second_reply().into()]);
    let (tools, conv0) = (tools(), base_conversation());
    let first = fake.respond(request(&conv0, &tools, None)).await.unwrap();
    let conv1 = extend(&conv0, &first, "ok");
    let second = fake
        .respond(request(&conv1, &tools, Some(&first.continuation)))
        .await
        .unwrap();

    // The whole first request is the prefix of the second, so exactly its tokens are cached.
    assert_eq!(
        second.usage.cache,
        CacheUsage::Reported {
            hit_tokens: first.usage.input_tokens
        }
    );
    assert!(second.usage.input_tokens > first.usage.input_tokens);
}

#[tokio::test]
async fn a_stateful_fake_plans_deltas_and_rejects_unknown_handles() {
    let fake = FakeProvider::new([
        first_reply().into(),
        second_reply().into(),
        second_reply().into(),
    ])
    .stateful();
    let (tools, conv0) = (tools(), base_conversation());
    let first = fake.respond(request(&conv0, &tools, None)).await.unwrap();
    let conv1 = extend(&conv0, &first, "ok");
    fake.respond(request(&conv1, &tools, Some(&first.continuation)))
        .await
        .unwrap();
    assert_eq!(
        fake.recorded()[1].plan,
        ReplayPlan::Delta {
            from: 4,
            handle: first.continuation.handle().unwrap().to_owned()
        }
    );

    // A continuation from another fake instance is unknown here.
    let other = FakeProvider::new([first_reply().into()]).stateful();
    let foreign = other.respond(request(&conv0, &tools, None)).await.unwrap();
    let conv1 = extend(&conv0, &foreign, "ok");
    let err = fake
        .respond(request(&conv1, &tools, Some(&foreign.continuation)))
        .await
        .unwrap_err();
    assert!(matches!(err, LlmError::InvalidRequest(_)));
}

#[tokio::test]
async fn scripted_failures_and_script_bugs_surface_as_errors() {
    let fake = FakeProvider::new([Step::Fail(LlmError::RateLimited { retry_after: None })]);
    let (tools, conv) = (tools(), base_conversation());
    assert_eq!(
        fake.respond(request(&conv, &tools, None))
            .await
            .unwrap_err(),
        LlmError::RateLimited { retry_after: None }
    );
    assert!(
        matches!(
            fake.respond(request(&conv, &tools, None)).await,
            Err(LlmError::Protocol(_))
        ),
        "exhausted script"
    );

    let bad = FakeProvider::new([FakeReply::new().call("undeclared", json!({})).into()]);
    assert!(matches!(
        bad.respond(request(&conv, &tools, None)).await,
        Err(LlmError::Protocol(_))
    ));

    let lazy = FakeProvider::new([second_reply().into()]);
    let mut req = request(&conv, &tools, None);
    req.tool_choice = ToolChoice::Required;
    assert!(
        matches!(lazy.respond(req).await, Err(LlmError::Protocol(_))),
        "a forced call must be honored"
    );
}

#[tokio::test]
async fn steps_can_be_added_while_a_test_runs() {
    let fake = FakeProvider::new([]);
    fake.push(second_reply());
    let (tools, conv) = (tools(), base_conversation());
    assert_eq!(
        fake.respond(request(&conv, &tools, None))
            .await
            .unwrap()
            .turn
            .text(),
        "all done"
    );
}
