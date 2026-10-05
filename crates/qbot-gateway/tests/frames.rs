#![allow(clippy::unwrap_used, clippy::expect_used)]

use qbot_gateway::notice::{archive_id, archive_text};
use qbot_gateway::render::{render, typed_text};
use qbot_gateway::trigger::{Nicknames, Why, decide};
use qbot_gateway::wire::{Frame, Mention, NoticeKind, Segment, parse_frame};

fn ctx() -> qbot_gateway::render::RenderContext {
    qbot_gateway::render::RenderContext {
        bot: bot(),
        forward_max_lines: 30,
    }
}

fn bot() -> qbot_core::AccountId {
    qbot_core::AccountId::new(100).unwrap()
}

fn message(extra: &str, segments: &str) -> String {
    format!(
        r#"{{"post_type":"message","message_type":"group","time":1700000000,"self_id":100,"user_id":200,"group_id":900,"message_id":55,"message":{segments}{extra}}}"#
    )
}

#[test]
fn group_messages_parse_with_ids_and_segments() {
    let frame = parse_frame(&message(
        "",
        r#"[{"type":"reply","data":{"id":"7"}},{"type":"at","data":{"qq":"100"}},{"type":"text","data":{"text":" hi "}},{"type":"face","data":{"id":"14"}},{"type":"image","data":{"file":"x"}}]"#,
    ));
    let Frame::Message(m) = frame else {
        panic!("{frame:?}")
    };
    assert_eq!(
        (
            m.group.get(),
            m.message.get(),
            m.sender.get(),
            m.self_id.get()
        ),
        (900, 55, 200, 100)
    );
    assert_eq!(m.at.get(), 1_700_000_000_000);
    assert!(!m.from_bot);
    assert!(matches!(m.segments[1], Segment::At(Mention::Account(a)) if a.get() == 100));
    assert_eq!(
        render(&m.segments, &ctx()),
        "[reply:7][at:bot] hi [face:14][image]"
    );
    assert_eq!(typed_text(&m.segments), "hi");
}

