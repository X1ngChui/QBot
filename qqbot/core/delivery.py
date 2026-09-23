"""Ordered, group-scoped delivery of current QQ message segments."""

from __future__ import annotations

import asyncio
import logging
from collections.abc import Sequence
from dataclasses import dataclass

from ..domain.ids import GroupId, MessageId
from ..util import why
from .botapi import BotApi
from .outbound import (
    AtSegment,
    SendSegment,
    TextSegment,
    reply_target,
    to_onebot,
    without_replies,
)

log = logging.getLogger("qqbot.delivery")


@dataclass(frozen=True, slots=True)
class DeliveredMessage:
    """One acknowledged protocol send after any reply-segment fallback."""

    message_id: MessageId | None
    segments: tuple[SendSegment, ...]
    reply_to: MessageId | None


class GroupDelivery:
    """Serialize batches within each group while leaving generation concurrent."""

    def __init__(self) -> None:
        self._locks: dict[GroupId, asyncio.Lock] = {}

    def _lock(self, group_id: GroupId) -> asyncio.Lock:
        return self._locks.setdefault(group_id, asyncio.Lock())

    async def _send_one(
        self,
        bot: BotApi,
        *,
        group_id: GroupId,
        segments: tuple[SendSegment, ...],
    ) -> DeliveredMessage | None:
        reply_id = reply_target(segments)
        sent_segments = segments
        try:
            sent = await bot.send_group_msg(
                group_id=group_id.to_onebot(),
                message=[to_onebot(segment) for segment in segments],
            )
        except Exception as exc:
            # A quoted message can be recalled between generation and delivery. Retry
            # only the adapter's known refusal and preserve all other segment order.
            if not reply_id or type(exc).__name__ != "ActionFailed":
                log.warning("group %s: send failed: %s", group_id, why(exc))
                return None
            log.warning(
                "group %s: send with a reply segment failed (%s), retrying without it",
                group_id,
                why(exc),
            )
            reply_id = None
            sent_segments = without_replies(segments)
            try:
                sent = await bot.send_group_msg(
                    group_id=group_id.to_onebot(),
                    message=[to_onebot(segment) for segment in sent_segments],
                )
            except Exception as retry_exc:
                log.warning("group %s: send failed: %s", group_id, why(retry_exc))
                return None

        raw_message_id = str((sent or {}).get("message_id") or "")
        return DeliveredMessage(
            MessageId(raw_message_id) if raw_message_id else None,
            sent_segments,
            MessageId(reply_id) if reply_id else None,
        )

    async def deliver(
        self,
        bot: BotApi,
        *,
        group_id: GroupId,
        messages: Sequence[Sequence[SendSegment]],
    ) -> tuple[DeliveredMessage, ...]:
        """Deliver a batch in order, stopping after its first failed message."""

        batch = tuple(tuple(message) for message in messages)
        delivered: list[DeliveredMessage] = []
        async with self._lock(group_id):
            for index, segments in enumerate(batch):
                item = await self._send_one(
                    bot,
                    group_id=group_id,
                    segments=segments,
                )
                if item is None:
                    log.warning(
                        "group %s: reply batch stopped after %d/%d messages",
                        group_id,
                        len(delivered),
                        len(batch),
                    )
                    break
                delivered.append(item)
                log.info(
                    "group %s: delivered %d/%d (%d text chars, %d segments, %d @, %s)",
                    group_id,
                    index + 1,
                    len(batch),
                    sum(
                        len(segment.text)
                        for segment in item.segments
                        if isinstance(segment, TextSegment)
                    ),
                    len(item.segments),
                    sum(isinstance(segment, AtSegment) for segment in item.segments),
                    "replying" if item.reply_to else "not replying",
                )
        return tuple(delivered)
