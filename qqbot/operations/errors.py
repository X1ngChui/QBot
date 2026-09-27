"""Runtime-owned bounded diagnostics, attached only during its active lifetime."""

from __future__ import annotations

import logging
from collections import deque

from qqbot.clock import Clock


class ErrorRing(logging.Handler):
    def __init__(
        self, *, clock: Clock, entries: int, message_chars: int, level: int = logging.WARNING
    ) -> None:
        if entries < 1 or message_chars < 1:
            raise ValueError("diagnostic capacities must be positive")
        super().__init__(level=level)
        self._clock = clock
        self._message_chars = message_chars
        self._ring: deque[tuple[str, str, str]] = deque(maxlen=entries)
        self._loggers: tuple[logging.Logger, ...] = ()

    def emit(self, record: logging.LogRecord) -> None:
        if self._closed:
            return
        try:
            self._ring.append(
                (
                    self._clock.format(self._clock.now()),
                    record.name,
                    record.getMessage()[: self._message_chars],
                )
            )
        except Exception:
            pass

    def install(self) -> None:
        if self._closed:
            raise RuntimeError("diagnostic ring is closed")
        if self._loggers:
            return
        self._loggers = tuple(logging.getLogger(name) for name in ("qqbot", "apscheduler"))
        for logger in self._loggers:
            logger.addHandler(self)

    def close(self) -> None:
        for logger in self._loggers:
            logger.removeHandler(self)
        self._loggers = ()
        super().close()

    def recent(self, limit: int) -> list[tuple[str, str, str]]:
        return list(self._ring)[-limit:] if limit > 0 else []

    def count(self) -> int:
        return len(self._ring)

    def clear(self) -> None:
        self._ring.clear()
