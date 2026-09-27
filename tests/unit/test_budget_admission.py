"""Ledger failures stop future spending without replaying already completed work."""

import _db as _test_db
import asyncio
from datetime import date, timedelta
from types import SimpleNamespace
from unittest.mock import AsyncMock

import pytest

from _budget import fake_budget
from qqbot.services.budget import Budget, BudgetExceeded, BudgetUnavailable, LedgerHealth


async def test_unreadable_ledger_never_means_zero_and_a_read_can_recover():
    ledger = SimpleNamespace(day_cost=AsyncMock(side_effect=[OSError("offline"), 2.0]))
    budget = Budget(ledger, daily_cap=3, today=_test_db.clock.today)
    with pytest.raises(BudgetUnavailable):
        await budget.check()
    assert budget.health is LedgerHealth.UNAVAILABLE
    await budget.check()
    assert await budget.spent_today() == 2
    assert budget.health is LedgerHealth.READY


async def test_uncertain_write_keeps_completed_result_but_latches_protection():
    budget = fake_budget()
    budget.ledger.ledger_add.side_effect = OSError("commit acknowledgement lost")
    with budget.scope(1) as scope:
        assert await budget.record(kind="reply", model="fictional", cny=0.25) == 0.25
        assert scope.spent == 0.25
    assert budget.health is LedgerHealth.UNCERTAIN
    budget.ledger.ledger_add.side_effect = None
    budget._loaded = False
    with pytest.raises(BudgetUnavailable):
        await budget.check()
    with pytest.raises(BudgetUnavailable):
        await budget.spent_today()
    assert await budget.exceeded()
    budget.ledger.ledger_add.assert_awaited_once()


async def test_cancelled_accounting_also_latches_protection():
    budget = fake_budget()
    entered = asyncio.Event()

    async def uncertain(**kwargs):
        entered.set()
        await asyncio.Event().wait()

    budget.ledger.ledger_add.side_effect = uncertain
    task = asyncio.create_task(budget.record(kind="reply", model="fictional", cny=0.1))
    await entered.wait()
    task.cancel()
    with pytest.raises(asyncio.CancelledError):
        await task
    with pytest.raises(BudgetUnavailable):
        await budget.check()


async def test_scope_cap_stops_next_call_but_never_refunds_completed_work():
    budget = fake_budget()
    with budget.scope(0.1) as scope:
        await budget.check()
        await budget.record(kind="reply", model="fictional", cny=0.2)
        assert scope.exhausted
        with pytest.raises(BudgetExceeded, match="operation"):
            await budget.check()
    await budget.check()
    assert await budget.spent_today() == 0.2


async def test_concurrent_bookings_and_rollover_do_not_lose_charges():
    budget = fake_budget(cap=1)
    today = date(2026, 1, 1)
    budget._today = lambda: today
    await asyncio.gather(
        *(budget.record(kind="reply", model="fictional", cny=0.25) for _ in range(8))
    )
    assert await budget.spent_today() == 2
    with pytest.raises(BudgetExceeded, match="daily"):
        await budget.check()
    today += timedelta(days=1)
    await budget.check()
    assert await budget.spent_today() == 0


async def test_free_completed_work_can_be_booked_while_paid_admission_is_closed():
    budget = fake_budget(cap=0)
    with pytest.raises(BudgetExceeded):
        await budget.check()
    assert await budget.record(kind="asr", model="fictional-local", cny=0, calls=1) == 0
    budget.ledger.ledger_add.assert_awaited_once()


async def test_scope_reuse_and_attribution_charge_only_the_owner_once():
    budget = fake_budget()
    with (
        budget.attribute("fictional-account"),
        budget.scope(1) as owner,
        budget.scope(0.01, reuse=True) as nested,
    ):
        await budget.record(kind="vision", model="fictional", cny=0.2)
    assert owner is nested and owner.spent == 0.2
    assert budget.ledger.ledger_add.call_args.kwargs["user_id"] == "fictional-account"
    await budget.record(kind="asr", model="fictional", cny=0)
    assert budget.ledger.ledger_add.call_args.kwargs["user_id"] == ""


@pytest.mark.parametrize("invalid", [float("nan"), float("inf"), -1])
async def test_invalid_ledger_total_is_not_a_safe_reading(invalid):
    budget = Budget(
        SimpleNamespace(day_cost=AsyncMock(return_value=invalid)),
        daily_cap=1,
        today=_test_db.clock.today,
    )
    with pytest.raises(BudgetUnavailable):
        await budget.check()
