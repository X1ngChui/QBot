"""The offline claim migration preserves real data shapes and fails atomically."""

from pathlib import Path
import uuid

import asyncpg
import pytest

from qqbot.db.repo import check_schema
from qqbot.domain.memory.embedding import VECTOR_DIMENSIONS

pytestmark = pytest.mark.database
MIGRATION = Path(__file__).parents[2] / "sql/migrations/20260927_job_claims.sql"


@pytest.mark.parametrize("database", ["old"], indirect=True)
async def test_nonempty_schema_migration_preserves_domain_data_and_job_history(database):
    db = database.pool
    person = await db.fetchval(
        "INSERT INTO entity(entity_type, canonical_name) VALUES ('person','Fictional') RETURNING id"
    )
    await db.execute(
        """INSERT INTO memory_fact
               (subject_entity_id, predicate, object_value, memory_type, confidence)
           VALUES ($1,'note',$2,'attribute',1)""",
        person,
        "Fictional note",
    )
    await db.execute("INSERT INTO episode(group_id, summary) VALUES (311,'Fictional episode')")
    await db.execute(
        """INSERT INTO scheduled_task(group_id, creator_id, intent, due_at, chain_id, chain_depth)
           VALUES (311,'101','Fictional wakeup',now()+interval '1 day',$1,0)""",
        uuid.uuid4(),
    )
    for status in ("pending", "running", "done", "dead"):
        await db.execute(
            """INSERT INTO memory_job
                   (job_type,payload,status,retry_count,locked_by,locked_at,last_error)
               VALUES ('embed',$1,$2,2,'old-worker',now(),'Fictional history')""",
            {"group_id": status},
            status,
        )
    snapshots = {
        table: await db.fetch(f"SELECT * FROM {table} ORDER BY id")
        for table in ("memory_fact", "episode", "scheduled_task")
    }
    jobs = await db.fetch(
        "SELECT id,payload,status,retry_count,last_error FROM memory_job ORDER BY id"
    )
    await database.control.execute(MIGRATION.read_text(encoding="utf-8"))
    for table, before in snapshots.items():
        assert await db.fetch(f"SELECT * FROM {table} ORDER BY id") == before
    assert (
        await db.fetch(
            "SELECT id,payload,status,retry_count,last_error FROM memory_job ORDER BY id"
        )
        == jobs
    )
    assert await db.fetchval(
        "SELECT bool_and(claim_token IS NOT NULL AND lease_until<=clock_timestamp()) "
        "FROM memory_job WHERE status='running'"
    )
    assert await db.fetchval(
        "SELECT bool_and(locked_at IS NULL AND locked_by IS NULL) "
        "FROM memory_job WHERE status<>'running'"
    )
    await database.control.execute(
        (MIGRATION.parent / "20260930_group_tasks_notes.sql").read_text(encoding="utf-8")
    )
    await check_schema(
        database.control, schema=database.schema, embedding_dimensions=VECTOR_DIMENSIONS
    )
    with pytest.raises(asyncpg.DuplicateColumnError):
        await database.control.execute(MIGRATION.read_text(encoding="utf-8"))
    await database.control.execute("ROLLBACK")
    assert (
        await db.fetch(
            "SELECT id,payload,status,retry_count,last_error FROM memory_job ORDER BY id"
        )
        == jobs
    )


@pytest.mark.parametrize("database", ["old"], indirect=True)
async def test_failed_migration_rolls_back_every_schema_and_data_change(database):
    await database.pool.execute(
        "INSERT INTO memory_job(job_type,payload,status) VALUES ('embed','{}','invalid-state')"
    )
    with pytest.raises(asyncpg.CheckViolationError):
        await database.control.execute(MIGRATION.read_text(encoding="utf-8"))
    await database.control.execute("ROLLBACK")
    assert not await database.pool.fetchval(
        """SELECT EXISTS(SELECT 1 FROM information_schema.columns
           WHERE table_schema=$1 AND table_name='memory_job' AND column_name='claim_token')""",
        database.schema,
    )
    assert await database.pool.fetchval("SELECT status FROM memory_job") == "invalid-state"


async def test_fresh_schema_matches_startup_checker(database):
    await check_schema(
        database.control, schema=database.schema, embedding_dimensions=VECTOR_DIMENSIONS
    )