#[test]
fn the_bots_own_messages_are_recognised_two_ways() {
    let sent = message("", "[]")
        .replace("\"message\"", "\"message_sent\"")
        .replacen("message_sent", "message", 0);
    let sent = sent.replace(r#""post_type":"message""#, r#""post_type":"message_sent""#);
    assert!(matches!(parse_frame(&sent), Frame::Message(m) if m.from_bot));
    let by_id = message("", "[]").replace(r#""user_id":200"#, r#""user_id":100"#);
    assert!(matches!(parse_frame(&by_id), Frame::Message(m) if m.from_bot));
}

#[test]
fn markers_cannot_be_forged_by_members() {
    let frame = parse_frame(&message(
        "",
        r#"[{"type":"text","data":{"text":"[at:1] [image]\u0000"}}]"#,
    ));
    let Frame::Message(m) = frame else { panic!() };
    let text = render(&m.segments, &ctx());
    assert!(
        !text.contains('[') && !text.contains(']') && !text.contains('\0'),
        "{text}"
    );
}

#[test]
fn dice_results_and_unknown_segments_render_safely() {
    let frame = parse_frame(&message(
        "",
        r#"[{"type":"dice","data":{"result":"4"}},{"type":"rps","data":{}},{"type":"weird kind!","data":{}},{"type":"file","data":{"name":"a b/../c.txt"}}]"#,
    ));
    let Frame::Message(m) = frame else { panic!() };
    assert_eq!(
        render(&m.segments, &ctx()),
        "[dice:4][rps][unsupported:weirdkind][file:ab..c.txt]"
    );
}

#[test]
fn irrelevant_and_malformed_frames_are_ignored_with_a_reason() {
    assert_eq!(parse_frame("nope"), Frame::Ignored("not json"));
    assert_eq!(
        parse_frame(r#"{"post_type":"meta_event","self_id":1}"#),
        Frame::Ignored("not a message or notice")
    );
    assert_eq!(
        parse_frame(&message("", "[]").replace("group\"", "private\"")),
        Frame::Ignored("not a group message")
    );
    assert_eq!(
        parse_frame(&message("", "[]").replace(r#""user_id":200"#, r#""user_id":0"#)),
        Frame::Ignored("group message without usable ids")
    );
}

#[test]
fn action_responses_carry_the_message_id_and_failure_detail() {
    let Frame::Response(ok) =
        parse_frame(r#"{"status":"ok","retcode":0,"data":{"message_id":77},"echo":"a1"}"#)
    else {
        panic!()
    };
    assert!(ok.ok && ok.message_id.map(|m| m.get()) == Some(77) && ok.echo == "a1");
    let Frame::Response(bad) =
        parse_frame(r#"{"status":"failed","retcode":1200,"wording":"muted","echo":"a2"}"#)
    else {
        panic!()
    };
    assert!(!bad.ok && bad.retcode == 1200 && bad.detail == "muted" && bad.message_id.is_none());
}

#[test]
fn notices_become_marker_lines_with_stable_synthetic_ids() {
    let raw = r#"{"post_type":"notice","notice_type":"group_ban","sub_type":"ban","self_id":100,"user_id":200,"group_id":900,"duration":600,"time":1700000000}"#;
    let Frame::Notice(n) = parse_frame(raw) else {
        panic!()
    };
    assert_eq!(n.kind, NoticeKind::Ban { seconds: 600 });
    assert_eq!(archive_text(&n), "[notice:muted:600s]");
    assert_eq!(
        archive_id(&n),
        archive_id(&n.clone()),
        "redelivery maps to the same id"
    );
    assert!(archive_id(&n).get() >= 1 << 52, "far above platform ids");
    let mut later = n.clone();
    later.at = qbot_core::UnixMillis::new(n.at.get() + 1000);
    assert_ne!(archive_id(&n), archive_id(&later));

    let poke = r#"{"post_type":"notice","notice_type":"notify","sub_type":"poke","self_id":100,"user_id":200,"target_id":100,"group_id":900,"time":1}"#;
    let Frame::Notice(p) = parse_frame(poke) else {
        panic!()
    };
    assert_eq!(archive_text(&p), "[notice:poked_you]");
    let recall = r#"{"post_type":"notice","notice_type":"group_recall","self_id":100,"user_id":200,"operator_id":300,"group_id":900,"message_id":5,"time":1}"#;
    let Frame::Notice(r) = parse_frame(recall) else {
        panic!()
    };
    assert_eq!(archive_text(&r), "[notice:message_recalled_by_admin]");
    assert_eq!(
        parse_frame(
            r#"{"post_type":"notice","notice_type":"friend_add","self_id":1,"group_id":2,"user_id":3}"#
        ),
        Frame::Ignored("unsupported notice")
    );
}

#[test]
fn nicknames_match_whole_words_only() {
    // Built from escapes: the source carries no CJK.
    let nick = "\u{5C0F}\u{53C9}"; // a two-character nickname
    let names = Nicknames::new(&[nick, "Bobo"]);
    assert_eq!(names.hit(&format!("hey {nick} come here")), Some(nick));
    assert_eq!(names.hit("hello BOBO!"), Some("Bobo"));
    assert_eq!(
        names.hit("bobox is a word"),
        None,
        "a Latin nickname is not carved out of a longer word"
    );
    assert_eq!(names.hit("nothing here"), None);
}

#[test]
fn an_at_outranks_a_nickname() {
    let names = Nicknames::new(&["Bobo"]);
    assert_eq!(decide(true, "bobo", &names), Some(Why::AtBot));
    assert_eq!(
        decide(false, "hi bobo", &names),
        Some(Why::Nickname("Bobo".into()))
    );
    assert_eq!(decide(false, "hi", &names), None);
}

fn parsed(segments: &str) -> qbot_gateway::wire::GroupMessage {
    let Frame::Message(m) = parse_frame(&message("", segments)) else {
        panic!()
    };
    m
}

#[test]
fn stickers_show_their_label_until_described_and_carry_their_id() {
    let m = parsed(
        r#"[{"type":"mface","data":{"emoji_id":"e9","url":"https://gxh.vip.qq.com/club/item/parcel/item/ab/abcd/raw300.gif","summary":"[shy]"}}]"#,
    );
    assert_eq!(render(&m.segments, &ctx()), "[sticker:\u{FF3B}shy\u{FF3D}]");
    let job = qbot_gateway::media::job_for(&m, &ctx());
    assert_eq!(job.items.len(), 1);
    assert_eq!(job.items[0].kind, qbot_media::Kind::Sticker);
    assert_eq!(job.items[0].reference.key.as_deref(), Some("e9"));
}

fn forward(nodes: usize) -> String {
    let node = |i: usize| {
        format!(
            r#"{{"sender":{{"nickname":"Friend {i}"}},"time":1,"message":[{{"type":"text","data":{{"text":"line {i}"}}}},{{"type":"image","data":{{"file":"F{i}.jpg"}}}}]}}"#
        )
    };
    let nodes: Vec<String> = (0..nodes).map(node).collect();
    format!(
        r#"{{"type":"forward","data":{{"id":"fw1","content":[{}]}}}}"#,
        nodes.join(",")
    )
}

#[test]
fn a_forwarded_record_is_shown_under_its_marker_up_to_the_limit() {
    let small = qbot_gateway::render::RenderContext {
        bot: bot(),
        forward_max_lines: 2,
    };
    let m = parsed(&format!(
        "[{},{}]",
        forward(3),
        r#"{"type":"image","data":{"file":"TOP.jpg"}}"#
    ));
    assert_eq!(
        render(&m.segments, &small),
        "[forward:3]\n  | Friend 0: line 0[image]\n  | Friend 1: line 1[image]\n  | [forward_more:1][image]"
    );
    // Positions follow the rendered markers: two shown pictures inside the record, then the
    // picture posted with it. The hidden third message contributes nothing.
    let job = qbot_gateway::media::job_for(&m, &small);
    let positions: Vec<_> = job
        .items
        .iter()
        .map(|i| (i.index, i.nested, i.reference.key.clone().unwrap()))
        .collect();
    assert_eq!(
        positions,
        [
            (0, true, "F0.jpg".into()),
            (1, true, "F1.jpg".into()),
            (2, false, "TOP.jpg".into())
        ]
    );
}

#[test]
fn inside_a_record_mentions_name_nobody_quotes_vanish_and_records_do_not_nest() {
    let node = r#"{"sender":{"card":"Pal","nickname":"x"},"message":[{"type":"reply","data":{"id":"5"}},{"type":"at","data":{"qq":"300"}},{"type":"text","data":{"text":" hi [x]"}},{"type":"record","data":{"file":"v.amr"}},{"type":"forward","data":{"content":[{"sender":{},"message":[]}]}}]}"#;
    let m = parsed(&format!(
        r#"[{{"type":"forward","data":{{"content":[{node}]}}}},{{"type":"record","data":{{"file":"top.amr"}}}}]"#
    ));
    assert_eq!(
        render(&m.segments, &ctx()),
        "[forward:1]\n  | Pal: @someone hi \u{FF3B}x\u{FF3D}[voice][forward:1][voice]"
    );
    // The clip inside the record is never transcribed, but it still takes a position.
    let job = qbot_gateway::media::job_for(&m, &ctx());
    assert_eq!(
        job.items
            .iter()
            .map(|i| (i.index, i.nested))
            .collect::<Vec<_>>(),
        [(1, false)]
    );
    assert!(
        qbot_gateway::render::mentioned(&m.segments, bot()).is_empty(),
        "mentions inside a record give nobody a number"
    );
    // A record whose content did not come inline cannot be shown.
    let bare = parsed(r#"[{"type":"forward","data":{"id":"fw9"}}]"#);
    assert_eq!(render(&bare.segments, &ctx()), "[forward]");
}
