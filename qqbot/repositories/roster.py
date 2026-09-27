"""One MVCC snapshot for complete holder-aware roster reads, without per-member queries."""

from collections.abc import Callable

import asyncpg

from qqbot.domain.ids import GroupId
from qqbot.domain.identity.reading import HolderReading
from qqbot.repositories.event import EventRepository
from qqbot.repositories.identity import IdentityRepository
from qqbot.repositories.memory import MemoryRepository


class RosterRepository:
    def __init__(
        self,
        database: Callable[[], asyncpg.Pool],
        *,
        identities: IdentityRepository,
        memory: MemoryRepository,
        events: EventRepository,
    ) -> None:
        self._database = database
        self._ids = identities
        self._memory = memory
        self._events = events

    async def revision(self, group_id: GroupId) -> tuple:
        row = await self._database().fetchrow(
            """SELECT (SELECT max(updated_at) FROM memory_fact WHERE group_id=$1) AS facts,
                      (SELECT max(updated_at) FROM alias
                        WHERE group_id=$1 OR group_id IS NULL) AS names,
                      (SELECT max(updated_at) FROM entity) AS people""",
            group_id.to_db(),
        )
        return row["facts"], row["names"], row["people"]

    async def read(self, group_id: GroupId, *, exclude: set[str]) -> tuple[HolderReading, ...]:
        async with (
            self._database().acquire() as conn,
            conn.transaction(isolation="repeatable_read", readonly=True),
        ):
            activity = {
                speaker.user_id: speaker
                for speaker in await self._events.speakers(group_id, _conn=conn)
                if speaker.user_id not in exclude
            }
            accounts = await self._ids.accounts_by_users("qq", list(activity), _conn=conn)
            grouped = {}
            for account in accounts:
                grouped.setdefault(account.entity_id, []).append(activity[account.platform_user_id])
            roots = list(grouped)
            linked = await self._ids.accounts_of_many(roots, _conn=conn)
            account_roots = {account.id: account.entity_id for account in linked}
            aliases = await self._ids.aliases_for_many(group_id, roots, _conn=conn)
            facts = await self._memory.current_facts(group_id, roots, _conn=conn)
            by_holder = {}
            for fact in facts:
                if fact.subject_entity_id is not None:
                    root = fact.subject_entity_id
                else:
                    assert fact.subject_account_id is not None
                    root = account_roots[fact.subject_account_id]
                by_holder.setdefault(root, []).append(fact)
            return tuple(
                HolderReading(
                    root,
                    tuple(speakers),
                    tuple(aliases.get(root, ())),
                    tuple(by_holder.get(root, ())),
                )
                for root, speakers in grouped.items()
            )
