"""The group-contract conversion preserves history and retires ambiguous pending work."""

from pathlib import Path
import uuid

import asyncpg
import pytest

from qqbot.db.repo import check_schema
from qqbot.domain.memory.embedding import VECTOR_DIMENSIONS

pytestmark = pytest.mark.database
ROOT = Path(__file__).parents[2]
CLAIMS = ROOT / "sql/migrations/20260927_job_claims.sql"
CONTRACT = ROOT / "sql/migrations/20260930_group_tasks_notes.sql"


@pytest.mark.parametrize("database", ["old"], indirect=True)
async def test_nonempty_group_conversion_preserves_notes_and_task_history(database):
    db = database.pool
    await database.control.execute(CLAIMS.read_text(encoding="utf-8"))
    entity = await db.fetchval(
        "INSERT INTO entity(entity_type,canonical_name) VALUES ('person','Fictional') RETURNING id"
    )
    account = await db.fetchval(
        "INSERT INTO identity_account(entity_id,platform,platform_user_id) "
        "VALUES ($1,'qq','fictional') "
        "RETURNING id",
        entity,
    )
    ids = []
    for status, until, value in (
        ("superseded", "now()", "First version"),
        ("active", "NULL", "Current version"),
    ):
        ids.append(
            await db.fetchval(
                "INSERT INTO memory_fact(group_id,subject_account_id,predicate,"
                "object_value,memory_type,"
                f"confidence,status,valid_to) VALUES (311,$1,'note',$2,'attribute',1,$3,{until}) "
                "RETURNING id",
                account,
                value,
                status,
            )
        )
    pending = await db.fetchval(
        "INSERT INTO scheduled_task(group_id,creator_id,intent,due_at,chain_id,chain_depth) "
        "VALUES (311,'fictional','提醒我检查虚构设备',now()+interval '1 day',$1,3) RETURNING id",
        uuid.uuid4(),
    )
    ended = await db.fetchval(
        "INSERT INTO scheduled_task(group_id,creator_id,intent,due_at,chain_id,chain_depth,status,"
        "finished_at,outcome) VALUES (311,'fictional','Fictional completed',now(),"
        "$1,2,'done',now(),"
        "'sent') RETURNING id",
        uuid.uuid4(),
    )
    before = [
        dict(row) for row in await db.fetch("SELECT * FROM memory_fact ORDER BY created_at,id")
    ]
    task_before = dict(await db.fetchrow("SELECT * FROM scheduled_task WHERE id=$1", ended))
    await database.control.execute(CONTRACT.read_text(encoding="utf-8"))
    after = [
        dict(row) for row in await db.fetch("SELECT * FROM memory_fact ORDER BY created_at,id")
    ]
    assert len(after) == len(before) and {row["id"] for row in after} == set(ids)
    assert len({row["object_key"] for row in after}) == 1
    for old, new in zip(before, after, strict=True):
        assert {k: v for k, v in new.items() if k != "object_key"} == {
            k: v for k, v in old.items() if k != "object_key"
        }
    finished = dict(await db.fetchrow("SELECT * FROM scheduled_task WHERE id=$1", ended))
    assert finished == {k: v for k, v in task_before.items() if k != "creator_id"}
    assert await db.fetchval("SELECT status FROM scheduled_task WHERE id=$1", pending) == "failed"
    assert (
        await db.fetchval("SELECT outcome FROM scheduled_task WHERE id=$1", pending)
        == "scope_changed"
    )
    await check_schema(
        database.control, schema=database.schema, embedding_dimensions=VECTOR_DIMENSIONS
    )
    with pytest.raises(asyncpg.UndefinedObjectError):
        await database.control.execute(CONTRACT.read_text(encoding="utf-8"))
    await database.control.execute("ROLLBACK")
    assert await db.fetchval("SELECT count(*) FROM scheduled_task") == 2


@pytest.mark.parametrize("database", ["old"], indirect=True)
async def test_group_conversion_failure_rolls_back_task_and_note_changes(database):
    db = database.pool
    await database.control.execute(CLAIMS.read_text(encoding="utf-8"))
    entity = await db.fetchval(
        "INSERT INTO entity(entity_type,canonical_name) VALUES ('person','Fictional') RETURNING id"
    )
    await db.execute(
        "INSERT INTO memory_fact(group_id,subject_entity_id,predicate,object_key,object_value,"
        "memory_type,confidence) VALUES (311,$1,'note','invalid-key',$2,'attribute',1)",
        entity,
        "Fictional",
    )
    pending = await db.fetchval(
        "INSERT INTO scheduled_task(group_id,creator_id,intent,due_at,chain_id,chain_depth) "
        "VALUES (311,'fictional','Fictional pending',now(),$1,0) RETURNING id",
        uuid.uuid4(),
    )
    with pytest.raises(asyncpg.CheckViolationError):
        await database.control.execute(CONTRACT.read_text(encoding="utf-8"))
    await database.control.execute("ROLLBACK")
    assert await db.fetchval("SELECT status FROM scheduled_task WHERE id=$1", pending) == "pending"
    assert (
        await db.fetchval("SELECT creator_id FROM scheduled_task WHERE id=$1", pending)
        == "fictional"
    )
    assert await db.fetchval("SELECT object_key FROM memory_fact") == "invalid-key"
