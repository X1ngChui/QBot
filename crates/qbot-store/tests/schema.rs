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
            "alias",
            "alias_evidence",
            "chat_line",
            "episode",
            "episode_embedding",
            "fact",
            "fact_evidence",
            "group_block",
            "group_state",
            "holder",
            "link_invitation",
            "media_cache",
            "media_ref",
            "member_number",
            "note",
            "recurrence",
            "run",
            "run_item",
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

    // Ordinals are unique per group and start at 1.
    assert!(rejected(&db, "INSERT INTO chat_line (group_id, ordinal, message_id, speaker, at_ms, text) VALUES (1, 1, 5, 'bot', 0, 'same ordinal')").await);
    assert!(rejected(&db, "INSERT INTO chat_line (group_id, ordinal, message_id, speaker, at_ms, text) VALUES (1, 0, 6, 'bot', 0, 'zero')").await);

    // identity: an alias points at exactly one target, evidence needs a support only when extracted.
    sqlx::query("INSERT INTO holder (created_ms) VALUES (0)")
        .execute(db.pool())
        .await
        .unwrap();
    sqlx::query("INSERT INTO account (account_id, holder_id, first_seen_ms) VALUES (7, 1, 0)")
        .execute(db.pool())
        .await
        .unwrap();
    assert!(rejected(&db, "INSERT INTO alias (group_id, text, target_kind, status, confidence, last_evidence_ms) VALUES (1, 'x', 'account', 'candidate', 0, 0)").await, "no target");
    assert!(rejected(&db, "INSERT INTO alias (group_id, text, target_kind, target_account, target_holder, status, confidence, last_evidence_ms) VALUES (1, 'x', 'account', 7, 1, 'candidate', 0, 0)").await, "two targets");
    assert!(rejected(&db, "INSERT INTO alias (group_id, text, target_kind, target_account, status, confidence, last_evidence_ms) VALUES (1, 'x', 'account', 7, 'candidate', 1.5, 0)").await, "confidence is a probability");
    assert!(rejected(&db, "INSERT INTO alias (group_id, text, target_kind, target_account, status, confidence, last_evidence_ms) VALUES (1, '', 'account', 7, 'candidate', 0, 0)").await, "empty name");
    sqlx::query("INSERT INTO alias (group_id, text, target_kind, target_account, status, confidence, last_evidence_ms) VALUES (1, 'x', 'account', 7, 'candidate', 0, 0)").execute(db.pool()).await.unwrap();
    assert!(rejected(&db, "INSERT INTO alias (group_id, text, target_kind, target_account, status, confidence, last_evidence_ms) VALUES (1, 'x', 'account', 7, 'candidate', 0, 0)").await, "one alias per name and target");
    assert!(rejected(&db, "INSERT INTO alias_evidence (alias_id, kind, support, at_ms) VALUES ((SELECT min(alias_id) FROM alias), 'extracted', NULL, 0)").await);
    assert!(rejected(&db, "INSERT INTO alias_evidence (alias_id, kind, support, at_ms) VALUES ((SELECT min(alias_id) FROM alias), 'manual', 5, 0)").await);
    sqlx::query("INSERT INTO alias_evidence (alias_id, kind, support, at_ms) VALUES ((SELECT min(alias_id) FROM alias), 'manual', NULL, 0)").execute(db.pool()).await.unwrap();
    assert!(rejected(&db, "INSERT INTO alias_evidence (alias_id, kind, support, at_ms) VALUES ((SELECT min(alias_id) FROM alias), 'manual', NULL, 9)").await, "the same evidence is stored once");
    assert!(rejected(&db, "INSERT INTO alias_evidence (alias_id, kind, support, at_ms) VALUES ((SELECT min(alias_id) FROM alias), 'display', NULL, 0)").await, "the current group name is never stored as evidence");

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

    // invitations: no self-links; one pending per account and group on each side.
    sqlx::query(
        "INSERT INTO account (account_id, holder_id, first_seen_ms) VALUES (8, 1, 0), (9, 1, 0)",
    )
    .execute(db.pool())
    .await
    .unwrap();
    assert!(rejected(&db, "INSERT INTO link_invitation (group_id, initiator, target, created_by, created_ms, initiator_revision, target_revision, state) VALUES (1, 7, 7, 1, 0, 0, 0, 'pending')").await);
    sqlx::query("INSERT INTO link_invitation (group_id, initiator, target, created_by, created_ms, initiator_revision, target_revision, state) VALUES (1, 7, 8, 1, 0, 0, 0, 'pending')").execute(db.pool()).await.unwrap();
    assert!(rejected(&db, "INSERT INTO link_invitation (group_id, initiator, target, created_by, created_ms, initiator_revision, target_revision, state) VALUES (1, 7, 9, 1, 0, 0, 0, 'pending')").await);
    assert!(rejected(&db, "INSERT INTO link_invitation (group_id, initiator, target, created_by, created_ms, initiator_revision, target_revision, state) VALUES (1, 9, 8, 1, 0, 0, 0, 'pending')").await);
    assert!(rejected(&db, "INSERT INTO link_invitation (group_id, initiator, target, created_by, created_ms, initiator_revision, target_revision, state, confirmed_by) VALUES (2, 7, 8, 1, 0, 0, 0, 'pending', 5)").await, "confirmed only when applied");
    db.drop_db().await;
}
