"""A small in-memory ring of recent warnings/errors, so the daily report can say what
went wrong without anyone reading log files."""

from __future__ import annotations

import logging
from collections import deque

from ..util import now_local

_RING: deque[tuple[str, str, str]] = deque()


class RingHandler(logging.Handler):
    def __init__(self, *, level: int, message_chars: int) -> None:
        super().__init__(level=level)
        self._message_chars = message_chars

    def emit(self, record: logging.LogRecord) -> None:
        try:
            _RING.append(
                (
                    now_local().strftime("%m-%d %H:%M"),
                    record.name,
                    record.getMessage()[: self._message_chars],
                )
            )
        except Exception:
            pass


def install(
    level: int = logging.WARNING,
    *,
    entries: int,
    message_chars: int,
) -> None:
    global _RING
    _RING = deque(maxlen=entries)
    handler = RingHandler(level=level, message_chars=message_chars)
    for logger_name in ("qqbot", "apscheduler"):
        logger = logging.getLogger(logger_name)
        for existing in tuple(logger.handlers):
            if isinstance(existing, RingHandler):
                logger.removeHandler(existing)
        logger.addHandler(handler)


def recent(limit: int) -> list[tuple[str, str, str]]:
    return list(_RING)[-limit:]


def count() -> int:
    return len(_RING)


def clear() -> None:
    _RING.clear()
