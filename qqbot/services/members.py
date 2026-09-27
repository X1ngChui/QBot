"""Runtime-owned, bounded caching of protocol member names."""

from __future__ import annotations

import logging
import time
from collections import OrderedDict
from dataclasses import dataclass, field

from qqbot.concurrency import SharedWork, WorkRejected
from qqbot.domain.ids import GroupId
from qqbot.gateway.botapi import BotApi
from qqbot.util import display_name, why

log = logging.getLogger("qqbot.members")

CACHE_TTL = 1800
MAX_GROUP_NAMES = 16_384
MAX_MISSING_NAMES = 1024

type CacheKey = tuple[str, GroupId]


@dataclass(slots=True)
class MemberNames:
    names: dict[str, str]
    fetched: float
    missing: set[str] = field(default_factory=set)


class MemberDirectory:
    def __init__(self, *, capacity: int = 256, max_names: int = 65_536, clock=time.monotonic):
        if capacity < 1 or max_names < 1:
            raise ValueError("member cache bounds must be positive")
        self._capacity = capacity
        self._max_names = max_names
        self._clock = clock
        self._cache: OrderedDict[CacheKey, MemberNames] = OrderedDict()
        self._work: SharedWork[CacheKey, MemberNames] = SharedWork(capacity=32, timeout=10)
        self._epoch = 0
        self._closed = False

    @property
    def cached_groups(self) -> int:
        return len(self._cache)

    @property
    def cached_names(self) -> int:
        return sum(len(entry.names) for entry in self._cache.values())

    def _store(self, key: CacheKey, entry: MemberNames) -> None:
        self._cache.pop(key, None)
        if len(entry.names) > self._max_names:
            return
        while self._cache and (
            len(self._cache) >= self._capacity
            or self.cached_names + len(entry.names) > self._max_names
        ):
            self._cache.popitem(last=False)
        self._cache[key] = entry

    async def _fetch(self, bot: BotApi, key: CacheKey) -> MemberNames:
        epoch = self._epoch
        previous = self._cache.get(key)
        names = previous.names if previous is not None else {}
        try:
            rows = await bot.call_api("get_group_member_list", group_id=key[1].to_onebot())
            if not isinstance(rows, list | tuple) or len(rows) > MAX_GROUP_NAMES:
                raise ValueError("invalid or oversized member list")
            names = {}
            for row in rows:
                if not isinstance(row, dict):
                    continue
                raw = row.get("user_id")
                if isinstance(raw, int) and not isinstance(raw, bool) and raw.bit_length() <= 64:
                    account = str(raw)
                elif isinstance(raw, str) and len(raw) <= 32:
                    account = raw.strip()
                else:
                    continue
                card, nickname = row.get("card"), row.get("nickname")
                name = display_name(
                    card[:128] if isinstance(card, str) else "",
                    nickname[:128] if isinstance(nickname, str) else "",
                )
                if account and name:
                    names[account] = name
        except Exception as exc:
            log.warning("group %s: member list unavailable: %s", key[1], why(exc))
        missing = (
            previous.missing - names.keys()
            if previous is not None and self._clock() - previous.fetched < CACHE_TTL
            else set()
        )
        entry = MemberNames(names, self._clock(), missing)
        if not self._closed and epoch == self._epoch:
            self._store(key, entry)
        return entry

    async def _current(self, bot: BotApi, group_id: GroupId, wanted: list[str]) -> dict[str, str]:
        if self._closed:
            return {}
        key = (str(bot.self_id), group_id)
        entry = self._cache.get(key)
        fresh = entry is not None and self._clock() - entry.fetched < CACHE_TTL
        unknown = (
            entry is not None
            and len(entry.missing) < MAX_MISSING_NAMES
            and any(
                account not in entry.names and account not in entry.missing for account in wanted
            )
        )
        if not fresh or unknown:
            try:
                entry = await self._work.run(key, lambda: self._fetch(bot, key))
            except (WorkRejected, TimeoutError):
                return entry.names if entry is not None else {}
        if entry is None:
            return {}
        if key in self._cache:
            self._cache.move_to_end(key)
        for account in wanted:
            if len(entry.missing) >= MAX_MISSING_NAMES:
                break
            if account not in entry.names:
                entry.missing.add(account)
        return entry.names

    async def name_of(self, bot: BotApi, group_id: GroupId, qq: str) -> str | None:
        """The display name for one member - see _current for when the list is refetched."""
        if not qq:
            return None
        return (await self._current(bot, group_id, [qq])).get(qq)

    async def names_of(self, bot: BotApi, group_id: GroupId, qqs: list[str]) -> dict[str, str]:
        """Display names for several members in one go - at most one API call."""
        wanted = [q for q in qqs if q]
        if not wanted:
            return {}
        table = await self._current(bot, group_id, wanted)
        return {q: table[q] for q in wanted if q in table}

    async def relabel(self, bot: BotApi, group_id: GroupId, msgs) -> int:
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

    def forget(self, group_id: GroupId | None = None) -> None:
        self._epoch += 1
        for key in list(self._cache):
            if group_id is None or key[1] == group_id:
                del self._cache[key]

    def abort(self) -> None:
        self._closed = True
        self._work.abort()
        self.forget()

    async def close(self) -> None:
        self.abort()
        await self._work.close()
