#![allow(clippy::unwrap_used, clippy::expect_used)]

//! A simulated NapCat talking to the real gateway, pipeline, supervisor, run loop and
//! `send_message` tool. Only the database (archive, run log) and the model are stand-ins.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use qbot_agent::sim::{MemorySink, SimWorld, renderer};
use qbot_agent::{
    Archive, ContextSource, Delivery, DeliveryError, EnvError, GroupPolicy, OutSegment, RunDeps,
    RunLimits, RunLog, Supervisor, SupervisorConfig, ToolSet,
};
use qbot_context::{ChatLine, Speaker};
use qbot_core::{AccountId, GroupId, MemberNo, MessageId, SystemClock};
use qbot_gateway::bridge::Bridge;
use qbot_gateway::delivery::{DeliveryTimeouts, OneBotDelivery};
use qbot_gateway::echo::EchoBoard;
use qbot_gateway::intake::{Incoming, Intake, Stored};
use qbot_gateway::pipeline::{CommandRequest, Commands, Pipeline, PipelineConfig};
use qbot_gateway::server::{GatewayServer, serve};
use qbot_gateway::trigger::Nicknames;
use qbot_llm::ReasoningEffort;
use qbot_llm::fake::{FakeProvider, FakeReply, Step};
use qbot_tools::SendMessage;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_util::sync::CancellationToken;

const BOT: i64 = 100;
const GROUP: i64 = 900;

#[derive(Default)]
struct MemoryIntake {
    lines: Mutex<Vec<Incoming>>,
}

#[async_trait]
impl Intake for MemoryIntake {
    async fn append(&self, line: Incoming) -> Result<Stored, EnvError> {
        let mut lines = self.lines.lock().unwrap();
        if lines
            .iter()
            .any(|l| l.group == line.group && l.message == line.message)
        {
            return Ok(Stored::Duplicate);
        }
        let speaker = line.author.map_or(Speaker::Bot, |account| Speaker::Member {
            account,
            number: MemberNo::new(1),
        });
        let stored = ChatLine {
            message: line.message,
            speaker,
            at: line.at,
            text: line.text.clone(),
        };
        lines.push(line);
        let ordinal = lines.len() as u64;
        Ok(Stored::New {
            line: stored,
            ordinal,
        })
    }

    async fn is_bot_message(&self, group: GroupId, message: MessageId) -> Result<bool, EnvError> {
        Ok(self
            .lines
            .lock()
            .unwrap()
            .iter()
            .any(|l| l.group == group && l.message == message && l.author.is_none()))
    }
}

#[derive(Default)]
struct RecordedCommands(Mutex<Vec<CommandRequest>>);

#[async_trait]
impl Commands for RecordedCommands {
    fn recognizes(&self, word: &str) -> bool {
        word == "/ping"
    }
    async fn run(&self, request: CommandRequest) {
        self.0.lock().unwrap().push(request);
    }
}

struct Rig {
    url: String,
    world: Arc<SimWorld>,
    intake: Arc<MemoryIntake>,
    commands: Arc<RecordedCommands>,
    delivery: Arc<OneBotDelivery>,
    fake: Arc<FakeProvider>,
    cancel: CancellationToken,
}

type Client =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn rig(script: Vec<Step>) -> Rig {
    rig_with(script, Duration::from_secs(5)).await
}

