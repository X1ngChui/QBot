#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use qbot_llm::embedding::sim::{Corruption, SimEmbeddingServer};
use qbot_llm::embedding::{EmbeddingConfig, cosine_distance, hash_embed};
use qbot_llm::responses::RetryPolicy;
use qbot_llm::{Embedder, FakeEmbedder, HttpEmbedder, LlmError};
use serde_json::json;

const DIMS: usize = 64;

fn texts(n: usize) -> Vec<String> {
    (0..n)
        .map(|i| format!("message number {i} about topic {}", i % 3))
        .collect()
}

fn embedder(server: Arc<SimEmbeddingServer>) -> HttpEmbedder {
    let mut cfg = EmbeddingConfig::dashscope_v4(DIMS);
    cfg.retry = RetryPolicy {
        retries: 2,
        base: Duration::from_millis(100),
        jitter: false,
    };
    HttpEmbedder::new(cfg, server)
}

#[tokio::test]
async fn any_number_of_texts_is_split_to_the_vendors_batch_size_and_kept_in_order() {
    let server = Arc::new(SimEmbeddingServer::new(10, DIMS));
    let e = embedder(server.clone());
    let input = texts(25);
    let out = e.embed(&input).await.unwrap();

    assert_eq!(out.vectors.len(), 25);
    assert_eq!(out.requests, 3);
    let sizes: Vec<usize> = server
        .requests()
        .iter()
        .map(|r| r["input"].as_array().unwrap().len())
        .collect();
    assert_eq!(sizes, [10, 10, 5]);
    for (text, vector) in input.iter().zip(&out.vectors) {
        let norm: f32 = vector.iter().map(|v| v * v).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-4, "unit length");
        assert!(
            cosine_distance(vector, &hash_embed(text, DIMS)) < 1e-4,
            "vector i belongs to text i"
        );
    }
    assert!(out.input_tokens.unwrap() > 0);
    assert_eq!(
        e.embed(&[]).await.unwrap().vectors.len(),
        0,
        "nothing to embed, nothing sent"
    );
    assert_eq!(server.requests().len(), 3);
}

#[tokio::test]
async fn the_wire_body_matches_the_vendor_shape() {
    let server = Arc::new(SimEmbeddingServer::new(10, DIMS));
    embedder(server.clone()).embed(&texts(2)).await.unwrap();
    let body = &server.requests()[0];
    assert_eq!(body["model"], "text-embedding-v4");
    assert_eq!(body["dimensions"], DIMS);
    assert_eq!(body["input"].as_array().unwrap().len(), 2);
    assert_eq!(body.as_object().unwrap().len(), 3, "nothing else is sent");
}

#[tokio::test]
async fn results_are_matched_by_index_not_by_position() {
    let server = Arc::new(SimEmbeddingServer::new(10, DIMS).shuffled());
    let input = texts(7);
    let out = embedder(server).embed(&input).await.unwrap();
    for (text, vector) in input.iter().zip(&out.vectors) {
        assert!(cosine_distance(vector, &hash_embed(text, DIMS)) < 1e-4);
    }
}

#[tokio::test]
async fn corrupt_answers_are_protocol_errors_not_guesses() {
    for corruption in [
        Corruption::WrongCount,
        Corruption::RepeatedIndex,
        Corruption::WrongWidth,
        Corruption::NotANumber,
        Corruption::ZeroVector,
    ] {
        let server = Arc::new(SimEmbeddingServer::new(10, DIMS));
        server.set_corruption(corruption);
        let result = embedder(server).embed(&texts(3)).await;
        assert!(
            matches!(result, Err(LlmError::Protocol(_))),
            "{corruption:?}: {result:?}"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn transient_failures_are_retried_per_request_and_errors_are_normalized() {
    let server = Arc::new(SimEmbeddingServer::new(10, DIMS));
    server.fail_first(vec![(429, Some(Duration::from_secs(2))), (503, None)]);
    let started = tokio::time::Instant::now();
    let out = embedder(server.clone()).embed(&texts(3)).await.unwrap();
    assert_eq!(out.requests, 3);
    assert_eq!(
        started.elapsed(),
        Duration::from_millis(2200),
        "Retry-After beats the base delay, then 200 ms"
    );

    let server = Arc::new(SimEmbeddingServer::new(10, DIMS));
    server.fail_first(vec![(401, None)]);
    assert_eq!(
        embedder(server.clone()).embed(&texts(1)).await.unwrap_err(),
        LlmError::Auth
    );
    assert_eq!(server.requests().len(), 1, "an auth error is not retried");
}

#[tokio::test]
async fn an_oversized_batch_would_be_rejected_which_is_why_the_adapter_splits() {
    let server = Arc::new(SimEmbeddingServer::new(10, DIMS));
    let mut cfg = EmbeddingConfig::dashscope_v4(DIMS);
    cfg.max_batch = 50; // a misconfigured adapter sends too much
    let e = HttpEmbedder::new(cfg, server);
    assert!(matches!(
        e.embed(&texts(11)).await,
        Err(LlmError::InvalidRequest(_))
    ));
}

#[tokio::test]
async fn the_fake_embedder_ranks_related_text_closer_and_records_calls() {
    let e = FakeEmbedder::new(256);
    let out = e
        .embed(&[
            "the deploy pipeline failed again".to_owned(),
            "pipeline deploy is failing".to_owned(),
            "what shall we cook for dinner tonight".to_owned(),
        ])
        .await
        .unwrap();
    let related = cosine_distance(&out.vectors[0], &out.vectors[1]);
    let unrelated = cosine_distance(&out.vectors[0], &out.vectors[2]);
    assert!(related < unrelated, "{related} vs {unrelated}");
    assert_eq!(e.calls().len(), 1);
    assert_eq!(
        hash_embed("same", 32),
        hash_embed("same", 32),
        "deterministic"
    );
    assert!(
        (hash_embed("", 32)[0] - 1.0).abs() < 1e-6,
        "empty text is still a valid unit vector"
    );

    e.fail_next(LlmError::Unavailable);
    assert_eq!(
        e.embed(&["x".to_owned()]).await.unwrap_err(),
        LlmError::Unavailable
    );
    assert!(
        e.embed(&["x".to_owned()]).await.is_ok(),
        "the failure was one-shot"
    );
    let _ = json!({});
}
