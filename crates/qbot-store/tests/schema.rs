#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

async fn rejected(db: &common::TestDb, sql: &str) -> bool {
    sqlx::query(sql).execute(db.pool()).await.is_err()
}

#[tokio::test]
async fn migrations_are_idempotent_and_the_expected_tables_exist() {
    let db = db!();
    db.store.migrate().await.unwrap();
    db.store.migrate().await.unwrap();
    let tables: Vec<String> = sqlx::query_scalar(
        "SELECT table_name FROM information_schema.tables WHERE table_schema = 'public' \
         AND table_type = 'BASE TABLE' AND table_name <> '_sqlx_migrations' ORDER BY table_name",
    )
    .fetch_all(db.pool())
    .await
    .unwrap();
    assert_eq!(
        tables,
        [
            "account",
            "account_link",
            "chat_line",
            "episode",
            "episode_embedding",
            "group_admin",
            "group_block",
            "group_state",
            "held_claim",
            "link_invitation",
            "media_cache",
            "media_ref",
            "member_number",
            "memory",
            "memory_evidence",
            "name_reference",
            "name_removed",
            "name_ruling",
            "name_set",
            "note",
            "recurrence",
            "run",
            "run_item",
            "slice_skip",
            "timer",
            "usage_event"
        ]
    );
    db.drop_db().await;
}

