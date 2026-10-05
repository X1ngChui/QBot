#![allow(clippy::unwrap_used, clippy::expect_used)]

//! The whole bot in one process: the real composition root against a real Postgres, a mock
//! Responses server reached over HTTP, and a simulated NapCat on the reverse WebSocket.
//!
//! Needs `QBOT_TEST_DATABASE_URL` like the store tests (and is skipped under
//! `QBOT_SKIP_DB_TESTS=1`).

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::Response;
use axum::routing::post;
use futures_util::{SinkExt, StreamExt};
use qbot_app::{Options, run};
use qbot_config::{Layout, MapEnv, load_in};
use qbot_llm::fake::FakeReply;
use qbot_llm::responses::sim::{SimServer, SimStep};
use qbot_llm::responses::{HttpBody, Transport};
use serde_json::{Value, json};
use sqlx::postgres::PgPoolOptions;
use tokio::sync::oneshot;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_util::sync::CancellationToken;

static COUNTER: AtomicU32 = AtomicU32::new(0);

struct Database {
    host: String,
    port: u16,
    user: String,
    password: String,
    name: String,
    url: String,
    admin: sqlx::PgPool,
}

async fn database() -> Option<Database> {
    let base = match std::env::var("QBOT_TEST_DATABASE_URL") {
        Ok(url) => url,
        Err(_) if std::env::var("QBOT_SKIP_DB_TESTS").is_ok() => {
            eprintln!("SKIPPED: QBOT_TEST_DATABASE_URL is not set");
            return None;
        }
        Err(_) => panic!("QBOT_TEST_DATABASE_URL is not set (or set QBOT_SKIP_DB_TESTS=1)"),
    };
    // postgres://user:password@host:port/database
    let rest = base.strip_prefix("postgres://").expect("a postgres:// URL");
    let (credentials, location) = rest.split_once('@').expect("credentials in the URL");
    let (user, password) = credentials.split_once(':').expect("a password in the URL");
    let (hostport, admin_db) = location.split_once('/').expect("a database in the URL");
    let (host, port) = hostport.split_once(':').expect("a port in the URL");
    assert!(
        admin_db.starts_with("qbot_test"),
        "refusing to run against {admin_db:?}"
    );
    let admin = PgPoolOptions::new()
        .max_connections(2)
        .connect(&base)
        .await
        .unwrap();
    let name = format!(
        "qbot_test_{}_{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::SeqCst)
    );
    sqlx::query(&format!("CREATE DATABASE \"{name}\""))
        .execute(&admin)
        .await
        .unwrap();
    Some(Database {
        host: host.into(),
        port: port.parse().unwrap(),
        user: user.into(),
        password: password.into(),
        url: format!("postgres://{user}:{password}@{host}:{port}/{name}"),
        name,
        admin,
    })
}

impl Database {
    async fn pool(&self) -> sqlx::PgPool {
        PgPoolOptions::new()
            .max_connections(2)
            .connect(&self.url)
            .await
            .unwrap()
    }

    async fn drop(self) {
        let _ = sqlx::query(&format!(
            "DROP DATABASE IF EXISTS \"{}\" WITH (FORCE)",
            self.name
        ))
        .execute(&self.admin)
        .await;
    }
}

/// Embeddings of width 4 (the test deployment's `providers.embedding.dims`), all pointing the
/// same way, which is enough to store and find an episode.
async fn embeddings(axum::Json(body): axum::Json<Value>) -> axum::Json<Value> {
    let inputs = body["input"].as_array().map_or(0, Vec::len);
    let data: Vec<Value> = (0..inputs)
        .map(|i| json!({"index": i, "embedding": [1.0, 0.0, 0.0, 0.0]}))
        .collect();
    axum::Json(json!({"data": data, "usage": {"total_tokens": inputs}}))
}

/// A Responses endpoint over real HTTP, answering from the adapter test simulator.
async fn mock_llm(sim: Arc<SimServer>) -> SocketAddr {
    async fn handle(
        State(sim): State<Arc<SimServer>>,
        axum::Json(body): axum::Json<Value>,
    ) -> Response {
        let stream = body.get("stream") == Some(&json!(true));
        let answer = sim.post("/responses", &body, stream).await.unwrap();
        let status = StatusCode::from_u16(answer.status).unwrap();
        match answer.body {
            HttpBody::Full(bytes) => Response::builder()
                .status(status)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(bytes))
                .unwrap(),
            HttpBody::Stream(chunks) => Response::builder()
                .status(status)
                .header(header::CONTENT_TYPE, "text/event-stream")
                .body(Body::from_stream(
                    chunks.map(|c| c.map_err(|e| std::io::Error::other(e.to_string()))),
                ))
                .unwrap(),
        }
    }
    let app = Router::new()
        .route("/responses", post(handle))
        .route("/embeddings", post(embeddings))
        .route(
            "/pic",
            axum::routing::get(|| async { b"\x89PNG\r\n\x1a\nfake-pixels".to_vec() }),
        )
        .with_state(sim);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    addr
}

