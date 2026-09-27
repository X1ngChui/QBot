"""Runtime resource ownership, teardown, and per-message media coordination."""

import ast
import asyncio
import uuid
from pathlib import Path
from datetime import timedelta

import pytest

from _budget import fake_budget
import _db as _test_db
from _fixtures import config, now_local
from qqbot.conversation.state import ChatMsg
from qqbot.domain.ids import AccountId, GroupId, MessageId
from qqbot.gateway.segments import parse_segments
from qqbot.media.coordinator import MediaCoordinator, MediaStatus
from qqbot.media.result import Resolution
from qqbot.operations import scheduled as scheduled_module
from qqbot.repositories.job import JobType
from qqbot.runtime import Runtime
import qqbot.runtime as runtime_module


@pytest.fixture(autouse=True)
def fake_database_credentials(monkeypatch):
    monkeypatch.setenv("DATABASE_URL", "postgresql://qbot_test@127.0.0.1:15432/qbot_test")
    monkeypatch.setenv("DATABASE_PASSWORD", "fake-test-password")
    monkeypatch.delenv("DATABASE_PASSWORD_FILE", raising=False)


class Capability:
    attachments = None

    def __init__(self, name, events):
        self.name = name
        self.events = events

    async def start(self):
        self.events.append("asr-start")

    async def aclose(self):
        self.events.append(f"{self.name}-close")


class Bundle:
    def __init__(self, events):
        self.text = Capability("text", events)
        self.vision = Capability("vision", events)
        self.asr = Capability("asr", events)
        self.embedding = Capability("embedding", events)
        self.search = Capability("search", events)
        self.page_reader = None
        self.events = events

    async def aclose(self):
        self.events.append("providers-close")


class FakeLease:
    async def acquire(self, on_lost):
        self.on_lost = on_lost

    async def close(self):
        pass


class ControlledProcessor:
    def __init__(self, results, gate=None):
        self.results = [Resolution(item) if isinstance(item, str) else item for item in results]
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
    def settled(parsed, resolved):
        return all(
            ref.slot in resolved and not resolved[ref.slot].retryable
            for ref in parsed.refs
            if not ref.free
        )


