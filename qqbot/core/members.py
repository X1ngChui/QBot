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

from ..settings import config
from ..util import display_name, why
from . import namesakes
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
                # lines and @-resolution, and the namesake tag appended below is
                # only unforgeable if the name half cannot carry system brackets.
                name = display_name(r.get("card"), r.get("nickname"))
                if qq and name:
                    table[qq] = name
            self._raw_by_group[group_id] = dict(table)
            # Two members sharing one display name is ordinary, and a name is
            # all the model ever sees - so a clashing member's entry wears the
            # namesake tag (core.namesakes: permanent serials, one per person,
            # none between a person's own accounts). The tag wears the system
            # brackets, which a card cannot contain: in parentheses it would be
            # indistinguishable from a member whose literal card ends in "(3)".
            # This runs on every refresh, which is what tracks renames: a new
            # clash gains tags, a dissolved one loses them. Every reader of
            # current names sits behind this table - relabel, the roster's live
            # names, @-resolution, the notice lines. A numbering failure
            # degrades to bare names rather than losing the fetch.
            try:
                for qq, tag in (await namesakes.tags(int(group_id), table)).items():
                    table[qq] = table[qq] + tag
            except Exception as e:
                log.warning("group %s: namesake numbering unavailable, "
                            "names stay bare: %s", group_id, why(e))
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

    async def raw_name_of(self, bot: BotApi, group_id: str, qq: str) -> str | None:
        """The display name before namesake suffixing - for anything that files
        a name into storage or compares against stored names. The suffix is a
        transcript rendering, never a name anyone carries."""
        if not qq:
            return None
        await self._current(bot, group_id, [qq])
        return (self._raw_by_group.get(group_id) or {}).get(qq)

    async def raw_names_of(self, bot: BotApi, group_id: str,
                           qqs: list[str]) -> dict[str, str]:
        """Pre-suffix display names for several members - see raw_name_of."""
        wanted = [q for q in qqs if q]
        if not wanted:
            return {}
        await self._current(bot, group_id, wanted)
        table = self._raw_by_group.get(group_id) or {}
        return {q: table[q] for q in wanted if q in table}

    async def relabel(self, bot: BotApi, group_id: str, msgs) -> int:
        """Update the speaker names on a batch of ChatMsg to the current group card.

        A name is captured when the message arrives, so history keeps whatever someone was
        called at the time. That disagrees with every other path - the group card
        transcript, the profiles - which read the name at query time, and the model then
        sees one person under two names. Renames are rare, so the cost of re-rendering
        history is rare too.

        Returns how many lines changed name. A line from a namesake arrives carrying the
        bare card and gains its tag here on every message; that is this render step
        doing its job, so a changed tag alone is applied without being counted.

        A member no longer in the group keeps the name their lines arrived with -
        but when that name is a live member's too, both sides are tagged: the live
        table alone sees no clash once one namesake leaves or renames, and a bare
        name beside a tagged one reads as the same account.
        """
        ids = {m.user_id for m in msgs if not m.is_bot and m.user_id}
        if not ids:
            return 0
        live = await self.names_of(bot, group_id, list(ids))
        raw = await self.raw_names_of(bot, group_id, list(ids))
        gone = {m.user_id: namesakes.bare(m.nickname) for m in msgs
                if not m.is_bot and m.user_id and m.user_id not in raw}
        if gone and set(gone.values()) & set(raw.values()):
            try:
                tags = await namesakes.tags(int(group_id), {**raw, **gone})
                live = {u: raw[u] + tags.get(u, "") for u in raw}
                live.update({u: n + tags.get(u, "") for u, n in gone.items()})
            except Exception as e:
                log.warning("group %s: namesake numbering unavailable, "
                            "names stay bare: %s", group_id, why(e))
        renamed = 0
        for m in msgs:
            new = live.get(m.user_id)
            if not new or m.is_bot or new == m.nickname:
                continue
            if namesakes.bare(new) != namesakes.bare(m.nickname):
                renamed += 1
            m.nickname = new
        if renamed:
            log.info("group %s: %d message(s) relabelled after a rename", group_id, renamed)
        return renamed

    def forget(self, group_id: str | None = None) -> None:
        if group_id is None:
            self._by_group.clear()
            self._raw_by_group.clear()
            self._fetched.clear()
            self._missing.clear()
        else:
            self._by_group.pop(group_id, None)
            self._raw_by_group.pop(group_id, None)
            self._fetched.pop(group_id, None)
            self._missing.pop(group_id, None)


MEMBERS = MemberDirectory()