#[tokio::test]
async fn constraints_reject_invalid_rows() {
    let db = db!();
    // chat_line: a member needs an account, the bot must not have one; speaker is closed.
    assert!(rejected(&db, "INSERT INTO chat_line (group_id, ordinal, message_id, speaker, at_ms, text) VALUES (1, 1, 1, 'member', 0, 'x')").await);
    assert!(rejected(&db, "INSERT INTO chat_line (group_id, ordinal, message_id, speaker, account_id, at_ms, text) VALUES (1, 1, 2, 'bot', 5, 0, 'x')").await);
    assert!(rejected(&db, "INSERT INTO chat_line (group_id, ordinal, message_id, speaker, at_ms, text) VALUES (1, 1, 3, 'robot', 0, 'x')").await);
    sqlx::query("INSERT INTO chat_line (group_id, ordinal, message_id, speaker, at_ms, text) VALUES (1, 1, 4, 'bot', 0, 'ok')").execute(db.pool()).await.unwrap();
    assert!(rejected(&db, "INSERT INTO chat_line (group_id, ordinal, message_id, speaker, at_ms, text) VALUES (1, 2, 4, 'bot', 0, 'dup')").await, "unique message id per group");

    // member numbers are positive and unique per group.
    assert!(rejected(&db, "INSERT INTO member_number VALUES (1, 10, 0)").await);
    sqlx::query("INSERT INTO member_number VALUES (1, 10, 1)")
        .execute(db.pool())
        .await
        .unwrap();
    assert!(rejected(&db, "INSERT INTO member_number VALUES (1, 11, 1)").await);
    // ... and, once assigned, never changed or removed.
    assert!(
        rejected(
            &db,
            "UPDATE member_number SET number = 2 WHERE account_id = 10"
        )
        .await
    );
    assert!(
        rejected(
            &db,
            "UPDATE member_number SET account_id = 11 WHERE number = 1"
        )
        .await
    );
    assert!(rejected(&db, "DELETE FROM member_number WHERE account_id = 10").await);
    assert!(rejected(&db, "TRUNCATE member_number").await);

    // run: ended_ms and end_reason are set together; items need a run.
    assert!(rejected(&db, "INSERT INTO run (group_id, trigger_kind, trigger, started_ms, ended_ms) VALUES (1, 'addressed', '{}', 0, 5)").await);
    assert!(rejected(&db, "INSERT INTO run (group_id, trigger_kind, trigger, started_ms) VALUES (1, 'other', '{}', 0)").await);
    assert!(
        rejected(
            &db,
            "INSERT INTO run_item (run_id, seq, kind, payload) VALUES (999, 0, 'meta', '{}')"
        )
        .await
    );

    // timer: wakes need their fields, jobs need a kind, done needs an outcome, leases only when claimed.
    assert!(
        rejected(
            &db,
            "INSERT INTO timer (kind, due_ms, state, created_ms) VALUES ('wake', 0, 'pending', 0)"
        )
        .await
    );
    assert!(
        rejected(
            &db,
            "INSERT INTO timer (kind, due_ms, state, created_ms) VALUES ('job', 0, 'pending', 0)"
        )
        .await
    );
    assert!(rejected(&db, "INSERT INTO timer (kind, due_ms, state, created_ms, job_kind) VALUES ('job', 0, 'done', 0, 'decay')").await);
    assert!(rejected(&db, "INSERT INTO timer (kind, due_ms, state, created_ms, job_kind, lease_until_ms) VALUES ('job', 0, 'pending', 0, 'decay', 5)").await);
    assert!(rejected(&db, "INSERT INTO timer (kind, due_ms, state, created_ms, job_kind) VALUES ('job', 0, 'pending', 0, 'mystery')").await);
    sqlx::query("INSERT INTO timer (kind, due_ms, state, created_ms, job_kind) VALUES ('job', 0, 'pending', 0, 'decay')").execute(db.pool()).await.unwrap();

    // Repeat rules: each kind has exactly its own fields and a zone, only on wakes; a last run
    // only on a repeating task.
    let wake = "INSERT INTO timer (kind, due_ms, state, created_ms, group_id, intent, chain_id, chain_depth, origin";
    let values = "VALUES ('wake', 0, 'pending', 0, 1, 'x', 1, 0, 'model'";
    for bad in [
        "repeat_kind, repeat_zone) {v}, 'interval', 'UTC')",
        "repeat_kind, repeat_interval_ms) {v}, 'interval', 3600000)",
        "repeat_kind, repeat_interval_ms, repeat_zone) {v}, 'interval', 90000, 'UTC')",
        "repeat_kind, repeat_interval_ms, repeat_minute, repeat_zone) {v}, 'interval', 3600000, 60, 'UTC')",
        "repeat_kind, repeat_minute, repeat_zone) {v}, 'daily', 1440, 'UTC')",
        "repeat_kind, repeat_minute, repeat_days, repeat_zone) {v}, 'daily', 60, 1, 'UTC')",
        "repeat_kind, repeat_minute, repeat_zone) {v}, 'weekly', 60, 'UTC')",
        "repeat_kind, repeat_minute, repeat_days, repeat_zone) {v}, 'weekly', 60, 128, 'UTC')",
        "repeat_kind, repeat_minute, repeat_zone) {v}, 'monthly', 60, 'UTC')",
        "repeat_zone) {v}, 'UTC')",
        "last_due_ms, last_outcome) {v}, 0, 'ran')",
        "repeat_kind, repeat_minute, repeat_zone, last_due_ms) {v}, 'daily', 60, 'UTC', 0)",
        "repeat_kind, repeat_minute, repeat_zone, last_due_ms, last_outcome) {v}, 'daily', 60, 'UTC', 0, 'job_ok')",
    ] {
        let sql = format!("{wake}, {}", bad.replace("{v}", values));
        assert!(rejected(&db, &sql).await, "{sql}");
    }
    assert!(rejected(&db, "INSERT INTO timer (kind, due_ms, state, created_ms, job_kind, repeat_kind, repeat_minute, repeat_zone) VALUES ('job', 0, 'pending', 0, 'decay', 'daily', 60, 'UTC')").await);
    for good in [
        "repeat_kind, repeat_interval_ms, repeat_zone) {v}, 'interval', 3600000, 'UTC')",
        "repeat_kind, repeat_minute, repeat_zone, last_due_ms, last_outcome, last_outcome_detail) {v}, 'daily', 0, 'Asia/Shanghai', 0, 'ran', 'completed')",
        "repeat_kind, repeat_minute, repeat_days, repeat_zone) {v}, 'weekly', 1439, 127, 'UTC')",
    ] {
        let sql = format!("{wake}, {}", good.replace("{v}", values));
        sqlx::query(&sql).execute(db.pool()).await.unwrap();
    }

    // Ordinals are unique per group and start at 1.
    assert!(rejected(&db, "INSERT INTO chat_line (group_id, ordinal, message_id, speaker, at_ms, text) VALUES (1, 1, 5, 'bot', 0, 'same ordinal')").await);
    assert!(rejected(&db, "INSERT INTO chat_line (group_id, ordinal, message_id, speaker, at_ms, text) VALUES (1, 0, 6, 'bot', 0, 'zero')").await);

    // names: a reference points at known accounts and names a member exactly when it says
    // so; how a line uses a name and what shows whom it means are closed sets; one reading of
    // an expression per episode and line. A name is set once per member, a ruling names a
    // member exactly when it is for one.
    sqlx::query("INSERT INTO account (account_id, first_seen_ms) VALUES (7, 0)")
        .execute(db.pool())
        .await
        .unwrap();
    let reference = |rest: &str| {
        format!(
            "INSERT INTO name_reference (group_id, episode_id, message_id, ordinal, speaker_account, key, text, who, account_id, basis, grounds, at_ms) VALUES {rest}"
        )
    };
    for (row, why) in [
        (
            "(1, 1, 1, 1, 99, 'x', 'x', 'unknown', NULL, 'about', 'none', 0)",
            "unknown speaker",
        ),
        (
            "(1, 1, 1, 1, 7, 'x', 'x', 'member', 99, 'about', 'content', 0)",
            "unknown account",
        ),
        (
            "(1, 1, 1, 1, 7, 'x', 'x', 'member', NULL, 'about', 'content', 0)",
            "a member is named",
        ),
        (
            "(1, 1, 1, 1, 7, 'x', 'x', 'unknown', 7, 'about', 'none', 0)",
            "and nobody else is",
        ),
        (
            "(1, 1, 1, 1, 7, 'x', 'x', 'unknown', NULL, 'set', 'none', 0)",
            "closed set of uses",
        ),
        (
            "(1, 1, 1, 1, 7, 'x', 'x', 'unknown', NULL, 'about', 'hunch', 0)",
            "closed set of grounds",
        ),
        (
            "(1, 1, 1, 1, 7, 'x', 'x', 'everyone', NULL, 'about', 'none', 0)",
            "closed set of referents",
        ),
        (
            "(1, 1, 1, 1, 7, '', '', 'unknown', NULL, 'about', 'none', 0)",
            "empty expression",
        ),
    ] {
        assert!(rejected(&db, &reference(row)).await, "{why}");
    }
    sqlx::query(&reference(
        "(1, 1, 1, 1, 7, 'x', 'X', 'member', 7, 'own', 'self', 0)",
    ))
    .execute(db.pool())
    .await
    .unwrap();
    assert!(
        rejected(
            &db,
            &reference("(1, 1, 1, 1, 7, 'x', 'x', 'unknown', NULL, 'about', 'none', 0)")
        )
        .await,
        "one reading per episode and line"
    );
    sqlx::query(&reference(
        "(1, 1, 2, 2, 7, 'x', 'x', 'unknown', NULL, 'about', 'none', 0)",
    ))
    .execute(db.pool())
    .await
    .unwrap();
    let set = "INSERT INTO name_set (group_id, account_id, key, text, at_ms) VALUES";
    assert!(
        rejected(&db, &format!("{set} (1, 99, 'x', 'x', 0)")).await,
        "unknown account"
    );
    sqlx::query(&format!("{set} (1, 7, 'x', 'x', 0)"))
        .execute(db.pool())
        .await
        .unwrap();
    assert!(
        rejected(&db, &format!("{set} (1, 7, 'x', 'x', 1)")).await,
        "set once"
    );
    let ruling =
        "INSERT INTO name_ruling (group_id, key, text, verdict, account_id, seen, at_ms) VALUES";
    assert!(
        rejected(
            &db,
            &format!("{ruling} (1, 'x', 'x', 'member', NULL, 3, 0)")
        )
        .await
    );
    assert!(rejected(&db, &format!("{ruling} (1, 'x', 'x', 'outside', 7, 3, 0)")).await);
    assert!(rejected(&db, &format!("{ruling} (1, 'x', 'x', 'maybe', NULL, 3, 0)")).await);
    sqlx::query(&format!("{ruling} (1, 'x', 'x', 'member', 7, 3, 0)"))
        .execute(db.pool())
        .await
        .unwrap();
    assert!(
        rejected(
            &db,
            &format!("{ruling} (1, 'x', 'x', 'general', NULL, 3, 0)")
        )
        .await,
        "one ruling per expression"
    );
    assert!(
        rejected(
            &db,
            "INSERT INTO held_claim (group_id, key, name, text, durability, episode_id, message_id, quote, at_ms) VALUES (1, 'x', 'x', 'keeps bees', 'forever', 1, 1, 'q', 0)"
        )
        .await,
        "closed set of durabilities"
    );

    // memories: a member memory has no term; the state's time and replacement agree with it;
    // two current memories of one subject never share a key; evidence once per episode.
    let memory = |rest: &str| {
        format!(
            "INSERT INTO memory (group_id, subject_account, term, text, key, durability, state, replaced_by, closed_ms, first_ms, last_ms) VALUES {rest}"
        )
    };
    assert!(
        rejected(
            &db,
            &memory("(1, 7, 'GG', 't', 'k', 'lasting', 'current', NULL, NULL, 0, 0)")
        )
        .await,
        "a term is the group's"
    );
    assert!(
        rejected(
            &db,
            &memory("(1, 7, NULL, 't', 'k', 'forever', 'current', NULL, NULL, 0, 0)")
        )
        .await,
        "closed set of durabilities"
    );
    assert!(
        rejected(
            &db,
            &memory("(1, 7, NULL, 't', 'k', 'lasting', 'ended', NULL, NULL, 0, 0)")
        )
        .await,
        "an end has its time"
    );
    assert!(
        rejected(
            &db,
            &memory("(1, 7, NULL, 't', 'k', 'lasting', 'current', NULL, 5, 0, 0)")
        )
        .await,
        "a current memory has none"
    );
    assert!(
        rejected(
            &db,
            &memory("(1, 7, NULL, 't', 'k', 'lasting', 'current', NULL, NULL, 5, 0)")
        )
        .await,
        "first before last"
    );
    assert!(
        rejected(
            &db,
            &memory("(1, 7, NULL, '', 'k', 'lasting', 'current', NULL, NULL, 0, 0)")
        )
        .await,
        "empty text"
    );
    sqlx::query(&memory(
        "(1, 7, NULL, 't', 'k', 'lasting', 'current', NULL, NULL, 0, 0)",
    ))
    .execute(db.pool())
    .await
    .unwrap();
    assert!(
        rejected(
            &db,
            &memory("(1, 7, NULL, 'T', 'k', 'lasting', 'current', NULL, NULL, 0, 0)")
        )
        .await,
        "the same memory twice"
    );
    sqlx::query(&memory(
        "(1, 7, NULL, 'T', 'k', 'lasting', 'ended', NULL, 3, 0, 0)",
    ))
    .execute(db.pool())
    .await
    .unwrap();
    sqlx::query(&memory(
        "(1, NULL, 'GG', 't', 'k', 'ongoing', 'current', NULL, NULL, 0, 0)",
    ))
    .execute(db.pool())
    .await
    .unwrap();
    assert!(rejected(&db, &memory("(1, 7, NULL, 'u', 'u', 'lasting', 'ended', (SELECT min(memory_id) FROM memory), 3, 0, 0)")).await, "only a replaced memory has a replacement");
    sqlx::query("INSERT INTO memory_evidence (memory_id, episode_id, message_id, author_account, quote, at_ms) VALUES ((SELECT min(memory_id) FROM memory), 1, 1, 7, 'q', 0)").execute(db.pool()).await.unwrap();
    assert!(rejected(&db, "INSERT INTO memory_evidence (memory_id, episode_id, message_id, author_account, quote, at_ms) VALUES ((SELECT min(memory_id) FROM memory), 1, 2, 7, 'q', 0)").await, "one support per episode");

    // episodes: ranges are valid and never overlap within a group; embeddings match their width.
    let episode = |first: i64, last: i64, group: i64| {
        format!(
            "INSERT INTO episode (group_id, first_batch, last_batch, first_ordinal, last_ordinal, batch_lines, first_message, last_message, started_ms, ended_ms, line_count, participants, title, summary, evidence, method, model, created_ms) \
         VALUES ({group}, 0, 0, {first}, {last}, 10, 1, 2, 0, 0, 5, '{{}}', 't', 's', '[]', 'm', 'm', 0)"
        )
    };
    assert!(rejected(&db, &episode(10, 5, 1)).await, "reversed range");
    sqlx::query(&episode(1, 30, 1))
        .execute(db.pool())
        .await
        .unwrap();
    assert!(
        rejected(&db, &episode(30, 40, 1)).await,
        "touching the last ordinal overlaps"
    );
    assert!(rejected(&db, &episode(10, 20, 1)).await, "inside overlaps");
    sqlx::query(&episode(31, 60, 1))
        .execute(db.pool())
        .await
        .unwrap();
    sqlx::query(&episode(1, 30, 2))
        .execute(db.pool())
        .await
        .unwrap();
    assert!(rejected(&db, "INSERT INTO episode_embedding (episode_id, model, dims, embedding) VALUES ((SELECT min(episode_id) FROM episode), 'm', 3, '[1,2]')").await, "width must match dims");
    sqlx::query("INSERT INTO episode_embedding (episode_id, model, dims, embedding) VALUES ((SELECT min(episode_id) FROM episode), 'm', 2, '[1,2]')").execute(db.pool()).await.unwrap();
    assert!(rejected(&db, "INSERT INTO episode_embedding (episode_id, model, dims, embedding) VALUES ((SELECT min(episode_id) FROM episode), 'm', 2, '[3,4]')").await, "one vector per episode and model");

    // links: stored once, lower account first, between known accounts.
    sqlx::query("INSERT INTO account (account_id, first_seen_ms) VALUES (8, 0), (9, 0)")
        .execute(db.pool())
        .await
        .unwrap();
    sqlx::query("INSERT INTO account_link (group_id, low_account, high_account, created_ms) VALUES (1, 7, 8, 0)").execute(db.pool()).await.unwrap();
    assert!(rejected(&db, "INSERT INTO account_link (group_id, low_account, high_account, created_ms) VALUES (1, 7, 8, 1)").await, "once");
    assert!(rejected(&db, "INSERT INTO account_link (group_id, low_account, high_account, created_ms) VALUES (1, 8, 7, 0)").await, "lower first");
    assert!(rejected(&db, "INSERT INTO account_link (group_id, low_account, high_account, created_ms) VALUES (1, 7, 7, 0)").await, "no self-link");
    assert!(rejected(&db, "INSERT INTO account_link (group_id, low_account, high_account, created_ms) VALUES (1, 7, 99, 0)").await, "known accounts");

    // invitations: no self-links; one pending per account and group on each side.
    assert!(rejected(&db, "INSERT INTO link_invitation (group_id, initiator, target, created_by, created_ms, initiator_linked, target_linked, state) VALUES (1, 7, 7, 1, 0, '{}', '{}', 'pending')").await);
    sqlx::query("INSERT INTO link_invitation (group_id, initiator, target, created_by, created_ms, initiator_linked, target_linked, state) VALUES (1, 7, 8, 1, 0, '{}', '{}', 'pending')").execute(db.pool()).await.unwrap();
    assert!(rejected(&db, "INSERT INTO link_invitation (group_id, initiator, target, created_by, created_ms, initiator_linked, target_linked, state) VALUES (1, 7, 9, 1, 0, '{}', '{}', 'pending')").await);
    assert!(rejected(&db, "INSERT INTO link_invitation (group_id, initiator, target, created_by, created_ms, initiator_linked, target_linked, state) VALUES (1, 9, 8, 1, 0, '{}', '{}', 'pending')").await);
    assert!(rejected(&db, "INSERT INTO link_invitation (group_id, initiator, target, created_by, created_ms, initiator_linked, target_linked, state, confirmed_by) VALUES (2, 7, 8, 1, 0, '{}', '{}', 'pending', 5)").await, "confirmed only when applied");
    db.drop_db().await;
}

