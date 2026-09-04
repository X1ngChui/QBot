"""L0 reads.

Writes go through the ingest path, which owns the append; this is the query side that
everything else needs - who has spoken here, and how much.

It exists so that no layer above has to know the shape of raw_event: a counting query
written out at a call site is one more place the group filter can be forgotten.
"""

from __future__ import annotations

from ..db import pool


class EventRepository:
    async def speaker_counts(self, group_id: int) -> dict[str, int]:
        """Accounts that have spoken in this group, and how often.

        Group-scoped by parameter, like every other read here: an account is only in
        this roster because it spoke *here*.
        """
        rows = await pool().fetch(
            """SELECT platform_user_id AS uid, count(*) AS n
                 FROM raw_event
                WHERE group_id=$1 AND event_type='message'
                  AND platform_user_id IS NOT NULL
                GROUP BY platform_user_id""",
            group_id,
        )
        return {r["uid"]: r["n"] for r in rows}