def picture_message(raw_event_id):
    parsed = parse_segments(
        [
            {
                "type": "image",
                "data": {"file": "a" * 32 + ".png", "url": "https://example.invalid/a"},
            }
        ],
        "999",
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


@pytest.mark.asyncio
async def test_cancelling_one_media_waiter_does_not_cancel_shared_work(monkeypatch):
    backfills = []

    async def backfill(message_id, text):
        backfills.append((str(message_id), text))

    monkeypatch.setattr(_test_db.archive, "backfill_plain_text", backfill)
    gate = asyncio.Event()
    processor = ControlledProcessor(["⟦图片:猫⟧"], gate)
    coordinator = MediaCoordinator(processor, budget=fake_budget(), archive=_test_db.archive)
    raw_id = uuid.uuid4()
    message, parsed = picture_message(raw_id)
    try:
        ticket = coordinator.admit(
            raw_id,
            parsed,
            message,
            bot=object(),
            group_id=GroupId("1"),
            cfg=config().default,
        )
        flight = ticket.task
        first = asyncio.create_task(coordinator.settle([message], wait_sec=1, who="100"))
        second = asyncio.create_task(coordinator.settle([message], wait_sec=1, who="100"))
        await asyncio.sleep(0)
        first.cancel()
        await asyncio.gather(first, return_exceptions=True)
        assert flight is ticket.task
        assert not processor.cancelled
        gate.set()
        await second
        if flight is not None:
            await flight
        assert processor.calls == 1
        assert ticket.status is MediaStatus.FINAL
        assert "猫" in message.text
        assert backfills[-1][1] == message.text
    finally:
        gate.set()
        await coordinator.close(timeout=1)


@pytest.mark.asyncio
async def test_media_timeout_allows_late_patch(monkeypatch):
    backfills = []

    async def backfill(message_id, text):
        backfills.append((str(message_id), text))

    monkeypatch.setattr(_test_db.archive, "backfill_plain_text", backfill)
    gate = asyncio.Event()
    processor = ControlledProcessor(["⟦图片:晚到⟧"], gate)
    coordinator = MediaCoordinator(processor, budget=fake_budget(), archive=_test_db.archive)
    raw_id = uuid.uuid4()
    message, parsed = picture_message(raw_id)
    try:
        ticket = coordinator.admit(
            raw_id,
            parsed,
            message,
            bot=object(),
            group_id=GroupId("1"),
            cfg=config().default,
        )
        await coordinator.settle([message], wait_sec=0, who="100")
        assert not ticket.task.done()
        gate.set()
        await ticket.task
        assert "晚到" in message.text
        assert backfills[-1][1] == message.text
    finally:
        gate.set()
        await coordinator.close(timeout=1)


@pytest.mark.asyncio
async def test_retryable_media_result_advances_on_later_settle(monkeypatch):
    async def backfill(message_id, text):
        del message_id, text

    monkeypatch.setattr(_test_db.archive, "backfill_plain_text", backfill)
    processor = ControlledProcessor(
        [
            Resolution("⟦图片⟧", retryable=True),
            "⟦图片:重试成功⟧",
        ]
    )
    coordinator = MediaCoordinator(processor, budget=fake_budget(), archive=_test_db.archive)
    raw_id = uuid.uuid4()
    message, parsed = picture_message(raw_id)
    try:
        ticket = coordinator.admit(
            raw_id,
            parsed,
            message,
            bot=object(),
            group_id=GroupId("1"),
            cfg=config().default,
        )
        if ticket.task is not None:
            await ticket.task
        assert ticket.status is MediaStatus.RETRYABLE
        await coordinator.settle([message], wait_sec=1, who="101")
        assert processor.calls == 2
        assert ticket.status is MediaStatus.FINAL
        assert "重试成功" in message.text
    finally:
        await coordinator.close(timeout=1)


def test_runtime_and_scheduled_modules_have_no_nonebot_imports():
    for module in (runtime_module, scheduled_module):
        source = ast.parse(Path(module.__file__).read_text(encoding="utf-8"))
        assert not any(
            isinstance(node, ast.ImportFrom)
            and (node.module or "").startswith("nonebot")
            or isinstance(node, ast.Import)
            and any(alias.name.startswith("nonebot") for alias in node.names)
            for node in ast.walk(source)
        )


@pytest.mark.asyncio
async def test_runtime_owns_independent_reply_scheduler_and_resource_graph():
    first = Runtime.build(config(), providers=Bundle([]), lease=FakeLease())
    second = Runtime.build(config(), providers=Bundle([]), lease=FakeLease())
    try:
        assert first.gateway is not second.gateway
        assert first.media is not second.media
        assert first.registry is not second.registry
        assert first.delivery is not second.delivery
        assert first.reply_executor is not second.reply_executor
        assert first.replies is not second.replies
        assert first.gateway._replies is first.replies
    finally:
        await first.aclose()
        await second.aclose()


@pytest.mark.asyncio
async def test_scheduled_stage_wait_filters_job_types():
    class StageQueue:
        def __init__(self):
            self.filters = []

        async def depth(self, *, job_types=None):
            self.filters.append(job_types)
            return {}

    queue = StageQueue()
    await scheduled_module._drain_wait(
        queue,
        timedelta(seconds=1),
        "extraction",
        (JobType.EXTRACT_MEMORY,),
    )
    assert queue.filters == [(JobType.EXTRACT_MEMORY,)]


@pytest.mark.asyncio
async def test_startup_and_idempotent_teardown_preserve_dependency_order(monkeypatch):
    events = []
    runtime = Runtime.build(config(), providers=Bundle(events), lease=FakeLease())

    async def init_pool():
        events.append("db-start")

    async def ensure_schema(database):
        del database
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

    monkeypatch.setattr(runtime.database, "start", init_pool)
    monkeypatch.setattr(runtime_module.repo, "ensure_schema", ensure_schema)
    monkeypatch.setattr(runtime.database, "close", close_pool)
    monkeypatch.setattr(runtime.worker, "run_forever", worker_loop)
    monkeypatch.setattr(runtime.gateway, "shutdown", gateway_close)
    monkeypatch.setattr(runtime.media, "close", coordinator_close)
    monkeypatch.setattr(runtime.media_processor, "close", processor_close)
    try:
        await runtime.start()
        await asyncio.sleep(0)
        assert events.index("asr-start") < events.index("worker-start")
        await runtime.aclose()
        assert (
            events.index("gateway-close")
            < events.index("worker-stop")
            < events.index("coordinator-close")
            < events.index("processor-close")
            < events.index("providers-close")
            < events.index("db-close")
        )
        before = list(events)
        await runtime.aclose()
        assert events == before
    finally:
        await runtime.aclose()
