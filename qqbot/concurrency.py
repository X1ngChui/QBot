"""Bounded service-owned shared work, independent of individual waiters."""

import asyncio
from collections.abc import Awaitable, Callable, Hashable
from enum import StrEnum


class Rejection(StrEnum):
    FULL = "full"
    CLOSED = "closed"


class WorkRejected(Exception):
    def __init__(self, reason: Rejection) -> None:
        self.reason = reason
        super().__init__(reason.value)


class SharedWork[K: Hashable, T]:
    """Each key owns one task; cancelling a waiter never releases its slot."""

    def __init__(self, *, capacity: int, timeout: float) -> None:
        if capacity < 1 or timeout <= 0:
            raise ValueError("shared work requires positive capacity and timeout")
        self._capacity = capacity
        self._timeout = timeout
        self._tasks: dict[K, asyncio.Task[T]] = {}
        self._closed = False
        self.high_water = 0

    @property
    def size(self) -> int:
        return len(self._tasks)

    async def run(self, key: K, start: Callable[[], Awaitable[T]]) -> T:
        if self._closed:
            raise WorkRejected(Rejection.CLOSED)
        task = self._tasks.get(key)
        if task is None:
            if len(self._tasks) >= self._capacity:
                raise WorkRejected(Rejection.FULL)
            task = asyncio.create_task(self._execute(start))
            self._tasks[key] = task
            self.high_water = max(self.high_water, len(self._tasks))
            task.add_done_callback(lambda done: self._finished(key, done))
        return await asyncio.shield(task)

    async def _execute(self, start: Callable[[], Awaitable[T]]) -> T:
        async with asyncio.timeout(self._timeout):
            return await start()

    def _finished(self, key: K, task: asyncio.Task[T]) -> None:
        if self._tasks.get(key) is task:
            del self._tasks[key]
        # The last waiter may have left before a failing owner finishes.
        if not task.cancelled():
            task.exception()

    def abort(self) -> None:
        self._closed = True
        for task in self._tasks.values():
            task.cancel()

    async def close(self) -> None:
        self.abort()
        tasks = tuple(self._tasks.values())
        if tasks:
            await asyncio.gather(*tasks, return_exceptions=True)
        self._tasks.clear()
