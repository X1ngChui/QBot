"""Instance-owned roster snapshots with bounded retention and detached return values."""

from collections import OrderedDict
from collections.abc import Callable
from dataclasses import dataclass
from qqbot.domain.ids import AccountId
import time
import uuid

from qqbot.domain.ids import GroupId


@dataclass(frozen=True, slots=True)
class RosterRow:
    user_id: AccountId
    entity_id: uuid.UUID
    accounts: tuple[AccountId, ...]
    nickname: str
    former_names: tuple[str, ...]
    aliases: tuple[str, ...]
    memory_hints: tuple[str, ...]
    manual_note: str
    msg_count: int

    def project(self) -> dict:
        return {
            "user_id": self.user_id,
            "entity_id": self.entity_id,
            "accounts": list(self.accounts),
            "nickname": self.nickname,
            "former_names": list(self.former_names),
            "aliases": list(self.aliases),
            "memory_hints": self.memory_hints,
            "manual_note": self.manual_note,
            "msg_count": self.msg_count,
        }


@dataclass(frozen=True, slots=True)
class CachedRoster:
    revision: tuple
    live: tuple[tuple[AccountId, str], ...]
    rows: tuple[RosterRow, ...]
    expires: float
    text_bytes: int

    @property
    def speakers(self) -> tuple[AccountId, ...]:
        return tuple(account for row in self.rows for account in row.accounts)

    def project(self) -> list[dict]:
        return [row.project() for row in self.rows]


class RosterCache:
    def __init__(
        self,
        *,
        capacity: int = 128,
        max_rows: int = 8192,
        max_text_bytes: int = 4 * 1024 * 1024,
        ttl: float = 60,
        clock: Callable[[], float] = time.monotonic,
    ) -> None:
        if min(capacity, max_rows, max_text_bytes, ttl) <= 0:
            raise ValueError("roster retention bounds must be positive")
        self._capacity = capacity
        self._max_rows = max_rows
        self._max_text_bytes = max_text_bytes
        self._ttl = ttl
        self._clock = clock
        self._entries: OrderedDict[tuple[AccountId | None, GroupId], CachedRoster] = OrderedDict()
        self._closed = False
        self.rows = 0
        self.text_bytes = 0

    @property
    def size(self) -> int:
        return len(self._entries)

    def _drop(self, key) -> None:
        previous = self._entries.pop(key, None)
        if previous is not None:
            self.rows -= len(previous.rows)
            self.text_bytes -= previous.text_bytes

    def clear(self) -> None:
        self._entries.clear()
        self.rows = self.text_bytes = 0

    def close(self) -> None:
        self._closed = True
        self.clear()

    def get(self, key: tuple[AccountId | None, GroupId]) -> CachedRoster | None:
        if self._closed:
            return None
        entry = self._entries.get(key)
        if entry is not None:
            if entry.expires <= self._clock():
                self._drop(key)
                return None
            self._entries.move_to_end(key)
        return entry

    def put(
        self, key: tuple[AccountId | None, GroupId], revision: tuple, live: dict, rows: list[dict]
    ) -> None:
        if self._closed:
            return
        self._drop(key)
        if len(rows) > self._max_rows:
            return
        size = 0

        def fits(value: str) -> bool:
            nonlocal size
            remaining = self._max_text_bytes - size
            if len(value) > remaining:
                return False
            size += len(value.encode("utf-8", errors="replace")) + 64
            return size <= self._max_text_bytes

        if not all(fits(str(value)) for value in key):
            return
        for account, name in live.items():
            if not fits(account) or not fits(name):
                return
        for row in rows:
            for name in ("user_id", "nickname", "manual_note"):
                if not fits(row[name]):
                    return
            for name in ("accounts", "former_names", "aliases", "memory_hints"):
                if not all(fits(value) for value in row[name]):
                    return
        frozen = tuple(
            RosterRow(
                row["user_id"],
                row["entity_id"],
                tuple(row["accounts"]),
                row["nickname"],
                tuple(row["former_names"]),
                tuple(row["aliases"]),
                tuple(row["memory_hints"]),
                row["manual_note"],
                row["msg_count"],
            )
            for row in rows
        )
        while self._entries and (
            len(self._entries) >= self._capacity
            or self.rows + len(frozen) > self._max_rows
            or self.text_bytes + size > self._max_text_bytes
        ):
            self._drop(next(iter(self._entries)))
        self._entries[key] = CachedRoster(
            revision, tuple(sorted(live.items())), frozen, self._clock() + self._ttl, size
        )
        self.rows += len(frozen)
        self.text_bytes += size