fn scratch() -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "qbot-app-e2e-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

struct Bot {
    addr: SocketAddr,
    stop: CancellationToken,
    finished: tokio::task::JoinHandle<Result<(), qbot_app::RunError>>,
}

fn deployment(db: &Database, llm: SocketAddr, root: &Path, asr_dir: Option<&Path>) {
    deployment_with(db, llm, root, asr_dir, "[maintenance]\nbackups = 0\n");
}

/// `extra` is more TOML appended to the configuration (for example a `[maintenance]` section).
fn deployment_with(
    db: &Database,
    llm: SocketAddr,
    root: &Path,
    asr_dir: Option<&Path>,
    extra: &str,
) {
    write(
        &root.join("etc/config.toml"),
        &(format!(
            r#"
[bot]
account = 100
owners = [1]
nicknames = ["Bobo"]
timezone = "Asia/Shanghai"

[gateway]
listen = "127.0.0.1:0"

[database]
host = "{}"
port = {}
name = "{}"
user = "{}"
ssl_mode = "disable"

[providers.text]
endpoint = "http://{llm}"

[providers.vision]
enabled = true
# The mock serves the standard Responses dialect (pictures sent inline), not DeepSeek's uploads.
kind = "openai_responses"
endpoint = "http://{llm}"

[media]
transcribe_voice = {asr_enabled}

[providers.embedding]
endpoint = "http://{llm}"
dims = 4
"#,
            db.host,
            db.port,
            db.name,
            db.user,
            asr_enabled = asr_dir.is_some(),
        ) + extra),
    );
    // The voice model has a fixed place under the data directory.
    if let Some(model) = asr_dir {
        let place = root.join("data/models/asr");
        std::fs::create_dir_all(&place).unwrap();
        std::os::unix::fs::symlink(model, place.join("sense-voice")).unwrap();
    }
    write(
        &root.join("etc/personas/default.toml"),
        "name = \"Bobo\"\nsystem_prompt = \"You are Bobo, a cheerful member.\"\n",
    );
    write(&root.join("secrets/database_password"), &db.password);
    write(&root.join("secrets/text_api_key"), "text-key");
    write(&root.join("secrets/embedding_api_key"), "embedding-key");
    write(&root.join("secrets/onebot_access_token"), "sesame");
    write(&root.join("secrets/vision_api_key"), "vision-key");
}

fn layout(root: &Path) -> Layout {
    Layout {
        config_dir: root.join("etc"),
        data_dir: root.join("data"),
        secrets_dir: root.join("secrets"),
    }
}

async fn start(db: &Database, llm: SocketAddr, root: &Path, asr_dir: Option<&Path>) -> Bot {
    deployment(db, llm, root, asr_dir);
    launch(db, root).await
}

async fn launch(_db: &Database, root: &Path) -> Bot {
    let env = Arc::new(MapEnv::default());
    let loaded = load_in(layout(root)).unwrap();
    let stop = CancellationToken::new();
    let (ready_tx, ready_rx) = oneshot::channel();
    let options = Options {
        env,
        shutdown: stop.clone(),
        ready: Some(ready_tx),
        logs: None,
    };
    let finished = tokio::spawn(run(loaded, options));
    let addr = match tokio::time::timeout(Duration::from_secs(30), ready_rx)
        .await
        .expect("the bot to start")
    {
        Ok(addr) => addr,
        // The bot stopped before it was ready: say why.
        Err(_) => panic!("the bot failed to start: {:?}", finished.await),
    };
    Bot {
        addr,
        stop,
        finished,
    }
}

type Client =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn connect(
    addr: SocketAddr,
    token: &str,
) -> Result<Client, tokio_tungstenite::tungstenite::Error> {
    let mut request = format!("ws://{addr}/onebot/v11/ws")
        .into_client_request()
        .unwrap();
    request
        .headers_mut()
        .insert("x-self-id", "100".parse().unwrap());
    request
        .headers_mut()
        .insert("authorization", format!("Bearer {token}").parse().unwrap());
    tokio_tungstenite::connect_async(request)
        .await
        .map(|(c, _)| c)
}

