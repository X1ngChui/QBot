"""OneBot v11 normalization into detached application ingress values."""

from __future__ import annotations

from dataclasses import replace
from datetime import datetime
from typing import Any, Self

from ..domain.archive import AuthorKind
from ..domain.ids import AccountId, GroupId, MessageId
from ..domain.ingress import GroupRole, InboundEvent, InboundSender
from ..util import defang, now_local, scrub_nul, tz

Role = GroupRole
Sender = InboundSender


def _sender(raw: dict | None, fallback_id: object = "") -> Sender:
    """Normalize detached OneBot sender metadata."""

    raw = raw or {}
    return Sender(
        user_id=AccountId(raw.get("user_id") or fallback_id),
        nickname=defang(str(raw.get("nickname") or "")),
        card=defang(str(raw.get("card") or "")),
        role=Role.parse(raw.get("role")),
        title=defang(str(raw.get("title") or "")),
    )


def _typed_text(segments: list[dict]) -> str:
    """Keep only top-level text exactly as the sender typed it."""

    typed: list[str] = []
    for segment in segments:
        if segment.get("type") != "text":
            continue
        data = segment.get("data")
        if not isinstance(data, dict):
            continue
        text = defang(str(data.get("text") or "")).strip()
        if text:
            typed.append(text)
    return " ".join(typed)


class GroupMessage(InboundEvent):
    """Compatibility name for a normalized OneBot group-message event."""

    @classmethod
    def from_event(cls, event: Any, self_id: str) -> Self:
        """Capture one adapter event into settled, detached values exactly once."""

        reply = getattr(event, "reply", None)
        when = getattr(event, "time", 0) or 0
        normalized_self = AccountId(self_id)
        author = AccountId(event.user_id)
        author_kind = AuthorKind.BOT if author == normalized_self else AuthorKind.MEMBER
        sender = _sender(
            getattr(event.sender, "__dict__", None) or dict(event.sender or {}),
            fallback_id=author,
        )
        # Top-level authorship is authoritative; nested sender metadata is descriptive.
        sender = replace(sender, user_id=author)
        segments = scrub_nul(
            [
                {
                    "type": str(getattr(segment, "type", "") or ""),
                    "data": dict(getattr(segment, "data", {}) or {}),
                }
                for segment in event.get_message()
            ]
        )
        return cls(
            message_id=MessageId(event.message_id),
            group_id=GroupId(event.group_id),
            sender=sender,
            segments=segments,
            self_id=normalized_self,
            occurred_at=(datetime.fromtimestamp(when, tz()) if when else now_local()),
            sub_type=str(getattr(event, "sub_type", "normal") or "normal"),
            typed_text=_typed_text(segments),
            reply_to_message_id=(
                MessageId(reply.message_id)
                if reply is not None and str(getattr(reply, "message_id", "") or "").strip()
                else None
            ),
            outbound_schema=1 if author_kind is AuthorKind.BOT else 0,
            author_kind=author_kind,
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


def notice_from_event(
    event: Any,
    *,
    self_id: object,
    plain_text: str,
) -> InboundEvent | None:
    """Normalize one supported notice envelope after its text projection is chosen."""

    try:
        group_id = GroupId(getattr(event, "group_id", ""))
        actor = AccountId(getattr(event, "user_id", ""))
    except ValueError:
        return None
    when = int(getattr(event, "time", 0) or 0)
    return InboundEvent(
        message_id=notice_message_id(event, group_id, actor),
        group_id=group_id,
        sender=Sender(user_id=actor),
        segments=[{"type": "text", "data": {"text": plain_text}}],
        self_id=AccountId(self_id),
        occurred_at=(datetime.fromtimestamp(when, tz()) if when else now_local()),
        event_type="notice",
        sub_type=str(getattr(event, "sub_type", "") or "notice"),
        notice_type=str(getattr(event, "notice_type", "") or ""),
        plain_text=plain_text,
    )


def notice_message_id(event: Any, group_id: GroupId, actor: AccountId) -> MessageId:
    """Build the stable platform key for a supported notice at the adapter boundary."""

    when = int(getattr(event, "time", 0) or 0)
    mark = str(getattr(event, "message_id", "") or getattr(event, "target_id", "") or "")
    base = f"notice-{getattr(event, 'notice_type', '')}-{group_id}-{actor}-{when}"
    return MessageId(base + (f"-{mark}" if mark else ""))
