"""The typed way in from OneBot v11 events.

Fields were checked against the NapCat and OneBot v11 specifications rather than written
from memory: a group message event carries message_id / group_id / user_id / sender /
message / sub_type, and sender carries nickname, card, role (owner|admin|member), title
and level.

Why this layer exists: what the adapter hands over is a dict whose fields may or may not
be there and whose types vary (group_id is sometimes int, sometimes str). Letting every
consumer getattr its own way spreads "does this field exist" across ten places. It is
answered once here, and everything downstream sees settled types.

It doubles as the anti-corruption layer for everything behind the gateway: live adapter
state is captured here once, and domain and services never learn what OneBot is. Content
parsing remains a separate concern, but it receives detached segment values from this
envelope rather than reading the event again.
"""

from __future__ import annotations

from dataclasses import dataclass, replace
from datetime import datetime
from enum import StrEnum
from typing import Any, Self

from ..domain.archive import AuthorKind
from ..util import defang, now_local, scrub_nul, tz


class Role(StrEnum):
    """Rank inside the QQ group. Unrelated to what the bot allows - running the group is
    not the same as running the bot."""

    OWNER = "owner"
    ADMIN = "admin"
    MEMBER = "member"

    @classmethod
    def parse(cls, raw: Any) -> Role:
        # Sender.role is declared optional: the attribute always exists but may be None,
        # so a getattr default can never fire. Reading None as member is the safe way to
        # be wrong.
        try:
            return cls(str(raw or "member"))
        except ValueError:
            return cls.MEMBER


@dataclass(frozen=True, slots=True)
class Sender:
    user_id: str
    nickname: str = ""
    card: str = ""
    role: Role = Role.MEMBER
    title: str = ""

    @property
    def display(self) -> str:
        """The name the group sees. Group card first - that is how this person asks to be
        addressed here."""
        return (self.card or self.nickname or self.user_id).strip()

    @classmethod
    def parse(cls, raw: dict | None, fallback_id: str = "") -> Self:
        # Names are member-chosen bytes headed for transcripts and the archive's
        # sender payloads, so the system brackets are neutralized at the envelope -
        # a card written to imitate a system tag arrives already harmless.
        raw = raw or {}
        return cls(
            user_id=str(raw.get("user_id") or fallback_id or ""),
            nickname=defang(str(raw.get("nickname") or "")),
            card=defang(str(raw.get("card") or "")),
            role=Role.parse(raw.get("role")),
            title=defang(str(raw.get("title") or "")),
        )


@dataclass(frozen=True, slots=True)
class GroupMessage:
    """One group message, with its fields settled.

    `segments` is passed through untouched to segments.parse_segments - reading the
    content is that layer's job; this one only opens the envelope.
    """

    message_id: str
    group_id: int
    sender: Sender
    segments: list[dict]
    self_id: str
    occurred_at: datetime
    sub_type: str = "normal"
    #: The rendered text, as the prompt and the extractor will read it. Stored alongside
    #: the segments rather than re-derived: parsing rules change, and a stored reading is
    #: what lets an old event be replayed under the rules that were in force then.
    plain_text: str = ""
    #: The adapter lifts the reply segment out, resolves it, and puts it here - so it is
    #: usually absent from `segments`.
    reply_to_message_id: str | None = None
    #: Versioned only for bot-authored outbound segment projections. Inbound and
    #: legacy rows stay at zero and retain their historical reconstruction rules.
    outbound_schema: int = 0
    author_kind: AuthorKind = AuthorKind.MEMBER
    #: Also decided by the adapter: a leading or trailing @bot is removed and flagged.
    to_me: bool = False

    @classmethod
    def from_event(cls, event: Any, self_id: str) -> Self:
        """Capture one adapter event into settled, detached values exactly once."""

        reply = getattr(event, "reply", None)
        when = getattr(event, "time", 0) or 0
        segments = [
            {
                "type": str(getattr(segment, "type", "") or ""),
                "data": dict(getattr(segment, "data", {}) or {}),
            }
            for segment in event.get_message()
        ]
        return cls(
            message_id=str(event.message_id),
            group_id=int(event.group_id),
            sender=Sender.parse(
                getattr(event.sender, "__dict__", None) or dict(event.sender or {}),
                fallback_id=str(event.user_id),
            ),
            segments=scrub_nul(segments),
            self_id=str(self_id),
            occurred_at=(datetime.fromtimestamp(when, tz()) if when else now_local()),
            sub_type=str(getattr(event, "sub_type", "normal") or "normal"),
            reply_to_message_id=(
                str(getattr(reply, "message_id", "") or "") or None
                if reply is not None
                else None
            ),
            to_me=bool(getattr(event, "to_me", False)),
        )

    def with_bot_mention(self, display_name: str) -> Self:
        """Restore the structured self-at an adapter may have removed."""

        shown = defang(display_name).strip() or "机器人"
        segments = [
            {"type": item.get("type"), "data": dict(item.get("data") or {})}
            for item in self.segments
            if isinstance(item, dict)
        ]
        self_mentions = [
            item
            for item in segments
            if item.get("type") == "at"
            and str((item.get("data") or {}).get("qq") or "") == self.self_id
        ]
        if self.to_me and not self_mentions:
            item = {"type": "at", "data": {"qq": self.self_id, "name": shown}}
            segments.insert(0, item)
            self_mentions = [item]
        for item in self_mentions:
            data = item["data"]
            if not data.get("name"):
                data["name"] = shown
        return replace(self, segments=segments)

    def as_payload(self) -> dict:
        """The shape stored in raw_event.payload. The segments are kept verbatim, so a
        change to the parsing rules can be replayed over old events.

        The derived reading is deliberately absent: plain_text lives in its own column,
        because it gets rewritten when a picture is later understood, and an updatable
        field has no place inside a payload the schema calls append-only."""
        return {
            "message_id": self.message_id,
            "sub_type": self.sub_type,
            "sender": {
                "user_id": self.sender.user_id,
                "nickname": self.sender.nickname,
                "card": self.sender.card,
                "role": self.sender.role.value,
            },
            "segments": self.segments,
            "reply_to": self.reply_to_message_id,
            "to_me": self.to_me,
            "outbound_schema": self.outbound_schema,
            "author_kind": self.author_kind.value,
            "self_id": self.self_id,
        }
