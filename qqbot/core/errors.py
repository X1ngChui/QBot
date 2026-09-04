"""A small in-memory ring of recent warnings/errors, so the daily report can say what
went wrong without anyone reading log files."""

from __future__ import annotations

import logging
from collections import deque

from ..util import now_local

_RING: deque[tuple[str, str, str]] = deque(maxlen=200)


class RingHandler(logging.Handler):
    def emit(self, record: logging.LogRecord) -> None:
        try:
            _RING.append(
                (now_local().strftime("%m-%d %H:%M"), record.name, record.getMessage()[:300])
            )
        except Exception:
            pass


def install(level: int = logging.WARNING) -> None:
    handler = RingHandler(level=level)
    logging.getLogger("qqbot").addHandler(handler)
    # A job function that raises escapes to APScheduler's own logger, not qqbot's.
    # Unhooked, a failing backup or drain would never reach the daily report - the
    # one place the ops loop actually reads.
    logging.getLogger("apscheduler").addHandler(handler)


def recent(limit: int = 10) -> list[tuple[str, str, str]]:
    return list(_RING)[-limit:]


def count() -> int:
    return len(_RING)


def clear() -> None:
    _RING.clear()
