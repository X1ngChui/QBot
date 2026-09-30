"""Durable scheduling, account isolation, and fresh-context wakeups."""

from __future__ import annotations

import pytest
import asyncio
import json
from datetime import UTC, datetime, timedelta
from types import SimpleNamespace


import _db as _test_db
from qqbot.conversation import engine
from qqbot.conversation import prompt
from qqbot.conversation import tools
from qqbot.delivery.service import GroupDelivery
from qqbot.conversation.member_numbers import MemberNumbers
from _test_owners import fresh_budget, fresh_members, fresh_tasks
from qqbot.conversation.history import HistoryWindow
from qqbot.conversation.state import ChatMsg
from qqbot.conversation.state import GroupState
from _db import pool
from qqbot.domain.reply import ReplyEnd, ReplyOutcome
from qqbot.domain.ids import AccountId, GroupId, MessageId
from qqbot.repositories.scheduled_task import ScheduledTaskRepository, TaskLimit
from _fixtures import config
from _fixtures import now_local
from qqbot.workers.scheduled import ScheduledTaskWorker
from qqbot.conversation.scheduler import ReplyScheduler
from qqbot.conversation.session import ReplyExecutor
from qqbot.providers.contracts import ModelTurn, ToolCall, ToolCallId


BUDGET = None
MEMBERS = None


class FakeState:
    muted = False
    history_anchor = None

    def __init__(self) -> None:
        self.history = HistoryWindow()
        self.recent: list[ChatMsg] = []
        self.loaded = False
        self.blocked = False

    async def load_history(self, *, self_id: str, owners) -> None:
        self.loaded = True
        self.recent.append(
            ChatMsg(
                msg_id=MessageId("500"),
                user_id=AccountId("222"),
                nickname="Member",
                text="The new status is ready",
                ts=now_local(),
            )
        )

    async def blocked_now(self, creator_id: str) -> bool:
        return self.blocked


@pytest.fixture
async def scheduled_state(test_database, monkeypatch):
    monkeypatch.setitem(globals(), "BUDGET", fresh_budget())
    monkeypatch.setitem(globals(), "MEMBERS", fresh_members())
    try:
        yield
    finally:
        await MEMBERS.close()


