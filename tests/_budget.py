"""Explicit test composition for real-ledger and isolated in-memory budgets."""

import _db as _test_db
from collections import defaultdict
from datetime import date
from unittest.mock import AsyncMock
from types import SimpleNamespace

from qqbot.services.budget import Budget


def fake_budget(*, cap: float = 1000) -> Budget:
    totals = defaultdict(float)

    async def record(*, day: date, cny: float, **kwargs):
        totals[day] += cny

    ledger = SimpleNamespace(
        day_cost=AsyncMock(side_effect=lambda day: totals[day]),
        ledger_add=AsyncMock(side_effect=record),
        month_calls=AsyncMock(return_value=0),
    )
    return Budget(ledger, daily_cap=cap, today=_test_db.clock.today)