async fn rig_with(script: Vec<Step>, echo: Duration) -> Rig {
    let group = GroupId::new(GROUP).unwrap();
    let world = SimWorld::new(group);
    let bridge = Arc::new(Bridge::new());
    let echoes = Arc::new(EchoBoard::new());
    let timeouts = DeliveryTimeouts {
        action: Duration::from_secs(5),
        echo,
    };
    let delivery = Arc::new(OneBotDelivery::new(
        bridge.clone(),
        echoes.clone(),
        timeouts,
    ));
    let fake = Arc::new(FakeProvider::new(script));
    let deps = Arc::new(RunDeps {
        provider: fake.clone(),
        tools: ToolSet::new()
            .with(SendMessage::new(delivery.clone(), 4))
            .unwrap(),
        archive: world.clone() as Arc<dyn Archive>,
        log: world.clone() as Arc<dyn RunLog>,
        sink: MemorySink::new(),
        renderer: renderer(),
        clock: Arc::new(SystemClock),
        limits: RunLimits::default(),
        reasoning: ReasoningEffort::Low,
        media: None,
    });
    let supervisor = Arc::new(Supervisor::new(
        SupervisorConfig {
            capacity: 8,
            concurrency: 4,
            reply_deadline: Duration::from_secs(30),
        },
        deps,
        world.clone() as Arc<dyn ContextSource>,
        world.clone() as Arc<dyn GroupPolicy>,
    ));
    let intake = Arc::new(MemoryIntake::default());
    let commands = Arc::new(RecordedCommands::default());
    let pipeline = Arc::new(Pipeline::new(
        PipelineConfig {
            bot: AccountId::new(BOT).unwrap(),
            echo_keep: echo,
            forward_max_lines: 30,
        },
        intake.clone(),
        commands.clone(),
        supervisor,
        Nicknames::new(&["Bobo"]),
        echoes,
    ));
    let cancel = CancellationToken::new();
    let server = GatewayServer {
        pipeline,
        bridge,
        bot: AccountId::new(BOT).unwrap(),
        access_token: Some("sesame".into()),
        path: "/onebot/v11/ws".into(),
        cancel: cancel.clone(),
    };
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}/onebot/v11/ws", listener.local_addr().unwrap());
    tokio::spawn(serve(listener, server, cancel.clone()));
    Rig {
        url,
        world,
        intake,
        commands,
        delivery,
        fake,
        cancel,
    }
}

async fn connect(
    rig: &Rig,
    token: Option<&str>,
) -> Result<Client, tokio_tungstenite::tungstenite::Error> {
    let mut request = rig.url.clone().into_client_request().unwrap();
    request
        .headers_mut()
        .insert("x-self-id", BOT.to_string().parse().unwrap());
    if let Some(token) = token {
        request
            .headers_mut()
            .insert("authorization", format!("Bearer {token}").parse().unwrap());
    }
    tokio_tungstenite::connect_async(request)
        .await
        .map(|(client, _)| client)
}

fn group_message(id: i64, user: i64, segments: Value) -> Message {
    Message::text(
        json!({"post_type": "message", "message_type": "group", "time": 1_700_000_000, "self_id": BOT,
               "user_id": user, "group_id": GROUP, "message_id": id, "message": segments})
        .to_string(),
    )
}

async fn next_action(client: &mut Client) -> Value {
    let frame = tokio::time::timeout(Duration::from_secs(5), client.next())
        .await
        .expect("an action in time")
        .unwrap()
        .unwrap();
    serde_json::from_str(frame.to_text().unwrap()).unwrap()
}

