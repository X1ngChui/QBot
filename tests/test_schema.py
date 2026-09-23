"""Canonical schema and read-only compatibility checks."""

from __future__ import annotations

import asyncio
import os
import pathlib
import sys
import uuid

ROOT = pathlib.Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT))
os.environ.setdefault("CONFIG_DIR", str(ROOT / "tests" / "fixtures" / "config"))

from _db import assert_disposable_database, configure_test_database

configure_test_database()

from qqbot.db import close_pool, init_pool, pool
from qqbot.db.repo import check_schema
from qqbot.settings import config

fails: list[str] = []


def check(name: str, condition: bool, detail: str = "") -> None:
    print(f"[{'ok ' if condition else 'FAIL'}] {name}  {detail}")
    if not condition:
        fails.append(name)


async def main() -> int:
    sql = (ROOT / "sql" / "init.sql").read_text(encoding="utf-8")
    check("the schema contains no migration ledger", "schema_migration" not in sql)
    check("the schema contains no episode participants", "episode_participant" not in sql)
    check("dump comments name the public schema", "qbot_schema_export" not in sql)

    await init_pool()
    await assert_disposable_database()
    schema = f"schema_test_{uuid.uuid4().hex}"
    dimensions = config().default.capabilities.embedding.dimensions
    async with pool().acquire() as conn, conn.transaction():
        await conn.execute(f'CREATE SCHEMA "{schema}"')
        await conn.execute(f'SET LOCAL search_path TO "{schema}", public')
        await conn.execute(sql)
        await check_schema(conn, schema=schema, embedding_dimensions=dimensions)
        check("a fresh canonical schema passes compatibility checks", True)

        await conn.execute("DROP INDEX alias_unique_account_scope")
        try:
            await check_schema(conn, schema=schema, embedding_dimensions=dimensions)
            rejected = False
        except RuntimeError:
            rejected = True
        check("missing invariant indexes are rejected", rejected)
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
        check("missing invariant constraints are rejected", rejected)
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
        check("retired compatibility columns are rejected", rejected)

    await close_pool()
    print(f"\nFAILED: {', '.join(fails) if fails else 'none'}")
    return 1 if fails else 0


if __name__ == "__main__":
    raise SystemExit(asyncio.run(main()))
