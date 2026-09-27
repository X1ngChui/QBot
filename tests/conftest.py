"""Isolated public inputs for executable behavior contracts."""

import os
from pathlib import Path

import pytest

from qqbot import configuration as settings
import _fixtures


@pytest.fixture
def bundle():
    return settings.load_bundle(Path(__file__).parent / "fixtures" / "config")


@pytest.fixture(autouse=True)
def isolated_configuration(monkeypatch, bundle):
    """Keep public test inputs independent of test collection order."""
    monkeypatch.setattr(_fixtures, "_bundle", bundle)


@pytest.fixture
async def test_database(monkeypatch):
    if "QBOT_TEST_DATABASE_URL" not in os.environ:
        pytest.skip("set QBOT_TEST_DATABASE_URL to a guarded disposable PostgreSQL database")
    import _db

    with monkeypatch.context() as environment:
        for key in ("DATABASE_URL", "DATABASE_PASSWORD", "DATABASE_PASSWORD_FILE"):
            environment.delenv(key, raising=False)
        try:
            await _db.init_pool()
            yield _db.pool()
        finally:
            await _db.close_pool()


@pytest.fixture(scope="module", autouse=True)
def isolated_tokenizer():
    import jieba
    from qqbot.gateway import nickname

    with pytest.MonkeyPatch.context() as patch:
        patch.setattr(nickname, "jieba", jieba.Tokenizer())
        patch.setattr(nickname, "_injected", set())
        patch.setattr(nickname, "_ready", False)
        yield
