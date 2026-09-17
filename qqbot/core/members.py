"""Who is in a group, straight from the protocol side.

The mapping from QQ id to the name shown in chat is something NapCat already knows, so
asking it once for the whole group beats accumulating names one message at a time. It also
answers for people who have never spoken, which watching the transcript never can.

Fetched lazily: a group with no mentions and no configured roster never triggers a call.
The TTL exists only to stop a group full of mentions asking once per message - it is a
refresh interval, not a coherence mechanism, and a name that is a few minutes stale is not
a problem worth more machinery than this.

Names here are plain display names. Members who share one are told apart in the prompt
by member numbers (core.member_numbers), which belong to one render, not to the name.
"""

from __future__ import annotations

import asyncio
import logging
import time

from ..settings import config
from ..util import display_name, why
from .botapi import BotApi

log = logging.getLogger("qqbot.members")

class MemberDirectory:
    def __init__(self) -> None:
        self._by_group: dict[str, dict[str, str]] = {}
        self._fetched: dict[str, float] = {}
        self._locks: dict[str, asyncio.Lock] = {}
        #: Accounts asked about and absent from the table as of its last fetch -
        #: members who have left, quoted or still in the window. A miss forces a
        #: refresh once; remembered here, it does not force one on every reply.
        self._missing: dict[str, set[str]] = {}

    def _fresh(self, group_id: str) -> bool:
        ttl = config().default.gateway.member_cache_ttl_sec
        return time.monotonic() - self._fetched.get(group_id, 0.0) < ttl

    async def _fetch(self, bot: BotApi, group_id: str, *, force: bool = False) -> None:
        """Refresh one group's table. `force` refetches inside the TTL - for a member
        the table has never heard of - but never twice for one decision: a refresh
        that landed while this caller waited for the lock is the refresh it wanted."""
        asked = time.monotonic()
        lock = self._locks.setdefault(group_id, asyncio.Lock())
        async with lock:
            if self._fetched.get(group_id, 0.0) > asked:
                return
            if not force and self._fresh(group_id):
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
                # defang at the fetch: these names flow to transcripts, notice
                # lines and @-resolution, where the system brackets must stay
                # unforgeable.
                name = display_name(r.get("card"), r.get("nickname"))
                if qq and name:
                    table[qq] = name
            self._by_group[group_id] = table
            self._missing.pop(group_id, None)
            self._fetched[group_id] = time.monotonic()
            log.info("group %s: member list refreshed (%d people)", group_id, len(table))

    async def _current(self, bot: BotApi, group_id: str,
                       wanted: list[str]) -> dict[str, str]:
        """The live table, refreshed when stale or when it lacks a member it has not
        been asked about since its last fetch.

        Staleness alone refreshes - not only a miss. The TTL is what makes renames
        visible at all: every reader of current names sits behind this cache, so a
        cache that only refreshed on misses would serve a fully-known group the
        same names forever. A miss refreshes too, inside the TTL: a member who
        joined a minute ago and was @-ed at once would otherwise archive as a bare
        account number until the next refresh. Misses that survive the refresh are
        remembered until the next one, so a member who has left the group does not
        cost a fetch on every reply their old lines appear in.
        """
        table = self._by_group.get(group_id) or {}
        gone = self._missing.get(group_id) or set()
        if any(q not in table and q not in gone for q in wanted):
            await self._fetch(bot, group_id, force=True)
        elif not self._fresh(group_id):
            await self._fetch(bot, group_id)
        table = self._by_group.get(group_id) or {}
        self._missing.setdefault(group_id, set()).update(
            q for q in wanted if q not in table)
        return table

    async def name_of(self, bot: BotApi, group_id: str, qq: str) -> str | None:
        """The display name for one member - see _current for when the list is refetched."""
        if not qq:
            return None
        return (await self._current(bot, group_id, [qq])).get(qq)

    async def names_of(self, bot: BotApi, group_id: str, qqs: list[str]) -> dict[str, str]:
        """Display names for several members in one go - at most one API call."""
        wanted = [q for q in qqs if q]
        if not wanted:
            return {}
        table = await self._current(bot, group_id, wanted)
        return {q: table[q] for q in wanted if q in table}

    async def relabel(self, bot: BotApi, group_id: str, msgs) -> int:
        """Update the names on a batch of ChatMsg to the current group cards: each
        member line's speaker, and whom each of the bot's own lines @-ed.

        A name is captured when the message arrives, so history keeps whatever someone was
        called at the time. That disagrees with every other path - the group card
        transcript, the profiles - which read the name at query time, and the model then
        sees one person under two names. Renames are rare, so the cost of re-rendering
        history is rare too. A member no longer in the group keeps the name their lines
        arrived with.

        Returns how many names changed.
        """
        ids = {m.user_id for m in msgs if not m.is_bot and m.user_id}
        ids |= {a for m in msgs if m.is_bot for a, _ in m.at}
        if not ids:
            return 0
        live = await self.names_of(bot, group_id, list(ids))
        renamed = 0
        for m in msgs:
            if m.is_bot:
                at = [(a, live.get(a) or n) for a, n in m.at]
                renamed += sum(x != y for x, y in zip(at, m.at, strict=True))
                m.at = at
                continue
            new = live.get(m.user_id)
            if new and new != m.nickname:
                m.nickname = new
                renamed += 1
        if renamed:
            log.info("group %s: %d name(s) relabelled after a rename", group_id, renamed)
        return renamed

    def forget(self, group_id: str | None = None) -> None:
        if group_id is None:
            self._by_group.clear()
            self._fetched.clear()
            self._missing.clear()
        else:
            self._by_group.pop(group_id, None)
            self._fetched.pop(group_id, None)
            self._missing.pop(group_id, None)


MEMBERS = MemberDirectory()
