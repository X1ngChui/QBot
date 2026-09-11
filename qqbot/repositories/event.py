"""L0 reads.

Writes go through the ingest path, which owns the append; this is the query side that
everything else needs - who has spoken here, how much, and what memory extraction has
not read yet.

It exists so that no layer above has to know the shape of raw_event: a counting query
written out at a call site is one more place the group filter can be forgotten, and
the extraction watermark is a predicate that several readers must agree on exactly.
"""

from __future__ import annotations

import uuid

from ..db import pool

#: A message memory extraction has not read. Written once and shared by the count
#: that gates a paid pass and the fetch that feeds it, so the two cannot disagree
#: about what "unread" means.
#:
#: The watermark lives on created_at (ingest time), not occurred_at: a chunk must be
#: a contiguous prefix of the axis the mark moves along, or marking its end would skip
#: rows never read. The two axes agree except for out-of-order redelivery; rendering
#: re-sorts by occurred_at so the model still reads conversation order.
#:
#: The newest few seconds are left out. created_at is the inserting transaction's
#: start time, so a row whose insert began before a read's snapshot but committed
#: after it carries a timestamp below the mark the read then sets - and would be
#: marked read without ever being read, silently. Ingest commits in well under a
#: second; excluding a margin many times that closes the window. Nothing is lost by
#: it: the nightly drain reads a whole day, and a row this young is simply next
#: night's.
UNREAD_MESSAGE = """group_id = $1 AND event_type = 'message'
                  AND created_at > COALESCE(
                      (SELECT last_extract_at FROM group_state WHERE group_id = $1),
                      'epoch')
                  AND created_at < NOW() - INTERVAL '5 seconds'"""

#: The columns the extraction worker renders a transcript from.
_MESSAGE_COLS = "id, platform_user_id, occurred_at, created_at, payload, plain_text"


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

    # -- the extraction watermark ------------------------------------------
    async def unread_since_extract(self, group_id: int) -> tuple[int, object]:
        """How many messages this group has that no extraction has read, and the
        newest one's arrival time.

        The watermark records what was actually *read*, not what was queued, so this
        count is the honest answer to "is there anything to extract" - and it is what
        stops a retried or hand-triggered job from paying to re-read a batch it
        already paid for.
        """
        row = await pool().fetchrow(
            f"""SELECT count(*) AS n, max(created_at) AS newest FROM raw_event
                 WHERE {UNREAD_MESSAGE}""",
            group_id,
        )
        return (row["n"] or 0), row["newest"]

    async def next_unread(self, group_id: int, *, limit: int) -> list:
        """The oldest unread messages, in ingest order, at most `limit` of them.

        Oldest-first from the watermark, because the nightly drain reads a whole day
        in several passes: newest-first would mark everything read after the first
        pass and silently skip the rest. Ordered by (created_at, id) so ties cannot
        reorder between two reads.
        """
        return list(await pool().fetch(
            f"""SELECT {_MESSAGE_COLS} FROM raw_event
                 WHERE {UNREAD_MESSAGE}
                 ORDER BY created_at ASC, id ASC LIMIT $2""",
            group_id, limit,
        ))

    async def batch_ending_at(
        self, group_id: int, *, anchor: uuid.UUID | None, size: int,
    ) -> list:
        """The `size` messages ingested up to and including `anchor`, oldest first.

        How an extraction batch is reproduced exactly: the anchor is the batch's last
        row on the ingest axis and the size is how many rows it held, so the pair
        names one set however much has arrived since. Ordered by (created_at, id) at
        both ends because ties must not reorder between the read and the replay.
        """
        return list(reversed(await pool().fetch(
            f"""SELECT {_MESSAGE_COLS} FROM raw_event
                 WHERE group_id=$1 AND event_type='message'
                   AND ($3::uuid IS NULL
                        OR (created_at, id)
                            <= (SELECT created_at, id FROM raw_event WHERE id=$3))
                 ORDER BY created_at DESC, id DESC LIMIT $2""",
            group_id, size, anchor,
        )))
