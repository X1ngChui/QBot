"""Display, accounting and repositories share an immutable runtime-local clock."""

import ast
from datetime import UTC, datetime
from pathlib import Path
from types import SimpleNamespace
from zoneinfo import ZoneInfoNotFoundError

import pytest

from qqbot.clock import Clock
from qqbot.configuration import ConfigBundle
from qqbot.gateway.onebot import GroupMessage, notice_from_event
from qqbot.runtime import Runtime


def test_same_instant_has_independent_display_and_accounting_days():
    instant = datetime(2026, 9, 28, 1, 30, tzinfo=UTC)
    east = Clock("Asia/Shanghai", wall=lambda: instant)
    west = Clock("America/New_York", wall=lambda: instant)
    assert east.today().isoformat() == "2026-09-28"
    assert west.today().isoformat() == "2026-09-27"
    assert east.format(instant) == "09-28 09:30"
    assert west.format(instant) == "09-27 21:30"
    assert "UTC+08:00" in east.describe()
    assert "UTC-04:00" in west.describe()
    assert east.timezone == "Asia/Shanghai"


def test_dst_transition_keeps_the_absolute_instant():
    before = datetime(2026, 11, 1, 5, 30, tzinfo=UTC)
    after = datetime(2026, 11, 1, 6, 30, tzinfo=UTC)
    clock = Clock("America/New_York")
    assert clock.format(before) == clock.format(after) == "11-01 01:30"
    assert Clock(clock.timezone, wall=lambda: before).now().fold == 0
    assert Clock(clock.timezone, wall=lambda: after).now().fold == 1


def test_invalid_zone_and_naive_wall_time_fail_explicitly():
    with pytest.raises(ZoneInfoNotFoundError):
        Clock("Fictional/Absent")
    naive = datetime(2026, 9, 28)
    with pytest.raises(ValueError, match="timezone-aware"):
        Clock("UTC", wall=lambda: naive).now()
    with pytest.raises(ValueError, match="timezone-aware"):
        Clock("UTC").format(naive)


def test_message_and_notice_fallback_use_only_the_supplied_clock():
    instant = datetime(2026, 9, 28, 1, 30, tzinfo=UTC)
    clock = Clock("America/New_York", wall=lambda: instant)
    event = SimpleNamespace(
        time=0,
        group_id="311",
        user_id="101",
        message_id="17",
        sender={},
        get_message=lambda: [],
    )
    message = GroupMessage.from_event(event, "999", clock=clock)
    notice = notice_from_event(event, self_id="999", plain_text="Fictional", clock=clock)
    assert message.occurred_at == notice.occurred_at == instant
    assert message.occurred_at.hour == notice.occurred_at.hour == 21


async def test_two_runtime_graphs_do_not_share_display_or_accounting_clocks(bundle, monkeypatch):
    monkeypatch.setattr("qqbot.runtime.dsn", lambda: "postgresql://fictional/unused")
    raw = bundle.default.model_dump()
    raw["bot"]["timezone"] = "America/New_York"
    alternate = ConfigBundle(
        raw, {"default": bundle._default_persona}, bundle.prompts, bundle.predicates
    )
    first, second = Runtime.build(bundle), Runtime.build(alternate)
    try:
        assert first.clock is not second.clock
        for runtime in (first, second):
            owners = (
                runtime.groups,
                runtime.media_cache,
                runtime.directory,
                runtime.registry,
                runtime.links,
                runtime.gateway,
                runtime.router,
                runtime.worker,
                runtime.scheduled,
            )
            assert all(owner._clock is runtime.clock for owner in owners)
            assert runtime.reply_executor.clock is runtime.clock
            assert runtime.gateway._replies is runtime.scheduled._replies is runtime.replies
            assert runtime.budget._today.__self__ is runtime.clock
            assert runtime.budget.ledger._today.__self__ is runtime.clock
    finally:
        await first.aclose()
        await second.aclose()


def test_production_no_longer_has_a_configuration_service_locator():
    root = Path(__file__).parents[2] / "qqbot"
    for path in root.rglob("*.py"):
        tree = ast.parse(path.read_text(encoding="utf-8"))
        for node in ast.walk(tree):
            if isinstance(node, ast.ImportFrom) and node.module == "qqbot.configuration":
                assert not {a.name for a in node.names} & {"config", "prompt_catalog"}, path
            if isinstance(node, ast.Call) and isinstance(node.func, ast.Name):
                assert node.func.id not in {"config", "prompt_catalog"}, path
    from qqbot import configuration

    assert not hasattr(configuration, "config")
    assert not hasattr(configuration, "prompt_catalog")
    assert not hasattr(configuration, "_bundle")
