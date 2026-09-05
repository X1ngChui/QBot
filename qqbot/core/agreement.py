"""The user agreement gate.

A member who has not accepted the agreement is not replied to: the reply path
holds at dispatch, and what they get instead - at most once per cooldown - is
the agreement text and how to accept it. Everything else stays untouched:
their messages are read and archived as always, and the command surface keeps
working, which is how /agree can reach them. Owners are exempt.

Acceptance is per group, account and version: each group is its own audience,
and an upgraded agreement (the vN on the file's title line) voids every older
acceptance, so the gate walks everyone through consent again. The in-memory
set caches only "yes" answers - a "no" must stay re-checkable the moment
/agree lands.
"""

from __future__ import annotations

import os
import re
import time
from pathlib import Path

from ..db import repo

#: (group, account, version) triples known to have accepted - yes answers only.
_AGREED: set[tuple[str, str, int]] = set()

#: When each (group, account) was last shown the agreement (monotonic
#: seconds). A member who keeps addressing the bot sees the text once per
#: window, not once per message - the gate must not become spam.
_PROMPTED: dict[tuple[str, str], float] = {}
PROMPT_EVERY_SEC = 600.0

_FALLBACK = "（用户协议内容暂缺，请联系拥有者。）"


def _body() -> str:
    """The file as it currently stands - read fresh so the owner edits it
    without needing a /reload."""
    path = Path(os.getenv("CONFIG_DIR", "config")) / "agreement.txt"
    try:
        return path.read_text(encoding="utf-8").strip() or _FALLBACK
    except OSError:
        return _FALLBACK


def text() -> str:
    """The agreement body plus the fixed how-to-accept line, appended in code
    so an edited body can never lose it."""
    return _body() + "\n\n同意请发送：/agree"


def version() -> int:
    """The agreement's version: the vN on its first line, 1 when unmarked.

    Bumping it is how the owner voids old acceptances - consent is stored
    against the version it was given for, so everyone on an older number is
    walked through the agreement again.
    """
    first = _body().splitlines()[0]
    m = re.search(r"[vV](\d+)", first)
    return int(m.group(1)) if m else 1


async def ok(group_id: str, user_id: str) -> bool:
    """Whether this account has accepted the current agreement in this group."""
    v = version()
    key = (str(group_id), user_id, v)
    if key in _AGREED:
        return True
    if await repo.has_agreed(int(group_id), user_id, v):
        _AGREED.add(key)
        return True
    return False


async def accept(group_id: str, user_id: str) -> bool:
    """Record acceptance of the current version; True when it changed
    anything - a first acceptance or an upgrade from an older version."""
    v = version()
    fresh = await repo.record_agreement(int(group_id), user_id, v)
    _AGREED.add((str(group_id), user_id, v))
    return fresh


def should_prompt(group_id: str, user_id: str) -> bool:
    """Whether to show the agreement now, marking the moment when yes."""
    key = (str(group_id), user_id)
    now = time.monotonic()
    last = _PROMPTED.get(key)
    if last is not None and now - last < PROMPT_EVERY_SEC:
        return False
    _PROMPTED[key] = now
    return True
