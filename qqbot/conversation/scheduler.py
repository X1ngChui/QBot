"""Bounded reply admission with one absolute deadline and isolated executions."""

from __future__ import annotations

import asyncio
import logging
from collections import Counter, deque
from collections.abc import Awaitable, Callable
from dataclasses import dataclass, field
from enum import StrEnum

from qqbot.domain.reply import ReplyEnd, ReplyOutcome, ReplyProgress

log = logging.getLogger("qqbot.reply_scheduler")


class Admission(StrEnum):
    ACCEPTED = "accepted"
    OVERLOADED = "overloaded"
    EXPIRED = "expired"
    CLOSED = "closed"


@dataclass(slots=True)
class ReplyWork[T]:
    value: T
    deadline: float
    progress: ReplyProgress = field(default_factory=ReplyProgress)
    on_finish: Callable[[T, ReplyOutcome], None] | None = None


class ReplyReservation[T]:
    def __init__(self, owner: ReplyScheduler[T]) -> None:
        self._owner = owner
        self._used = False

    def submit(
        self,
        value: T,
        *,
        deadline: float,
        on_finish: Callable[[T, ReplyOutcome], None] | None = None,
    ) -> Admission:
        if self._used:
            raise RuntimeError("reply reservation has already been released")
        self._used = True
        return self._owner._submit(
            ReplyWork(value, deadline, on_finish=on_finish), reservation=self
        )

    def cancel(self) -> None:
        self._used = True
        self._owner._reservations.discard(self)
        if not self._owner.size:
            self._owner._idle.set()


class ReplyScheduler[T]:
    """Capacity covers active and waiting work; waiters do not own asyncio tasks."""

    def __init__(
        self,
        execute: Callable[[ReplyWork[T]], Awaitable[ReplyOutcome]],
        *,
        capacity: int,
        concurrency: int,
        on_finish: Callable[[T, ReplyOutcome], None] | None = None,
    ) -> None:
        if capacity < 1 or concurrency < 1:
            raise ValueError("reply capacity and concurrency must be positive")
        self._execute = execute
        self._capacity = capacity
        self._concurrency = min(capacity, concurrency)
        self._on_finish = on_finish
        self._pending: deque[ReplyWork[T]] = deque()
        self._reservations: set[ReplyReservation[T]] = set()
        self._running: dict[asyncio.Task[ReplyOutcome], ReplyWork[T]] = {}
        self._pump_task: asyncio.Task[None] | None = None
        self._wake = asyncio.Event()
        self._idle = asyncio.Event()
        self._idle.set()
        self._closed = False
        self.outcomes: Counter[ReplyEnd] = Counter()
        self.high_water = 0

    @property
    def size(self) -> int:
        return len(self._pending) + len(self._running) + len(self._reservations)

    @property
    def active(self) -> int:
        return len(self._running)

    def _publish(self, work: ReplyWork[T], outcome: ReplyOutcome) -> None:
        self.outcomes[outcome.end] += 1
        for callback in (work.on_finish, self._on_finish):
            if callback is not None:
                try:
                    callback(work.value, outcome)
                except Exception:
                    log.exception("reply completion observer failed")

    def _expire(self, now: float) -> None:
        alive: deque[ReplyWork[T]] = deque()
        expired: list[ReplyWork[T]] = []
        for work in self._pending:
            (expired if work.deadline <= now else alive).append(work)
        self._pending = alive
        for work in expired:
            self._publish(work, work.progress.finish(ReplyEnd.EXPIRED))
        if not self.size:
            self._idle.set()

    def reserve(self) -> ReplyReservation[T] | None:
        if self._closed:
            return None
        self._expire(asyncio.get_running_loop().time())
        if self._closed or self.size >= self._capacity:
            return None
        reservation = ReplyReservation(self)
        self._reservations.add(reservation)
        self._idle.clear()
        self.high_water = max(self.high_water, self.size)
        return reservation

    def submit(
        self,
        value: T,
        *,
        deadline: float,
        on_finish: Callable[[T, ReplyOutcome], None] | None = None,
    ) -> Admission:
        return self._submit(ReplyWork(value, deadline, on_finish=on_finish))

    def _submit(
        self, work: ReplyWork[T], *, reservation: ReplyReservation[T] | None = None
    ) -> Admission:
        now = asyncio.get_running_loop().time()
        if not self._closed:
            self._expire(now)
        if reservation is not None:
            self._reservations.discard(reservation)
        if self._closed:
            admission, end = Admission.CLOSED, ReplyEnd.CANCELLED
        elif work.deadline <= now:
            admission, end = Admission.EXPIRED, ReplyEnd.EXPIRED
        elif self.size >= self._capacity:
            admission, end = Admission.OVERLOADED, ReplyEnd.OVERLOADED
        else:
            self._pending.append(work)
            self._idle.clear()
            self.high_water = max(self.high_water, self.size)
            self._wake.set()
            if self._pump_task is None:
                self._pump_task = asyncio.create_task(self._pump(), name="reply-dispatch")
            return Admission.ACCEPTED
        self._publish(work, work.progress.finish(end))
        if not self.size:
            self._idle.set()
        return admission

    async def _run(self, work: ReplyWork[T]) -> ReplyOutcome:
        try:
            async with asyncio.timeout_at(work.deadline):
                return await self._execute(work)
        except TimeoutError:
            return work.progress.finish(ReplyEnd.TIMEOUT)
        except asyncio.CancelledError:
            return work.progress.finish(ReplyEnd.CANCELLED)
        except Exception:
            log.exception("reply execution failed")
            return work.progress.finish(ReplyEnd.FAILED)

    def _finished(self, task: asyncio.Task[ReplyOutcome]) -> None:
        work = self._running.pop(task)
        outcome = work.progress.finish(ReplyEnd.CANCELLED) if task.cancelled() else task.result()
        self._publish(work, outcome)
        if not self.size:
            self._idle.set()
        self._wake.set()

    async def _pump(self) -> None:
        loop = asyncio.get_running_loop()
        while not self._closed:
            self._wake.clear()
            self._expire(loop.time())
            while self._pending and len(self._running) < self._concurrency:
                work = self._pending.popleft()
                task = asyncio.create_task(self._run(work), name="reply-session")
                self._running[task] = work
                task.add_done_callback(self._finished)
            nearest = min((item.deadline for item in self._pending), default=None)
            try:
                async with asyncio.timeout_at(nearest):
                    await self._wake.wait()
            except TimeoutError:
                pass

    async def drain(self) -> None:
        """Wait for accepted work, not for the lifetime of the dispatcher."""
        while self.size:
            await self._idle.wait()

    def abort(self) -> None:
        """Synchronously revoke admission and cancel work before yielding control."""
        self._closed = True
        self._reservations.clear()
        if self._pump_task is not None:
            self._pump_task.cancel()
        while self._pending:
            work = self._pending.popleft()
            self._publish(work, work.progress.finish(ReplyEnd.CANCELLED))
        for task in self._running:
            task.cancel()
        if not self.size:
            self._idle.set()

    async def close(self) -> None:
        """Join every owned task, including work already aborted after lease loss."""
        self.abort()
        pump, self._pump_task = self._pump_task, None
        tasks: list[asyncio.Task[object]] = [*self._running]
        if pump is not None:
            tasks.append(pump)
        if tasks:
            await asyncio.gather(*tasks, return_exceptions=True)
        self._idle.set()
