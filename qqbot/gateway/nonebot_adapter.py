"""NoneBot-only OneBot event models kept outside normalization and core runtime."""

from typing import Any, Literal

from nonebot.adapters.onebot.v11 import Bot, GroupMessageEvent

from qqbot.domain.ids import AccountId


class NapCatGroupMessageSentEvent(GroupMessageEvent):
    """NapCat's self-message extension as an ordinary typed group message."""

    post_type: Literal["message_sent"]


class OneBotClient:
    """Translate framework identity once while forwarding protocol I/O unchanged."""

    def __init__(self, bot: Bot) -> None:
        self._bot = bot
        self.self_id = AccountId(bot.self_id)

    async def call_api(self, api: str, **params: Any) -> Any:
        return await self._bot.call_api(api, **params)

    async def send_group_msg(self, *, group_id: int, message: Any) -> Any:
        return await self._bot.send_group_msg(group_id=group_id, message=message)