async fn settle(rig: &Rig) {
    for _ in 0..200 {
        if let Some(run) = rig.world.logged_runs().first().copied()
            && rig.world.summary(run).is_some()
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("the run did not finish");
}

fn send_script(text: &str) -> Vec<Step> {
    vec![Step::Reply(
        FakeReply::new().call("send_message", json!({ "text": text })),
    )]
}

#[tokio::test(flavor = "multi_thread")]
async fn an_at_runs_the_agent_and_its_send_round_trips_through_the_platform() {
    let rig = rig(send_script("hello there")).await;
    let mut napcat = connect(&rig, Some("sesame")).await.unwrap();

    napcat
        .send(group_message(
            1,
            200,
            json!([{"type": "at", "data": {"qq": BOT.to_string()}}, {"type": "text", "data": {"text": " hi"}}]),
        ))
        .await
        .unwrap();

    let action = next_action(&mut napcat).await;
    assert_eq!(action["action"], "send_group_msg");
    assert_eq!(action["params"]["group_id"], GROUP);
    assert_eq!(
        action["params"]["message"][0],
        json!({"type": "text", "data": {"text": "hello there"}})
    );

    // The platform accepts the send, then reports the bot's own message.
    let echo = action["echo"].as_str().unwrap();
    napcat
        .send(Message::text(
            json!({"status": "ok", "retcode": 0, "data": {"message_id": 9001}, "echo": echo})
                .to_string(),
        ))
        .await
        .unwrap();
    napcat
        .send(Message::text(
            json!({"post_type": "message_sent", "message_type": "group", "time": 1_700_000_001, "self_id": BOT,
                   "user_id": BOT, "group_id": GROUP, "message_id": 9001,
                   "message": [{"type": "text", "data": {"text": "hello there"}}]})
            .to_string(),
        ))
        .await
        .unwrap();

    settle(&rig).await;
    let run = rig.world.logged_runs()[0];
    let summary = rig.world.summary(run).unwrap();
    assert_eq!(summary.sends, 1);
    let texts: Vec<String> = rig
        .intake
        .lines
        .lock()
        .unwrap()
        .iter()
        .map(|l| l.text.clone())
        .collect();
    assert_eq!(texts, ["[at:bot] hi", "hello there"]);
    rig.cancel.cancel();
}

/// The platform accepts the send and reports the bot's own message back with `segments`.
async fn ack_and_echo(napcat: &mut Client, action: &Value, id: i64, segments: Value) {
    let echo = action["echo"].as_str().unwrap();
    napcat
        .send(Message::text(
            json!({"status": "ok", "retcode": 0, "data": {"message_id": id}, "echo": echo})
                .to_string(),
        ))
        .await
        .unwrap();
    napcat
        .send(Message::text(
            json!({"post_type": "message_sent", "message_type": "group", "time": 1_700_000_001, "self_id": BOT,
                   "user_id": BOT, "group_id": GROUP, "message_id": id, "message": segments})
            .to_string(),
        ))
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_dice_roll_is_requested_without_a_result_and_the_run_continues_from_the_platforms() {
    let rig = rig(vec![
        // A numbered marker is refused before anything reaches the platform...
        Step::Reply(FakeReply::new().call(
            "send_message",
            json!({ "text": "[dice:6]", "end_turn": false }),
        )),
        // ...so the model asks the platform to roll and waits for what it came up with...
        Step::Reply(FakeReply::new().call(
            "send_message",
            json!({ "text": "[dice]", "end_turn": false }),
        )),
        // ...and reacts to it.
        Step::Reply(FakeReply::new().call("send_message", json!({ "text": "two, you win" }))),
    ])
    .await;
    let mut napcat = connect(&rig, Some("sesame")).await.unwrap();
    napcat
        .send(group_message(
            1,
            200,
            json!([{"type": "at", "data": {"qq": BOT.to_string()}}, {"type": "text", "data": {"text": " roll, odd I win"}}]),
        ))
        .await
        .unwrap();

    // The first action the platform sees is the bare request: no result in it.
    let roll = next_action(&mut napcat).await;
    assert_eq!(roll["action"], "send_group_msg");
    assert_eq!(
        roll["params"]["message"],
        json!([{"type": "dice", "data": {}}])
    );
    // The platform rolls a 2, whatever the model would have liked.
    ack_and_echo(
        &mut napcat,
        &roll,
        9001,
        json!([{"type": "dice", "data": {"result": "2"}}]),
    )
    .await;

    let reaction = next_action(&mut napcat).await;
    assert_eq!(
        reaction["params"]["message"][0]["data"]["text"],
        "two, you win"
    );
    ack_and_echo(
        &mut napcat,
        &reaction,
        9002,
        reaction["params"]["message"].clone(),
    )
    .await;
    settle(&rig).await;

    let requests = rig.fake.recorded();
    let seen = |i: usize| format!("{:?}", requests[i].conversation);
    assert!(
        seen(1).contains("cannot choose the result"),
        "the refused marker is explained"
    );
    assert!(!seen(1).contains("result:6"));
    assert!(
        seen(2).contains("sent [msg:9001] [dice result:2]"),
        "the follow-up turn sees the platform's roll: {}",
        seen(2)
    );
    let archived: Vec<String> = rig
        .intake
        .lines
        .lock()
        .unwrap()
        .iter()
        .map(|l| l.text.clone())
        .collect();
    assert_eq!(
        archived,
        [
            "[at:bot] roll, odd I win",
            "[dice result:2]",
            "two, you win"
        ]
    );
    rig.cancel.cancel();
}

#[tokio::test(flavor = "multi_thread")]
async fn nicknames_trigger_but_blocked_members_and_plain_chatter_do_not() {
    let rig = rig(send_script("hi")).await;
    rig.world.block(300);
    let mut napcat = connect(&rig, Some("sesame")).await.unwrap();

    napcat
        .send(group_message(
            1,
            200,
            json!([{"type": "text", "data": {"text": "just chatting"}}]),
        ))
        .await
        .unwrap();
    napcat
        .send(group_message(
            2,
            300,
            json!([{"type": "text", "data": {"text": "hey bobo, answer me"}}]),
        ))
        .await
        .unwrap();
    // A redelivered event must not start a second run or a second line.
    napcat
        .send(group_message(
            2,
            300,
            json!([{"type": "text", "data": {"text": "hey bobo, answer me"}}]),
        ))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        rig.world.logged_runs().is_empty(),
        "no run for chatter or for a blocked member"
    );
    assert_eq!(
        rig.intake.lines.lock().unwrap().len(),
        2,
        "both lines are archived; the duplicate is not"
    );

    napcat
        .send(group_message(
            3,
            200,
            json!([{"type": "text", "data": {"text": "BOBO are you there"}}]),
        ))
        .await
        .unwrap();
    let action = next_action(&mut napcat).await;
    assert_eq!(action["action"], "send_group_msg");
    rig.cancel.cancel();
}

#[tokio::test(flavor = "multi_thread")]
async fn commands_are_recognised_exactly_and_never_reach_the_agent() {
    let rig = rig(send_script("unused")).await;
    let mut napcat = connect(&rig, Some("sesame")).await.unwrap();
    napcat
        .send(group_message(
            1,
            200,
            json!([{"type": "text", "data": {"text": "/ping  one two "}}, {"type": "at", "data": {"qq": "300"}}, {"type": "at", "data": {"qq": BOT.to_string()}}]),
        ))
        .await
        .unwrap();
    napcat
        .send(group_message(
            2,
            200,
            json!([{"type": "text", "data": {"text": "/PING"}}]),
        ))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let seen = rig.commands.0.lock().unwrap().clone();
    assert_eq!(seen.len(), 1, "only the exact lower-case word is a command");
    assert_eq!(seen[0].word, "/ping");
    assert_eq!(seen[0].args, "one two");
    assert_eq!(
        seen[0].mentions.iter().map(|a| a.get()).collect::<Vec<_>>(),
        [300],
        "the bot is not a mention target"
    );
    assert!(rig.world.logged_runs().is_empty());
    rig.cancel.cancel();
}

#[tokio::test(flavor = "multi_thread")]
async fn connections_need_the_token_and_the_right_account() {
    let rig = rig(vec![]).await;
    assert!(connect(&rig, None).await.is_err());
    assert!(connect(&rig, Some("wrong")).await.is_err());
    assert!(connect(&rig, Some("sesame")).await.is_ok());
    rig.cancel.cancel();
}

#[tokio::test(flavor = "multi_thread")]
async fn sending_without_a_connection_or_after_a_rejection_reports_why() {
    let rig = rig_with(vec![], Duration::from_millis(300)).await;
    let group = GroupId::new(GROUP).unwrap();
    let nowhere = rig
        .delivery
        .send(group, vec![OutSegment::Text("x".into())])
        .await;
    assert!(
        matches!(nowhere, Err(DeliveryError::Unavailable(_))),
        "{nowhere:?}"
    );

    let mut napcat = connect(&rig, Some("sesame")).await.unwrap();
    let delivery = rig.delivery.clone();
    let send = tokio::spawn(async move { delivery.send(group, vec![OutSegment::Dice]).await });
    let action = next_action(&mut napcat).await;
    assert_eq!(
        action["params"]["message"][0],
        json!({"type": "dice", "data": {}})
    );
    napcat
        .send(Message::text(json!({"status": "failed", "retcode": 1200, "wording": "bot is muted", "echo": action["echo"]}).to_string()))
        .await
        .unwrap();
    assert_eq!(
        send.await.unwrap(),
        Err(DeliveryError::Rejected("bot is muted".into()))
    );

    // Accepted but never reported back.
    let delivery = rig.delivery.clone();
    let send = tokio::spawn(async move {
        delivery
            .send(group, vec![OutSegment::Text("y".into())])
            .await
    });
    let action = next_action(&mut napcat).await;
    napcat.send(Message::text(json!({"status": "ok", "retcode": 0, "data": {"message_id": 5}, "echo": action["echo"]}).to_string())).await.unwrap();
    assert_eq!(send.await.unwrap(), Err(DeliveryError::EchoMissing));
    rig.cancel.cancel();
}