@pytest.mark.database
@pytest.mark.asyncio
async def test_scheduled(scheduled_state, monkeypatch):
    repo = ScheduledTaskRepository(database=_test_db.pool)
    cfg = config().default
    group, other_group = GroupId("881221"), GroupId("881222")
    async with pool().acquire() as conn:
        await conn.execute(
            "DELETE FROM scheduled_task WHERE group_id=ANY($1::bigint[])",
            [group.to_db(), other_group.to_db()],
        )
    due = datetime.now(UTC) + timedelta(minutes=10)
    first = await repo.create(group, "Check tomorrow's status", due, cfg.tasks)
    assert first.id == first.chain_id, "a group task stores a unique chain"
    assert not await repo.cancel(first.id, other_group), "another group cannot cancel a task"
    assert len((await repo.active(group)).items) == 1 and await repo.cancel(first.id, group), (
        "group management does not require an initiating account"
    )

    root = await repo.create(group, "Wait for the decision", due, cfg.tasks)
    child = await repo.create(group, "Recheck the decision", due, cfg.tasks, parent=root)
    assert child.chain_id == root.id and child.chain_depth == 1, (
        "a follow-up inherits its original group chain"
    )
    try:
        await repo.create(
            group,
            "Keep checking",
            due,
            cfg.tasks.model_copy(update={"max_chain_depth": 1}),
            parent=child,
        )
        bounded = False
    except TaskLimit:
        bounded = True
    assert bounded, "chain depth stops unbounded self-renewal"

    history = ChatMsg(
        msg_id=MessageId("42"),
        user_id=AccountId("222"),
        nickname="Member",
        text="The plan changed today",
        ts=now_local(),
    )
    nums, marks = prompt.numbered([history])
    people = MemberNumbers(self_id="999", lookup=_test_db.identities.holder_ids_for_accounts)
    people.number("222", spoke=True)
    packet = prompt.assemble(
        persona=config().persona_for(group),
        cfg=cfg,
        st=GroupState(
            group,
            groups=_test_db.groups,
            archive=_test_db.archive,
            display_zone=_test_db.clock.zone,
        ),
        msg=None,
        task_intent="Check whether the plan is still current",
        task_id=str(root.id),
        task_due_at=root.due_at.isoformat(),
        profiles=[],
        window=[history],
        nums=nums,
        marks=marks,
        people=people,
        prompts=_test_db.test_bundle().prompts,
        clock=_test_db.clock,
    )
    assert (
        "The plan changed today" in str(packet[-2])
        and "Check whether the plan is still current" in str(packet[-1])
        and str(root.id) in str(packet[-1])
        and "#2" not in str(packet[-1])
    ), "scheduled prompt sees fresh history and no fabricated message number"

    original_recent = _test_db.archive.recent
    gate = asyncio.Event()
    loading = asyncio.Event()
    reads = 0

    async def slow_recent(*_args, **_kwargs):
        nonlocal reads
        reads += 1
        loading.set()
        await gate.wait()
        return []

    monkeypatch.setattr(_test_db.archive, "recent", slow_recent)
    live_state = GroupState(
        group, groups=_test_db.groups, archive=_test_db.archive, display_zone=_test_db.clock.zone
    )
    try:
        first_load = asyncio.create_task(live_state.load_history(self_id="999", owners=[]))
        await loading.wait()
        second_load = asyncio.create_task(live_state.load_history(self_id="999", owners=[]))
        await asyncio.sleep(0)
        assert not second_load.done(), "another wakeup waits for complete archive hydration"
        gate.set()
        await asyncio.gather(first_load, second_load)
        assert reads == 1 and live_state.history_loaded, (
            "concurrent history loaders share one complete snapshot"
        )
        gate.clear()
        loading.clear()
        interrupted_state = GroupState(
            other_group,
            groups=_test_db.groups,
            archive=_test_db.archive,
            display_zone=_test_db.clock.zone,
        )
        interrupted_load = asyncio.create_task(
            interrupted_state.load_history(self_id="999", owners=[])
        )
        await loading.wait()
        interrupted_load.cancel()
        await asyncio.gather(interrupted_load, return_exceptions=True)
        assert not interrupted_state.history_loaded, "cancelled history hydration stays retryable"
        gate.set()
        await interrupted_state.load_history(self_id="999", owners=[])
        assert interrupted_state.history_loaded, "next load can recover after cancellation"
    finally:
        _test_db.archive.recent = original_recent

    ctx = tools.ToolCtx(
        providers=object(),
        media=object(),
        tasks=fresh_tasks(),
        identities=_test_db.identities,
        database=_test_db.pool,
        clock=_test_db.clock,
    )
    call = ToolCall(
        ToolCallId("schedule"),
        "schedule_task",
        '{"intent":"Check the fixture","delay_seconds":600}',
    )
    result = await tools.execute(
        call, cfg=cfg, group_id=other_group, ctx=ctx, prompts=_test_db.test_bundle().prompts
    )
    assert json.loads(str(result))["ok"] and len((await repo.active(other_group)).items) == 1, (
        "schedule tool binds only its group outside model arguments"
    )
    bad = ToolCall(
        ToolCallId("bad"), "schedule_task", '{"intent":"Wrong time","run_at":"2030-01-01T09:00:00"}'
    )
    assert "必须" in str(
        await tools.execute(
            bad, cfg=cfg, group_id=other_group, ctx=ctx, prompts=_test_db.test_bundle().prompts
        )
    ), "naive wall times cannot create a task"
    forged = ToolCall(
        ToolCallId("forged"),
        "schedule_task",
        '{"intent":"Do not accept fake scope","delay_seconds":600,"creator_id":"999"}',
    )
    rejected = await tools.execute(
        forged, cfg=cfg, group_id=other_group, ctx=ctx, prompts=_test_db.test_bundle().prompts
    )
    assert (
        isinstance(rejected, tools.Failure) and len((await repo.active(other_group)).items) == 1
    ), "a model cannot supply an account or group scope"
    assert await repo.cancel(
        (await repo.active(other_group)).items[0].id,
        other_group,
    ), "any task operation is group-owned, not creator-owned"

    limited_group = GroupId("881223")
    await pool().execute("DELETE FROM scheduled_task WHERE group_id=$1", limited_group.to_db())
    one_pending = cfg.tasks.model_copy(update={"max_pending_per_group": 1})

    async def offer():
        try:
            return await repo.create(limited_group, "Wait for confirmation", due, one_pending)
        except TaskLimit:
            return None

    concurrent = await asyncio.gather(offer(), offer())
    assert sum(task is not None for task in concurrent) == 1, (
        "concurrent creates respect the group pending limit"
    )

    daily_group = GroupId("881224")
    await pool().execute("DELETE FROM scheduled_task WHERE group_id=$1", daily_group.to_db())
    for _ in range(2):
        await repo.create(daily_group, "Check once", due, cfg.tasks)
    await pool().execute(
        "UPDATE scheduled_task SET due_at=now()-interval '1 second' WHERE group_id=$1",
        daily_group.to_db(),
    )
    now = now_local()
    start = now.replace(hour=0, minute=0, second=0, microsecond=0)
    once = cfg.tasks.model_copy(update={"max_executions_per_group_day": 1})
    chosen, _ = await repo.claim_due(once, day_start=start, day_end=start + timedelta(days=1))
    await repo.finish(chosen.id, "silent")
    second_claim, skipped = await repo.claim_due(
        once, day_start=start, day_end=start + timedelta(days=1)
    )
    assert (
        second_claim is None
        and skipped
        and await pool().fetchval(
            "SELECT count(*) FROM scheduled_task WHERE group_id=$1 AND outcome='daily_limit'",
            daily_group.to_db(),
        )
        == 1
    ), "the daily group cap drops extra due tasks without invoking a model"

    await pool().execute(
        "UPDATE scheduled_task SET due_at=now()-interval '1 second' WHERE id=$1", root.id
    )
    now = now_local()
    start = now.replace(hour=0, minute=0, second=0, microsecond=0)
    claims = await asyncio.gather(
        *(
            ScheduledTaskRepository(database=_test_db.pool).claim_due(
                cfg.tasks, day_start=start, day_end=start + timedelta(days=1)
            )
            for _ in range(2)
        )
    )
    assert sum(task is not None for task, _ in claims) == 1, "competing claimers run one task only"
    await repo.finish(root.id, "silent")
    assert not (
        await repo.claim_due(cfg.tasks, day_start=start, day_end=start + timedelta(days=1))
    )[0], "completed tasks are never reclaimed"

    wake = await repo.create(group, "Use the latest group context", due, cfg.tasks)
    await pool().execute(
        "UPDATE scheduled_task SET due_at=now()-interval '1 second' WHERE id=$1", wake.id
    )
    state = FakeState()
    seen: list[tuple] = []

    async def respond(**kwargs):
        seen.append(
            (kwargs["msg"], kwargs["scheduled"].intent, tuple(m.text for m in kwargs["window"]))
        )
        return ReplyOutcome(ReplyEnd.FINISHED, acknowledged=1, observed=1)

    original_respond = engine.respond
    monkeypatch.setattr(engine, "respond", respond)

    async def fast_pause(seconds):
        await asyncio.sleep(0.01)

    def make_runner():
        executor = ReplyExecutor(
            tasks=fresh_tasks(),
            bundle=_test_db.bundle_for_settings(cfg),
            clock=_test_db.clock,
            database=_test_db.pool,
            identities=_test_db.identities,
            evidence_store=_test_db.evidence,
            archive=_test_db.archive,
            budget=BUDGET,
            members=MEMBERS,
            registry=SimpleNamespace(get=lambda _: asyncio.sleep(0, result=state)),
            delivery=object(),
            providers=object(),
            directory=object(),
            media=SimpleNamespace(
                settle=lambda *args, **kwargs: asyncio.sleep(0), processor=object()
            ),
        )
        replies = ReplyScheduler(
            executor,
            capacity=cfg.runtime.reply_capacity,
            concurrency=cfg.backends.text.max_concurrency,
        )
        return ScheduledTaskWorker(
            cfg,
            replies,
            pause=fast_pause,
            database=_test_db.pool,
            clock=_test_db.clock,
        )

    async def close_runner(service):
        await service.stop()
        await service._replies.close()
        await service.close()

    async def fire(service, bot, task):
        reservation = service._replies.reserve()
        assert reservation is not None
        service.submit(
            bot,
            task,
            reservation=reservation,
            deadline=asyncio.get_running_loop().time() + cfg.conversation.reply_deadline_sec,
        )
        await service._replies.drain()
        await service.flush()

    runner = make_runner()
    try:
        await runner.start(lambda: None)
        await asyncio.sleep(0.04)
        assert (
            await pool().fetchval("SELECT status FROM scheduled_task WHERE id=$1", wake.id)
            == "pending"
        ), "an offline bot leaves the due task pending"
        await close_runner(runner)
        runner = make_runner()
        await runner.start(lambda: SimpleNamespace(self_id="999"))
        for _ in range(50):
            if seen:
                break
            await asyncio.sleep(0.02)
        await close_runner(runner)
        assert state.loaded and seen == [
            (None, "Use the latest group context", ("The new status is ready",))
        ], "wake-up rebuilds current group history without a fake inbound message"
        assert (
            await pool().fetchval("SELECT status FROM scheduled_task WHERE id=$1", wake.id)
            == "done"
        ), "successful wake-up is completed once"

        runner = make_runner()
        blocked = await repo.create(other_group, "Do not reply while blocked", due, cfg.tasks)
        await pool().execute(
            "UPDATE scheduled_task SET due_at=now()-interval '1 second' WHERE id=$1",
            blocked.id,
        )
        state.blocked = True
        claimed, _ = await repo.claim_due(
            cfg.tasks, day_start=start, day_end=start + timedelta(days=1)
        )
        await fire(runner, SimpleNamespace(self_id="999"), claimed)
        assert (
            len(seen) == 2
            and await pool().fetchval("SELECT outcome FROM scheduled_task WHERE id=$1", blocked.id)
            == "sent"
        ), "a group wakeup is independent of member blocking"
        state.blocked = False

        muted = await repo.create(other_group, "Do not reply while muted", due, cfg.tasks)
        await pool().execute(
            "UPDATE scheduled_task SET due_at=now()-interval '1 second' WHERE id=$1",
            muted.id,
        )
        state.muted = True
        claimed, _ = await repo.claim_due(
            cfg.tasks, day_start=start, day_end=start + timedelta(days=1)
        )
        await fire(runner, SimpleNamespace(self_id="999"), claimed)
        assert (
            len(seen) == 2
            and await pool().fetchval("SELECT outcome FROM scheduled_task WHERE id=$1", muted.id)
            == "muted"
        ), "a muted group cannot trigger a paid reply"
        state.muted = False

        capped = await repo.create(other_group, "Do not spend after the cap", due, cfg.tasks)
        await pool().execute(
            "UPDATE scheduled_task SET due_at=now()-interval '1 second' WHERE id=$1",
            capped.id,
        )
        claimed, _ = await repo.claim_due(
            cfg.tasks, day_start=start, day_end=start + timedelta(days=1)
        )
        original_exceeded = BUDGET.exceeded

        async def over_cap():
            return True

        monkeypatch.setattr(BUDGET, "exceeded", over_cap)
        try:
            await fire(runner, SimpleNamespace(self_id="999"), claimed)
        finally:
            BUDGET.exceeded = original_exceeded
        assert (
            len(seen) == 2
            and await pool().fetchval("SELECT outcome FROM scheduled_task WHERE id=$1", capped.id)
            == "budget"
        ), "the daily cost cap suppresses a due task before model spending"

        interrupted = await repo.create(other_group, "Do not retry after crash", due, cfg.tasks)
        await pool().execute(
            "UPDATE scheduled_task SET due_at=now()-interval '1 second' WHERE id=$1",
            interrupted.id,
        )
        await repo.claim_due(cfg.tasks, day_start=start, day_end=start + timedelta(days=1))
        await repo.interrupt()
        assert (
            await pool().fetchval("SELECT status FROM scheduled_task WHERE id=$1", interrupted.id)
            == "failed"
            and (
                await repo.claim_due(cfg.tasks, day_start=start, day_end=start + timedelta(days=1))
            )[0]
            is None
        ), "a claimed task interrupted by restart is never retried"

        engine.respond = original_respond
        model_prompts = []
        sent_batches = []

        class TextSession:
            async def __aenter__(self):
                return self

            async def __aexit__(self, *_):
                return None

            async def start(self):
                return ModelTurn(
                    tool_calls=(
                        ToolCall(
                            ToolCallId("scheduled-send"),
                            "send_message",
                            '{"content":[{"type":"text","data":{"text":"Updated now"}}]}',
                        ),
                    )
                )

            async def continue_with(self, results, *, directive=None):
                return ModelTurn(
                    tool_calls=(ToolCall(ToolCallId("scheduled-finish"), "finish_reply", "{}"),)
                )

        class TextModel:
            def open_session(self, request):
                model_prompts.append(request.prompt)
                return TextSession()

        class Bot:
            self_id = "999"

            async def send_group_msg(self, *, group_id, message):
                sent_batches.append((group_id, message))
                own = ChatMsg(
                    msg_id=MessageId("99100"),
                    user_id=AccountId(self.self_id),
                    nickname="Bot",
                    text="Updated now",
                    ts=now_local(),
                    is_bot=True,
                )
                genuine_state.add(own)
                delivery.echo.publish(self.self_id, group, own)
                return {"message_id": 99100}

        original_gather = engine.retrieval.gather
        original_knowledge = engine.retrieval.group_knowledge
        original_relabel = MEMBERS.relabel

        async def no_profiles(**_kwargs):
            return []

        async def no_knowledge(*_args, **_kwargs):
            return []

        async def no_relabel(*_args):
            return None

        monkeypatch.setattr(engine.retrieval, "gather", no_profiles)
        monkeypatch.setattr(engine.retrieval, "group_knowledge", no_knowledge)
        monkeypatch.setattr(MEMBERS, "relabel", no_relabel)
        try:
            current = ChatMsg(
                msg_id=MessageId("501"),
                user_id=AccountId("222"),
                nickname="Member",
                text="Updated just now",
                ts=now_local(),
            )
            genuine_state = GroupState(
                group_id=group,
                recent=[current],
                groups=_test_db.groups,
                archive=_test_db.archive,
                display_zone=_test_db.clock.zone,
            )
            delivery = GroupDelivery()
            delivered = await engine.respond(
                tasks=fresh_tasks(),
                bot=Bot(),
                st=genuine_state,
                cfg=cfg,
                persona=config().persona_for(group),
                msg=None,
                scheduled=wake,
                providers=SimpleNamespace(text=TextModel()),
                media=object(),
                delivery=delivery,
                directory=object(),
                window=[current],
                budget=BUDGET,
                members=MEMBERS,
                database=_test_db.pool,
                identities=_test_db.identities,
                evidence_store=_test_db.evidence,
                archive=_test_db.archive,
                clock=_test_db.clock,
                prompts=_test_db.test_bundle().prompts,
            )
            assert (
                delivered.sent
                and len(sent_batches) == 1
                and sent_batches[0][1][0]["data"]["text"] == "Updated now"
            ), "the real agent session sends from a scheduled wakeup"
            assert (
                str(wake.id) in str(model_prompts[0][-1])
                and "Updated just now" in str(model_prompts[0][-2])
                and "Use the latest group context" in str(model_prompts[0][-1])
            ), "the due model request binds its group task and fresh history"
        finally:
            engine.retrieval.gather = original_gather
            engine.retrieval.group_knowledge = original_knowledge
            MEMBERS.relabel = original_relabel
    finally:
        engine.respond = original_respond
        await close_runner(runner)
