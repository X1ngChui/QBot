"""One durable claim processes one batch and only progress publishes a continuation."""

from contextlib import asynccontextmanager
from types import SimpleNamespace
from unittest.mock import AsyncMock
import uuid

import pytest

from qqbot.domain.ids import GroupId
from qqbot.domain.memory import ExtractionBatch, ExtractionSnapshot, ExtractionStatus
from qqbot.repositories.job import Job, JobType
from qqbot.workers.lease import Deferred
from qqbot.workers.memory import EXTRACTION_EVENT_LIMIT, EXTRACTION_GAP, MemoryWorker

GROUP = GroupId("311")


@pytest.fixture
def execution(monkeypatch):
    @asynccontextmanager
    async def slot(_):
        yield True

    batch = ExtractionBatch(uuid.uuid4(), GROUP, ExtractionStatus.EXTRACTING, ())
    job = Job(uuid.uuid4(), JobType.EXTRACT_MEMORY, {"group_id": GROUP}, 2, 5, uuid.uuid4())
    instance = MemoryWorker.__new__(MemoryWorker)
    instance._cfg = SimpleNamespace(budget=SimpleNamespace(daily_cny_cap=1))
    instance._extractions = SimpleNamespace(
        model_slot=slot,
        open=AsyncMock(return_value=None),
        claim=AsyncMock(return_value=batch),
        has_unconsumed=AsyncMock(return_value=True),
        begin_attempt=AsyncMock(return_value=False),
        stage=AsyncMock(),
    )
    instance._queue = SimpleNamespace(submit=AsyncMock())
    instance._extract_batch = AsyncMock(return_value=2)
    instance._apply_batch = AsyncMock()
    budget = AsyncMock(return_value=False)
    instance._budget = SimpleNamespace(exceeded=budget)
    return instance, batch, job, budget


async def test_one_claim_never_drains_multiple_batches(execution):
    instance, batch, job, _ = execution
    assert await instance.extract(GROUP, fence=job) == 2
    instance._extractions.claim.assert_awaited_once_with(
        GROUP, limit=EXTRACTION_EVENT_LIMIT, floor=1, gap=EXTRACTION_GAP, fence=job
    )
    instance._extract_batch.assert_awaited_once_with(batch, fence=job)
    instance._queue.submit.assert_awaited_once_with(
        JobType.EXTRACT_MEMORY, {"group_id": GROUP}, fence=job
    )


async def test_failed_batch_never_publishes_a_fresh_job(execution):
    instance, _, job, _ = execution
    instance._extract_batch.side_effect = RuntimeError("synthetic failure")
    with pytest.raises(RuntimeError, match="synthetic"):
        await instance.extract(GROUP, fence=job)
    instance._queue.submit.assert_not_awaited()
    instance._extractions.has_unconsumed.assert_not_awaited()


async def test_staged_resume_applies_without_model_or_budget_admission(execution):
    instance, batch, job, budget = execution
    staged = ExtractionBatch(
        batch.id, GROUP, ExtractionStatus.STAGED, (), ExtractionSnapshot((), ())
    )
    instance._extractions.open.return_value = staged
    assert await instance.extract(GROUP, fence=job) == 0
    instance._apply_batch.assert_awaited_once_with(staged, fence=job)
    budget.assert_not_awaited()
    instance._extract_batch.assert_not_awaited()
    instance._extractions.claim.assert_not_awaited()


async def test_budget_deferral_does_not_claim_or_publish_new_work(execution):
    instance, _, job, budget = execution
    budget.return_value = True
    with pytest.raises(Deferred, match="budget"):
        await instance.extract(GROUP, fence=job)
    instance._extractions.claim.assert_not_awaited()
    instance._queue.submit.assert_not_awaited()


async def test_busy_slot_defers_before_opening_a_batch(execution):
    instance, _, job, budget = execution

    @asynccontextmanager
    async def busy(_):
        yield False

    instance._extractions.model_slot = busy
    with pytest.raises(Deferred, match="busy"):
        await instance.extract(GROUP, fence=job)
    instance._extractions.open.assert_not_awaited()
    budget.assert_not_awaited()
    instance._queue.submit.assert_not_awaited()


async def test_exhausted_batch_cannot_call_model_or_checkpoint(execution):
    instance, batch, job, _ = execution
    instance._render_snapshot = AsyncMock(return_value=({1: uuid.uuid4()}, "", [], None))
    instance._known = AsyncMock(return_value="")
    instance._extractor = SimpleNamespace(extract=AsyncMock())
    assert await MemoryWorker._extract_batch(instance, batch, fence=job) == 0
    instance._extractions.begin_attempt.assert_awaited_once_with(batch.id, fence=job)
    instance._extractor.extract.assert_not_awaited()
    instance._extractions.stage.assert_not_awaited()
    instance._apply_batch.assert_not_awaited()
