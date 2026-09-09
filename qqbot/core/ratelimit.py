"""Sliding-window rate limiting and inbound dedup.

The window backs the per-group image-describe cap (media.py); DedupSet drops
the adapter's replayed events.
"""

from __future__ import annotations

import time
from collections import deque


class SlidingWindow:
    def __init__(self, limit: int, window_sec: float = 60.0) -> None:
        self.limit = limit
        self.window = window_sec
        self._hits: deque[float] = deque()

    def _trim(self, now: float) -> None:
        while self._hits and now - self._hits[0] > self.window:
            self._hits.popleft()

    def take(self, limit: int | None = None) -> bool:
        """Consume one slot if quota remains."""
        now = time.monotonic()
        self._trim(now)
        if len(self._hits) >= (self.limit if limit is None else limit):
            return False
        self._hits.append(now)
        return True


class DedupSet:
    """Deduplicate by msg_id, evicting entries once the TTL passes."""

    def __init__(self, ttl_sec: float) -> None:
        self.ttl = ttl_sec
        self._seen: dict[str, float] = {}
        self._order: deque[str] = deque()

    def seen(self, key: str) -> bool:
        now = time.monotonic()
        while self._order and now - self._seen.get(self._order[0], 0) > self.ttl:
            old = self._order.popleft()
            self._seen.pop(old, None)
        if key in self._seen:
            return True
        self._seen[key] = now
        self._order.append(key)
        return False

    def discard(self, key: str) -> None:
        """Take a key back, so a replay gets another chance.

        For the caller that marked a message seen and then failed before doing
        anything with it: leaving the mark would swallow the adapter's replay, and
        the message would be neither archived nor answered."""
        self._seen.pop(key, None)
