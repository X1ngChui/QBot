"""SQL for the ops and infrastructure tables: switches, ledger, blocklist, traces,
schema checks - the state that belongs to running the bot rather than to what it
remembers. The domain aggregates (identity, memory, episodes, jobs, the archive's
read side) live in `qqbot.repositories`; a handful of hot-path readers
(tools.search_history) issue their own SQL where the query is the logic.
"""

from __future__ import annotations

import re

from qqbot.domain.memory.embedding import VECTOR_DIMENSIONS
from collections.abc import Callable
import asyncpg

# -- raw events -------------------------------------------------------------


_REQUIRED_SCHEMA_TABLES = {
    "account_link_challenge",
    "alias",
    "alias_evidence",
    "cost_ledger",
    "embedding_index",
    "entity",
    "episode",
    "episode_event",
    "group_blocklist",
    "group_state",
    "identity_account",
    "image_cache",
    "memory_candidate",
    "memory_extraction",
    "memory_extraction_event",
    "memory_fact",
    "memory_fact_evidence",
    "memory_job",
    "raw_event",
    "reply_trace",
    "scheduled_task",
}
_REQUIRED_SCHEMA_COLUMNS = {
    "account_link_challenge": {
        "initiator_account_id",
        "target_account_id",
        "initiator_entity_revision",
        "target_entity_revision",
        "token_hash",
    },
    "alias": {"target_entity_id", "target_account_id"},
    "episode": {"extraction_id", "summary"},
    "group_blocklist": {"id", "user_id", "entity_id", "blocked_until"},
    "identity_account": {"id", "entity_id", "platform", "platform_user_id"},
    "memory_extraction": {"status", "snapshot", "model_attempts"},
    "memory_job": {"claim_token", "lease_until", "locked_at", "locked_by"},
    "memory_fact": {"subject_entity_id", "subject_account_id"},
    "raw_event": {"archive_schema", "payload", "plain_text"},
    "scheduled_task": {
        "id",
        "group_id",
        "creator_id",
        "intent",
        "due_at",
        "status",
        "chain_id",
        "chain_depth",
        "started_at",
        "finished_at",
        "outcome",
    },
}
_REQUIRED_NOT_NULL_COLUMNS = {
    "memory_extraction": {"model_attempts"},
    "account_link_challenge": {
        "group_id",
        "token_hash",
        "initiator_account_id",
        "target_account_id",
        "initiator_entity_id",
        "target_entity_id",
        "initiator_entity_revision",
        "target_entity_revision",
        "status",
        "created_event_id",
        "created_at",
        "expires_at",
    },
    "reply_trace": {"memo", "expires_at"},
    "scheduled_task": {
        "id",
        "group_id",
        "creator_id",
        "intent",
        "due_at",
        "created_at",
        "status",
        "chain_id",
        "chain_depth",
    },
}
_SCHEMA_CONSTRAINTS = {
    "memory_extraction_attempts_valid": ("memory_extraction", "c"),
    "memory_job_status_valid": ("memory_job", "c"),
    "memory_job_claim_valid": ("memory_job", "c"),
    "memory_job_retries_valid": ("memory_job", "c"),
    "account_link_distinct_accounts": ("account_link_challenge", "c"),
    "account_link_revision_valid": ("account_link_challenge", "c"),
    "account_link_status_valid": ("account_link_challenge", "c"),
    "alias_exactly_one_target": ("alias", "c"),
    "fact_exactly_one_subject": ("memory_fact", "c"),
    "group_blocklist_exactly_one_target": ("group_blocklist", "c"),
    "identity_account_platform_platform_user_id_key": ("identity_account", "u"),
    "memory_extraction_event_extraction_id_ordinal_key": ("memory_extraction_event", "u"),
    "memory_extraction_event_raw_event_id_key": ("memory_extraction_event", "u"),
    "memory_extraction_live_snapshot_v2": ("memory_extraction", "c"),
    "memory_extraction_status_valid": ("memory_extraction", "c"),
    "raw_event_archive_schema_valid": ("raw_event", "c"),
    "scheduled_task_status_valid": ("scheduled_task", "c"),
    "scheduled_task_depth_valid": ("scheduled_task", "c"),
    "scheduled_task_intent_valid": ("scheduled_task", "c"),
    "scheduled_task_pkey": ("scheduled_task", "p"),
}
_SCHEMA_CHECK_DEFINITIONS = {
    "memory_extraction_attempts_valid": "CHECK (model_attempts >= 0)",
    "memory_job_status_valid": "CHECK (status = ANY (ARRAY['pending', 'running', 'done', 'dead']))",
    "memory_job_claim_valid": (
        "CHECK (status='running' AND "
        "num_nonnulls(claim_token, lease_until, locked_at, locked_by)=4 "
        "OR status<>'running' AND num_nonnulls(claim_token, lease_until, locked_at, locked_by)=0)"
    ),
    "memory_job_retries_valid": "CHECK (retry_count >= 0 AND max_retry >= 0)",
    "account_link_distinct_accounts": "CHECK (initiator_account_id <> target_account_id)",
    "account_link_revision_valid": (
        "CHECK (initiator_entity_revision >= 1 AND target_entity_revision >= 1)"
    ),
    "account_link_status_valid": (
        "CHECK (status = ANY (ARRAY['pending', 'applied', 'cancelled', 'expired']))"
    ),
    "alias_exactly_one_target": ("CHECK (num_nonnulls(target_entity_id, target_account_id) = 1)"),
    "fact_exactly_one_subject": ("CHECK (num_nonnulls(subject_entity_id, subject_account_id) = 1)"),
    "group_blocklist_exactly_one_target": "CHECK (num_nonnulls(user_id, entity_id) = 1)",
    "identity_account_platform_platform_user_id_key": "UNIQUE (platform, platform_user_id)",
    "memory_extraction_event_extraction_id_ordinal_key": "UNIQUE (extraction_id, ordinal)",
    "memory_extraction_event_raw_event_id_key": "UNIQUE (raw_event_id)",
    "memory_extraction_live_snapshot_v2": (
        "CHECK (status = 'applied' OR status = 'failed' "
        "OR status = 'extracting' AND snapshot IS NULL "
        "OR status = 'staged' AND snapshot IS NOT NULL "
        "AND snapshot @> '{\"version\": 2}'::jsonb)"
    ),
    "memory_extraction_status_valid": (
        "CHECK (status = ANY (ARRAY['extracting', 'staged', 'applied', 'failed']))"
    ),
    "raw_event_archive_schema_valid": "CHECK (archive_schema >= 1)",
    "scheduled_task_status_valid": (
        "CHECK (status = ANY (ARRAY['pending', 'running', 'done', 'failed', 'cancelled']))"
    ),
    "scheduled_task_depth_valid": "CHECK (chain_depth >= 0)",
    "scheduled_task_intent_valid": (
        "CHECK (char_length(intent) >= 1 AND char_length(intent) <= 500)"
    ),
    "scheduled_task_pkey": "PRIMARY KEY (id)",
}
_SCHEMA_INDEXES = {
    "job_expired": ("memory_job", ("lease_until", "id"), "status='running'"),
    "account_link_pending_pair": (
        "account_link_challenge",
        (
            "group_id",
            "LEAST(initiator_account_id, target_account_id)",
            "GREATEST(initiator_account_id, target_account_id)",
        ),
        "status='pending'",
    ),
    "alias_unique_account_scope": (
        "alias",
        ("COALESCE(group_id, 0)", "normalized_text", "target_account_id"),
        "target_account_id IS NOT NULL",
    ),
    "alias_unique_entity_scope": (
        "alias",
        ("COALESCE(group_id, 0)", "normalized_text", "target_entity_id"),
        "target_entity_id IS NOT NULL",
    ),
    "fact_one_current_account": (
        "memory_fact",
        ("COALESCE(group_id, 0)", "subject_account_id", "predicate", "COALESCE(object_key, '')"),
        "subject_account_id IS NOT NULL AND valid_to IS NULL AND status='active'",
    ),
    "fact_one_current_entity": (
        "memory_fact",
        ("COALESCE(group_id, 0)", "subject_entity_id", "predicate", "COALESCE(object_key, '')"),
        "subject_entity_id IS NOT NULL AND valid_to IS NULL AND status='active'",
    ),
    "group_blocklist_account": (
        "group_blocklist",
        ("group_id", "user_id"),
        "user_id IS NOT NULL",
    ),
    "group_blocklist_holder": (
        "group_blocklist",
        ("group_id", "entity_id"),
        "entity_id IS NOT NULL",
    ),
    "raw_event_platform_key": (
        "raw_event",
        ("platform", "platform_event_id"),
        "platform_event_id IS NOT NULL",
    ),
    "reply_trace_reply": ("reply_trace", ("group_id", "reply_event_id"), ""),
    "scheduled_task_due": ("scheduled_task", ("due_at", "id"), "status='pending'"),
}
_RETIRED_SCHEMA_TABLES = {"episode_participant", "schema_migration", "user_agreement"}
_RETIRED_SCHEMA_COLUMNS = {"reply_trace": {"content"}}


