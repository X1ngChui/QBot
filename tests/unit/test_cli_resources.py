"""CLI imports are inert and partial startup closes only explicitly owned resources."""

import importlib
from datetime import UTC, datetime
from types import SimpleNamespace
from unittest.mock import AsyncMock, Mock

import pytest

from qqbot.clock import Clock

SCRIPTS = (
    "generate_prompts",
    "review_prompts",
    "eval_extract",
    "eval_replies",
    "check_schema",
    "preflight",
)


@pytest.mark.parametrize("name", SCRIPTS)
def test_import_never_loads_deployment_inputs_or_opens_resources(name, monkeypatch):
    def forbidden(*args, **kwargs):
        raise AssertionError("import attempted to access deployment inputs")

    monkeypatch.setattr("scripts._env.load_dotenv", forbidden)
    monkeypatch.setattr("qqbot.configuration.load_bundle", forbidden)
    monkeypatch.setattr("qqbot.db.dsn", forbidden)
    monkeypatch.setattr("_db.configure_test_database", forbidden)
    module = importlib.import_module("scripts." + name)
    importlib.reload(module)
    assert callable(module.main)


@pytest.mark.parametrize("name", SCRIPTS)
async def test_provider_or_database_startup_failure_closes_database(name, bundle, monkeypatch):
    module = importlib.import_module("scripts." + name)
    monkeypatch.setattr(module, "load_bundle", lambda: bundle)
    monkeypatch.setattr(module, "dsn", lambda: "postgresql://fictional/unused")
    database = SimpleNamespace(
        pool=Mock(side_effect=AssertionError("no actual database calls allowed")),
        start=AsyncMock(side_effect=RuntimeError("synthetic startup failure")),
        close=AsyncMock(),
    )
    monkeypatch.setattr(module, "Database", lambda *args, **kwargs: database)
    if name in ("preflight", "eval_replies"):
        monkeypatch.setattr(
            module, "build_providers", Mock(side_effect=RuntimeError("synthetic startup failure"))
        )
    entry = module._check if name == "check_schema" else module.main
    with pytest.raises(RuntimeError, match="synthetic startup failure"):
        await entry()
    database.close.assert_awaited_once()
    database.pool.assert_not_called()


@pytest.mark.parametrize("name", ["generate_prompts", "review_prompts", "eval_extract"])
async def test_failed_disposable_guard_closes_the_open_database(name, bundle, monkeypatch):
    module = importlib.import_module("scripts." + name)
    monkeypatch.setattr(module, "load_bundle", lambda: bundle)
    monkeypatch.setattr(module, "dsn", lambda: "postgresql://fictional/unused")
    database = SimpleNamespace(pool=Mock(), start=AsyncMock(), close=AsyncMock())
    monkeypatch.setattr(module, "Database", lambda *args, **kwargs: database)
    monkeypatch.setattr(
        module, "assert_disposable_database", AsyncMock(side_effect=RuntimeError("guard"))
    )
    build = Mock(side_effect=AssertionError("providers must not start after failed guard"))
    monkeypatch.setattr(module, "build_providers", build)
    with pytest.raises(RuntimeError, match="guard"):
        await module.main()
    database.close.assert_awaited_once()
    build.assert_not_called()


def test_reply_cases_use_the_supplied_clock_without_import_time_timestamps():
    module = importlib.import_module("scripts.eval_replies")
    stamp = datetime(2026, 9, 28, 12, 0, tzinfo=UTC)
    clock = Clock("America/New_York", wall=lambda: stamp)
    assert not hasattr(module, "CASES")
    cases = module.build_cases(clock)
    assert cases
    assert all(case["trigger"].ts == stamp for case in cases)
    assert all(case["trigger"].ts.tzinfo == clock.zone for case in cases)
