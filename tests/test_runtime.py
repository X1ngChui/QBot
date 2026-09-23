"""Runtime ownership, teardown ordering, and per-message media coordination."""

from __future__ import annotations

import asyncio
import os
import pathlib
import sys
import uuid
from datetime import timedelta

ROOT = pathlib.Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT))
os.environ.setdefault("CONFIG_DIR", str(ROOT / "tests" / "fixtures" / "config"))

from qqbot.core.media import MediaCoordinator, MediaStatus, Unsettled
from qqbot.core.segments import parse_segments
from qqbot.core.state import ChatMsg
from qqbot.domain.ids import AccountId, MessageId
from qqbot.runtime import Runtime
from qqbot.repositories.job import JobType
from qqbot.settings import config
from qqbot.util import now_local
import qqbot.core.media as media_module
import qqbot.runtime as runtime_module
import qqbot.scheduled as scheduled_module

fails: list[str] = []


def check(name: str, condition: bool, detail: str = "") -> None:
    print(f"[{'ok ' if condition else 'FAIL'}] {name}  {detail}")
    if not condition:
        fails.append(name)


class Capability:
    attachments = None

    def __init__(self, name: str, events: list[str]) -> None:
        self.name = name
        self._events = events

    async def start(self) -> None:
        self._events.append("asr-start")

    async def aclose(self) -> None:
        self._events.append(f"{self.name}-close")


class Bundle:
    def __init__(self, events: list[str]) -> None:
        self.text = Capability("text", events)
        self.vision = Capability("vision", events)
        self.asr = Capability("asr", events)
        self.embedding = Capability("embedding", events)
        self.search = Capability("search", events)
        self.page_reader = None
        self._events = events

    async def aclose(self) -> None:
        self._events.append("providers-close")


class ControlledProcessor:
    def __init__(self, results: list[str], gate: asyncio.Event | None = None) -> None:
        self.results = list(results)
        self.gate = gate
        self.calls = 0
        self.cancelled = False

    async def resolve(self, parsed, **kwargs):
        del kwargs
        self.calls += 1
        try:
            if self.gate is not None:
                await self.gate.wait()
        except asyncio.CancelledError:
            self.cancelled = True
            raise
        return {parsed.refs[0].slot: self.results.pop(0)}

    @staticmethod
    def settled(parsed, resolved) -> bool:
        return all(
            ref.slot in resolved and not isinstance(resolved[ref.slot], Unsettled)
            for ref in parsed.refs
            if not ref.free
        )


def picture_message(raw_event_id: uuid.UUID) -> tuple[ChatMsg, object]:
    parsed = parse_segments(
        [{"type": "image", "data": {"file": "a" * 32 + ".png", "url": "https://x"}}],
        "999",
        limits=config().default.prompt,
        self_name="小X",
    )
    message = ChatMsg(
        msg_id=MessageId(str(raw_event_id)),
        user_id=AccountId("100"),
        nickname="成员",
        text=parsed.render(),
        ts=now_local(),
        raw_event_id=raw_event_id,
        image_refs=parsed.pictures,
    )
    return message, parsed


async def media_tests() -> None:
    original_backfill = media_module.repo.backfill_plain_text
    backfills: list[tuple[str, str]] = []

    async def backfill(message_id, text):
        backfills.append((str(message_id), text))

    media_module.repo.backfill_plain_text = backfill
    try:
        gate = asyncio.Event()
        processor = ControlledProcessor(["⟦图片:猫⟧"], gate)
        coordinator = MediaCoordinator(processor)
        raw_id = uuid.uuid4()
        message, parsed = picture_message(raw_id)
        ticket = coordinator.admit(
            raw_id,
            parsed,
            message,
            bot=object(),
            group_id=message_id_group(),
            cfg=config().default,
        )
        flight = ticket.task
        first = asyncio.create_task(
            coordinator.settle([message], wait_sec=1, who="100")
        )
        second = asyncio.create_task(
            coordinator.settle([message], wait_sec=1, who="100")
        )
        await asyncio.sleep(0)
        first.cancel()
        await asyncio.gather(first, return_exceptions=True)
        check(
            "cancelling one media waiter does not cancel shared work",
            flight is ticket.task and not processor.cancelled,
        )
        gate.set()
        await second
        if flight is not None:
            await flight
        check(
            "two media waiters share one per-message task",
            processor.calls == 1 and ticket.status is MediaStatus.FINAL,
            f"{processor.calls} calls",
        )
        check(
            "a completed media task patches live text and archive",
            "猫" in message.text and backfills[-1][1] == message.text,
        )

        late_gate = asyncio.Event()
        late_processor = ControlledProcessor(["⟦图片:晚到⟧"], late_gate)
        late_coordinator = MediaCoordinator(late_processor)
        late_id = uuid.uuid4()
        late_message, late_parsed = picture_message(late_id)
        late_ticket = late_coordinator.admit(
            late_id,
            late_parsed,
            late_message,
            bot=object(),
            group_id=message_id_group(),
            cfg=config().default,
        )
        await late_coordinator.settle([late_message], wait_sec=0, who="100")
        check("a bounded media wait leaves slow work running", not late_ticket.task.done())
        late_gate.set()
        late_flight = late_ticket.task
        if late_flight is not None:
            await late_flight
        check("late media completion still patches the message", "晚到" in late_message.text)

        retry_processor = ControlledProcessor([
            Unsettled("⟦图片⟧"),
            "⟦图片:重试成功⟧",
        ])
        retry_coordinator = MediaCoordinator(retry_processor)
        retry_id = uuid.uuid4()
        retry_message, retry_parsed = picture_message(retry_id)
        retry_ticket = retry_coordinator.admit(
            retry_id,
            retry_parsed,
            retry_message,
            bot=object(),
            group_id=message_id_group(),
            cfg=config().default,
        )
        first_retry = retry_ticket.task
        if first_retry is not None:
            await first_retry
        check("a transient media result remains retryable",
              retry_ticket.status is MediaStatus.RETRYABLE)
        await retry_coordinator.settle([retry_message], wait_sec=1, who="101")
        check(
            "a later settle advances retryable media to final",
            retry_processor.calls == 2
            and retry_ticket.status is MediaStatus.FINAL
            and "重试成功" in retry_message.text,
        )
    finally:
        media_module.repo.backfill_plain_text = original_backfill


