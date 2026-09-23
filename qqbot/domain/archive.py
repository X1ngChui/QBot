"""Canonical immutable messages read from the versioned archive schema."""

from __future__ import annotations

import uuid
from collections.abc import Mapping
from dataclasses import dataclass
from datetime import datetime
from enum import StrEnum
from types import MappingProxyType
from typing import Any, Literal

from ..util import defang, display_name
from .ids import AccountId, GroupId, MessageId

ARCHIVE_SCHEMA = 1


class AuthorKind(StrEnum):
    """Who authored an archived message, independent of account identity."""

    MEMBER = "member"
    BOT = "bot"


class ArchiveFormatError(ValueError):
    """A row claims the current archive version but violates its canonical shape."""


def _freeze(value: Any) -> Any:
    if isinstance(value, Mapping):
        return MappingProxyType({str(key): _freeze(item) for key, item in value.items()})
    if isinstance(value, list | tuple):
        return tuple(_freeze(item) for item in value)
    return value


def _thaw(value: Any) -> Any:
    if isinstance(value, Mapping):
        return {key: _thaw(item) for key, item in value.items()}
    if isinstance(value, tuple):
        return [_thaw(item) for item in value]
    return value


def _required_mapping(value: Any, label: str) -> Mapping[str, Any]:
    if not isinstance(value, Mapping):
        raise ArchiveFormatError(f"{label} must be an object")
    return value


@dataclass(frozen=True, slots=True)
class ArchivedSender:
    account_id: AccountId
    nickname: str
    card: str
    role: str

    @property
    def display_name(self) -> str:
        return display_name(self.card, self.nickname, "成员")


@dataclass(frozen=True, slots=True)
class ArchivedMessage:
    """One schema-v1 archive row, validated at the SQL-row boundary."""

    raw_event_id: uuid.UUID
    message_id: MessageId
    group_id: GroupId
    event_type: Literal["message", "notice"]
    notice_type: str
    author_kind: AuthorKind
    sender: ArchivedSender
    self_id: AccountId | None
    occurred_at: datetime
    created_at: datetime
    text: str
    typed_text: str
    reply_to: MessageId | None
    to_me: bool
    segments: tuple[Mapping[str, Any], ...]

    @classmethod
    def from_row(cls, row: Mapping[str, Any]) -> ArchivedMessage:
        try:
            version = int(row["archive_schema"])
        except (KeyError, TypeError, ValueError) as exc:
            raise ArchiveFormatError("archive_schema is missing or invalid") from exc
        if version != ARCHIVE_SCHEMA:
            raise ArchiveFormatError(
                f"unsupported archive schema {version}; expected {ARCHIVE_SCHEMA}"
            )

        payload = _required_mapping(row["payload"], "payload")
        sender_payload = _required_mapping(payload.get("sender"), "payload.sender")
        raw_segments = payload.get("segments")
        if not isinstance(raw_segments, list | tuple):
            raise ArchiveFormatError("payload.segments must be an array")
        segments: list[Mapping[str, Any]] = []
        for index, item in enumerate(raw_segments):
            segment = _required_mapping(item, f"payload.segments[{index}]")
            kind = segment.get("type")
            data = segment.get("data")
            if not isinstance(kind, str) or not isinstance(data, Mapping):
                raise ArchiveFormatError(
                    f"payload.segments[{index}] needs string type and object data"
                )
            segments.append(_freeze({"type": kind, "data": data}))

        event_type = str(row["event_type"])
        if event_type not in ("message", "notice"):
            raise ArchiveFormatError(f"unsupported archive event type {event_type!r}")
        try:
            author = AuthorKind(str(payload["author_kind"]))
            group_id = GroupId(row["group_id"])
            sender_id = AccountId(sender_payload.get("user_id"))
            message_id = MessageId(payload.get("message_id") or row["platform_event_id"])
        except (KeyError, TypeError, ValueError) as exc:
            raise ArchiveFormatError("archive identity fields are invalid") from exc

        raw_self = str(payload.get("self_id") or "").strip()
        raw_reply = str(payload.get("reply_to") or "").strip()
        return cls(
            raw_event_id=row["id"],
            message_id=message_id,
            group_id=group_id,
            event_type=event_type,
            notice_type=str(payload.get("notice_type") or ""),
            author_kind=author,
            sender=ArchivedSender(
                account_id=sender_id,
                nickname=defang(str(sender_payload.get("nickname") or "")),
                card=defang(str(sender_payload.get("card") or "")),
                role=str(sender_payload.get("role") or "member"),
            ),
            self_id=AccountId(raw_self) if raw_self else None,
            occurred_at=row["occurred_at"],
            created_at=row["created_at"],
            text=str(row["plain_text"] or ""),
            typed_text=defang(str(payload.get("typed_text") or "")),
            reply_to=MessageId(raw_reply) if raw_reply else None,
            to_me=bool(payload.get("to_me", False)),
            segments=tuple(segments),
        )

    def onebot_segments(self) -> list[dict]:
        """Return detached mutable segment dictionaries for existing parsers."""

        return [_thaw(segment) for segment in self.segments]

    @property
    def mentions(self) -> tuple[tuple[AccountId, str], ...]:
        """Structured at targets in protocol order, excluding at-all."""

        mentions: list[tuple[AccountId, str]] = []
        for segment in self.segments:
            if segment.get("type") != "at":
                continue
            data = segment.get("data")
            if not isinstance(data, Mapping):
                continue
            raw_account = str(data.get("qq") or "")
            if not raw_account or raw_account == "all":
                continue
            mentions.append(
                (
                    AccountId(raw_account),
                    defang(str(data.get("name") or "")).strip(),
                )
            )
        return tuple(mentions)
