"""What this package needs from a bot, stated as a type.

`bot` is threaded through twenty-odd signatures here - the media path, the member
directory, the memory path, the schedulers. What it actually is depends on who is
calling: NoneBot's OneBot v11 Bot in production, a hand-written stand-in in every test,
and None in the places that treat it as optional.

A Protocol rather than importing the adapter's Bot, because that is the dependency
direction that matters: core must not import a chat framework to describe what it needs
from one, and the stand-ins in the tests are not subclasses of anything. Structural
typing is exactly the shape of the problem - two attributes, checked by a type checker
against every fake as well as the real one.
"""

from __future__ import annotations

from typing import Any, Protocol, runtime_checkable


@runtime_checkable
class BotApi(Protocol):
    """The three things anything here ever asks of a bot."""

    #: The bot's own account id. Read as a string everywhere; the adapter types it int.
    self_id: Any

    async def call_api(self, api: str, **params: Any) -> Any:
        """Call one OneBot endpoint and hand back its `data` field."""
        ...

    async def send_group_msg(self, *, group_id: int, message: Any) -> Any:
        """Say something in a group. The adapter resolves this dynamically, but the
        engine calls it by name - so a fake that omits it fails at send time, not
        at construction."""
        ...
