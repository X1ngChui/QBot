"""Provider retries and semaphore waiters recheck the same budget at the wire boundary."""

from unittest.mock import AsyncMock

import pytest

from _budget import fake_budget
from qqbot.providers.base import Rate, RetryPolicy
from qqbot.providers.contracts import CallContext, GenerationPolicy
from qqbot.providers.openai_responses import ResponsesCodec, ResponsesExecutor
from qqbot.providers.openai_transport import StreamInterrupted
from qqbot.services.budget import BudgetExceeded, BudgetUnavailable


def executor(budget):
    return ResponsesExecutor(
        provider="fictional",
        endpoint="https://example.invalid",
        credential_env="UNUSED",
        max_concurrency=1,
        retry=RetryPolicy(2, 0),
        codec=ResponsesCodec(),
        rate_for=lambda _: Rate("Mtoken", in_miss=1, out=1),
        budget=budget,
    )


@pytest.mark.parametrize("write_failure", [False, True])
async def test_retry_rechecks_after_an_estimated_charge(write_failure):
    budget = fake_budget(cap=0.001)
    if write_failure:
        budget.ledger.ledger_add.side_effect = OSError("synthetic ledger failure")
    adapter = executor(budget)
    adapter._transport.complete = AsyncMock(side_effect=StreamInterrupted("lost", started=True))
    try:
        expected = BudgetUnavailable if write_failure else BudgetExceeded
        with pytest.raises(expected):
            await adapter.complete(
                [], [], policy=GenerationPolicy("m", retries=2), context=CallContext()
            )
        adapter._transport.complete.assert_awaited_once()
        budget.ledger.ledger_add.assert_awaited_once()
    finally:
        await adapter.aclose()


async def test_budget_is_checked_after_acquiring_the_provider_slot():
    budget = fake_budget(cap=1)
    adapter = executor(budget)
    adapter._transport.complete = AsyncMock()

    class ChargeOnAdmission:
        async def __aenter__(self):
            await budget.record(kind="reply", model="concurrent", cny=1)

        async def __aexit__(self, *args):
            pass

    adapter._gate = ChargeOnAdmission()
    try:
        with pytest.raises(BudgetExceeded):
            await adapter.complete([], [], policy=GenerationPolicy("m"), context=CallContext())
        adapter._transport.complete.assert_not_awaited()
    finally:
        await adapter.aclose()