#[tokio::test]
async fn archived_game_results_are_rewritten_to_the_observed_form() {
    let db = db!();
    let lines = [
        ("bot", "[dice:4]"),
        ("bot", "[rps:1]"),
        ("member", "[rps:2]"),
        ("member", "[rps:3] and [dice:6]"),
        ("bot", "[dice]"),
        ("bot", "[dice:9]"),
        ("bot", "\u{ff3b}dice:5\u{ff3d}"),
    ];
    for (i, (speaker, text)) in lines.iter().enumerate() {
        let account = (*speaker == "member").then_some(7_i64);
        sqlx::query("INSERT INTO chat_line (group_id, ordinal, message_id, speaker, account_id, at_ms, text) VALUES (1, $1, $1, $2, $3, 0, $4)")
            .bind(i as i64 + 1)
            .bind(speaker)
            .bind(account)
            .bind(text)
            .execute(db.pool())
            .await
            .unwrap();
    }
    sqlx::raw_sql(include_str!("../migrations/0003_game_result_markers.sql"))
        .execute(db.pool())
        .await
        .unwrap();
    let texts: Vec<String> = sqlx::query_scalar("SELECT text FROM chat_line ORDER BY ordinal")
        .fetch_all(db.pool())
        .await
        .unwrap();
    assert_eq!(
        texts,
        [
            "[dice result:4]",
            "[rps result:paper]",
            "[rps result:scissors]",
            "[rps result:rock] and [dice result:6]",
            "[dice]",
            "[dice:9]",
            "\u{ff3b}dice:5\u{ff3d}",
        ],
        "only real result markers change; a fullwidth look-alike stays text"
    );
    db.drop_db().await;
}

