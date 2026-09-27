"""Maintenance reports failed phases and derives backup freshness from its schedule."""

from datetime import UTC, datetime, timedelta
from types import SimpleNamespace
from unittest.mock import AsyncMock

from qqbot.operations import scheduled
from qqbot.repositories.job import JobType


async def test_completed_stage_ignores_independent_job_types():
    queue = SimpleNamespace(depth=AsyncMock(return_value={}))
    result = await scheduled._drain_wait(
        queue,
        timedelta(seconds=1),
        "extraction",
        (JobType.EXTRACT_MEMORY,),
    )
    assert result.end is scheduled.StageEnd.COMPLETE
    queue.depth.assert_awaited_once_with(job_types=(JobType.EXTRACT_MEMORY,))


async def test_timed_out_stage_retains_observed_backlog():
    queue = SimpleNamespace(depth=AsyncMock(return_value={"pending": 2}))
    result = await scheduled._drain_wait(
        queue,
        timedelta(seconds=0.01),
        "extraction",
        (JobType.EXTRACT_MEMORY,),
    )
    assert result.end is scheduled.StageEnd.TIMED_OUT
    assert result.remaining == (("pending", 2),)


async def test_unreadable_queue_has_an_explicit_failed_phase():
    queue = SimpleNamespace(depth=AsyncMock(side_effect=RuntimeError("fictional outage")))
    result = await scheduled._drain_wait(
        queue,
        timedelta(seconds=1),
        "extraction",
        (JobType.EXTRACT_MEMORY,),
    )
    assert result.end is scheduled.StageEnd.FAILED


async def test_memory_phase_failure_does_not_suppress_backup_or_cleanup(monkeypatch):
    memory = AsyncMock(side_effect=RuntimeError("fictional queue failure"))
    backup, cleanup = AsyncMock(), AsyncMock()
    monkeypatch.setattr(scheduled, "_memory_stages", memory)
    monkeypatch.setattr(scheduled, "backup", backup)
    monkeypatch.setattr(scheduled, "clean_napcat_cache", cleanup)
    runtime = SimpleNamespace(evidence=SimpleNamespace(evidence_prune=AsyncMock(return_value=0)))
    await scheduled.nightly(runtime)
    backup.assert_awaited_once_with(runtime)
    cleanup.assert_awaited_once_with(runtime)
    runtime.evidence.evidence_prune.assert_awaited_once()


def test_weekly_backup_is_not_declared_stale_after_twenty_six_hours():
    last = datetime(2026, 1, 1, 3, tzinfo=UTC)
    assert not scheduled.backup_is_overdue(
        last,
        datetime(2026, 1, 3, tzinfo=UTC),
        cron="0 2 * * mon",
        zone="UTC",
    )
    assert scheduled.backup_is_overdue(
        last,
        datetime(2026, 1, 6, tzinfo=UTC),
        cron="0 2 * * mon",
        zone="UTC",
    )


def test_backup_deadline_includes_owned_phase_and_subprocess_budgets():
    last = datetime(2026, 1, 1, 3, tzinfo=UTC)
    assert not scheduled.backup_is_overdue(
        last,
        datetime(2026, 1, 2, 5, 59, tzinfo=UTC),
        cron="0 2 * * *",
        zone="UTC",
    )
    assert scheduled.backup_is_overdue(
        last,
        datetime(2026, 1, 2, 6, 1, tzinfo=UTC),
        cron="0 2 * * *",
        zone="UTC",
    )
