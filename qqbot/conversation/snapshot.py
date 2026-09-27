"""Detached model-visible values and a post-admission continuation cursor."""

from __future__ import annotations

from dataclasses import dataclass, fields
from datetime import datetime
from types import MappingProxyType
from collections.abc import Mapping, Sequence
import uuid

from qqbot.conversation import prompt
from qqbot.conversation.state import ChatMsg, TranscriptRendering
from qqbot.delivery.segments import HistoricalSegment
from qqbot.domain.ids import AccountId, MessageId
from qqbot.gateway.segments import ImageRef

CONTINUATION_MESSAGES = 50


@dataclass(frozen=True, slots=True)
class ImageSnapshot:
    key: str | None
    url: str | None
    file: str | None
    size: int | None

    @classmethod
    def capture(cls, ref: ImageRef) -> ImageSnapshot:
        return cls(ref.key, ref.url, ref.file, ref.size)


@dataclass(frozen=True, slots=True)
class MessageSnapshot(TranscriptRendering):
    msg_id: MessageId
    user_id: AccountId
    nickname: str
    text: str
    ts: datetime
    raw_event_id: uuid.UUID | None
    is_bot: bool
    is_owner: bool
    reply_to: MessageId | None
    mentions: tuple[tuple[AccountId, str], ...]
    at: tuple[tuple[AccountId, str], ...]
    outbound: tuple[HistoricalSegment, ...]
    image_refs: tuple[ImageSnapshot, ...]
    arrival_seq: int

    @classmethod
    def capture(cls, message: ChatMsg) -> MessageSnapshot:
        values = {field.name: getattr(message, field.name) for field in fields(cls)}
        values.update(
            mentions=tuple(message.mentions),
            at=tuple(message.at),
            outbound=tuple(message.outbound),
            image_refs=tuple(ImageSnapshot.capture(ref) for ref in message.image_refs),
        )
        return cls(**values)


@dataclass(frozen=True, slots=True)
class PromptSnapshot:
    history: tuple[MessageSnapshot, ...]
    current: MessageSnapshot | None
    cursor: int
    numbers: Mapping[str, int]
    quotes: Mapping[str, str]
    image_numbers: Mapping[str, tuple[int, ...]]
    pictures: Mapping[int, tuple[MessageSnapshot, int]]

    @property
    def shown(self) -> tuple[MessageSnapshot, ...]:
        return self.history + ((self.current,) if self.current is not None else ())

    @classmethod
    def capture(cls, history: Sequence[ChatMsg], current: ChatMsg | None, *, cursor: int):
        frozen = tuple(MessageSnapshot.capture(message) for message in history)
        asked = MessageSnapshot.capture(current) if current is not None else None
        shown = frozen + ((asked,) if asked is not None else ())
        numbers, quotes = prompt.numbered(shown)
        images, pictures = prompt.numbered_images(shown)
        return cls(
            frozen,
            asked,
            cursor,
            MappingProxyType(numbers),
            MappingProxyType(quotes),
            MappingProxyType({key: tuple(value) for key, value in images.items()}),
            MappingProxyType(pictures),
        )


@dataclass(frozen=True, slots=True)
class ContinuationSnapshot:
    messages: tuple[MessageSnapshot, ...]
    cursor: int
    gap: bool

    @classmethod
    def capture(cls, recent: Sequence[ChatMsg], cursor: int, *, own: ChatMsg | None = None):
        pending = [message for message in recent if message.arrival_seq > cursor]
        gap = bool(pending and pending[0].arrival_seq > cursor + 1)
        if len(pending) > CONTINUATION_MESSAGES:
            pending = pending[-CONTINUATION_MESSAGES:]
            gap = True
        # An acknowledged send remains representable even if live traffic evicted it.
        if (
            own is not None
            and own.arrival_seq > cursor
            and all(message.msg_id != own.msg_id for message in pending)
        ):
            pending.append(own)
            pending.sort(key=lambda message: message.arrival_seq)
        return cls(
            tuple(MessageSnapshot.capture(message) for message in pending),
            max((message.arrival_seq for message in pending), default=cursor),
            gap,
        )
