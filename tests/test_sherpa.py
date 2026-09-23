"""Deterministic lifecycle tests for the bounded local CPU ASR service."""

from __future__ import annotations

import asyncio
import pathlib
import sys
import threading

ROOT = pathlib.Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT))

from qqbot.core.budget import BUDGET
from qqbot.providers.sherpa import AsrBusy, AsrUnavailable, SherpaAsr
from qqbot.settings import AsrCfg

fails: list[str] = []
CFG = AsrCfg(model_dir="unused", threads=1, queue_capacity=1)


def check(name: str, condition: bool, detail: str = "") -> None:
    print(f"[{'ok ' if condition else 'FAIL'}] {name}  {detail}")
    if not condition:
        fails.append(name)


class ImmediateAsr(SherpaAsr):
    @staticmethod
    def _build(cfg):
        del cfg
        return object()

    @staticmethod
    def _decode(recognizer, data):
        del recognizer
        return data.decode(), 0.1


async def expect(kind: type[BaseException], awaitable) -> BaseException | None:
    try:
        await awaitable
    except kind as exc:
        return exc
    return None


async def startup_checks() -> None:
    entered = threading.Event()
    release = threading.Event()

    class SlowBuild(ImmediateAsr):
        @staticmethod
        def _build(cfg):
            del cfg
            entered.set()
            release.wait(2)
            return object()

    service = SlowBuild(CFG)
    startup = asyncio.create_task(service.start())
    await asyncio.to_thread(entered.wait, 1)
    heartbeat = False
    await asyncio.sleep(0)
    heartbeat = True
    check("ASR model loading leaves the event loop responsive", heartbeat)
    release.set()
    await startup
    await service.aclose()

    class BrokenBuild(ImmediateAsr):
        @staticmethod
        def _build(cfg):
            del cfg
            raise RuntimeError("broken model")

    broken = BrokenBuild(CFG)
    failure = await expect(RuntimeError, broken.start())
    check("a broken model fails startup", failure is not None)
    unavailable = await expect(
        AsrUnavailable,
        broken.transcribe(b"x"),
    )
    check("a failed startup never advertises readiness", unavailable is not None)


async def queue_checks() -> None:
    entered = threading.Event()
    release = threading.Event()
    decoded: list[str] = []

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

    service = BlockingDecode(CFG)
    await service.start()
    first = asyncio.create_task(service.transcribe(b"first"))
    await asyncio.to_thread(entered.wait, 1)
    second = asyncio.create_task(service.transcribe(b"second"))
    await asyncio.sleep(0)
    busy = await expect(AsrBusy, service.transcribe(b"third"))
    check("a full ASR queue fails fast with AsrBusy", busy is not None)
    release.set()
    check(
        "the ASR queue is FIFO",
        await first == "first" and await second == "second" and decoded == ["first", "second"],
        repr(decoded),
    )
    await service.aclose()


async def cancellation_checks() -> None:
    entered = threading.Event()
    release = threading.Event()
    completed = threading.Event()

    class BlockingDecode(ImmediateAsr):
        @staticmethod
        def _decode(recognizer, data):
            del recognizer
            if data == b"cancelled":
                entered.set()
                release.wait(2)
                completed.set()
            return data.decode(), 0.1

    service = BlockingDecode(CFG)
    await service.start()
    caller = asyncio.create_task(service.transcribe(b"cancelled"))
    await asyncio.to_thread(entered.wait, 1)
    caller.cancel()
    cancelled = await expect(asyncio.CancelledError, caller)
    release.set()
    await asyncio.to_thread(completed.wait, 1)
    check(
        "caller cancellation does not claim to stop native decode",
        cancelled is not None and completed.is_set(),
    )
    check(
        "the worker remains usable after a caller cancels",
        await service.transcribe(b"next") == "next",
    )
    await service.aclose()


async def shutdown_checks() -> None:
    entered = threading.Event()
    release = threading.Event()

    class BlockingDecode(ImmediateAsr):
        @staticmethod
        def _decode(recognizer, data):
            del recognizer
            if data == b"active":
                entered.set()
                release.wait(2)
            return data.decode(), 0.1

    service = BlockingDecode(CFG)
    await service.start()
    active = asyncio.create_task(service.transcribe(b"active"))
    await asyncio.to_thread(entered.wait, 1)
    queued = asyncio.create_task(service.transcribe(b"queued"))
    await asyncio.sleep(0)
    await service.aclose()
    release.set()
    active_failure = await expect(AsrUnavailable, active)
    queued_failure = await expect(AsrUnavailable, queued)
    check(
        "shutdown fails both active and queued callers explicitly",
        active_failure is not None and queued_failure is not None,
    )
    refused = await expect(AsrUnavailable, service.transcribe(b"late"))
    check("shutdown rejects new ASR work", refused is not None)


async def main() -> int:
    original_record = BUDGET.record

    async def no_record(**kwargs):
        del kwargs

    BUDGET.record = no_record
    try:
        await startup_checks()
        await queue_checks()
        await cancellation_checks()
        await shutdown_checks()
    finally:
        BUDGET.record = original_record
    print()
    print("FAILED:", fails if fails else "none")
    return 1 if fails else 0


if __name__ == "__main__":
    raise SystemExit(asyncio.run(main()))
