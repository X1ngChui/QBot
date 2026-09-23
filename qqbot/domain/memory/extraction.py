"""Durable exact-event memory extraction batches and staged snapshots."""

from __future__ import annotations

import uuid
from dataclasses import dataclass
from datetime import datetime
from enum import StrEnum
from typing import Any

from ..archive import ArchivedMessage
from ..ids import GroupId

SNAPSHOT_VERSION = 2


class ExtractionStatus(StrEnum):
    EXTRACTING = "extracting"
    STAGED = "staged"
    APPLIED = "applied"


@dataclass(frozen=True, slots=True)
class SnapshotTarget:
    """One exact account that a source line resolves without model inference."""

    account_id: uuid.UUID
    reason: str
    marker: str = ""

    def as_payload(self) -> dict[str, str]:
        return {
            "account_id": str(self.account_id),
            "reason": self.reason,
            "marker": self.marker,
        }

    @classmethod
    def from_payload(cls, payload: dict[str, Any]) -> SnapshotTarget:
        return cls(
            account_id=uuid.UUID(str(payload["account_id"])),
            reason=str(payload["reason"]),
            marker=str(payload.get("marker", "")),
        )


@dataclass(frozen=True, slots=True)
class SnapshotLine:
    ordinal: int
    event_id: uuid.UUID
    occurred_at: datetime
    text: str
    evidence_text: str
    event_type: str = "message"
    own: bool = False
    author_account_id: uuid.UUID | None = None
    targets: tuple[SnapshotTarget, ...] = ()

    def as_payload(self) -> dict[str, Any]:
        return {
            "ordinal": self.ordinal,
            "event_id": str(self.event_id),
            "occurred_at": self.occurred_at.isoformat(),
            "text": self.text,
            "evidence_text": self.evidence_text,
            "event_type": self.event_type,
            "own": self.own,
            "author_account_id": (
                str(self.author_account_id) if self.author_account_id is not None else None
            ),
            "targets": [target.as_payload() for target in self.targets],
        }

    @classmethod
    def from_payload(cls, payload: dict[str, Any]) -> SnapshotLine:
        author = payload.get("author_account_id")
        return cls(
            ordinal=int(payload["ordinal"]),
            event_id=uuid.UUID(str(payload["event_id"])),
            occurred_at=datetime.fromisoformat(str(payload["occurred_at"])),
            text=str(payload["text"]),
            evidence_text=str(payload["evidence_text"]),
            event_type=str(payload.get("event_type", "message")),
            own=bool(payload.get("own", False)),
            author_account_id=uuid.UUID(str(author)) if author else None,
            targets=tuple(
                SnapshotTarget.from_payload(item)
                for item in payload.get("targets", [])
            ),
        )


@dataclass(frozen=True, slots=True)
class ExtractionSnapshot:
    """Immutable validation input for one paid extraction response."""

    account_codes: tuple[tuple[int, uuid.UUID], ...]
    lines: tuple[SnapshotLine, ...]

    @property
    def codes(self) -> dict[int, uuid.UUID]:
        return dict(self.account_codes)

    @property
    def sources(self) -> dict[int, SnapshotLine]:
        return {line.ordinal: line for line in self.lines}

    def as_payload(self) -> dict[str, Any]:
        return {
            "version": SNAPSHOT_VERSION,
            "account_codes": [
                {"code": code, "account_id": str(account_id)}
                for code, account_id in self.account_codes
            ],
            "lines": [line.as_payload() for line in self.lines],
        }

    @classmethod
    def from_payload(cls, payload: dict[str, Any]) -> ExtractionSnapshot:
        version = int(payload.get("version", 0))
        if version != SNAPSHOT_VERSION:
            raise ValueError(
                f"unsupported extraction snapshot version {version}; "
                f"expected {SNAPSHOT_VERSION}"
            )
        return cls(
            account_codes=tuple(
                (int(item["code"]), uuid.UUID(str(item["account_id"])))
                for item in payload.get("account_codes", [])
            ),
            lines=tuple(
                SnapshotLine.from_payload(item)
                for item in payload.get("lines", [])
            ),
        )


@dataclass(frozen=True, slots=True)
class ExtractionBatch:
    id: uuid.UUID
    group_id: GroupId
    status: ExtractionStatus
    events: tuple[ArchivedMessage, ...]
    snapshot: ExtractionSnapshot | None = None
