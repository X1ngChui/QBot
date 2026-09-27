"""Diagnostic state belongs to one Runtime and detaches on every shutdown path."""

from datetime import UTC, datetime
import logging
from types import SimpleNamespace
from unittest.mock import AsyncMock

import pytest

from qqbot.clock import Clock
from qqbot.operations.errors import ErrorRing
from qqbot.runtime import Runtime


STAMP = datetime(2026, 9, 28, 1, 30, tzinfo=UTC)


def record(message):
    return logging.LogRecord("qqbot.synthetic", logging.WARNING, "", 0, message, (), None)


def ring(zone="UTC", *, entries=2, message_chars=5):
    return ErrorRing(
        clock=Clock(zone, wall=lambda: STAMP), entries=entries, message_chars=message_chars
    )


def test_entries_text_and_recent_projection_are_bounded():
    handler = ring()
    try:
        for text in ("first", "second", "third-long"):
            handler.handle(record(text))
        assert handler.count() == 2
        assert handler.recent(9) == [
            ("09-28 01:30", "qqbot.synthetic", "secon"),
            ("09-28 01:30", "qqbot.synthetic", "third"),
        ]
        assert handler.recent(0) == handler.recent(-1) == []
        detached = handler.recent(1)
        detached.clear()
        assert handler.count() == 2
        handler.clear()
        assert handler.count() == 0
    finally:
        handler.close()


def test_rings_do_not_share_records_or_display_time():
    east, west = ring("Asia/Shanghai"), ring("America/New_York")
    try:
        east.handle(record("east"))
        west.handle(record("west"))
        assert east.recent(1) == [("09-28 09:30", "qqbot.synthetic", "east")]
        assert west.recent(1) == [("09-27 21:30", "qqbot.synthetic", "west")]
        east.clear()
        assert west.count() == 1
    finally:
        east.close()
        west.close()


def test_install_is_idempotent_and_close_only_detaches_its_own_handler():
    first, second = ring(), ring()
    loggers = tuple(logging.getLogger(name) for name in ("qqbot", "apscheduler"))
    try:
        first.install()
        first.install()
        second.install()
        assert all(logger.handlers.count(first) == 1 for logger in loggers)
        second.close()
        assert all(first in logger.handlers and second not in logger.handlers for logger in loggers)
        second.handle(record("late"))
        assert second.count() == 0
        with pytest.raises(RuntimeError, match="closed"):
            second.install()
    finally:
        first.close()
        second.close()
    assert all(first not in logger.handlers and second not in logger.handlers for logger in loggers)


@pytest.mark.parametrize("entries,message_chars", [(0, 1), (1, 0), (-1, 1)])
def test_invalid_diagnostic_bounds_are_rejected(entries, message_chars):
    with pytest.raises(ValueError, match="positive"):
        ring(entries=entries, message_chars=message_chars)


def test_unrenderable_record_does_not_break_application_logging():
    class BrokenMessage:
        def __str__(self):
            raise RuntimeError("synthetic formatting failure")

    handler = ring()
    try:
        handler.handle(record(BrokenMessage()))
        assert handler.count() == 0
        handler.handle(record("next"))
        assert handler.count() == 1
    finally:
        handler.close()


async def test_unstarted_runtime_close_preserves_the_other_runtime_handler(bundle, monkeypatch):
    monkeypatch.setattr("qqbot.runtime.dsn", lambda: "postgresql://fictional/unused")
    first, second = Runtime.build(bundle), Runtime.build(bundle)
    try:
        first.diagnostics.install()
        assert first.router._diagnostics is first.diagnostics
        assert first.diagnostics._clock is first.clock
        assert second.diagnostics is not first.diagnostics
        await second.aclose()
        assert first.diagnostics in logging.getLogger("qqbot").handlers
        assert second.diagnostics not in logging.getLogger("qqbot").handlers
    finally:
        await first.aclose()
        await second.aclose()
    assert first.diagnostics not in logging.getLogger("qqbot").handlers


async def test_failed_startup_detaches_its_diagnostic_handler(bundle, monkeypatch):
    monkeypatch.setattr("qqbot.runtime.dsn", lambda: "postgresql://fictional/unused")
    monkeypatch.setattr("qqbot.runtime.repo.ensure_schema", AsyncMock())
    providers = SimpleNamespace(
        text=object(),
        embedding=SimpleNamespace(name="fictional"),
        asr=SimpleNamespace(start=AsyncMock(side_effect=RuntimeError("synthetic ASR failure"))),
        aclose=AsyncMock(),
    )
    lease = SimpleNamespace(acquire=AsyncMock(), close=AsyncMock())
    runtime = Runtime.build(bundle, providers=providers, lease=lease)
    monkeypatch.setattr(runtime.database, "start", AsyncMock())
    monkeypatch.setattr(runtime.database, "close", AsyncMock())
    with pytest.raises(RuntimeError, match="synthetic ASR failure"):
        await runtime.start()
    assert runtime.diagnostics._closed
    assert runtime.diagnostics not in logging.getLogger("qqbot").handlers
    assert runtime.diagnostics not in logging.getLogger("apscheduler").handlers
    runtime.database.close.assert_awaited_once()
    providers.aclose.assert_awaited_once()


def test_legacy_diagnostic_and_timezone_locators_are_absent():
    from qqbot.operations import errors
    from qqbot import util

    assert not any(
        hasattr(errors, name) for name in ("_RING", "install", "recent", "count", "clear")
    )
    assert not any(
        hasattr(util, name)
        for name in (
            "_TZ",
            "set_timezone",
            "tz",
            "tz_sql",
            "now_local",
            "today_local",
            "describe_now",
            "fmt_when",
        )
    )
