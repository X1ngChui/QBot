"""Normalized inbound events shared by messages, self-observation and notices."""

from __future__ import annotations

from dataclasses import dataclass
from datetime import datetime
from enum import StrEnum
from typing import Any, Literal

from .archive import AuthorKind
from .ids import AccountId, GroupId, MessageId


class GroupRole(StrEnum):
    """Platform group rank, unrelated to command authorization."""

    OWNER = "owner"
    ADMIN = "admin"
    MEMBER = "member"

    @classmethod
    def parse(cls, raw: Any) -> GroupRole:
        try:
            return cls(str(raw or "member"))
        except ValueError:
            return cls.MEMBER


@dataclass(frozen=True, slots=True)
class InboundSender:
    user_id: AccountId
    nickname: str = ""
    card: str = ""
    role: GroupRole = GroupRole.MEMBER
    title: str = ""

    @property
    def display(self) -> str:
        return (self.card or self.nickname or self.user_id).strip()


@dataclass(frozen=True, slots=True)
class InboundEvent:
    """One detached event after adapter normalization and before admission."""

    message_id: MessageId
    group_id: GroupId
    sender: InboundSender
    segments: list[dict]
    self_id: AccountId
    occurred_at: datetime
    event_type: Literal["message", "notice"] = "message"
    sub_type: str = "normal"
    plain_text: str = ""
    #: Text segments exactly as typed, before adapter mention restoration or rich rendering.
    typed_text: str = ""
    reply_to_message_id: MessageId | None = None
    outbound_schema: int = 0
    author_kind: AuthorKind = AuthorKind.MEMBER
    to_me: bool = False
    notice_type: str = ""

    def as_payload(self) -> dict:
        """Return the code-owned archive payload for this normalized event."""

        payload = {
            "message_id": self.message_id,
            "sub_type": self.sub_type,
            "sender": {
                "user_id": self.sender.user_id,
                "nickname": self.sender.nickname,
                "card": self.sender.card,
                "role": self.sender.role.value,
            },
            "segments": self.segments,
            "typed_text": self.typed_text,
            "reply_to": self.reply_to_message_id,
            "to_me": self.to_me,
            "outbound_schema": self.outbound_schema,
            "author_kind": self.author_kind.value,
            "self_id": self.self_id,
        }
        if self.notice_type:
            payload["notice_type"] = self.notice_type
        return payload