#[tokio::test]
async fn only_content_keyed_picture_descriptions_are_kept() {
    let db = db!();
    let current = format!("0123456789abcdef:{}", "a".repeat(64));
    for key in [
        current.as_str(),
        "p:ABCDEF.jpg",
        &format!("h:{}", "b".repeat(64)),
    ] {
        sqlx::query("INSERT INTO media_cache (key, description, created_ms) VALUES ($1, 'x', 0)")
            .bind(key)
            .execute(db.pool())
            .await
            .unwrap();
    }
    sqlx::raw_sql(include_str!(
        "../migrations/0005_media_cache_by_content.sql"
    ))
    .execute(db.pool())
    .await
    .unwrap();
    let keys: Vec<String> = sqlx::query_scalar("SELECT key FROM media_cache")
        .fetch_all(db.pool())
        .await
        .unwrap();
    assert_eq!(
        keys,
        [current],
        "platform-id and bare-hash keys can no longer be looked up"
    );
    db.drop_db().await;
}

/// Applies the migrations before 0014, in order, to a bare database.
async fn schema_before_0014(db: &common::TestDb) {
    for sql in [
        include_str!("../migrations/0001_schema.sql"),
        include_str!("../migrations/0002_member_number_immutable.sql"),
        include_str!("../migrations/0003_game_result_markers.sql"),
        include_str!("../migrations/0004_slice_skip.sql"),
        include_str!("../migrations/0005_media_cache_by_content.sql"),
        include_str!("../migrations/0006_spontaneous_runs.sql"),
        include_str!("../migrations/0007_provider_usage.sql"),
        include_str!("../migrations/0008_archive_identity.sql"),
        include_str!("../migrations/0009_group_links.sql"),
        include_str!("../migrations/0010_group_admins.sql"),
        include_str!("../migrations/0011_repeating_tasks.sql"),
        include_str!("../migrations/0012_competing_claims.sql"),
        include_str!("../migrations/0013_topics_side_by_side.sql"),
    ] {
        sqlx::raw_sql(sql).execute(db.pool()).await.unwrap();
    }
}