fn group_message(id: i64, user: i64, segments: Value) -> Message {
    Message::text(
        json!({"post_type": "message", "message_type": "group", "time": 1_800_000_000, "self_id": 100,
               "user_id": user, "group_id": 900, "message_id": id, "message": segments})
        .to_string(),
    )
}

async fn next_action(client: &mut Client) -> Value {
    let frame = tokio::time::timeout(Duration::from_secs(10), client.next())
        .await
        .expect("an action in time")
        .unwrap()
        .unwrap();
    serde_json::from_str(frame.to_text().unwrap()).unwrap()
}

/// The platform accepts a send and reports the bot's own message back.
async fn accept(client: &mut Client, action: &Value, message_id: i64) {
    let echo = action["echo"].as_str().unwrap();
    client
        .send(Message::text(
            json!({"status": "ok", "retcode": 0, "data": {"message_id": message_id}, "echo": echo})
                .to_string(),
        ))
        .await
        .unwrap();
    client
        .send(Message::text(
            json!({"post_type": "message_sent", "message_type": "group", "time": 1_800_000_001, "self_id": 100,
                   "user_id": 100, "group_id": 900, "message_id": message_id, "message": action["params"]["message"]})
            .to_string(),
        ))
        .await
        .unwrap();
}

async fn eventually<F, Fut>(what: &str, mut check: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    for _ in 0..200 {
        if check().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for: {what}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_platform_message_becomes_a_stored_reply_and_commands_change_behaviour() {
    let Some(db) = database().await else { return };
    let root = scratch();
    let sim = Arc::new(SimServer::new([SimStep::Reply(
        FakeReply::new().call("send_message", json!({ "text": "hello from the model" })),
    )]));
    let llm = mock_llm(sim.clone()).await;
    let bot = start(&db, llm, &root, None).await;
    let pool = db.pool().await;

    // Authentication is enforced end to end.
    assert!(connect(bot.addr, "wrong").await.is_err());
    let mut napcat = connect(bot.addr, "sesame").await.unwrap();

    // 1. A member @s the bot: the run starts, the model answers through send_message, the
    //    platform echoes it, and the run ends delivered.
    napcat
        .send(group_message(1, 2, json!([{"type": "at", "data": {"qq": "100"}}, {"type": "text", "data": {"text": " are you there?"}}])))
        .await
        .unwrap();
    let action = next_action(&mut napcat).await;
    assert_eq!(action["action"], "send_group_msg");
    assert_eq!(
        action["params"]["message"][0],
        json!({"type": "text", "data": {"text": "hello from the model"}})
    );
    accept(&mut napcat, &action, 9001).await;
    eventually("the run to end delivered", || async {
        sqlx::query_scalar::<_, Option<String>>("SELECT end_reason FROM run")
            .fetch_optional(&pool)
            .await
            .unwrap()
            .flatten()
            .as_deref()
            == Some("delivered")
    })
    .await;

    // The model saw the persona, the rules and the chat line, with a member number and an id.
    let sent_to_model = sim.requests()[0].to_string();
    assert!(
        sent_to_model.contains("You are Bobo, a cheerful member."),
        "{sent_to_model}"
    );
    assert!(
        sent_to_model.contains("[msg:1]")
            && sent_to_model.contains("member:1")
            && sent_to_model.contains("[at:bot] are you there?"),
        "{sent_to_model}"
    );
    let lines: Vec<(String, String)> =
        sqlx::query_as("SELECT speaker, text FROM chat_line ORDER BY ordinal")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(
        lines,
        [
            ("member".into(), "[at:bot] are you there?".into()),
            ("bot".into(), "hello from the model".into())
        ]
    );

    // 2. The owner mutes the group by command. The reply quotes the command and mentions the sender.
    napcat
        .send(group_message(
            2,
            1,
            json!([{"type": "text", "data": {"text": "/mute on"}}]),
        ))
        .await
        .unwrap();
    let action = next_action(&mut napcat).await;
    assert_eq!(
        action["params"]["message"][0],
        json!({"type": "reply", "data": {"id": "2"}})
    );
    assert_eq!(
        action["params"]["message"][1],
        json!({"type": "at", "data": {"qq": "1"}})
    );
    assert_eq!(
        action["params"]["message"][2],
        json!({"type": "text", "data": {"text": " Muted this group."}})
    );
    accept(&mut napcat, &action, 9002).await;

    // 3. A muted group starts no run and makes no model call, but still archives the line.
    eventually("the mute to be stored", || async {
        sqlx::query_scalar::<_, bool>("SELECT muted FROM group_state WHERE group_id = 900")
            .fetch_optional(&pool)
            .await
            .unwrap()
            == Some(true)
    })
    .await;
    napcat
        .send(group_message(3, 2, json!([{"type": "at", "data": {"qq": "100"}}, {"type": "text", "data": {"text": " hello?"}}])))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(sim.requests().len(), 1, "no model call while muted");
    let runs: i64 = sqlx::query_scalar("SELECT count(*) FROM run")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(runs, 1);
    let archived: i64 = sqlx::query_scalar("SELECT count(*) FROM chat_line WHERE message_id = 3")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(archived, 1);

    // 4. A non-owner is refused an owner command, in words.
    napcat
        .send(group_message(
            4,
            2,
            json!([{"type": "text", "data": {"text": "/mute off"}}]),
        ))
        .await
        .unwrap();
    let action = next_action(&mut napcat).await;
    assert_eq!(
        action["params"]["message"][2]["data"]["text"],
        " This action needs bot-owner permission."
    );
    accept(&mut napcat, &action, 9003).await;

    // Graceful shutdown ends with Ok and flushes usage.
    bot.stop.cancel();
    tokio::time::timeout(Duration::from_secs(30), bot.finished)
        .await
        .expect("shutdown in time")
        .unwrap()
        .unwrap();
    let usage: i64 = sqlx::query_scalar("SELECT count(*) FROM usage_event WHERE kind = 'model'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(usage, 1);
    pool.close().await;
    db.drop().await;
    std::fs::remove_dir_all(&root).ok();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_second_instance_on_the_same_database_refuses_to_start() {
    let Some(db) = database().await else { return };
    let root = scratch();
    let llm = mock_llm(Arc::new(SimServer::new([]))).await;
    let first = start(&db, llm, &root, None).await;

    // Same database, same lease key: the second must fail before serving anything.
    let second_root = scratch();
    deployment(&db, llm, &second_root, None);
    let env = Arc::new(MapEnv::default());
    let loaded = load_in(layout(&second_root)).unwrap();
    let options = Options {
        env,
        shutdown: CancellationToken::new(),
        ready: None,
        logs: None,
    };
    let refused = tokio::time::timeout(Duration::from_secs(30), run(loaded, options))
        .await
        .expect("a prompt refusal")
        .unwrap_err();
    assert!(
        refused
            .to_string()
            .contains("another instance holds the runtime lease"),
        "{refused}"
    );

    first.stop.cancel();
    first.finished.await.unwrap().unwrap();
    db.drop().await;
    std::fs::remove_dir_all(&root).ok();
    std::fs::remove_dir_all(&second_root).ok();
}

/// Answers the actions the bot sends while handling media, until the reply it was waiting for.
async fn serve_until_send(napcat: &mut Client, wav: Option<&[u8]>) -> Value {
    use base64::Engine;
    loop {
        let action = next_action(napcat).await;
        match action["action"].as_str().unwrap() {
            "get_record" => {
                let data = match wav {
                    Some(bytes) => {
                        json!({"base64": base64::engine::general_purpose::STANDARD.encode(bytes)})
                    }
                    None => panic!("an unexpected get_record"),
                };
                let echo = action["echo"].clone();
                napcat
                    .send(Message::text(
                        json!({"status": "ok", "retcode": 0, "data": data, "echo": echo})
                            .to_string(),
                    ))
                    .await
                    .unwrap();
            }
            "get_group_member_info" => {
                let echo = action["echo"].clone();
                napcat
                    .send(Message::text(
                        json!({"status": "failed", "retcode": 1, "echo": echo}).to_string(),
                    ))
                    .await
                    .unwrap();
            }
            "send_group_msg" => return action,
            other => panic!("unexpected action {other}"),
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn pictures_and_voice_reach_the_model_as_words() {
    let Some(db) = database().await else { return };
    // The real recogniser takes part when a model directory is supplied.
    let asr_dir = std::env::var("QBOT_ASR_MODEL_DIR").ok().map(PathBuf::from);
    let root = scratch();
    let mut script = vec![SimStep::Reply(
        FakeReply::new().text("A red bicycle leaning against a brick wall"),
    )];
    script.push(SimStep::Reply(
        FakeReply::new().call("send_message", json!({ "text": "nice bike" })),
    ));
    script.push(SimStep::Reply(
        FakeReply::new().call("send_message", json!({ "text": "got it" })),
    ));
    // Used only when the voice part runs.
    script.push(SimStep::Reply(
        FakeReply::new().call("send_message", json!({ "text": "heard you" })),
    ));
    let sim = Arc::new(SimServer::new(script));
    let llm = mock_llm(sim.clone()).await;
    let bot = start(&db, llm, &root, asr_dir.as_deref()).await;
    let pool = db.pool().await;
    let mut napcat = connect(bot.addr, "sesame").await.unwrap();

    // A picture with an @: described first, then the reply sees the description in the line.
    napcat
        .send(group_message(
            1,
            2,
            json!([
                {"type": "at", "data": {"qq": "100"}},
                {"type": "text", "data": {"text": " look at this"}},
                {"type": "image", "data": {"file": "BIKE.png", "url": format!("http://{llm}/pic")}}
            ]),
        ))
        .await
        .unwrap();
    let action = serve_until_send(&mut napcat, None).await;
    accept(&mut napcat, &action, 9001).await;

    let requests = sim.requests();
    assert!(
        requests[0].to_string().contains("input_image")
            && requests[0].to_string().contains("data:image/png;base64,"),
        "the vision call carries the picture"
    );
    assert!(
        requests[0]
            .to_string()
            .contains("Write the description in English"),
        "instructions come from the template"
    );
    let main = requests[1].to_string();
    assert!(
        main.contains("[image:A red bicycle leaning against a brick wall]"),
        "{main}"
    );
    let stored: String = sqlx::query_scalar("SELECT text FROM chat_line WHERE message_id = 1")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        stored,
        "[at:bot] look at this[image:A red bicycle leaning against a brick wall]"
    );
    let cached: i64 = sqlx::query_scalar("SELECT count(*) FROM media_cache")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(cached, 2, "cached by platform id and by content");

    // The same picture again is free: no new vision call (the next model call is the reply).
    napcat
        .send(group_message(
            2,
            3,
            json!([
                {"type": "at", "data": {"qq": "100"}},
                {"type": "image", "data": {"file": "BIKE.png", "url": format!("http://{llm}/pic")}}
            ]),
        ))
        .await
        .unwrap();
    let action = serve_until_send(&mut napcat, None).await;
    accept(&mut napcat, &action, 9002).await;
    assert!(
        sim.requests()[2]
            .to_string()
            .contains("[image:A red bicycle leaning against a brick wall]")
    );
    assert_eq!(sim.requests().len(), 3, "one vision call, two replies");

    // Voice, through the real recogniser, when a model is available.
    if let Some(dir) = &asr_dir {
        let wav = std::fs::read(dir.join("test_wavs/en.wav")).unwrap();
        napcat
            .send(group_message(3, 2, json!([{"type": "at", "data": {"qq": "100"}}, {"type": "record", "data": {"file": "clip.amr"}}])))
            .await
            .unwrap();
        let action = serve_until_send(&mut napcat, Some(&wav)).await;
        accept(&mut napcat, &action, 9003).await;
        let heard = sim.requests()[3].to_string().to_lowercase();
        assert!(
            heard.contains("[voice:") && heard.contains("tribal chieftain"),
            "{heard}"
        );
    } else {
        eprintln!("voice part SKIPPED: QBOT_ASR_MODEL_DIR is not set");
    }

    bot.stop.cancel();
    tokio::time::timeout(Duration::from_secs(30), bot.finished)
        .await
        .expect("shutdown in time")
        .unwrap()
        .unwrap();
    pool.close().await;
    db.drop().await;
    std::fs::remove_dir_all(&root).ok();
}

#[cfg(unix)]
fn fake_pg_tools(dir: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let script = |name: &str, body: &str| {
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    };
    script(
        "pg_dump",
        "[ \"$1\" = --version ] && { echo pg_dump 17; exit 0; }; out=\"\"; while [ $# -gt 0 ]; do case \"$1\" in -f) out=\"$2\";; esac; shift; done; printf 'PGDMP' > \"$out\"",
    );
    script(
        "pg_restore",
        "[ \"$1\" = --version ] && { echo pg_restore 17; exit 0; }; echo '215; 1259 16401 TABLE public chat_line qbot'",
    );
}

/// A daily cron expression (with a seconds field) that next fires `seconds` from now in Shanghai.
/// A daily schedule a few seconds from now that already ran for yesterday's occurrence, so it
/// fires exactly once for today's whether the bot is up before that moment or starts after it (a
/// schedule with no history starts from now and would skip an occurrence it started late for).
async fn schedule_in(db: &Database, name: &str, seconds: i64) -> String {
    let now = jiff::Timestamp::now() + jiff::SignedDuration::from_secs(seconds);
    let at = jiff::Timestamp::from_second(now.as_second())
        .unwrap()
        .to_zoned(jiff::tz::TimeZone::get("Asia/Shanghai").unwrap());
    let store = qbot_store::Store::connect(&db.url).await.unwrap();
    store.migrate().await.unwrap();
    sqlx::query("INSERT INTO recurrence (name, last_fired_ms) VALUES ($1, $2)")
        .bind(name)
        .bind(at.timestamp().as_millisecond() - 86_400_000)
        .execute(store.pool())
        .await
        .unwrap();
    format!("{} {} {} * * *", at.second(), at.minute(), at.hour())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn the_nightly_pipeline_and_the_daily_report_run_on_schedule() {
    let Some(db) = database().await else { return };
    let (root, tools) = (scratch(), scratch());
    fake_pg_tools(&tools);
    let llm = mock_llm(Arc::new(SimServer::new([]))).await;
    let (nightly, report) = (
        schedule_in(&db, "nightly", 3).await,
        schedule_in(&db, "report", 5).await,
    );
    deployment_with(
        &db,
        llm,
        &root,
        None,
        &format!(
            "[maintenance]\nnightly_cron = \"{nightly}\"\nreport_cron = \"{report}\"\nbackups = 2\npostgres_bin_dir = \"{}\"\n",
            tools.display()
        ),
    );
    let bot = launch(&db, &root).await;
    let pool = db.pool().await;
    let mut napcat = connect(bot.addr, "sesame").await.unwrap();

    // The nightly cleanup asks NapCat to clean its own cache (answered here), and the report
    // arrives as a private message to the owner; the two jobs may come in either order.
    let mut cleaned = 0;
    let action = loop {
        let action = tokio::time::timeout(Duration::from_secs(30), next_action(&mut napcat))
            .await
            .expect("the report");
        if action["action"] != "clean_cache" {
            break action;
        }
        cleaned += 1;
        napcat
            .send(Message::text(
                json!({"status": "ok", "retcode": 0, "data": null, "echo": action["echo"]})
                    .to_string(),
            ))
            .await
            .unwrap();
    };
    assert_eq!(action["action"], "send_private_msg");
    assert_eq!(action["params"]["user_id"], 1);
    let text = action["params"]["message"][0]["data"]["text"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(text.starts_with("Daily report | "), "{text}");
    // Whether the backup exists yet depends on how the two jobs interleave under load; the
    // backup itself is checked below.
    assert!(text.contains("Last backup:"), "{text}");
    let echo = action["echo"].clone();
    napcat
        .send(Message::text(
            json!({"status": "ok", "retcode": 0, "data": {}, "echo": echo}).to_string(),
        ))
        .await
        .unwrap();

    // The cache request may also come after the report.
    while cleaned == 0 {
        let action = tokio::time::timeout(Duration::from_secs(30), next_action(&mut napcat))
            .await
            .expect("the cache cleanup");
        if action["action"] == "clean_cache" {
            cleaned += 1;
            napcat
                .send(Message::text(
                    json!({"status": "ok", "retcode": 0, "data": null, "echo": action["echo"]})
                        .to_string(),
                ))
                .await
                .unwrap();
        }
    }
    assert_eq!(
        cleaned, 1,
        "NapCat is asked to clean its cache once a night"
    );
    // Both jobs finished, the dump is in the backups directory, and each occurrence fired once.
    let jobs_ok = || async {
        let done: i64 = sqlx::query_scalar("SELECT count(*) FROM timer WHERE kind = 'job' AND state = 'done' AND outcome = 'job_ok'")
            .fetch_one(&pool)
            .await
            .unwrap();
        done == 2
    };
    for _ in 0..200 {
        if jobs_ok().await {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    if !jobs_ok().await {
        let rows: Vec<(String, String, Option<String>, Option<String>)> = sqlx::query_as(
            "SELECT job_kind, state, outcome, outcome_detail FROM timer WHERE kind = 'job' ORDER BY job_kind",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        panic!("both jobs did not finish ok: {rows:?}");
    }
    let kinds: Vec<String> =
        sqlx::query_scalar("SELECT job_kind FROM timer WHERE kind = 'job' ORDER BY job_kind")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(kinds, ["nightly", "report"]);
    let dumps: Vec<_> = std::fs::read_dir(root.join("data/backups"))
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(dumps.len(), 1, "{dumps:?}");
    assert!(dumps[0].starts_with("qbot-") && dumps[0].ends_with(".dump"));
    let recurrences: i64 = sqlx::query_scalar("SELECT count(*) FROM recurrence")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(recurrences, 2);

    bot.stop.cancel();
    tokio::time::timeout(Duration::from_secs(30), bot.finished)
        .await
        .expect("shutdown in time")
        .unwrap()
        .unwrap();
    pool.close().await;
    db.drop().await;
    std::fs::remove_dir_all(&root).ok();
    std::fs::remove_dir_all(&tools).ok();
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn a_missing_backup_tool_stops_startup_with_a_clear_error() {
    let Some(db) = database().await else { return };
    let (root, tools) = (scratch(), scratch());
    let llm = mock_llm(Arc::new(SimServer::new([]))).await;
    deployment_with(
        &db,
        llm,
        &root,
        None,
        &format!(
            "[maintenance]\nbackups = 14\npostgres_bin_dir = \"{}\"\n",
            tools.display()
        ),
    );
    let env = Arc::new(MapEnv::default());
    let loaded = load_in(layout(&root)).unwrap();
    let options = Options {
        env,
        shutdown: CancellationToken::new(),
        ready: None,
        logs: None,
    };
    let error = tokio::time::timeout(Duration::from_secs(30), run(loaded, options))
        .await
        .expect("a prompt refusal")
        .unwrap_err();
    assert!(error.to_string().contains("pg_dump"), "{error}");
    db.drop().await;
    std::fs::remove_dir_all(&root).ok();
    std::fs::remove_dir_all(&tools).ok();
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn a_filled_batch_turns_archived_chat_into_a_stored_episode() {
    let Some(db) = database().await else { return };
    let root = scratch();
    // The extractor's model answers with a valid episode: a title, a summary, and a quote copied
    // exactly from the first line.
    let sim = Arc::new(SimServer::new([
        SimStep::Reply(FakeReply::new().call(
            "submit_episode",
            json!({"title": "Planning a trip", "summary": "Members discussed planning a trip together.",
                   "evidence": [{"line": 1, "quote": "plan the trip"}]}),
        )),
        SimStep::Reply(FakeReply::new().call(
            "submit_episode",
            json!({"title": "Packing", "summary": "Members talked about what to pack.",
                   "evidence": [{"line": 1, "quote": "pack light"}]}),
        )),
        // The reply run looks and stays silent.
        SimStep::Reply(FakeReply::new().call("stay_silent", json!({}))),
    ]));
    let llm = mock_llm(sim.clone()).await;
    // One-line batches and three-batch slices: the third line completes a slice, and the batch
    // it fills queues the extraction at once, without waiting for the nightly run.
    deployment_with(
        &db,
        llm,
        &root,
        None,
        "[history]\nbatch_lines = 1\n[maintenance]\nbackups = 0\n",
    );
    let bot = launch(&db, &root).await;
    let pool = db.pool().await;
    let mut napcat = connect(bot.addr, "sesame").await.unwrap();
    for (id, text) in [
        (1, "we should plan the trip together"),
        (2, "good idea, where to?"),
        (3, "somewhere warm"),
    ] {
        napcat
            .send(group_message(
                id,
                2 + id % 2,
                json!([{"type": "text", "data": {"text": text}}]),
            ))
            .await
            .unwrap();
    }

    eventually("the filled batch to store the episode", || async {
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM episode")
            .fetch_one(&pool)
            .await
            .unwrap()
            == 1
    })
    .await;
    let (group, first, last, title, quote): (i64, i64, i64, String, String) = sqlx::query_as(
        "SELECT group_id, first_ordinal, last_ordinal, title, evidence->0->>'quote' FROM episode",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        (group, first, last, title.as_str(), quote.as_str()),
        (900, 1, 3, "Planning a trip", "plan the trip")
    );
    let vectors: i64 = sqlx::query_scalar("SELECT count(*) FROM episode_embedding")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(vectors, 1, "the episode is searchable");
    // Every filled batch queued a job (three lines, three batches); the first two had no
    // complete slice yet and found nothing to do.
    eventually("the extraction jobs to be done", || async {
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM timer WHERE job_kind = 'extract' AND state = 'done' AND outcome = 'job_ok'")
            .fetch_one(&pool)
            .await
            .unwrap()
            == 3
    })
    .await;
    assert_eq!(sim.requests().len(), 1, "the slice was extracted once");

    // Three more lines complete the next slice; then a mention starts a reply. With one-line
    // batches and four verbatim batches, lines 4-7 are shown as they are and lines 1-3 (the
    // summary tier) only through their episode.
    for (id, text) in [(4, "pack light"), (5, "sunscreen too"), (6, "and a hat")] {
        napcat
            .send(group_message(
                id,
                2 + id % 2,
                json!([{"type": "text", "data": {"text": text}}]),
            ))
            .await
            .unwrap();
    }
    eventually("the second episode", || async {
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM episode")
            .fetch_one(&pool)
            .await
            .unwrap()
            == 2
    })
    .await;
    napcat
        .send(group_message(7, 2, json!([{"type": "at", "data": {"qq": "100"}}, {"type": "text", "data": {"text": " any tips?"}}])))
        .await
        .unwrap();
    eventually("the reply request", || async { sim.requests().len() == 3 }).await;
    let seen = sim.requests()[2]["input"].to_string();
    assert!(
        seen.contains("Members discussed planning a trip together."),
        "the older slice is shown as its summary: {seen}"
    );
    assert!(
        !seen.contains("we should plan the trip together"),
        "and not verbatim: {seen}"
    );
    assert!(
        seen.contains("pack light") && seen.contains("any tips?"),
        "{seen}"
    );
    assert!(
        !seen.contains("Members talked about what to pack."),
        "an episode still in the verbatim tier is not summarized"
    );
    let asked = sim.requests()[0].to_string();
    assert!(
        asked.contains("we should plan the trip together") && asked.contains("submit_episode"),
        "{asked}"
    );

    bot.stop.cancel();
    tokio::time::timeout(Duration::from_secs(30), bot.finished)
        .await
        .expect("shutdown in time")
        .unwrap()
        .unwrap();
    pool.close().await;
    db.drop().await;
    std::fs::remove_dir_all(&root).ok();
}

#[tokio::test(flavor = "multi_thread")]
async fn the_model_can_open_a_picture_and_mentions_become_member_numbers() {
    let Some(db) = database().await else { return };
    let root = scratch();
    let sim = Arc::new(SimServer::new([
        // The vision model describes the picture on arrival.
        SimStep::Reply(FakeReply::new().text("A street sign")),
        // The reply model wants the detail, opens the picture, then answers.
        SimStep::Reply(FakeReply::new().call(
            "open_images",
            json!({"pictures": [{"message": 1, "position": 1}]}),
        )),
        SimStep::Reply(FakeReply::new().call("send_message", json!({ "text": "it says Elm St" }))),
    ]));
    let llm = mock_llm(sim.clone()).await;
    deployment(&db, llm, &root, None);
    // A text model on the standard Responses dialect takes images, so open_images is offered.
    write(
        &root.join("etc/conf.d/50-text.toml"),
        "[providers.text]\nkind = \"openai_responses\"\nmodel = \"vision-capable\"\n",
    );
    let bot = launch(&db, &root).await;
    let pool = db.pool().await;
    let mut napcat = connect(bot.addr, "sesame").await.unwrap();

    napcat
        .send(group_message(
            1,
            2,
            json!([
                {"type": "at", "data": {"qq": "100"}},
                {"type": "text", "data": {"text": " what does this say? asking for "}},
                {"type": "at", "data": {"qq": "777"}},
                {"type": "image", "data": {"file": "SIGN.png", "url": format!("http://{llm}/pic")}}
            ]),
        ))
        .await
        .unwrap();
    let action = serve_until_send(&mut napcat, None).await;
    assert_eq!(
        action["params"]["message"][0]["data"]["text"],
        "it says Elm St"
    );
    accept(&mut napcat, &action, 9001).await;

    let requests = sim.requests();
    assert_eq!(requests.len(), 3, "describe, then two reply turns");
    let tools = requests[1]["tools"].to_string();
    assert!(
        tools.contains("open_images"),
        "offered to an image-capable model: {tools}"
    );
    let second = requests[2].to_string();
    assert!(
        second.contains("input_image") && second.contains("data:image/png;base64,"),
        "the opened picture went to the model: {second}"
    );

    // The mentioned account got a member number, and its id never reached the archive.
    let stored: String = sqlx::query_scalar("SELECT text FROM chat_line WHERE message_id = 1")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(
        stored.starts_with("[at:bot] what does this say? asking for [at:")
            && !stored.contains("777"),
        "{stored}"
    );
    let numbered: i64 =
        sqlx::query_scalar("SELECT count(*) FROM member_number WHERE account_id = 777")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(numbered, 1);

    bot.stop.cancel();
    tokio::time::timeout(Duration::from_secs(30), bot.finished)
        .await
        .expect("shutdown in time")
        .unwrap()
        .unwrap();
    pool.close().await;
    db.drop().await;
    std::fs::remove_dir_all(&root).ok();
}
