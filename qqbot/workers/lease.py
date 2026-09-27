"""Keep durable execution alive, and cancel it when renewal becomes uncertain."""

import asyncio
from collections.abc import Awaitable, Callable
from datetime import timedelta

from qqbot.repositories.job import Job, JobQueue, LeaseLost


class Deferred(Exception):
    def __init__(self, reason: str, *, delay: timedelta) -> None:
        super().__init__(reason)
        self.delay = delay


class ClaimLease:
    def __init__(
        self,
        queue: JobQueue,
        job: Job,
        *,
        duration: timedelta = JobQueue.LEASE,
        pause: Callable[[float], Awaitable[None]] = asyncio.sleep,
    ) -> None:
        if duration <= timedelta():
            raise ValueError("lease duration must be positive")
        self._queue = queue
        self._job = job
        self._duration = duration
        self._pause = pause

    async def _renew(self) -> None:
        try:
            renewed = await self._queue.renew(self._job, lease=self._duration)
        except Exception as exc:
            raise LeaseLost("job lease renewal could not be confirmed") from exc
        if not renewed:
            raise LeaseLost("job lease has expired or changed owner")

    async def _heartbeat(self) -> None:
        while True:
            await self._pause(self._duration.total_seconds() / 3)
            await self._renew()

    async def run[T](self, execute: Callable[[], Awaitable[T]]) -> T:
        await self._renew()

        async def body() -> T:
            return await execute()

        renewal = asyncio.create_task(self._heartbeat())
        operation = asyncio.create_task(body())
        try:
            finished, _ = await asyncio.wait(
                (renewal, operation),
                return_when=asyncio.FIRST_COMPLETED,
            )
            if renewal in finished:
                await renewal
            return await operation
        finally:
            for task in (renewal, operation):
                if not task.done():
                    task.cancel()
            await asyncio.gather(renewal, operation, return_exceptions=True)
