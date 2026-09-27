"""Bulk rosters keep group scope, linked holders and a consistent topology snapshot."""

import _db as _test_db
from contextlib import asynccontextmanager

import pytest

from qqbot.domain.ids import GroupId
from qqbot.repositories.event import EventRepository
from qqbot.repositories.identity import IdentityRepository
from qqbot.repositories.memory import MemoryRepository
from qqbot.repositories.roster import RosterRepository

pytestmark = pytest.mark.database


class CountedPool:
    def __init__(self, pool):
        self.pool = pool
        self.queries = 0
        self.after_accounts = None

    @asynccontextmanager
    async def acquire(self):
        async with self.pool.acquire() as conn:
            owner = self

            class Reader:
                def transaction(self, **kwargs):
                    return conn.transaction(**kwargs)

                async def fetch(self, *args):
                    owner.queries += 1
                    rows = await conn.fetch(*args)
                    if owner.queries == 2 and owner.after_accounts is not None:
                        await owner.after_accounts()
                    return rows

            yield Reader()


def reader(database):
    counted = CountedPool(database.pool)
    return counted, RosterRepository(
        lambda: counted,
        identities=IdentityRepository(database=(lambda: database.pool), clock=_test_db.clock),
        memory=MemoryRepository(database=(lambda: database.pool), clock=_test_db.clock),
        events=EventRepository(database=(lambda: database.pool)),
    )


async def seed(database, user, group=311):
    entity = await database.pool.fetchval(
        "INSERT INTO entity(entity_type,canonical_name) VALUES ('person','Fictional') RETURNING id"
    )
    account = await database.pool.fetchval(
        "INSERT INTO identity_account(entity_id,platform,platform_user_id) VALUES ($1,'qq',$2) "
        "RETURNING id",
        entity,
        user,
    )
    await database.pool.execute(
        """INSERT INTO raw_event(platform,event_type,group_id,platform_user_id,occurred_at,payload)
           VALUES ('qq','message',$1,$2,now(),'{}')""",
        group,
        user,
    )
    return entity, account


@pytest.mark.parametrize("size", [1, 50])
async def test_query_count_does_not_grow_with_roster_size(database, size):
    for n in range(size):
        await seed(database, f"fictional-{n}")
    counted, repository = reader(database)
    result = await repository.read(GroupId("311"), exclude=set())
    assert len(result) == size
    assert counted.queries == 5


async def test_bulk_read_preserves_group_notes_candidates_and_linked_accounts(database):
    first, first_account = await seed(database, "fictional-a")
    second, second_account = await seed(database, "fictional-b")
    await database.pool.execute(
        "UPDATE identity_account SET entity_id=$1 WHERE id=$2", first, second_account
    )
    await database.pool.execute("UPDATE entity SET merged_into=$1 WHERE id=$2", first, second)
    for group, value in ((311, "Fictional note"), (312, "Other group note")):
        await database.pool.execute(
            """INSERT INTO memory_fact(group_id,subject_account_id,predicate,object_value,
                                       memory_type,confidence)
               VALUES ($1,$2,'note',$3,'attribute',1)""",
            group,
            second_account,
            value,
        )
    await database.pool.execute(
        """INSERT INTO alias(alias_text,normalized_text,target_account_id,group_id,
                             alias_type,confidence,status)
           VALUES ('Fictional candidate','fictional candidate',
                   $1,311,'nickname',0.2,'candidate')""",
        first_account,
    )
    _, repository = reader(database)
    result = await repository.read(GroupId("311"), exclude=set())
    assert len(result) == 1
    assert {speaker.user_id for speaker in result[0].speakers} == {"fictional-a", "fictional-b"}
    assert [fact.object_value for fact in result[0].facts] == ["Fictional note"]
    assert len(result[0].aliases) == 1 and not result[0].aliases[0].is_usable
    excluded = await repository.read(GroupId("311"), exclude={"fictional-b"})
    assert len(excluded[0].speakers) == 1
    assert [fact.object_value for fact in excluded[0].facts] == ["Fictional note"]


async def test_concurrent_topology_change_does_not_mix_read_epochs(database):
    first, _ = await seed(database, "fictional-a")
    second, second_account = await seed(database, "fictional-b")
    counted, repository = reader(database)

    async def merge():
        async with database.pool.acquire() as conn, conn.transaction():
            await conn.execute(
                "UPDATE identity_account SET entity_id=$1 WHERE id=$2", first, second_account
            )
            await conn.execute("UPDATE entity SET merged_into=$1 WHERE id=$2", first, second)

    counted.after_accounts = merge
    before = await repository.read(GroupId("311"), exclude=set())
    assert len(before) == 2
    counted.after_accounts = None
    after = await repository.read(GroupId("311"), exclude=set())
    assert len(after) == 1 and len(after[0].speakers) == 2