#[tokio::test]
async fn the_active_memory_migration_keeps_set_names_removals_and_facts_with_their_history() {
    let db = db!(bare);
    schema_before_0014(&db).await;
    let pool = db.pool().clone();
    let run = |sql: &'static str| {
        let pool = pool.clone();
        async move {
            sqlx::raw_sql(sql).execute(&pool).await.unwrap();
        }
    };
    run("INSERT INTO account (account_id, first_seen_ms) VALUES (7, 0), (8, 0);
         INSERT INTO chat_line (group_id, ordinal, message_id, speaker, account_id, at_ms, text)
         VALUES (1, 1, 501, 'member', 7, 100, 'I moved to Shanghai'),
                (1, 2, 502, 'member', 8, 110, 'GG means good game');
         INSERT INTO episode (group_id, first_batch, last_batch, first_ordinal, last_ordinal, batch_lines,
             first_message, last_message, started_ms, ended_ms, line_count, participants, title, summary,
             evidence, method, model, created_ms, findings)
         VALUES (1, 0, 0, 1, 30, 30, 501, 502, 100, 120, 2, '{7,8}', 't', 's', '[]', 'slice-v5', 'm', 130,
             '{\"names\": [{\"account\": 7, \"name\": \"Kit\", \"message\": 501, \"quote\": \"q\"}]}');
         INSERT INTO alias (group_id, text, account_id, status, confidence, removed, last_evidence_ms)
         VALUES (1, 'kit', 7, 'confirmed', 1, false, 50),
                (1, 'boss', 8, 'inactive', 0.67, true, 60),
                (1, 'lead', 8, 'candidate', 0.67, false, 70);
         INSERT INTO alias_evidence (alias_id, kind, support, at_ms)
         SELECT alias_id, 'manual', NULL, 40 FROM alias WHERE text = 'kit';
         INSERT INTO alias_evidence (alias_id, kind, support, at_ms)
         SELECT alias_id, 'extracted', 1, 70 FROM alias WHERE text = 'lead';
         INSERT INTO fact (group_id, subject_account, predicate, key, object, label, status, supports,
             decay, first_seen_ms, last_confirmed_ms, ended_ms, slot)
         VALUES (1, 7, 'lives_in', 'beijing', 'Beijing', NULL, 'active', 3, 'stable', 10, 50, NULL, 'lives_in'),
                (1, 7, 'lives_in', 'shanghai', 'Shanghai', NULL, 'active', 1, 'stable', 100, 100, NULL, 'lives_in'),
                (1, 7, 'plays', 'genshin', 'Genshin', NULL, 'active', 1, 'fast', 20, 20, 90, 'plays:genshin'),
                (1, 7, 'likes', 'cats', 'cats', NULL, 'forgotten', 1, 'default', 20, 20, NULL, 'dislikes/likes:cats'),
                (1, NULL, 'group_term', 'good game', 'good game', 'GG', 'active', 1, 'default', 110, 110, NULL, 'group_term:gg'),
                (1, NULL, 'group_topic', 'games', 'games', NULL, 'active', 2, 'stable', 5, 110, NULL, 'group_topic:games');
         INSERT INTO fact_evidence (fact_id, episode_id, message_id, quote)
         SELECT fact_id, (SELECT min(episode_id) FROM episode), 501, 'I moved to Shanghai'
         FROM fact WHERE object = 'Shanghai';").await;
    run(include_str!("../migrations/0014_active_memory.sql")).await;

    let names: Vec<(i64, String, String)> =
        sqlx::query_as("SELECT account_id, key, basis FROM name_use ORDER BY account_id, key")
            .fetch_all(db.pool())
            .await
            .unwrap();
    assert_eq!(
        names,
        [(7, "kit".to_owned(), "set".to_owned())],
        "set names stay; names learned from chat are dropped"
    );
    let removed: Vec<(i64, String)> = sqlx::query_as("SELECT account_id, key FROM name_removed")
        .fetch_all(db.pool())
        .await
        .unwrap();
    assert_eq!(
        removed,
        [(8, "boss".to_owned())],
        "a removed name stays removed"
    );

    type Row = (Option<i64>, Option<String>, String, String, String);
    let memories: Vec<Row> = sqlx::query_as(
        "SELECT subject_account, term, text, durability, state FROM memory ORDER BY memory_id",
    )
    .fetch_all(db.pool())
    .await
    .unwrap();
    let row =
        |subject: Option<i64>, term: Option<&str>, text: &str, durability: &str, state: &str| {
            (
                subject,
                term.map(str::to_owned),
                text.to_owned(),
                durability.to_owned(),
                state.to_owned(),
            )
        };
    assert_eq!(
        memories,
        [
            row(Some(7), None, "lives in: Beijing", "lasting", "replaced"),
            row(Some(7), None, "lives in: Shanghai", "lasting", "current"),
            row(Some(7), None, "plays: Genshin", "current", "ended"),
            row(None, Some("GG"), "good game", "ongoing", "current"),
            row(None, None, "games", "lasting", "current"),
        ],
        "the older of two competing values is replaced by the newer; a forgotten fact is gone"
    );
    let (replaced_by, author): (Option<i64>, Option<i64>) = sqlx::query_as(
        "SELECT (SELECT replaced_by FROM memory WHERE text = 'lives in: Beijing'),
                (SELECT e.author_account FROM memory_evidence e JOIN memory m USING (memory_id)
                 WHERE m.text = 'lives in: Shanghai')",
    )
    .fetch_one(db.pool())
    .await
    .unwrap();
    let shanghai: i64 =
        sqlx::query_scalar("SELECT memory_id FROM memory WHERE text = 'lives in: Shanghai'")
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(replaced_by, Some(shanghai));
    assert_eq!(author, Some(7), "evidence keeps who wrote the line");

    let (findings, legacy, applied): (serde_json::Value, Option<serde_json::Value>, Option<i64>) =
        sqlx::query_as("SELECT findings, legacy_findings, consolidated_ms FROM episode")
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(findings, serde_json::json!({}));
    assert!(
        legacy.is_some_and(|l| l["names"][0]["name"] == "Kit"),
        "kept as a record"
    );
    assert_eq!(applied, Some(130), "and counted as applied");
    for gone in ["alias", "alias_evidence", "fact", "fact_evidence"] {
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM information_schema.tables WHERE table_name = $1)",
        )
        .bind(gone)
        .fetch_one(db.pool())
        .await
        .unwrap();
        assert!(!exists, "{gone} is dropped");
    }
    db.drop_db().await;
}

