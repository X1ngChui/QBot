"""Injected ledger persistence and account/holder reporting."""

from collections.abc import Callable
from datetime import date
from typing import TypedDict

from qqbot.domain.ids import AccountId
import asyncpg

from qqbot.domain.ids import GroupId


class Spender(TypedDict):
    accounts: list[AccountId]
    cny: float
    calls: int


def _spender(row: asyncpg.Record) -> Spender:
    return Spender(
        accounts=[AccountId(account) for account in row["accounts"]],
        cny=float(row["cny"]),
        calls=int(row["calls"]),
    )


class LedgerRepository:
    def __init__(
        self, database: Callable[[], asyncpg.Pool], *, today: Callable[[], date | str]
    ) -> None:
        self._database = database
        self._today = today

    async def ledger_add(
        self,
        *,
        day: date | None = None,
        group_id: GroupId | None,
        kind: str,
        model: str,
        in_hit: int = 0,
        in_miss: int = 0,
        out: int = 0,
        calls: int = 1,
        cny: float = 0.0,
        user_id: AccountId | None = None,
    ) -> None:
        """Add one call to the day's running total.

        An upsert rather than an append: the table's whole point is that one day, one
        group, one kind and one causing account have exactly one row, which is what makes
        a billing figure a lookup rather than a scan. `user_id` is the account whose
        action caused the spend ('' when no single account did); the caller usually
        leaves it to the operation attribution scope rather than passing it.

        The day comes from here rather than from CURRENT_DATE, so the boundary is midnight
        in the configured timezone rather than midnight wherever the container thinks it is.
        Those are the same day for most of the day and different for the hours that matter -
        a budget that resets eight hours early is a budget nobody set.
        """
        await self._database().execute(
            """INSERT INTO cost_ledger
                   (day, group_id, kind, model, user_id, in_hit, in_miss, out, calls, cny)
               VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)
               ON CONFLICT (day, group_id, kind, model, user_id) DO UPDATE
                 SET calls   = cost_ledger.calls   + EXCLUDED.calls,
                     in_hit  = cost_ledger.in_hit  + EXCLUDED.in_hit,
                     in_miss = cost_ledger.in_miss + EXCLUDED.in_miss,
                     out     = cost_ledger.out     + EXCLUDED.out,
                     cny     = cost_ledger.cny     + EXCLUDED.cny""",
            day if day is not None else _as_date(self._today()),
            _group(group_id),
            kind,
            model,
            user_id or "",
            in_hit,
            in_miss,
            out,
            calls,
            cny,
        )

    async def top_spenders(
        self, group_id: GroupId, *, k: int, all_linked: bool = False
    ) -> list[Spender]:
        """This group's costliest exact accounts or explicitly linked holders."""

        today = _as_date(self._today())
        if not all_linked:
            rows = await self._database().fetch(
                """SELECT user_id AS person, ARRAY[user_id] AS accounts,
                          sum(cny) AS cny, sum(calls) AS calls
                     FROM cost_ledger
                    WHERE group_id=$1 AND user_id <> ''
                      AND day >= $2 AND day <= $3
                    GROUP BY user_id
                    ORDER BY sum(cny) DESC, user_id
                    LIMIT $4""",
                _group(group_id),
                today.replace(day=1),
                today,
                k,
            )
            return [_spender(row) for row in rows]
        rows = await self._database().fetch(
            """SELECT COALESCE(ia.entity_id::text, l.user_id) AS person,
                      array_agg(DISTINCT l.user_id) AS accounts,
                      sum(l.cny) AS cny, sum(l.calls) AS calls
                 FROM cost_ledger l
                 LEFT JOIN identity_account ia
                        ON ia.platform = 'qq' AND ia.platform_user_id = l.user_id
                WHERE l.group_id = $1 AND l.user_id <> ''
                  AND l.day >= $2 AND l.day <= $3
                GROUP BY person
                ORDER BY sum(l.cny) DESC, person
                LIMIT $4""",
            _group(group_id),
            today.replace(day=1),
            today,
            k,
        )
        return [_spender(row) for row in rows]

    async def month_calls(self, kind: str, model: str) -> int:
        """Calls of one kind and model booked in the calendar month holding today.

        The quota meter for a capability with a monthly free allowance: the ledger already
        counts every call, so the allowance is read back rather than tracked in a second
        place that could drift. Month boundaries follow the configured timezone, like the
        day boundaries every ledger row is filed under.
        """
        today = _as_date(self._today())
        v = await self._database().fetchval(
            """SELECT COALESCE(sum(calls),0) FROM cost_ledger
                WHERE day >= $1 AND day <= $2 AND kind=$3 AND model=$4""",
            today.replace(day=1),
            today,
            kind,
            model,
        )
        return int(v or 0)

    async def day_cost(self, day: str | date) -> float:
        v = await self._database().fetchval(
            "SELECT COALESCE(sum(cny),0) FROM cost_ledger WHERE day=$1", _as_date(day)
        )
        return float(v or 0.0)

    async def day_breakdown(self, day: str | date, group_id: GroupId | None = None) -> list[dict]:
        """Cost and call counts for one day, by kind and model.

        Without group_id this covers every group, which is what the budget is measured
        against - the cap is shared, not per-group.
        """
        rows = await self._database().fetch(
            """SELECT kind, model, sum(calls) AS calls, sum(in_hit) AS in_hit,
                      sum(in_miss) AS in_miss, sum(out) AS out, sum(cny) AS cny
                 FROM cost_ledger WHERE day=$1 AND ($2::bigint IS NULL OR group_id=$2)
                GROUP BY kind, model ORDER BY sum(cny) DESC""",
            _as_date(day),
            None if group_id is None else _group(group_id),
        )
        return [dict(r) for r in rows]


def _group(group_id: GroupId | None) -> int:
    """Encode an optional domain group id for the ledger and SQL parameters."""
    return 0 if group_id is None else group_id.to_db()


def _as_date(day: str | date) -> date:
    """Callers pass 'YYYY-MM-DD'; asyncpg binds a date column as datetime.date."""
    return day if isinstance(day, date) else date.fromisoformat(day)
