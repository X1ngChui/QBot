"""Claim durable wakeups only after reserving capacity in the shared reply inbox."""

from __future__ import annotations

import asyncio
from collections import deque
from collections.abc import Awaitable, Callable
from datetime import timedelta
import logging

import asyncpg

from qqbot.clock import Clock
from qqbot.configuration import Settings
from qqbot.conversation.scheduler import Admission, ReplyReservation, ReplyScheduler
from qqbot.conversation.session import DueTask, ReplyRequest
from qqbot.domain.ids import GroupId
from qqbot.domain.reply import ReplyEnd, ReplyOutcome
from qqbot.gateway.botapi import BotApi
from qqbot.repositories.scheduled_task import ScheduledTask, ScheduledTaskRepository

log = logging.getLogger("qqbot.operations.tasks")
POLL_SECONDS = 15


class ScheduledTaskWorker:
    def __init__(
        self,
        cfg: Settings,
        replies: ReplyScheduler[ReplyRequest],
        *,
        clock: Clock,
        database: Callable[[], asyncpg.Pool],
        pause: Callable[[float], Awaitable[None]] = asyncio.sleep,
    ) -> None:
        self._cfg = cfg
        self._clock = clock
        self._replies = replies
        self._pause = pause
        self._repo = ScheduledTaskRepository(database=database)
        self._closed = False
        self._starter: asyncio.Task | None = None
        self._poller: asyncio.Task | None = None
        self._active_groups: set[GroupId] = set()
        self._completed: deque[tuple[ReplyRequest, ReplyOutcome]] = deque()
        self._wake = asyncio.Event()

    async def start(self, bot_getter: Callable[[], BotApi | None]) -> None:
        if self._closed:
            raise RuntimeError("scheduled worker is closed")
        if self._starter is None:
            self._starter = asyncio.create_task(self._initialize(bot_getter), name="timer-start")
        await asyncio.shield(self._starter)

    async def _initialize(self, bot_getter: Callable[[], BotApi | None]) -> None:
        await self._repo.interrupt()
        await self._repo.purge(self._cfg.maintenance.completed_task_keep_days)
        if not self._closed:
            self._poller = asyncio.create_task(self._poll(bot_getter), name="timer-poll")

    def abort(self) -> None:
        self._closed = True
        for task in (self._starter, self._poller):
            if task is not None and not task.done():
                task.cancel()

    async def stop(self) -> None:
        self.abort()
        tasks = [task for task in (self._starter, self._poller) if task is not None]
        if tasks:
            await asyncio.gather(*tasks, return_exceptions=True)

    async def close(self) -> None:
        await self.stop()
        await self.flush()

    async def _wait(self) -> None:
        pause = asyncio.ensure_future(self._pause(POLL_SECONDS))
        wake = asyncio.create_task(self._wake.wait())
        try:
            await asyncio.wait((pause, wake), return_when=asyncio.FIRST_COMPLETED)
        finally:
            for task in (pause, wake):
                task.cancel()
            await asyncio.gather(pause, wake, return_exceptions=True)

    async def _poll(self, bot_getter: Callable[[], BotApi | None]) -> None:
        last_purge_day = self._clock.today()
        while not self._closed:
            self._wake.clear()
            try:
                await self.flush()
                if (day := self._clock.today()) != last_purge_day:
                    await self._repo.purge(self._cfg.maintenance.completed_task_keep_days)
                    last_purge_day = day
                if (bot := bot_getter()) is not None:
                    for _ in range(64):
                        if len(self._active_groups) >= self._cfg.backends.text.max_concurrency:
                            break
                        reservation = self._replies.reserve()
                        if reservation is None:
                            break
                        deadline = (
                            asyncio.get_running_loop().time()
                            + self._cfg.conversation.reply_deadline_sec
                        )
                        try:
                            async with asyncio.timeout_at(deadline):
                                local = self._clock.now()
                                day_start = local.replace(hour=0, minute=0, second=0, microsecond=0)
                                task, skipped = await self._repo.claim_due(
                                    self._cfg.tasks,
                                    day_start=day_start,
                                    day_end=day_start + timedelta(days=1),
                                    exclude_groups=tuple(self._active_groups),
                                )
                            if task is not None:
                                self.submit(bot, task, reservation=reservation, deadline=deadline)
                            elif not skipped:
                                break
                        finally:
                            reservation.cancel()
                await self._wait()
            except asyncio.CancelledError:
                raise
            except Exception:
                log.exception("scheduled task poll failed")
                await self._pause(POLL_SECONDS)

    def submit(
        self,
        bot: BotApi,
        task: ScheduledTask,
        *,
        reservation: ReplyReservation[ReplyRequest],
        deadline: float,
    ) -> Admission:
        if task.group_id in self._active_groups:
            raise RuntimeError("a wakeup from this group is already admitted")
        self._active_groups.add(task.group_id)
        request = ReplyRequest(bot, task.group_id, DueTask(task))
        if self._closed:
            reservation.cancel()
            self._finished(request, ReplyOutcome(ReplyEnd.CANCELLED))
            return Admission.CLOSED
        return reservation.submit(
            request,
            deadline=deadline,
            on_finish=self._finished,
        )

    def _finished(self, request: ReplyRequest, outcome: ReplyOutcome) -> None:
        self._completed.append((request, outcome))
        self._wake.set()

    @staticmethod
    def _outcome(result: ReplyOutcome) -> str:
        if result.sent:
            return "sent" if result.end is ReplyEnd.FINISHED else f"sent_{result.end.value}"
        if result.uncertain:
            return "uncertain"
        return "interrupted" if result.end is ReplyEnd.CANCELLED else result.end.value

    async def flush(self) -> None:
        while self._completed:
            request, outcome = self._completed[0]
            assert isinstance(request.cause, DueTask)
            failed = outcome.end in {
                ReplyEnd.FAILED,
                ReplyEnd.TIMEOUT,
                ReplyEnd.UNOBSERVED,
                ReplyEnd.CANCELLED,
            }
            async with asyncio.timeout(5):
                await self._repo.finish(
                    request.cause.task.id, self._outcome(outcome), failed=failed
                )
            self._completed.popleft()
            self._active_groups.discard(request.group_id)
