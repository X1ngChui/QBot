"""A failed embedding page never manufactures a fresh continuation job."""

from types import SimpleNamespace
from unittest.mock import AsyncMock

import pytest

from qqbot.domain.ids import GroupId
from qqbot.workers.memory import MemoryWorker


def worker(bundle, monkeypatch, *, embedding_error=False, storage_error=False, short=False):
    events = []
    instance = MemoryWorker.__new__(MemoryWorker)
    instance._cfg = bundle.default
    instance._budget = SimpleNamespace(exceeded=AsyncMock(return_value=False))

    async def embed(*args, **kwargs):
        events.append("embed")
        if embedding_error:
            raise RuntimeError("synthetic model failure")
        return [[0.0]] if short else [[0.0], [1.0]]

    async def put(**kwargs):
        events.append("put")
        if storage_error:
            raise RuntimeError("synthetic persistence failure")
        return True

    async def submit(*args, **kwargs):
        events.append("next")

    instance._embed = SimpleNamespace(batch_size=2, embed=embed)
    instance._vec = SimpleNamespace(
        unembedded_episodes=AsyncMock(
            return_value=[("first", "fictional"), ("second", "fictional")]
        ),
        put_episode=put,
    )
    instance._queue = SimpleNamespace(submit=submit)
    return instance, events


async def test_continuation_is_enqueued_only_after_all_vectors_are_persisted(bundle, monkeypatch):
    instance, events = worker(bundle, monkeypatch)
    assert await instance.embed(GroupId("1")) == 2
    assert events == ["embed", "put", "put", "next"]
    assert instance._vec.unembedded_episodes.call_args.kwargs["limit"] == 2


@pytest.mark.parametrize("failure", ["embedding_error", "storage_error", "short"])
async def test_failed_pages_do_not_enqueue_new_jobs(bundle, monkeypatch, failure):
    instance, events = worker(bundle, monkeypatch, **{failure: True})
    with pytest.raises((RuntimeError, ValueError)):
        await instance.embed(GroupId("1"))
    assert "next" not in events
    if failure == "short":
        assert "put" not in events
