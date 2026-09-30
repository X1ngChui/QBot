"""Database reporting constructs platform IDs before values return to core."""

import pytest

from _fixtures import clock
from qqbot.domain.ids import AccountId, GroupId
from qqbot.repositories.event import EventRepository
from qqbot.repositories.groups import GroupRepository
from qqbot.repositories.identity import IdentityRepository
from qqbot.repositories.ledger import LedgerRepository

pytestmark = pytest.mark.database
GROUP = GroupId("311")


async def test_account_read_models_preserve_nominal_ids(database):
    def pool():
        return database.pool

    accounts = IdentityRepository(database=pool, clock=clock)
    first = await accounts.ensure_account("qq", AccountId("fictional-a"), seen_at=clock.now())
    second = await accounts.ensure_account("qq", AccountId("fictional-b"), seen_at=clock.now())
    await accounts.merge_accounts(first.id, second.id)
    for user in (first.platform_user_id, second.platform_user_id):
        await database.pool.execute(
            "INSERT INTO raw_event(platform,event_type,group_id,platform_user_id,"
            "occurred_at,payload) "
            "VALUES ('qq','message',$1,$2,now(),'{}')",
            GROUP.to_db(),
            user,
        )
    events = EventRepository(database=pool)
    assert all(type(user) is AccountId for user in await events.speaker_counts(GROUP))
    assert all(type(user) is AccountId for user in await events.first_appearances(GROUP))
    groups = GroupRepository(pool, clock=clock)
    await groups.block(GROUP, first.platform_user_id)
    assert type((await groups.block_rules(GROUP))[0]["user_id"]) is AccountId
    assert await groups.unblock(GROUP, first.platform_user_id)
    ledger = LedgerRepository(pool, today=clock.today)
    for user in (first.platform_user_id, second.platform_user_id):
        await ledger.ledger_add(
            group_id=GROUP, user_id=user, kind="reply", model="fictional", cny=1
        )
    for linked in (False, True):
        rows = await ledger.top_spenders(GROUP, k=5, all_linked=linked)
        assert len(rows) == (1 if linked else 2)
        assert sum(row["cny"] for row in rows) == 2
        assert all(type(user) is AccountId for row in rows for user in row["accounts"])
    await groups.block(GROUP, [first.platform_user_id, second.platform_user_id])
    assert len(await groups.block_rules(GROUP)) == 2
    assert await groups.unblock(GROUP, [first.platform_user_id, second.platform_user_id])
