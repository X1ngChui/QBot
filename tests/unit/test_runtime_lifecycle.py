"""Startup failure and shutdown release the exact resources owned by one Runtime."""

import asyncio
from types import SimpleNamespace
from unittest.mock import AsyncMock

import pytest

from qqbot import runtime as module
from qqbot.db.lease import RuntimeAlreadyActive
from qqbot.runtime import Runtime


@pytest.fixture
def graph(bundle, monkeypatch):
    lease = SimpleNamespace(acquire=AsyncMock(), close=AsyncMock())
    providers = SimpleNamespace(
        text=object(),
        embedding=SimpleNamespace(name="fictional"),
        asr=SimpleNamespace(start=AsyncMock()),
        aclose=AsyncMock(),
    )
    resources = SimpleNamespace(init=AsyncMock(), close=AsyncMock(), schema=AsyncMock())
    monkeypatch.setattr(module.repo, "ensure_schema", resources.schema)
    runtime = Runtime.build(bundle, providers=providers, lease=lease)
    monkeypatch.setattr(runtime.database, "start", resources.init)
    monkeypatch.setattr(runtime.database, "close", resources.close)

    async def work():
        await asyncio.Event().wait()

    runtime.worker.run_forever = work
    return runtime, lease, providers, resources


@pytest.mark.parametrize("stage", ["schema", "asr"])
async def test_partial_startup_failure_releases_every_open_resource(graph, stage):
    runtime, lease, providers, resources = graph
    target = resources.schema if stage == "schema" else providers.asr.start
    target.side_effect = RuntimeError("synthetic startup failure")
    with pytest.raises(RuntimeError, match="synthetic startup"):
        await runtime.start()
    assert runtime._closed and runtime._worker_task is None
    providers.aclose.assert_awaited_once()
    lease.close.assert_awaited_once()
    resources.close.assert_awaited_once()
    with pytest.raises(RuntimeError, match="closed"):
        await runtime.start()


async def test_rejected_second_runtime_never_opens_or_closes_the_first_pool(graph):
    runtime, lease, providers, resources = graph
    lease.acquire.side_effect = RuntimeAlreadyActive()
    with pytest.raises(RuntimeAlreadyActive):
        await runtime.start()
    resources.init.assert_not_awaited()
    resources.close.assert_awaited_once()
    assert runtime.database._pool is None
    providers.aclose.assert_awaited_once()


async def test_concurrent_starts_share_one_initialization(graph):
    runtime, lease, providers, resources = graph
    await asyncio.gather(runtime.start(), runtime.start())
    resources.init.assert_awaited_once()
    providers.asr.start.assert_awaited_once()
    lease.acquire.assert_awaited_once()
    await runtime.aclose()


async def test_shutdown_during_startup_joins_it_before_closing_dependencies(graph):
    runtime, _, providers, resources = graph
    entered = asyncio.Event()

    async def start_asr():
        entered.set()
        await asyncio.Event().wait()

    providers.asr.start.side_effect = start_asr
    start = asyncio.create_task(runtime.start())
    await entered.wait()
    await asyncio.wait_for(runtime.aclose(), 1)
    with pytest.raises(asyncio.CancelledError):
        await start
    resources.close.assert_awaited_once()
    assert runtime._worker_task is None


async def test_cancelling_a_close_waiter_does_not_abandon_teardown(graph):
    runtime, _, providers, resources = graph
    entered, release = asyncio.Event(), asyncio.Event()

    async def close_providers():
        entered.set()
        await release.wait()

    providers.aclose.side_effect = close_providers
    await runtime.start()
    close = asyncio.create_task(runtime.aclose())
    await entered.wait()
    close.cancel()
    with pytest.raises(asyncio.CancelledError):
        await close
    resources.close.assert_not_awaited()
    release.set()
    await runtime.aclose()
    resources.close.assert_awaited_once()
    providers.aclose.assert_awaited_once()


async def test_lease_loss_immediately_revokes_all_owned_work(graph):
    runtime, lease, _, resources = graph
    entered, stopped = asyncio.Event(), asyncio.Event()
    await runtime.start()

    async def shared():
        entered.set()
        try:
            await asyncio.Event().wait()
        finally:
            stopped.set()

    waiter = asyncio.create_task(runtime.media_processor._work.run("fictional", shared))
    await entered.wait()
    lease.acquire.call_args.args[0]()
    assert runtime._closed and runtime.gateway._closing
    assert runtime.gateway._replies._closed and runtime.scheduled._closed
    assert runtime.media._closed and runtime.media_processor._work._closed
    await runtime.aclose()
    with pytest.raises(asyncio.CancelledError):
        await waiter
    assert stopped.is_set()
    resources.close.assert_awaited_once()


async def test_maintenance_is_unregistered_and_joined_before_dependencies_close(graph):
    runtime, _, _, resources = graph
    entered = asyncio.Event()
    events = []
    await runtime.start()
    runtime.own_registration(lambda: events.append("unregistered"))

    async def maintenance(owner):
        assert owner is runtime
        entered.set()
        try:
            await asyncio.Event().wait()
        finally:
            events.append("stopped")

    task = asyncio.create_task(runtime.run_maintenance(maintenance))
    await entered.wait()
    await runtime.aclose()
    assert task.done() and task.cancelled()
    assert events == ["unregistered", "stopped"]
    resources.close.assert_awaited_once()
