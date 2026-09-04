"""The user agreement gate.

A member who has not accepted the agreement is not replied to: the reply path
holds at dispatch, and what they get instead - at most once per cooldown - is
the agreement text and how to accept it. Everything else stays untouched:
their messages are read and archived as always, and the command surface keeps
working, which is how /agree can reach them. Owners are exempt.

Acceptance is per account and permanent: one row in user_agreement. The
in-memory set caches only "yes" answers - a "no" must stay re-checkable the
moment /agree lands.
"""

from __future__ import annotations

import os
import time
from pathlib import Path

from ..db import repo

#: Accounts known to have accepted - a cache of yes answers only.
_AGREED: set[str] = set()

#: When each account was last shown the agreement (monotonic seconds). A
#: member who keeps addressing the bot sees the text once per window, not once
#: per message - the gate must not become spam.
_PROMPTED: dict[str, float] = {}
PROMPT_EVERY_SEC = 600.0

_FALLBACK = "（用户协议内容暂缺，请联系拥有者。）"


def text() -> str:
    """The agreement body plus the fixed how-to-accept line.

    Read from config/agreement.txt on every prompt: prompts are rare, and the
    owner edits the file without needing a /reload. The accept instruction is
    appended in code so an edited body can never lose it.
    """
    path = Path(os.getenv("CONFIG_DIR", "config")) / "agreement.txt"
    try:
        body = path.read_text(encoding="utf-8").strip() or _FALLBACK
    except OSError:
        body = _FALLBACK
    return body + "\n\n同意请发送：/agree"


async def ok(user_id: str) -> bool:
    """Whether this account has accepted the agreement."""
    if user_id in _AGREED:
        return True
    if await repo.has_agreed(user_id):
        _AGREED.add(user_id)
        return True
    return False


async def accept(user_id: str) -> bool:
    """Record acceptance; True when this was the first time."""
    fresh = await repo.record_agreement(user_id)
    _AGREED.add(user_id)
    return fresh


def should_prompt(user_id: str) -> bool:
    """Whether to show the agreement now, marking the moment when yes."""
    now = time.monotonic()
    last = _PROMPTED.get(user_id)
    if last is not None and now - last < PROMPT_EVERY_SEC:
        return False
    _PROMPTED[user_id] = now
    return True
