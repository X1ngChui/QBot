"""Group policy and visibility persistence."""

from __future__ import annotations

from qqbot.clock import Clock

from collections.abc import Callable
from datetime import date
import uuid

import asyncpg

from qqbot.domain.ids import GroupId


class GroupRepository:
    def __init__(self, database: Callable[[], asyncpg.Pool], clock: Clock) -> None:
        self._clock = clock
        self._database = database

    async def group_muted(self, group_id: GroupId) -> bool:
        """Return this group's persisted mute switch."""

        muted = await self._database().fetchval(
            "SELECT muted FROM group_state WHERE group_id=$1", _group(group_id)
        )
        return bool(muted)

    async def set_group_muted(self, group_id: GroupId, muted: bool) -> None:
        await self._database().execute(
            """INSERT INTO group_state (group_id, muted) VALUES ($1,$2)
               ON CONFLICT (group_id) DO UPDATE SET muted=$2, updated_at=NOW()""",
            _group(group_id),
            muted,
        )

    async def block(self, group_id: GroupId, user_ids: list[str] | str, *, until=None) -> None:
        """Create or replace exact-account block rules."""

        ids = [user_ids] if isinstance(user_ids, str) else list(user_ids)
        if not ids:
            return
        await self._database().executemany(
            """INSERT INTO group_blocklist (group_id, user_id, blocked_until)
               VALUES ($1,$2,$3)
               ON CONFLICT (group_id, user_id) WHERE user_id IS NOT NULL
                 DO UPDATE SET blocked_until = EXCLUDED.blocked_until""",
            [(_group(group_id), user_id, until) for user_id in ids],
        )

    async def block_holder(self, group_id: GroupId, entity_id: uuid.UUID, *, until=None) -> None:
        """Create or replace a dynamic linked-holder block rule."""

        await self._database().execute(
            """INSERT INTO group_blocklist (group_id, entity_id, blocked_until)
               VALUES ($1,$2,$3)
               ON CONFLICT (group_id, entity_id) WHERE entity_id IS NOT NULL
                 DO UPDATE SET blocked_until = EXCLUDED.blocked_until""",
            _group(group_id),
            entity_id,
            until,
        )

    async def unblock(self, group_id: GroupId, user_ids: list[str] | str) -> bool:
        """Delete exact-account block rules."""

        ids = [user_ids] if isinstance(user_ids, str) else list(user_ids)
        if not ids:
            return False
        tag = await self._database().execute(
            "DELETE FROM group_blocklist WHERE group_id=$1 AND user_id=ANY($2::text[])",
            _group(group_id),
            ids,
        )
        return not tag.endswith(" 0")

    async def unblock_holder(self, group_id: GroupId, entity_id: uuid.UUID) -> bool:
        """Delete every holder rule that currently resolves to this linked set."""

        tag = await self._database().execute(
            """WITH RECURSIVE family AS (
                   SELECT $2::uuid AS id
                   UNION ALL
                   SELECT e.id FROM entity e JOIN family f ON e.merged_into=f.id
               )
               DELETE FROM group_blocklist
                WHERE group_id=$1 AND entity_id IN (SELECT id FROM family)""",
            _group(group_id),
            entity_id,
        )
        return not tag.endswith(" 0")

    async def blocked(self, group_id: GroupId, user_id: str) -> bool:
        """Resolve exact and linked-holder rules against current identity membership."""

        return bool(
            await self._database().fetchval(
                """WITH RECURSIVE family AS (
                   SELECT entity_id AS id FROM identity_account
                    WHERE platform='qq' AND platform_user_id=$2
                   UNION ALL
                   SELECT e.id FROM entity e JOIN family f ON e.merged_into=f.id
               )
               SELECT EXISTS (
                   SELECT 1 FROM group_blocklist b
                    WHERE b.group_id=$1
                      AND (b.blocked_until IS NULL OR b.blocked_until > NOW())
                      AND (b.user_id=$2 OR b.entity_id IN (SELECT id FROM family))
               )""",
                _group(group_id),
                user_id,
            )
        )

    async def block_rules(self, group_id: GroupId) -> list[dict]:
        """Return active rules in their current exact-account or holder scope."""

        rows = await self._database().fetch(
            """WITH RECURSIVE resolved AS (
                   SELECT b.id AS rule_id, b.user_id, b.blocked_until, b.created_at,
                          e.id AS current_entity_id, e.merged_into
                     FROM group_blocklist b
                     LEFT JOIN entity e ON e.id=b.entity_id
                    WHERE b.group_id=$1
                      AND (b.blocked_until IS NULL OR b.blocked_until > NOW())
                   UNION ALL
                   SELECT r.rule_id, r.user_id, r.blocked_until, r.created_at,
                          e.id, e.merged_into
                     FROM resolved r
                     JOIN entity e ON e.id=r.merged_into
               )
               SELECT rule_id AS id, user_id,
                      CASE WHEN user_id IS NULL THEN current_entity_id END AS entity_id,
                      blocked_until, created_at
                 FROM resolved
                WHERE user_id IS NOT NULL OR merged_into IS NULL
                ORDER BY created_at, rule_id""",
            _group(group_id),
        )
        combined: dict[tuple[str | None, uuid.UUID | None], dict] = {}
        for row in rows:
            item = dict(row)
            item.pop("created_at")
            key = (item["user_id"], item["entity_id"])
            previous = combined.get(key)
            if previous is None:
                combined[key] = item
                continue
            old_until = previous["blocked_until"]
            new_until = item["blocked_until"]
            previous["blocked_until"] = (
                None if old_until is None or new_until is None else max(old_until, new_until)
            )
        return list(combined.values())

    async def note_group_seen(self, group_id: GroupId) -> bool:
        """Record that this group exists. True only for the call that first saw it.

        The insert is the claim: whoever creates the row is the discoverer, and everyone
        after them conflicts and gets False. There is no allowlist, so this row is the only
        thing that distinguishes a group the bot has served for months from one that added
        it a minute ago.
        """
        row = await self._database().fetchval(
            """INSERT INTO group_state (group_id, first_seen_at) VALUES ($1, NOW())
               ON CONFLICT (group_id) DO NOTHING
               RETURNING group_id""",
            _group(group_id),
        )
        return row is not None

    async def groups_first_seen_on(self, day: str | date) -> list[GroupId]:
        """Groups whose first message arrived on this day, for the daily report."""
        rows = await self._database().fetch(
            """SELECT group_id FROM group_state
                WHERE first_seen_at IS NOT NULL
                  AND (first_seen_at AT TIME ZONE $2)::date = $1::date
                ORDER BY first_seen_at""",
            _as_date(day),
            self._clock.timezone,
        )
        return [GroupId(r["group_id"]) for r in rows]

    async def groups_with_state(
        self,
    ) -> list[GroupId]:
        rows = await self._database().fetch("SELECT group_id FROM group_state")
        return [GroupId(r["group_id"]) for r in rows]

    async def muted_groups(
        self,
    ) -> list[GroupId]:
        """Read from the table, not the in-memory registry: a group muted before the
        last restart and quiet since is exactly the one the daily report must not
        forget to list."""
        rows = await self._database().fetch("SELECT group_id FROM group_state WHERE muted")
        return [GroupId(r["group_id"]) for r in rows]


def _group(group_id: GroupId | None) -> int:
    """Encode an optional domain group id for the ledger and SQL parameters."""
    return 0 if group_id is None else group_id.to_db()


def _as_date(day: str | date) -> date:
    """Callers pass 'YYYY-MM-DD'; asyncpg binds a date column as datetime.date."""
    return day if isinstance(day, date) else date.fromisoformat(day)
