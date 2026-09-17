"""Typed outbound QQ message segments.

The model may choose only these closed variants.  Raw OneBot dictionaries are created at
the gateway edge, so text can never smuggle a control segment and unsupported segment
kinds cannot be constructed accidentally.
"""

from __future__ import annotations

import json
from dataclasses import dataclass
from enum import StrEnum
from typing import Any


@dataclass(frozen=True, slots=True)
class TextSegment:
    text: str


@dataclass(frozen=True, slots=True)
class AtSegment:
    account: str


@dataclass(frozen=True, slots=True)
class ReplySegment:
    message_id: str


@dataclass(frozen=True, slots=True)
class FaceSegment:
    face_id: int


@dataclass(frozen=True, slots=True)
class MarketFaceSegment:
    package_id: str
    emoji_id: str
    key: str
    summary: str = ""


@dataclass(frozen=True, slots=True)
class DiceSegment:
    pass


@dataclass(frozen=True, slots=True)
class RpsSegment:
    pass


class ContactKind(StrEnum):
    MEMBER = "member"
    CURRENT_GROUP = "current_group"


@dataclass(frozen=True, slots=True)
class ContactSegment:
    kind: ContactKind
    target_id: str


class MusicPlatform(StrEnum):
    QQ = "qq"
    NETEASE = "163"
    KUGOU = "kugou"
    KUWO = "kuwo"
    MIGU = "migu"


@dataclass(frozen=True, slots=True)
class MusicSegment:
    platform: MusicPlatform
    track_id: str


@dataclass(frozen=True, slots=True)
class CustomMusicSegment:
    url: str
    audio: str
    title: str
    image: str
    singer: str = ""


@dataclass(frozen=True, slots=True)
class JsonCardSegment:
    """A validated, canonical JSON object encoded for NapCat's ARK segment."""

    data: str


type OutboundSegment = (
    TextSegment
    | AtSegment
    | ReplySegment
    | FaceSegment
    | MarketFaceSegment
    | DiceSegment
    | RpsSegment
    | ContactSegment
    | MusicSegment
    | CustomMusicSegment
    | JsonCardSegment
)


def from_onebot(items: list[dict]) -> tuple[OutboundSegment, ...]:
    """Best-effort decoding of segments previously written by this bot."""

    out: list[OutboundSegment] = []
    for item in items:
        if not isinstance(item, dict):
            continue
        kind, data = item.get("type"), item.get("data") or {}
        if not isinstance(data, dict):
            continue
        try:
            if kind == "text" and isinstance(data.get("text"), str):
                out.append(TextSegment(data["text"]))
            elif kind == "at" and data.get("qq"):
                out.append(AtSegment(str(data["qq"])))
            elif kind == "reply" and data.get("id"):
                out.append(ReplySegment(str(data["id"])))
            elif kind == "face":
                out.append(FaceSegment(int(data["id"])))
            elif kind == "mface" and data.get("emoji_package_id") and data.get("emoji_id"):
                out.append(MarketFaceSegment(
                    str(data["emoji_package_id"]),
                    str(data["emoji_id"]),
                    str(data.get("key") or ""),
                    str(data.get("summary") or ""),
                ))
            elif kind == "dice":
                out.append(DiceSegment())
            elif kind == "rps":
                out.append(RpsSegment())
            elif kind == "contact" and data.get("id"):
                contact_kind = (
                    ContactKind.MEMBER if data.get("type", "qq") == "qq"
                    else ContactKind.CURRENT_GROUP
                )
                out.append(ContactSegment(contact_kind, str(data["id"])))
            elif kind == "music" and data.get("type") == "custom":
                out.append(CustomMusicSegment(
                    str(data.get("url") or ""),
                    str(data.get("audio") or ""),
                    str(data.get("title") or ""),
                    str(data.get("image") or ""),
                    str(data.get("singer") or data.get("content") or ""),
                ))
            elif kind == "music" and data.get("id"):
                out.append(MusicSegment(MusicPlatform(data["type"]), str(data["id"])))
            elif kind == "json":
                raw = data.get("data")
                encoded = (
                    raw if isinstance(raw, str)
                    else json.dumps(raw, ensure_ascii=False, sort_keys=True, separators=(",", ":"))
                )
                out.append(JsonCardSegment(encoded))
        except (KeyError, TypeError, ValueError):
            continue
    return tuple(out)


