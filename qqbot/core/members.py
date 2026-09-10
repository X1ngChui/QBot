"""Who is in a group, straight from the protocol side.

The mapping from QQ id to the name shown in chat is something NapCat already knows, so
asking it once for the whole group beats accumulating names one message at a time. It also
answers for people who have never spoken, which watching the transcript never can.

Fetched lazily: a group with no mentions and no configured roster never triggers a call.
The TTL exists only to stop a group full of mentions asking once per message - it is a
refresh interval, not a coherence mechanism, and a name that is a few minutes stale is not
a problem worth more machinery than this.

This is also where namesakes are told apart. The fetch sees every member's current card
at once, so it is the one place a clash can be detected whole: members sharing a display
name get it tagged with their permanent per-group serial (the member_seq table, worn
in the reserved system brackets) before the table is stored, and every consumer of
current names inherits the distinction.
"""

from __future__ import annotations

import asyncio
import logging
import time

from ..db import repo
from ..settings import config
from ..util import defang, sysmark, why
from .botapi import BotApi

log = logging.getLogger("qqbot.members")


class MemberDirectory:
    def __init__(self) -> None:
        self._by_group: dict[str, dict[str, str]] = {}
        #: The same table before namesake suffixing. Anything that files a name
        #: into storage or compares against stored names must read this one:
        #: the suffix is a rendering, and a rendering written into the alias
        #: table would assert the platform reported a name nobody carries.
        self._raw_by_group: dict[str, dict[str, str]] = {}
        self._fetched: dict[str, float] = {}
        self._locks: dict[str, asyncio.Lock] = {}

    def _fresh(self, group_id: str) -> bool:
        ttl = config().default.gateway.member_cache_ttl_sec
        return time.monotonic() - self._fetched.get(group_id, 0.0) < ttl

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
                # defang at the fetch: these names flow to transcripts, notice
                # lines and @-resolution, and the namesake tag appended below is
                # only unforgeable if the name half cannot carry system brackets.
                name = defang(r.get("card") or r.get("nickname") or "").strip()
                if qq and name:
                    table[qq] = name
            self._raw_by_group[group_id] = dict(table)
            # Two members sharing one display name is ordinary, and a name is
            # all the model ever sees - so each clashing member's entry becomes
            # name(N), with N the group's permanent serial for that account
            # (member_seq: never reused, never reassigned, so a suffixed name
            # keeps meaning the same person in every transcript that carried
            # it). This runs on every refresh, which is what tracks renames:
            # a new clash gains suffixes, a dissolved one loses them. Every
            # reader of current names sits behind this table - relabel, the
            # roster's live names, @-resolution, the notice lines - so live
            # names are numbered only here (the roster keeps its own pass for
            # rows falling back to archived names). A numbering failure
            # degrades to bare names rather than losing the fetch.
            names: dict[str, list[str]] = {}
            for qq, name in table.items():
                names.setdefault(name, []).append(qq)
            clashing = sorted(
                q for qqs in names.values() if len(qqs) > 1 for q in qqs)
            if clashing:
                try:
                    seqs = await repo.member_seqs(int(group_id), clashing)
                    for qq in clashing:
                        if qq in seqs:
                            # The namesake tag wears the system brackets, which a
                            # card cannot contain: in parentheses it would be
                            # indistinguishable from a member whose literal card
                            # ends in "(3)".
                            table[qq] = table[qq] + sysmark(f"同名{seqs[qq]}")
                except Exception as e:
                    log.warning("group %s: namesake numbering unavailable, "
                                "names stay bare: %s", group_id, why(e))
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

    async def raw_name_of(self, bot: BotApi, group_id: str, qq: str) -> str | None:
        """The display name before namesake suffixing - for anything that files
        a name into storage or compares against stored names. The suffix is a
        transcript rendering, never a name anyone carries."""
        if not qq:
            return None
        if not self._fresh(group_id):
            await self._fetch(bot, group_id)
        return (self._raw_by_group.get(group_id) or {}).get(qq)

    async def raw_names_of(self, bot: BotApi, group_id: str,
                           qqs: list[str]) -> dict[str, str]:
        """Pre-suffix display names for several members - see raw_name_of."""
        wanted = [q for q in qqs if q]
        if not wanted:
            return {}
        if not self._fresh(group_id):
            await self._fetch(bot, group_id)
        table = self._raw_by_group.get(group_id) or {}
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
            self._raw_by_group.clear()
            self._fetched.clear()
        else:
            self._by_group.pop(group_id, None)
            self._raw_by_group.pop(group_id, None)
            self._fetched.pop(group_id, None)


MEMBERS = MemberDirectory()
