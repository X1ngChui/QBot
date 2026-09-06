"""The user agreement gate.

A member who has not accepted the agreement is not replied to: the reply path
holds at dispatch, and what they get instead - at most once per cooldown - is
a one-line pointer at /terms (the full text on demand) and /agree. Their
messages are still read and archived as always, but of the commands only
those two answer before consent. Owners are exempt.

Acceptance is per group, account and version: each group is its own audience,
and a bumped `agreement.version` in the config voids every older acceptance,
so the gate walks everyone through consent again. The version and the path of
the text file are mandatory config (settings.yaml `agreement:`); the file is
read at load, so /reload swaps text and version together and a deployment
without an agreement fails at load - there is no placeholder state. The
in-memory set caches only "yes" answers - a "no" must stay re-checkable the
moment /agree lands.
"""

from __future__ import annotations

import time

from ..db import repo
from ..settings import config

#: (group, account, version) triples known to have accepted - yes answers only.
_AGREED: set[tuple[str, str, int]] = set()

#: When each (group, account) was last shown the agreement (monotonic
#: seconds). A member who keeps addressing the bot sees the pointer once per
#: window, not once per message - the gate must not become spam.
_PROMPTED: dict[tuple[str, str], float] = {}
PROMPT_EVERY_SEC = 600.0

#: What an unconsenting member is told instead of a reply - one line, because
#: the full agreement re-sent on every cooldown reads as spam. /terms serves
#: the full text on demand; both commands answer before consent.
POINTER = "使用机器人前，请先同意用户协议：发送 /terms 查看全文，发送 /agree 表示同意。"


def text() -> str:
    """The agreement body plus the fixed how-to-accept line, appended in code
    so an edited body can never lose it."""
    return config().agreement_text + "\n\n同意请发送：/agree"


def version() -> int:
    """The agreement's configured version. Bumping it is how the owner voids
    old acceptances - consent is stored against the version it was given for,
    so everyone on an older number is walked through the agreement again."""
    return config().default.agreement.version


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
    # An accepted member never sees the pointer again; their cooldown entry
    # is dead weight from here on.
    _PROMPTED.pop((str(group_id), user_id), None)
    return fresh


def should_prompt(group_id: str, user_id: str) -> bool:
    """Whether to show the pointer now, marking the moment when yes."""
    key = (str(group_id), user_id)
    now = time.monotonic()
    last = _PROMPTED.get(key)
    if last is not None and now - last < PROMPT_EVERY_SEC:
        return False
    _PROMPTED[key] = now
    return True
