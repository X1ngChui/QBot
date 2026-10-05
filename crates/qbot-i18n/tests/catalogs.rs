#![allow(clippy::unwrap_used, clippy::expect_used)]

use qbot_i18n::{Locale, LocaleError, Locales, Msg, SCHEMA};

const EN: &str = include_str!("../../../locales/en.ftl");

#[test]
fn every_shipped_catalog_is_complete_and_matches_the_schema() {
    let locales = Locales::shipped().unwrap();
    assert_eq!(
        locales
            .iter()
            .map(|l| l.tag().to_owned())
            .collect::<Vec<_>>(),
        ["en", "zh-CN"]
    );
    assert!(!SCHEMA.is_empty());
}

#[test]
fn rendering_substitutes_arguments_and_picks_plural_forms() {
    let en = Locales::english();
    assert_eq!(
        en.render(&Msg::UsageTop {}),
        "Usage: /top [--linked] [count]"
    );
    assert_eq!(
        en.render(&Msg::NameEntry {
            text: "Bobo".into(),
            confidence: "0.80".into()
        }),
        "Bobo (confidence 0.80)"
    );
    assert_eq!(
        en.render(&Msg::MentionsExact { count: 1 }),
        "Mention exactly one account."
    );
    assert_eq!(
        en.render(&Msg::MentionsExact { count: 2 }),
        "Mention exactly 2 accounts."
    );
    assert_eq!(
        en.render(&Msg::TopRow {
            rank: 1,
            name: "Alice".into(),
            tag: String::new(),
            runs: 1,
            tokens: 90
        }),
        "1. Alice  one reply, 90 tokens"
    );
    assert_eq!(
        en.render(&Msg::MembersRow {
            display: "Bob".into(),
            messages: 7
        }),
        "- Bob (7 messages)"
    );
    // No invisible bidi isolation marks around arguments.
    assert!(
        !en.render(&Msg::WhoMessages { count: "3".into() })
            .contains(['\u{2068}', '\u{2069}'])
    );
    // Leading and trailing blanks survive where the wording needs them.
    assert_eq!(en.render(&Msg::ListSeparator {}), ", ");
    assert_eq!(
        en.render(&Msg::BlockUntilSuffix {
            time: "01-15 16:30".into()
        }),
        "  (until 01-15 16:30)"
    );

    let zh = Locales::load("zh-CN", std::path::Path::new("/nonexistent")).unwrap();
    assert_eq!(zh.active().tag(), "zh-CN");
    assert!(zh.render(&Msg::OwnerOnly {}).contains("owner"));
    assert_eq!(zh.render(&Msg::ListSeparator {}), "\u{3001}");
    assert_eq!(
        zh.render(&Msg::MentionsExact { count: 2 }),
        "\u{9700}\u{8981}\u{51C6}\u{786E} @ 2 \u{4E2A}\u{8D26}\u{53F7}\u{3002}"
    );
}

#[test]
fn multi_line_messages_keep_their_lines() {
    let en = Locales::english();
    let help = en.render(&Msg::DetailTasks {});
    assert!(
        help.lines().count() >= 5 && help.starts_with("/tasks [list [page]]\n/tasks show ID"),
        "{help}"
    );
    let detail = en.render(&Msg::HelpDetail {
        command: "/x".into(),
        what: "w".into(),
        access: "a".into(),
        detail: "d".into(),
    });
    assert_eq!(detail, "/x  w\na\n\nd");
}

fn parse(text: &str) -> Vec<LocaleError> {
    Locale::parse("en", text).unwrap_err().0
}

#[test]
fn a_bad_catalog_reports_every_problem_at_once() {
    let found = parse(
        "command-mentions_exact = Mention { $n }\n\
         command-mentions_max = { $max ->\n    [one] only\n}\n\
         command-owner_only = fine\n\
         no-such-message = x\n",
    );
    let text = found
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        text.contains("message `command-mentions_exact` cannot be rendered"),
        "an undeclared variable: {text}"
    );
    assert!(
        text.contains("syntax error"),
        "a selector without a default variant: {text}"
    );
    assert!(text.contains("unknown message `no-such-message`"), "{text}");
    assert!(
        text.contains("missing message `command-bad_number`"),
        "{text}"
    );
    let missing = found
        .iter()
        .filter(|e| matches!(e, LocaleError::Missing { .. }))
        .count();
    assert!(
        missing >= SCHEMA.len() - 3,
        "every other message is reported missing ({missing})"
    );
}

#[test]
fn a_translation_may_omit_a_variable_but_not_invent_one() {
    let with = |id: &str, line: &str| {
        EN.lines()
            .filter(|l| !l.starts_with(&format!("{id} =")))
            .chain(std::iter::once(line))
            .collect::<Vec<_>>()
            .join("\n")
    };
    // Dropping a variable the code supplies is allowed (some languages do not need it).
    Locale::parse(
        "en",
        &with("command-name_entry", "command-name_entry = { $text }"),
    )
    .unwrap();
    // Inventing one is an error.
    let errors = Locale::parse(
        "en",
        &with(
            "command-name_entry",
            "command-name_entry = { $text } { $nope }",
        ),
    )
    .unwrap_err()
    .0;
    assert!(
        matches!(&errors[0], LocaleError::Unrenderable { id, .. } if id == "command-name_entry"),
        "{errors:?}"
    );
}

#[test]
fn locales_load_from_the_directory_and_unknown_tags_are_errors() {
    let dir = std::env::temp_dir().join(format!("qbot-i18n-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("en.ftl"),
        EN.replace("Invalid duration", "Bad duration"),
    )
    .unwrap();
    std::fs::write(dir.join("fr.ftl"), EN).unwrap();

    let en = Locales::load("en", &dir).unwrap();
    assert!(
        en.render(&Msg::DurationInvalid {})
            .starts_with("Bad duration"),
        "a file overrides the built-in catalog"
    );
    assert_eq!(
        Locales::load("fr", &dir).unwrap().active().tag(),
        "fr",
        "a file supplies a locale that is not built in"
    );
    assert!(matches!(
        Locales::load("de", &dir).unwrap_err().0[0],
        LocaleError::UnknownLocale(_)
    ));
    assert!(matches!(
        Locales::load("not a tag!", &dir).unwrap_err().0[0],
        LocaleError::UnknownLocale(_)
    ));
    std::fs::write(dir.join("fr.ftl"), "# nothing\n").unwrap();
    assert_eq!(
        Locales::load("fr", &dir).unwrap_err().0.len(),
        SCHEMA.len(),
        "an incomplete override is refused, not partly used"
    );
    std::fs::remove_dir_all(&dir).ok();
}