#[tokio::test]
async fn the_name_ledger_migration_carries_set_names_and_learned_uses_with_their_lines() {
    let db = db!(bare);
    schema_before_0014(&db).await;
    for sql in [
        include_str!("../migrations/0014_active_memory.sql"),
        include_str!("../migrations/0015_name_anchor.sql"),
        "INSERT INTO account (account_id, first_seen_ms) VALUES (7, 0), (8, 0);
         INSERT INTO chat_line (group_id, ordinal, message_id, speaker, account_id, at_ms, text)
         VALUES (1, 1, 501, 'member', 7, 10, 'call me Kit'),
                (1, 2, 502, 'member', 8, 20, '[at:1] Kit hello'),
                (1, 3, 503, 'member', 8, 30, 'Kit is late'),
                (1, 4, 504, 'member', 8, 40, 'Kitty again'),
                (1, 5, 505, 'member', 8, 50, 'I am Kit too');
         INSERT INTO name_use (group_id, account_id, key, text, basis, episode_id, message_id,
                               quote, at_ms, anchored)
         VALUES (1, 7, 'boss', 'Boss', 'set', NULL, NULL, NULL, 5, false),
                (1, 7, 'kit', 'Kit', 'own', 1, 501, 'call me Kit', 10, false),
                (1, 7, 'kit', 'Kit', 'addressed', 2, 502, 'Kit hello', 20, false),
                (1, 7, 'kit', 'Kit', 'about', 3, 503, 'Kit is late', 30, true),
                (1, 7, 'kitty', 'Kitty', 'about', 4, 504, 'Kitty again', 40, false),
                (1, 7, 'kit', 'Kit', 'own', 5, 505, 'I am Kit too', 50, false),
                (1, 7, 'gone', 'Gone', 'about', 6, 999, 'no such line', 60, false);",
        include_str!("../migrations/0016_name_ledger.sql"),
    ] {
        sqlx::raw_sql(sql).execute(db.pool()).await.unwrap();
    }
    let set: Vec<(i64, String, String)> =
        sqlx::query_as("SELECT account_id, key, text FROM name_set")
            .fetch_all(db.pool())
            .await
            .unwrap();
    assert_eq!(set, [(7, "boss".to_owned(), "Boss".to_owned())]);
    type Row = (i64, i64, i64, String, String, i64, String, String, bool);
    let carried: Vec<Row> = sqlx::query_as(
        "SELECT message_id, ordinal, speaker_account, key, who, account_id, basis, grounds, \
         anchored FROM name_reference ORDER BY message_id",
    )
    .fetch_all(db.pool())
    .await
    .unwrap();
    let row = |message, ordinal, speaker, key: &str, basis: &str, grounds: &str, anchored| {
        (
            message,
            ordinal,
            speaker,
            key.to_owned(),
            "member".to_owned(),
            7,
            basis.to_owned(),
            grounds.to_owned(),
            anchored,
        )
    };
    assert_eq!(
        carried,
        [
            row(501, 1, 7, "kit", "own", "self", false),
            row(502, 2, 8, "kit", "addressed", "marker", false),
            row(503, 3, 8, "kit", "about", "answer", true),
            row(504, 4, 8, "kitty", "about", "content", false),
        ],
        "each use with its line and who wrote it; a use whose line is gone, or whose line the \
         member did not write as the use says, is not carried"
    );
    let gone: Option<String> = sqlx::query_scalar("SELECT to_regclass('name_use')::text")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(gone, None);
    db.drop_db().await;
}