def message_id_group():
    from qqbot.domain.ids import GroupId

    return GroupId("1")


async def runtime_tests() -> None:
    check(
        "core Runtime and scheduled bodies import without NoneBot",
        "nonebot" not in sys.modules,
    )

    one = Runtime.build(config(), providers=Bundle([]))
    two = Runtime.build(config(), providers=Bundle([]))
    check(
        "each Runtime owns one independent resource graph",
        one.gateway is not two.gateway
        and one.media is not two.media
        and one.registry is not two.registry
        and one.delivery is not two.delivery,
    )

    class StageQueue:
        def __init__(self):
            self.filters = []

        async def depth(self, *, job_types=None):
            self.filters.append(job_types)
            return {}

    stage_queue = StageQueue()
    await scheduled_module._drain_wait(
        one,
        stage_queue,
        timedelta(seconds=1),
        "extraction",
        (JobType.EXTRACT_MEMORY,),
    )
    check(
        "nightly waits only for the current stage's job type",
        stage_queue.filters == [(JobType.EXTRACT_MEMORY,)],
        str(stage_queue.filters),
    )

    events: list[str] = []
    runtime = Runtime.build(config(), providers=Bundle(events))
    originals = (
        runtime_module.init_pool,
        runtime_module.repo.ensure_schema,
        runtime_module.close_pool,
    )

    async def init_pool():
        events.append("db-start")

    async def ensure_schema():
        events.append("schema-check")

    async def close_pool():
        events.append("db-close")

    async def worker_loop():
        events.append("worker-start")
        try:
            await asyncio.Event().wait()
        finally:
            events.append("worker-stop")

    async def gateway_close():
        events.append("gateway-close")
        raise RuntimeError("scripted gateway close failure")

    async def coordinator_close(*, timeout):
        del timeout
        events.append("coordinator-close")

    async def processor_close():
        events.append("processor-close")

    runtime_module.init_pool = init_pool
    runtime_module.repo.ensure_schema = ensure_schema
    runtime_module.close_pool = close_pool
    runtime.worker.run_forever = worker_loop
    runtime.gateway.shutdown = gateway_close
    runtime.media.close = coordinator_close
    runtime.media_processor.close = processor_close
    try:
        await runtime.start()
        await asyncio.sleep(0)
        check(
            "ASR is ready before the memory worker starts",
            events.index("asr-start") < events.index("worker-start"),
            str(events),
        )
        await runtime.aclose()
        check(
            "teardown keeps dependency order and isolates close failures",
            events.index("worker-stop") < events.index("gateway-close")
            < events.index("coordinator-close") < events.index("processor-close")
            < events.index("providers-close") < events.index("db-close"),
            str(events),
        )
        before = list(events)
        await runtime.aclose()
        check("Runtime close is idempotent", events == before)
    finally:
        (
            runtime_module.init_pool,
            runtime_module.repo.ensure_schema,
            runtime_module.close_pool,
        ) = originals


async def main() -> int:
    await media_tests()
    await runtime_tests()
    print()
    print("FAILED:", fails if fails else "none")
    return 1 if fails else 0


raise SystemExit(asyncio.run(main()))
