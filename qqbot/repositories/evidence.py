"""Bounded reply evidence persistence and retention."""

from __future__ import annotations


from collections.abc import Callable

import asyncpg

from qqbot.domain.evidence import EvidenceMemo
from qqbot.domain.ids import GroupId, MessageId


class EvidenceRepository:
    def __init__(self, database: Callable[[], asyncpg.Pool]) -> None:
        self._database = database

    async def evidence_add(
        self, group_id: GroupId, reply_event_id: MessageId, memo: EvidenceMemo
    ) -> None:
        """Store one structured memo; a replayed reply keeps its original evidence."""

        await self._database().execute(
            """INSERT INTO reply_trace
                   (group_id, reply_event_id, memo, expires_at)
               VALUES ($1,$2,$3,$4)
               ON CONFLICT (group_id, reply_event_id) DO NOTHING""",
            _group(group_id),
            reply_event_id,
            memo.to_dict(),
            memo.expires_at,
        )

    async def evidence_for(
        self, group_id: GroupId, reply_event_ids: list[MessageId]
    ) -> dict[MessageId, str]:
        """Render unexpired structured memos for prompt replay."""

        if not reply_event_ids:
            return {}
        rows = await self._database().fetch(
            """SELECT reply_event_id, memo FROM reply_trace
                WHERE group_id=$1 AND reply_event_id = ANY($2::text[])
                  AND memo IS NOT NULL AND expires_at > NOW()""",
            _group(group_id),
            reply_event_ids,
        )
        rendered: dict[MessageId, str] = {}
        for row in rows:
            try:
                content = EvidenceMemo.from_dict(row["memo"]).render()
            except (TypeError, ValueError):
                continue
            if content:
                rendered[MessageId(row["reply_event_id"])] = content
        return rendered

    async def evidence_prune(
        self,
    ) -> int:
        """Delete expired evidence in one bounded nightly database operation."""

        result = await self._database().execute(
            "DELETE FROM reply_trace WHERE expires_at IS NOT NULL AND expires_at <= NOW()"
        )
        return int(result.rpartition(" ")[2])


def _group(group_id: GroupId | None) -> int:
    """Encode an optional domain group id for the ledger and SQL parameters."""
    return 0 if group_id is None else group_id.to_db()
