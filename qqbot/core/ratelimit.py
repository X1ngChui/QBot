"""Sliding-window rate limiting for media admission."""

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
