"""Canonical read boundary for versioned archived messages."""

from __future__ import annotations

from collections.abc import Iterable, Mapping
from typing import Any

from ..db import pool
from ..domain.archive import ArchivedMessage
from ..domain.ids import GroupId

_ARCHIVE_COLUMN_NAMES = (
    "id",
    "platform_event_id",
    "group_id",
    "event_type",
    "occurred_at",
    "created_at",
    "payload",
    "plain_text",
    "archive_schema",
)


def archive_columns(alias: str = "") -> str:
    prefix = f"{alias}." if alias else ""
    return ", ".join(prefix + name for name in _ARCHIVE_COLUMN_NAMES)


ARCHIVE_COLUMNS = archive_columns()


def archived_message(row: Mapping[str, Any]) -> ArchivedMessage:
    """Validate one SQL row into the only archive representation above repositories."""

    return ArchivedMessage.from_row(row)


def archived_messages(rows: Iterable[Mapping[str, Any]]) -> list[ArchivedMessage]:
    return [archived_message(row) for row in rows]


class ArchiveRepository:
    async def recent(self, group_id: GroupId, *, limit: int) -> list[ArchivedMessage]:
        """The newest archived messages of one group, returned oldest first."""

        rows = await pool().fetch(
            f"""SELECT {ARCHIVE_COLUMNS}
                  FROM raw_event
                 WHERE group_id=$1 AND event_type IN ('message','notice')
                 ORDER BY occurred_at DESC, id DESC LIMIT $2""",
            group_id.to_db(),
            limit,
        )
        return list(reversed(archived_messages(rows)))