def to_onebot(segment: OutboundSegment) -> dict[str, Any]:
    """Project one domain segment to the literal structure NapCat accepts."""

    match segment:
        case TextSegment(text):
            return {"type": "text", "data": {"text": text}}
        case AtSegment(account):
            return {"type": "at", "data": {"qq": account}}
        case ReplySegment(message_id):
            return {"type": "reply", "data": {"id": message_id}}
        case FaceSegment(face_id):
            return {"type": "face", "data": {"id": str(face_id)}}
        case MarketFaceSegment(package_id, emoji_id, key, summary):
            data = {
                "emoji_package_id": package_id,
                "emoji_id": emoji_id,
                "key": key,
            }
            if summary:
                data["summary"] = summary
            return {"type": "mface", "data": data}
        case DiceSegment():
            return {"type": "dice", "data": {}}
        case RpsSegment():
            return {"type": "rps", "data": {}}
        case ContactSegment(kind, target_id):
            onebot_kind = "qq" if kind is ContactKind.MEMBER else "group"
            return {"type": "contact", "data": {"type": onebot_kind, "id": target_id}}
        case MusicSegment(platform, track_id):
            return {"type": "music", "data": {"type": platform.value, "id": track_id}}
        case CustomMusicSegment(url, audio, title, image, singer):
            data = {
                "type": "custom",
                "url": url,
                "audio": audio,
                "title": title,
                "image": image,
            }
            if singer:
                data["singer"] = singer
            return {"type": "music", "data": data}
        case JsonCardSegment(data):
            return {"type": "json", "data": {"data": data}}


def without_replies(segments: tuple[OutboundSegment, ...]) -> tuple[OutboundSegment, ...]:
    return tuple(segment for segment in segments if not isinstance(segment, ReplySegment))


def text_content(segments: tuple[OutboundSegment, ...]) -> str:
    return "".join(segment.text for segment in segments if isinstance(segment, TextSegment))


def at_accounts(segments: tuple[OutboundSegment, ...]) -> list[str]:
    return [segment.account for segment in segments if isinstance(segment, AtSegment)]


def reply_target(segments: tuple[OutboundSegment, ...]) -> str | None:
    return next(
        (segment.message_id for segment in segments if isinstance(segment, ReplySegment)),
        None,
    )


def display_text(
    segments: tuple[OutboundSegment, ...],
    *,
    names: dict[str, str] | None = None,
) -> str:
    """A stable textual reading for the transcript and archive.

    Opaque JSON is deliberately represented by a label rather than replayed as chat text.
    The exact payload remains in the archived segment and historical send call.
    """

    names = names or {}
    parts: list[str] = []
    for segment in segments:
        match segment:
            case TextSegment(text):
                parts.append(text)
            case AtSegment(account):
                parts.append("@" + (names.get(account) or "成员"))
            case ReplySegment():
                pass
            case FaceSegment(face_id):
                parts.append(f"[QQ表情{face_id}]")
            case MarketFaceSegment(summary=summary):
                parts.append(summary or "[商城表情]")
            case DiceSegment():
                parts.append("[骰子]")
            case RpsSegment():
                parts.append("[猜拳]")
            case ContactSegment(kind=ContactKind.MEMBER):
                parts.append("[推荐联系人]")
            case ContactSegment():
                parts.append("[推荐本群]")
            case MusicSegment(platform, track_id):
                parts.append(f"[音乐 {platform.value}:{track_id}]")
            case CustomMusicSegment(title=title, singer=singer):
                parts.append(f"[音乐 {singer + ' - ' if singer else ''}{title}]")
            case JsonCardSegment():
                parts.append("[JSON卡片]")
    return "".join(parts).strip()
