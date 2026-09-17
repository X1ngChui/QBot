"""Bounded, expiring evidence retained beside one bot reply."""

from __future__ import annotations

from dataclasses import dataclass
from datetime import datetime
from enum import StrEnum
from typing import Any, ClassVar

from ..util import sysmark


class EvidenceSource(StrEnum):
    WEB_SEARCH = "web_search"
    HISTORY = "search_history"
    EVENTS = "recall_events"
    PAGE = "read_url"
    IMAGE = "open_images"


class EvidenceOutcome(StrEnum):
    VERIFIED = "verified"
    UNCONFIRMED = "unconfirmed"


_LABELS = {
    EvidenceSource.WEB_SEARCH: "搜索",
    EvidenceSource.HISTORY: "查档",
    EvidenceSource.EVENTS: "回忆",
    EvidenceSource.PAGE: "读网页",
    EvidenceSource.IMAGE: "看了图",
}


@dataclass(frozen=True, slots=True)
class EvidenceItem:
    source: EvidenceSource
    request: str
    outcome: EvidenceOutcome
    digest: str

    def to_dict(self) -> dict[str, str]:
        return {
            "source": self.source.value,
            "request": self.request,
            "outcome": self.outcome.value,
            "digest": self.digest,
        }

    @classmethod
    def from_dict(cls, value: Any) -> EvidenceItem:
        if not isinstance(value, dict):
            raise ValueError("evidence item must be an object")
        return cls(
            source=EvidenceSource(value.get("source")),
            request=str(value.get("request") or ""),
            outcome=EvidenceOutcome(value.get("outcome")),
            digest=str(value.get("digest") or ""),
        )


@dataclass(frozen=True, slots=True)
class EvidenceMemo:
    items: tuple[EvidenceItem, ...]
    created_at: datetime
    expires_at: datetime
    schema: ClassVar[int] = 1

    def __post_init__(self) -> None:
        if not self.items:
            raise ValueError("evidence memo must contain at least one item")
        if self.created_at.tzinfo is None or self.expires_at.tzinfo is None:
            raise ValueError("evidence memo timestamps must be timezone-aware")
        if self.expires_at <= self.created_at:
            raise ValueError("evidence memo must expire after it is created")

    def to_dict(self) -> dict[str, Any]:
        return {
            "schema": self.schema,
            "created_at": self.created_at.isoformat(),
            "expires_at": self.expires_at.isoformat(),
            "items": [item.to_dict() for item in self.items],
        }

    @classmethod
    def from_dict(cls, value: Any) -> EvidenceMemo:
        if not isinstance(value, dict) or value.get("schema") != 1:
            raise ValueError("unsupported evidence memo schema")
        items = value.get("items")
        if not isinstance(items, list):
            raise ValueError("evidence memo items must be an array")
        created_at = datetime.fromisoformat(str(value.get("created_at") or ""))
        expires_at = datetime.fromisoformat(str(value.get("expires_at") or ""))
        return cls(
            items=tuple(EvidenceItem.from_dict(item) for item in items),
            created_at=created_at,
            expires_at=expires_at,
        )

    def render(self) -> str:
        """Provider-neutral prompt projection; raw evidence never leaves storage."""

        lines = [sysmark("检索记录")]
        for item in self.items:
            label = _LABELS[item.source]
            request = f"“{item.request}”" if item.request else ""
            qualifier = "" if item.outcome is EvidenceOutcome.VERIFIED else "（未确认）"
            lines.append(f"{label}{request}{qualifier}：{item.digest}")
        return "\n".join(lines) if len(lines) > 1 else ""
