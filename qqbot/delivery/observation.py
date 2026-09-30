"""Short-lived, group-scoped observation of a bot's admitted own messages."""

from __future__ import annotations

from qqbot.domain.ids import AccountId
import asyncio
import time
from collections import OrderedDict

from qqbot.domain.ids import GroupId
from qqbot.domain.ids import MessageId
from qqbot.conversation.state import ChatMsg

Key = tuple[AccountId, GroupId, MessageId]
ECHO_TIMEOUT_SEC = 8


class SelfEcho:
    """Publish only after archive admission and live-window projection are complete."""

    def __init__(self) -> None:
        self._recent: OrderedDict[Key, tuple[float, ChatMsg]] = OrderedDict()
        self._waiters: dict[Key, set[asyncio.Future[ChatMsg]]] = {}
        self._closed = False

    def publish(self, self_id: AccountId, group_id: GroupId, msg: ChatMsg) -> None:
        if self._closed or not msg.is_bot or msg.user_id != self_id:
            return
        key = (self_id, group_id, msg.msg_id)
        self._recent[key] = (time.monotonic(), msg)
        self._recent.move_to_end(key)
        while len(self._recent) > 256:
            self._recent.popitem(last=False)
        for waiter in self._waiters.get(key, ()):
            if not waiter.done():
                waiter.set_result(msg)

    async def wait(
        self, self_id: AccountId, group_id: GroupId, message_id: MessageId, *, timeout: float
    ) -> ChatMsg | None:
        if self._closed:
            return None
        key = (self_id, group_id, message_id)
        future: asyncio.Future[ChatMsg] = asyncio.get_running_loop().create_future()
        self._waiters.setdefault(key, set()).add(future)
        try:
            cached = self._recent.get(key)
            if cached is not None and time.monotonic() - cached[0] <= 60:
                return cached[1]
            try:
                return await asyncio.wait_for(future, timeout)
            except TimeoutError:
                return None
        finally:
            subscribers = self._waiters.get(key)
            if subscribers is not None:
                subscribers.discard(future)
                if not subscribers:
                    del self._waiters[key]
            if not future.done():
                future.cancel()

    def close(self) -> None:
        self._closed = True
        self._recent.clear()
        for subscribers in self._waiters.values():
            for waiter in subscribers:
                if not waiter.done():
                    waiter.cancel()
        self._waiters.clear()
