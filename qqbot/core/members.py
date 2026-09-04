"""Who is in a group, straight from the protocol side.

The mapping from QQ id to the name shown in chat is something NapCat already knows, so
asking it once for the whole group beats accumulating names one message at a time. It also
answers for people who have never spoken, which watching the transcript never can.

Fetched lazily: a group with no mentions and no configured roster never triggers a call.
The TTL exists only to stop a group full of mentions asking once per message - it is a
refresh interval, not a coherence mechanism, and a name that is a few minutes stale is not
a problem worth more machinery than this.
"""

from __future__ import annotations

import asyncio
import logging
import time

from ..util import why
from .botapi import BotApi

log = logging.getLogger("qqbot.members")

#: How long a fetched roster is reused. Group cards change rarely; this is about call
#: volume, not freshness.
TTL_SEC = 1800


class MemberDirectory:
    def __init__(self) -> None:
        self._by_group: dict[str, dict[str, str]] = {}
        self._fetched: dict[str, float] = {}
        self._locks: dict[str, asyncio.Lock] = {}

    def _fresh(self, group_id: str) -> bool:
        return time.monotonic() - self._fetched.get(group_id, 0.0) < TTL_SEC

    async def _fetch(self, bot: BotApi, group_id: str) -> None:
        lock = self._locks.setdefault(group_id, asyncio.Lock())
        async with lock:
            if self._fresh(group_id):  # another waiter refreshed it
                return
            try:
                rows = await bot.call_api("get_group_member_list", group_id=int(group_id))
            except Exception as e:
                # Mark the attempt so a persistently failing group does not retry per
                # message; the old table, if any, stays usable - names captured when each
                # message arrived are still correct for anyone who has not renamed.
                #
                # Logged at warning rather than swallowed, because the visible symptom is
                # the bot calling somebody by a name they dropped last week, and nothing
                # about that says the member list is what failed.
                self._fetched[group_id] = time.monotonic()
                log.warning("group %s: member list unavailable, names may be stale: %s",
                            group_id, why(e))
                return
            table: dict[str, str] = {}
            for r in rows or []:
                qq = str(r.get("user_id") or "").strip()
                name = (r.get("card") or r.get("nickname") or "").strip()
                if qq and name:
                    table[qq] = name
            self._by_group[group_id] = table
            self._fetched[group_id] = time.monotonic()
            log.info("group %s: member list refreshed (%d people)", group_id, len(table))

    async def name_of(self, bot: BotApi, group_id: str, qq: str) -> str | None:
        """The display name for one member, refreshing the group's list when stale.

        Staleness alone triggers the refetch - not only a miss. The TTL is what makes
        renames visible at all: every reader of current names sits behind this cache, so
        a cache that only refreshed on misses would serve a fully-known group the same
        names forever.
        """
        if not qq:
            return None
        if not self._fresh(group_id):
            await self._fetch(bot, group_id)
        return (self._by_group.get(group_id) or {}).get(qq)

    async def names_of(self, bot: BotApi, group_id: str, qqs: list[str]) -> dict[str, str]:
        """Display names for several members in one go - at most one API call.

        Refetches when stale or when any wanted member is unknown, for the same reason
        as name_of: staleness must refresh even with zero misses.
        """
        wanted = [q for q in qqs if q]
        if not wanted:
            return {}
        table = self._by_group.get(group_id) or {}
        if not self._fresh(group_id) or any(q not in table for q in wanted):
            await self._fetch(bot, group_id)
            table = self._by_group.get(group_id) or {}
        return {q: table[q] for q in wanted if q in table}

    async def relabel(self, bot: BotApi, group_id: str, msgs) -> int:
        """Update the speaker names on a batch of ChatMsg to the current group card.

        A name is captured when the message arrives, so history keeps whatever someone was
        called at the time. That disagrees with every other path - the group card
        transcript, the profiles - which read the name at query time, and the model then
        sees one person under two names. Renames are rare, so the cost of re-rendering
        history is rare too.
        """
        ids = {m.user_id for m in msgs if not m.is_bot and m.user_id}
        if not ids:
            return 0
        live = await self.names_of(bot, group_id, list(ids))
        changed = 0
        for m in msgs:
            new = live.get(m.user_id)
            if new and not m.is_bot and new != m.nickname:
                m.nickname = new
                changed += 1
        if changed:
            log.info("group %s: %d message(s) relabelled after a rename", group_id, changed)
        return changed

    def forget(self, group_id: str | None = None) -> None:
        if group_id is None:
            self._by_group.clear()
            self._fetched.clear()
        else:
            self._by_group.pop(group_id, None)
            self._fetched.pop(group_id, None)


MEMBERS = MemberDirectory()
