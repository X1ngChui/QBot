"""Bounded local CPU ASR lifecycle with no native model or network."""

import asyncio
import threading

import pytest

from _budget import fake_budget
from qqbot.configuration import AsrCfg
from qqbot.providers.sherpa import AsrBusy, AsrUnavailable, SherpaAsr


@pytest.fixture
def cfg():
    return AsrCfg(model_dir="unused", threads=1)


class ImmediateAsr(SherpaAsr):
    @staticmethod
    def _build(cfg):
        del cfg
        return object()

    @staticmethod
    def _decode(recognizer, data):
        del recognizer
        return data.decode(), 0.1


@pytest.mark.asyncio
async def test_model_loading_does_not_block_loop_and_failure_never_claims_readiness(cfg):
    entered, release = threading.Event(), threading.Event()

    class SlowBuild(ImmediateAsr):
        @staticmethod
        def _build(cfg):
            del cfg
            entered.set()
            release.wait(2)
            return object()

    service = SlowBuild(cfg, budget=fake_budget(), queue_capacity=1)
    startup = asyncio.create_task(service.start())
    try:
        assert await asyncio.to_thread(entered.wait, 1)
        await asyncio.sleep(0)
        assert not startup.done()
    finally:
        release.set()
        await startup
        await service.aclose()

    class BrokenBuild(ImmediateAsr):
        @staticmethod
        def _build(cfg):
            del cfg
            raise RuntimeError("broken model")

    broken = BrokenBuild(cfg, budget=fake_budget(), queue_capacity=1)
    try:
        with pytest.raises(RuntimeError, match="broken model"):
            await broken.start()
        with pytest.raises(AsrUnavailable):
            await broken.transcribe(b"x")
    finally:
        await broken.aclose()


@pytest.mark.asyncio
async def test_full_queue_is_busy_and_decoding_remains_fifo(cfg):
    entered, release = threading.Event(), threading.Event()
    decoded = []

    class BlockingDecode(ImmediateAsr):
        @staticmethod
        def _decode(recognizer, data):
            del recognizer
            value = data.decode()
            decoded.append(value)
            if value == "first":
                entered.set()
                release.wait(2)
            return value, 0.1

    service = BlockingDecode(cfg, budget=fake_budget(), queue_capacity=1)
    await service.start()
    try:
        first = asyncio.create_task(service.transcribe(b"first"))
        assert await asyncio.to_thread(entered.wait, 1)
        second = asyncio.create_task(service.transcribe(b"second"))
        await asyncio.sleep(0)
        with pytest.raises(AsrBusy):
            await service.transcribe(b"third")
        release.set()
        assert await first == "first"
        assert await second == "second"
        assert decoded == ["first", "second"]
    finally:
        release.set()
        await service.aclose()


@pytest.mark.asyncio
async def test_cancelled_caller_does_not_cancel_worker_decode(cfg):
    entered, release, completed = threading.Event(), threading.Event(), threading.Event()

    class BlockingDecode(ImmediateAsr):
        @staticmethod
        def _decode(recognizer, data):
            del recognizer
            if data == b"cancelled":
                entered.set()
                release.wait(2)
                completed.set()
            return data.decode(), 0.1

    service = BlockingDecode(cfg, budget=fake_budget(), queue_capacity=1)
    await service.start()
    try:
        caller = asyncio.create_task(service.transcribe(b"cancelled"))
        assert await asyncio.to_thread(entered.wait, 1)
        caller.cancel()
        with pytest.raises(asyncio.CancelledError):
            await caller
        release.set()
        assert await asyncio.to_thread(completed.wait, 1)
        assert await service.transcribe(b"next") == "next"
    finally:
        release.set()
        await service.aclose()


@pytest.mark.asyncio
async def test_shutdown_fails_active_queued_and_late_calls(cfg):
    entered, release = threading.Event(), threading.Event()

    class BlockingDecode(ImmediateAsr):
        @staticmethod
        def _decode(recognizer, data):
            del recognizer
            if data == b"active":
                entered.set()
                release.wait(2)
            return data.decode(), 0.1

    service = BlockingDecode(cfg, budget=fake_budget(), queue_capacity=1)
    await service.start()
    try:
        active = asyncio.create_task(service.transcribe(b"active"))
        assert await asyncio.to_thread(entered.wait, 1)
        queued = asyncio.create_task(service.transcribe(b"queued"))
        await asyncio.sleep(0)
        await service.aclose()
        release.set()
        with pytest.raises(AsrUnavailable):
            await active
        with pytest.raises(AsrUnavailable):
            await queued
        with pytest.raises(AsrUnavailable):
            await service.transcribe(b"late")
    finally:
        release.set()
        await service.aclose()