def _schema_expression(value: str | None) -> str:
    """Compare catalog-deparsed expressions independent of harmless casts and spacing."""

    without_casts = re.sub(
        r"::(?:bigint|text(?:\[\])?|character varying)", "", value or "", flags=re.I
    )
    return re.sub(r"\s+", "", without_casts).lower()


async def check_schema(conn, *, schema: str, embedding_dimensions: int) -> None:
    """Validate the current schema contract without executing DDL."""

    rows = await conn.fetch(
        """SELECT table_name, column_name, is_nullable
             FROM information_schema.columns
            WHERE table_schema=$1""",
        schema,
    )
    columns: dict[str, set[str]] = {}
    not_null: dict[str, set[str]] = {}
    for row in rows:
        table = row["table_name"]
        column = row["column_name"]
        columns.setdefault(table, set()).add(column)
        if row["is_nullable"] == "NO":
            not_null.setdefault(table, set()).add(column)

    missing = sorted(_REQUIRED_SCHEMA_TABLES - columns.keys())
    missing.extend(
        f"{table}.{column}"
        for table, expected in _REQUIRED_SCHEMA_COLUMNS.items()
        for column in sorted(expected - columns.get(table, set()))
    )
    missing.extend(
        f"{table}.{column} NOT NULL"
        for table, expected in _REQUIRED_NOT_NULL_COLUMNS.items()
        for column in sorted(expected - not_null.get(table, set()))
    )
    constraint_rows = await conn.fetch(
        """SELECT c.conname, t.relname AS table_name, c.contype, c.convalidated,
                  pg_get_constraintdef(c.oid, true) AS definition
             FROM pg_constraint c
             JOIN pg_class t ON t.oid=c.conrelid
             JOIN pg_namespace n ON n.oid=t.relnamespace
            WHERE n.nspname=$1""",
        schema,
    )
    constraints = {row["conname"]: row for row in constraint_rows}
    for name, (table, kind) in _SCHEMA_CONSTRAINTS.items():
        row = constraints.get(name)
        if (
            row is None
            or row["table_name"] != table
            or row["contype"] != kind.encode()
            or not row["convalidated"]
            or _schema_expression(row["definition"])
            != _schema_expression(_SCHEMA_CHECK_DEFINITIONS[name])
        ):
            missing.append(f"{name} definition")
    index_rows = await conn.fetch(
        """SELECT idx.relname AS index_name, t.relname AS table_name,
                  i.indisunique, i.indisvalid, i.indisready, i.indislive, am.amname,
                  ARRAY(SELECT pg_get_indexdef(i.indexrelid, pos, true)
                          FROM generate_series(1, i.indnkeyatts) AS pos ORDER BY pos) AS keys,
                  pg_get_expr(i.indpred, i.indrelid, true) AS predicate
             FROM pg_index i
             JOIN pg_class idx ON idx.oid=i.indexrelid
             JOIN pg_class t ON t.oid=i.indrelid
             JOIN pg_am am ON am.oid=idx.relam
             JOIN pg_namespace n ON n.oid=idx.relnamespace
            WHERE n.nspname=$1""",
        schema,
    )
    indexes = {row["index_name"]: row for row in index_rows}
    for name, (table, keys, predicate) in _SCHEMA_INDEXES.items():
        row = indexes.get(name)
        if (
            row is None
            or row["table_name"] != table
            or row["amname"] != "btree"
            or not row["indisunique"]
            or not row["indisvalid"]
            or not row["indisready"]
            or not row["indislive"]
            or tuple(map(_schema_expression, row["keys"])) != tuple(map(_schema_expression, keys))
            or _schema_expression(row["predicate"]) != _schema_expression(predicate)
        ):
            missing.append(f"{name} definition")
    retired = sorted(_RETIRED_SCHEMA_TABLES & columns.keys())
    retired_columns = [
        f"{table}.{column}"
        for table, forbidden in _RETIRED_SCHEMA_COLUMNS.items()
        for column in sorted(forbidden & columns.get(table, set()))
    ]
    width = await conn.fetchval(
        """SELECT a.atttypmod FROM pg_attribute a
             JOIN pg_class c ON c.oid=a.attrelid
             JOIN pg_namespace n ON n.oid=c.relnamespace
            WHERE n.nspname=$1 AND c.relname='embedding_index'
              AND a.attname='embedding' AND NOT a.attisdropped""",
        schema,
    )

    details = []
    if missing:
        details.append("missing " + ", ".join(missing))
    if retired:
        details.append("retired tables present: " + ", ".join(retired))
    if retired_columns:
        details.append("retired columns present: " + ", ".join(retired_columns))
    if width != embedding_dimensions:
        details.append(
            f"embedding_index.embedding is VECTOR({width}) but configuration "
            f"requires VECTOR({embedding_dimensions})"
        )
    if details:
        raise RuntimeError(
            "database schema is incompatible; apply the manual schema update first ("
            + "; ".join(details)
            + ")"
        )


async def ensure_schema(database: Callable[[], asyncpg.Pool]) -> None:
    """Refuse startup unless the manually managed schema matches this runtime."""

    dimensions = VECTOR_DIMENSIONS
    async with database().acquire() as conn:
        await check_schema(conn, schema="public", embedding_dimensions=dimensions)


# -- group_state (per-group switches) ----------------------------------------
# One typed column per field, so the schema itself says what runtime state the system
# keeps - no magic strings inside a jsonb blob.


# -- reply evidence ----------------------------------------------------------
