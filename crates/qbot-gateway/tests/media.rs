#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::http::StatusCode;
use axum::routing::get;
use base64::Engine;
use qbot_core::MediaKind;
use qbot_gateway::bridge::Bridge;
use qbot_gateway::media::{FetchSettings, OneBotFetcher, job_for};
use qbot_gateway::wire::{Frame, parse_frame};
use qbot_media::{FetchError, Fetcher, MediaRef};
use serde_json::{Value, json};
use tokio::sync::mpsc;

async fn web() -> SocketAddr {
    let app = Router::new()
        .route("/pic", get(|| async { vec![7u8; 100] }))
        .route("/empty", get(|| async { Vec::<u8>::new() }))
        .route("/gone", get(|| async { StatusCode::NOT_FOUND }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    addr
}

/// A platform connection that answers actions from a table: (action name) -> data.
fn platform<'a>(
    bridge: &'a Arc<Bridge>,
    answers: Vec<(&'static str, Value)>,
) -> qbot_gateway::bridge::Attachment<'a> {
    let (out, mut rx) = mpsc::unbounded_channel::<String>();
    let attachment = bridge.attach(out);
    let bridge = Arc::clone(bridge);
    tokio::spawn(async move {
        while let Some(frame) = rx.recv().await {
            let request: Value = serde_json::from_str(&frame).unwrap();
            let name = request["action"].as_str().unwrap().to_owned();
            let data = answers
                .iter()
                .find(|(n, _)| *n == name)
                .map(|(_, d)| d.clone());
            let response = match data {
                Some(data) => {
                    json!({"status": "ok", "retcode": 0, "data": data, "echo": request["echo"]})
                }
                None => json!({"status": "failed", "retcode": 1404, "echo": request["echo"]}),
            };
            let Frame::Response(r) = parse_frame(&response.to_string()) else {
                panic!()
            };
            bridge.complete(r);
        }
    });
    attachment
}

fn fetcher(bridge: &Arc<Bridge>) -> OneBotFetcher {
    OneBotFetcher::new(
        bridge.clone(),
        FetchSettings {
            http_timeout: Duration::from_secs(5),
            protocol_timeout: Duration::from_secs(2),
        },
        &qbot_llm::net::Route::Direct,
    )
    .unwrap()
}

fn reference(url: Option<String>, file: Option<&str>) -> MediaRef {
    MediaRef {
        key: file.map(Into::into),
        url,
        file: file.map(Into::into),
        size: None,
    }
}

#[test]
fn a_message_lists_its_pictures_and_clips_by_position() {
    let raw = json!({"post_type": "message", "message_type": "group", "time": 1, "self_id": 100, "user_id": 2, "group_id": 9, "message_id": 5,
    "message": [
        {"type": "image", "data": {"file": "A.jpg", "url": "http://x/a", "file_size": "1234"}},
        {"type": "text", "data": {"text": "hi"}},
        {"type": "record", "data": {"file": "r.amr"}},
        {"type": "image", "data": {"file": "B.jpg"}}
    ]});
    let Frame::Message(m) = parse_frame(&raw.to_string()) else {
        panic!()
    };
    let job = job_for(
        &m,
        &qbot_gateway::render::RenderContext {
            bot: qbot_core::AccountId::new(100).unwrap(),
            forward_max_lines: 30,
        },
    );
    let summary: Vec<_> = job
        .items
        .iter()
        .map(|i| {
            (
                i.kind,
                i.index,
                i.reference.key.clone().unwrap(),
                i.reference.size,
            )
        })
        .collect();
    assert_eq!(
        summary,
        [
            (MediaKind::Image, 0, "A.jpg".into(), Some(1234)),
            (MediaKind::Voice, 0, "r.amr".into(), None),
            (MediaKind::Image, 1, "B.jpg".into(), None)
        ]
    );
}

#[tokio::test]
async fn pictures_download_over_http_within_the_size_cap() {
    let addr = web().await;
    let bridge = Arc::new(Bridge::new());
    let f = fetcher(&bridge);
    let url = |p: &str| Some(format!("http://{addr}{p}"));
    assert_eq!(
        f.image(&reference(url("/pic"), None), 1000)
            .await
            .unwrap()
            .len(),
        100
    );
    assert_eq!(
        f.image(&reference(url("/pic"), None), 50).await,
        Err(FetchError::TooLarge)
    );
    assert_eq!(
        f.image(&reference(url("/gone"), None), 1000).await,
        Err(FetchError::Unreadable),
        "no other route"
    );
    assert_eq!(
        f.image(&reference(url("/empty"), None), 1000).await,
        Err(FetchError::Unreadable)
    );
}

#[tokio::test]
async fn an_expired_link_falls_back_to_a_fresh_one_from_the_platform() {
    let addr = web().await;
    let bridge = Arc::new(Bridge::new());
    let _platform = platform(
        &bridge,
        vec![(
            "get_image",
            json!({"file": "/nowhere", "url": format!("http://{addr}/pic")}),
        )],
    );
    let f = fetcher(&bridge);
    let got = f
        .image(
            &reference(Some(format!("http://{addr}/gone")), Some("A.jpg")),
            1000,
        )
        .await
        .unwrap();
    assert_eq!(got.len(), 100);
}

#[tokio::test]
async fn the_platform_answers_inline_and_its_local_file_paths_are_never_read() {
    let bridge = Arc::new(Bridge::new());
    let picture = vec![7u8; 30];
    let encoded = base64::engine::general_purpose::STANDARD.encode(&picture);
    let _platform = platform(
        &bridge,
        vec![(
            "get_image",
            json!({"file": "/app/.config/QQ/nt_qq/pic/a.jpg", "base64": encoded}),
        )],
    );
    let f = fetcher(&bridge);
    assert_eq!(
        f.image(&reference(None, Some("A.jpg")), 1000)
            .await
            .unwrap(),
        picture
    );
    drop(_platform);

    // Without inline bytes, a local path (in `file` or in `url`) is NapCat's own file: no route.
    let _paths_only = platform(
        &bridge,
        vec![(
            "get_image",
            json!({"file": "/etc/passwd", "url": "/app/.config/QQ/nt_qq/pic/a.jpg"}),
        )],
    );
    assert_eq!(
        f.image(&reference(None, Some("A.jpg")), 1000).await,
        Err(FetchError::Unreadable)
    );
}

#[tokio::test]
async fn voice_comes_from_get_record_as_wav_inline_and_is_capped() {
    let bridge = Arc::new(Bridge::new());
    let wav = vec![9u8; 64];
    let encoded = base64::engine::general_purpose::STANDARD.encode(&wav);
    let _platform = platform(&bridge, vec![("get_record", json!({"base64": encoded}))]);
    let f = fetcher(&bridge);
    assert_eq!(
        f.voice(&reference(None, Some("r.amr")), 1000)
            .await
            .unwrap(),
        wav
    );
    assert_eq!(
        f.voice(&reference(None, Some("r.amr")), 10).await,
        Err(FetchError::TooLarge)
    );
    assert_eq!(
        f.voice(&reference(None, None), 1000).await,
        Err(FetchError::Unreadable)
    );
}

#[tokio::test]
async fn without_a_connection_nothing_is_readable() {
    let bridge = Arc::new(Bridge::new());
    let f = fetcher(&bridge);
    assert_eq!(
        f.voice(&reference(None, Some("r.amr")), 1000).await,
        Err(FetchError::Unreadable)
    );
    assert_eq!(
        f.image(&reference(None, Some("A.jpg")), 1000).await,
        Err(FetchError::Unreadable)
    );
}
