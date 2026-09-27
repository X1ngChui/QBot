"""Extraction billing reaches the real ledger through the production Responses session."""

from datetime import date
from decimal import Decimal
from unittest.mock import AsyncMock

import pytest

from qqbot.domain.ids import GroupId
from qqbot.providers.base import Rate, RetryPolicy
from qqbot.providers.openai_responses import ResponsesCodec, ResponsesTextModel
from qqbot.providers.openai_transport import TerminalResponse
from qqbot.repositories.ledger import LedgerRepository
from qqbot.services.budget import Budget, LedgerHealth
from qqbot.services.memory_extractor import ExtractionInput, MemoryExtractor

pytestmark = pytest.mark.database


async def test_extraction_preserves_group_identity_and_persists_provider_usage(database, bundle):
    today = date(2030, 1, 2)
    ledger = LedgerRepository(lambda: database.pool, today=lambda: today)
    budget = Budget(ledger, daily_cap=3, today=lambda: today)
    text = ResponsesTextModel(
        bundle.default.backends.text,
        RetryPolicy(0, 0),
        name="fictional",
        codec=ResponsesCodec(),
        rate_for=lambda _: Rate("Mtoken", in_hit=1, in_miss=2, out=3),
        budget=budget,
    )
    text._executor._transport.complete = AsyncMock(
        return_value=TerminalResponse(
            {
                "status": "completed",
                "model": "fictional-extraction-model",
                "usage": {
                    "input_tokens": 1000,
                    "input_tokens_details": {"cached_tokens": 200},
                    "output_tokens": 100,
                },
                "output": [],
            },
            "completed",
        )
    )
    try:
        extractor = MemoryExtractor(bundle, text)
        result = await extractor.extract(
            ExtractionInput(
                group_id=GroupId("311"),
                transcript="Fictional member discussed a fictional project.",
                roster="Fictional member",
                account_codes={},
            )
        )
        assert result == []
        text._executor._transport.complete.assert_awaited_once()
        row = await database.pool.fetchrow("SELECT * FROM cost_ledger")
        assert row is not None
        assert row["day"] == today
        assert row["group_id"] == 311
        assert row["kind"] == "extract"
        assert row["model"] == "fictional-extraction-model"
        assert row["user_id"] == ""
        assert (row["in_hit"], row["in_miss"], row["out"], row["calls"]) == (200, 800, 100, 1)
        assert row["cny"] == Decimal("0.002100")
        assert budget.health is LedgerHealth.READY
        assert await budget.spent_today() == pytest.approx(0.0021)
        await budget.check()
    finally:
        await text.aclose()
