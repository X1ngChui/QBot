"""NoneBot-only OneBot event models kept outside normalization and core runtime."""

from typing import Literal

from nonebot.adapters.onebot.v11 import GroupMessageEvent


class NapCatGroupMessageSentEvent(GroupMessageEvent):
    """NapCat's self-message extension as an ordinary typed group message."""

    post_type: Literal["message_sent"]
