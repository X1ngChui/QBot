"""Group-scoped counts and first appearances over the append-once event archive."""

from __future__ import annotations

from ..db import pool
from ..domain.ids import GroupId


class EventRepository:
    async def speaker_counts(self, group_id: GroupId) -> dict[str, int]:
        """Accounts that have spoken in this group, and how often.

        Group-scoped by parameter, like every other read here: an account is only in
        this roster because it spoke *here*.
        """
        rows = await pool().fetch(
            """SELECT platform_user_id AS uid, count(*) AS n
                 FROM raw_event
                WHERE group_id=$1 AND event_type IN ('message','notice')
                  AND platform_user_id IS NOT NULL
                GROUP BY platform_user_id""",
            group_id.to_db(),
        )
        return {r["uid"]: r["n"] for r in rows}

    async def first_appearances(self, group_id: GroupId) -> dict[str, object]:
        """When each account first appeared in this group's archive.

        The roster is ordered by this, so a person's place in it - and with it their
        member number - does not move when somebody new turns up.
        """
        rows = await pool().fetch(
            """SELECT platform_user_id AS uid, min(occurred_at) AS first
                 FROM raw_event
                WHERE group_id=$1 AND event_type IN ('message','notice')
                  AND platform_user_id IS NOT NULL
                GROUP BY platform_user_id""",
            group_id.to_db(),
        )
        return {r["uid"]: r["first"] for r in rows}
