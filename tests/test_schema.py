"""Canonical schema and read-only compatibility checks."""

from __future__ import annotations

import pytest
import pathlib
import uuid

ROOT = pathlib.Path(__file__).resolve().parent.parent


from _db import pool
from qqbot.db.repo import check_schema
from qqbot.domain.memory.embedding import VECTOR_DIMENSIONS


@pytest.mark.database
@pytest.mark.asyncio
async def test_schema(test_database):
    sql = (ROOT / "sql" / "init.sql").read_text(encoding="utf-8")
    assert "schema_migration" not in sql, "the schema contains no migration ledger"
    assert "episode_participant" not in sql, "the schema contains no episode participants"
    assert "user_agreement" not in sql, "the schema contains no agreement table"
    assert "CREATE TABLE scheduled_task" in sql, (
        "the schema contains a durable scheduled task table"
    )
    assert "qbot_schema_export" not in sql, "dump comments name the public schema"

    schema = f"schema_test_{uuid.uuid4().hex}"
    dimensions = VECTOR_DIMENSIONS
    async with pool().acquire() as conn, conn.transaction():
        await conn.execute(f'CREATE SCHEMA "{schema}"')
        await conn.execute(f'SET LOCAL search_path TO "{schema}", public')
        await conn.execute(sql)
        await check_schema(conn, schema=schema, embedding_dimensions=dimensions)

        await conn.execute("DROP INDEX scheduled_task_due")
        try:
            await check_schema(conn, schema=schema, embedding_dimensions=dimensions)
            rejected = False
        except RuntimeError:
            rejected = True
        assert rejected, "a missing scheduled task claim index is rejected"
        await conn.execute(
            "CREATE UNIQUE INDEX scheduled_task_due ON scheduled_task (due_at, id) "
            "WHERE status='pending'"
        )
        await conn.execute("ALTER TABLE scheduled_task DROP CONSTRAINT scheduled_task_intent_valid")
        try:
            await check_schema(conn, schema=schema, embedding_dimensions=dimensions)
            rejected = False
        except RuntimeError:
            rejected = True
        assert rejected, "a missing scheduled task intent bound is rejected"
        await conn.execute(
            "ALTER TABLE scheduled_task ADD CONSTRAINT scheduled_task_intent_valid "
            "CHECK (char_length(intent) BETWEEN 1 AND 500)"
        )

        await conn.execute(f'CREATE TABLE "{schema}".user_agreement (id bigint)')
        try:
            await check_schema(conn, schema=schema, embedding_dimensions=dimensions)
            rejected = False
        except RuntimeError:
            rejected = True
        assert rejected, "a retired agreement table is rejected"
        await conn.execute(f'DROP TABLE "{schema}".user_agreement')

        for table, name, values in (
            (
                "account_link_challenge",
                "account_link_status_valid",
                "'pending', 'applied', 'cancelled', 'expired'",
            ),
            (
                "memory_extraction",
                "memory_extraction_status_valid",
                "'extracting', 'staged', 'applied', 'failed'",
            ),
        ):
            await conn.execute(f'ALTER TABLE "{schema}"."{table}" DROP CONSTRAINT "{name}"')
            await conn.execute(
                f'ALTER TABLE "{schema}"."{table}" ADD CONSTRAINT "{name}" '
                f"CHECK (status IN ({values}))"
            )
        definitions = await conn.fetch(
            """SELECT pg_get_constraintdef(c.oid, true) AS definition
                 FROM pg_constraint c
                 JOIN pg_namespace n ON n.oid=c.connamespace
                WHERE n.nspname=$1
                  AND c.conname IN ('account_link_status_valid',
                                    'memory_extraction_status_valid')""",
            schema,
        )
        assert len(definitions) == 2 and all(
            "::text[]" in row["definition"] for row in definitions
        ), "manual status checks cast the entire allowed-value array"
        await check_schema(conn, schema=schema, embedding_dimensions=dimensions)

        await conn.execute("DROP INDEX raw_event_platform_key")
        for statement, description in (
            (
                "CREATE INDEX raw_event_platform_key ON raw_event (group_id)",
                "non-unique index with the expected name",
            ),
            (
                "CREATE UNIQUE INDEX raw_event_platform_key "
                "ON raw_event (group_id, platform_event_id) "
                "WHERE platform_event_id IS NOT NULL",
                "wrong conflict key columns",
            ),
            (
                "CREATE UNIQUE INDEX raw_event_platform_key "
                "ON raw_event (platform, platform_event_id)",
                "missing conflict predicate",
            ),
        ):
            await conn.execute(statement)
            try:
                await check_schema(conn, schema=schema, embedding_dimensions=dimensions)
                rejected = False
            except RuntimeError:
                rejected = True
            assert rejected, f"schema rejects {description}"
            await conn.execute("DROP INDEX raw_event_platform_key")
        await conn.execute(
            """CREATE UNIQUE INDEX raw_event_platform_key
                 ON raw_event (platform, platform_event_id)
              WHERE platform_event_id IS NOT NULL"""
        )

        await conn.execute("DROP INDEX alias_unique_account_scope")
        try:
            await check_schema(conn, schema=schema, embedding_dimensions=dimensions)
            rejected = False
        except RuntimeError:
            rejected = True
        assert rejected, "missing invariant indexes are rejected"
        await conn.execute(
            """CREATE UNIQUE INDEX alias_unique_account_scope
                 ON alias (COALESCE(group_id, 0::bigint), normalized_text, target_account_id)
              WHERE target_account_id IS NOT NULL"""
        )

        await conn.execute(f'ALTER TABLE "{schema}".alias DROP CONSTRAINT alias_exactly_one_target')
        try:
            await check_schema(conn, schema=schema, embedding_dimensions=dimensions)
            rejected = False
        except RuntimeError:
            rejected = True
        assert rejected, "missing invariant constraints are rejected"
        await conn.execute(
            f'''ALTER TABLE "{schema}".alias
                ADD CONSTRAINT alias_exactly_one_target
                CHECK (num_nonnulls(target_entity_id, target_account_id) = 1)'''
        )

        await conn.execute(f'ALTER TABLE "{schema}".alias DROP CONSTRAINT alias_exactly_one_target')
        await conn.execute(
            f'''ALTER TABLE "{schema}".alias
                ADD CONSTRAINT alias_exactly_one_target
                CHECK (num_nonnulls(target_entity_id, target_account_id) >= 0)'''
        )
        try:
            await check_schema(conn, schema=schema, embedding_dimensions=dimensions)
            rejected = False
        except RuntimeError:
            rejected = True
        assert rejected, "same-name constraint with the wrong definition is rejected"
        await conn.execute(f'ALTER TABLE "{schema}".alias DROP CONSTRAINT alias_exactly_one_target')
        await conn.execute(
            f'''ALTER TABLE "{schema}".alias
                ADD CONSTRAINT alias_exactly_one_target
                CHECK (num_nonnulls(target_entity_id, target_account_id) = 1)'''
        )

        await conn.execute(f'ALTER TABLE "{schema}".reply_trace ADD COLUMN content text')
        try:
            await check_schema(conn, schema=schema, embedding_dimensions=dimensions)
            rejected = False
        except RuntimeError:
            rejected = True
        assert rejected, "retired compatibility columns are rejected"
        await conn.execute(f'DROP SCHEMA "{schema}" CASCADE')
