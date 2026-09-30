"""Process composition root and sole owner of startup, tasks and resource teardown."""

from __future__ import annotations

import asyncio
import logging
from collections.abc import Awaitable, Callable
from contextlib import AsyncExitStack
from dataclasses import dataclass, field

import asyncpg

from qqbot.clock import Clock
from qqbot.commands.router import CommandRouter
from qqbot.configuration import ConfigBundle
from qqbot.conversation.state import Registry
from qqbot.conversation.scheduler import ReplyScheduler
from qqbot.conversation.session import ReplyExecutor, ReplyRequest
from qqbot.db import repo
from qqbot.db.connection import Database
from qqbot.repositories.groups import GroupRepository
from qqbot.repositories.media_cache import MediaCacheRepository
from qqbot.repositories.evidence import EvidenceRepository
from qqbot.repositories.archive import ArchiveRepository
from qqbot.repositories.ledger import LedgerRepository
from qqbot.services.budget import Budget
from qqbot.services.members import MemberDirectory
from qqbot.services.identity_limits import CHALLENGE_LIMITS
from qqbot.operations.limits import DIAGNOSTIC_LIMITS
from qqbot.db.lease import RuntimeLease
from qqbot.db.pool import dsn
from qqbot.delivery.service import GroupDelivery
from qqbot.gateway import nickname
from qqbot.gateway.ingest import Ingestor
from qqbot.gateway.pipeline import Gateway
from qqbot.media.coordinator import MediaCoordinator
from qqbot.media.limits import MEDIA_IO
from qqbot.media.service import MediaProcessor
from qqbot.operations import errors
from qqbot.providers.base import Providers
from qqbot.providers.registry import build as build_providers
from qqbot.repositories import IdentityLinkRepository, IdentityRepository
from qqbot.services import Directory, IdentityLinkService, IdentityResolver, retrieval
from qqbot.workers import MemoryWorker
from qqbot.services.scheduled_tasks import ScheduledTaskService
from qqbot.repositories.scheduled_task import ScheduledTaskRepository
from qqbot.workers.scheduled import ScheduledTaskWorker

log = logging.getLogger("qqbot.runtime")


@dataclass(slots=True)
class Runtime:
    lease: RuntimeLease
    bundle: ConfigBundle
    clock: Clock
    diagnostics: errors.ErrorRing
    database: Database
    groups: GroupRepository
    media_cache: MediaCacheRepository
    evidence: EvidenceRepository
    archive: ArchiveRepository
    identities: IdentityRepository
    budget: Budget
    tasks: ScheduledTaskService
    members: MemberDirectory
    providers: Providers
    directory: Directory
    links: IdentityLinkService
    ingestor: Ingestor
    registry: Registry
    delivery: GroupDelivery
    media_processor: MediaProcessor
    media: MediaCoordinator
    router: CommandRouter
    gateway: Gateway
    reply_executor: ReplyExecutor
    replies: ReplyScheduler[ReplyRequest]
    worker: MemoryWorker
    scheduled: ScheduledTaskWorker
    _worker_task: asyncio.Task | None = field(default=None, init=False, repr=False)
    _started: bool = field(default=False, init=False, repr=False)
    _closed: bool = field(default=False, init=False, repr=False)
    _stack: AsyncExitStack = field(default_factory=AsyncExitStack, init=False, repr=False)
    _start_task: asyncio.Task | None = field(default=None, init=False, repr=False)
    _close_task: asyncio.Task | None = field(default=None, init=False, repr=False)
    _maintenance: set[asyncio.Task] = field(default_factory=set, init=False, repr=False)
    _registrations: list[Callable[[], None]] = field(default_factory=list, init=False, repr=False)

    def __post_init__(self) -> None:
        callbacks = (
            ("database pool", self._close_database),
            ("runtime lease", lambda: self.lease.close()),
            ("providers", lambda: self.providers.aclose()),
            ("member directory", lambda: self.members.close()),
            ("roster cache", lambda: self.directory.close()),
            ("media processor", lambda: self.media_processor.close()),
            ("media coordinator", lambda: self.media.close(timeout=MEDIA_IO.shutdown_wait_sec)),
            ("own observation", self._close_observation),
            ("memory worker", self._stop_worker),
            ("scheduled tasks", lambda: self.scheduled.close()),
            ("reply scheduler", lambda: self.replies.close()),
            ("timer polling", lambda: self.scheduled.stop()),
            ("gateway", lambda: self.gateway.shutdown()),
            ("maintenance", self._stop_maintenance),
        )
        for name, close in callbacks:
            self._stack.push_async_callback(self._close_step, name, close)

    @classmethod
    def build(
        cls,
        bundle: ConfigBundle,
        *,
        providers: Providers | None = None,
        lease: RuntimeLease | None = None,
    ) -> Runtime:
        """Compose one owned object graph without starting background work."""
        clock = Clock(bundle.default.bot.timezone)
        diagnostics = errors.ErrorRing(
            clock=clock,
            entries=DIAGNOSTIC_LIMITS.error_ring_entries,
            message_chars=DIAGNOSTIC_LIMITS.error_message_chars,
        )
        database_url = dsn()
        database = Database(bundle.default.runtime.database, url=database_url)
        pool = database.pool
        groups = GroupRepository(pool, clock=clock)
        media_cache = MediaCacheRepository(pool, clock=clock)
        evidence = EvidenceRepository(pool)
        archive = ArchiveRepository(database=pool)
        budget = Budget(
            LedgerRepository(pool, today=clock.today),
            daily_cap=bundle.default.budget.daily_cny_cap,
            today=clock.today,
        )
        tasks = ScheduledTaskService(ScheduledTaskRepository(database=pool), clock=clock)
        capabilities = providers or build_providers(bundle.default, budget)
        members = MemberDirectory()
        identities = IdentityRepository(database=pool, clock=clock)
        resolver = IdentityResolver(identities)
        directory = retrieval.build_directory(
            database=pool,
            identities=identities,
            resolver=resolver,
            predicates=bundle.predicates,
            clock=clock,
        )
        links = IdentityLinkService(
            CHALLENGE_LIMITS,
            resolver,
            identities,
            IdentityLinkRepository(database=pool),
            clock=clock,
        )
        ingestor = Ingestor(resolver, database=pool)
        registry = Registry(
            groups=groups,
            archive=archive,
            bundle=bundle,
            clock=clock,
            history_messages=bundle.default.conversation.history_messages,
        )
        delivery = GroupDelivery()
        media_processor = MediaProcessor(
            capabilities,
            directory,
            budget=budget,
            members=members,
            cache=media_cache,
            prompts=bundle.prompts,
        )
        media = MediaCoordinator(media_processor, budget=budget, archive=archive)
        router = CommandRouter(
            delivery,
            registry,
            directory,
            links,
            capabilities,
            budget,
            members,
            database=pool,
            groups=groups,
            bundle=bundle,
            clock=clock,
            diagnostics=diagnostics,
            tasks=tasks,
        )
        reply_executor = ReplyExecutor(
            bundle=bundle,
            clock=clock,
            database=pool,
            identities=identities,
            evidence_store=evidence,
            archive=archive,
            budget=budget,
            members=members,
            registry=registry,
            delivery=delivery,
            media=media,
            providers=capabilities,
            directory=directory,
            tasks=tasks,
        )
        replies = ReplyScheduler(
            reply_executor,
            capacity=bundle.default.runtime.reply_capacity,
            concurrency=bundle.default.backends.text.max_concurrency,
        )
        gateway = Gateway(
            bundle=bundle,
            clock=clock,
            replies=replies,
            members=members,
            ingestor=ingestor,
            registry=registry,
            router=router,
            delivery=delivery,
            media=media,
        )
        worker = MemoryWorker(bundle, capabilities, budget=budget, database=pool, clock=clock)
        scheduled = ScheduledTaskWorker(
            bundle.default,
            replies,
            database=pool,
            clock=clock,
        )
        return cls(
            lease=lease
            if lease is not None
            else RuntimeLease(lambda: cls._connect_lease(database_url)),
            bundle=bundle,
            clock=clock,
            diagnostics=diagnostics,
            tasks=tasks,
            database=database,
            groups=groups,
            media_cache=media_cache,
            evidence=evidence,
            archive=archive,
            identities=identities,
            budget=budget,
            members=members,
            providers=capabilities,
            directory=directory,
            links=links,
            ingestor=ingestor,
            registry=registry,
            delivery=delivery,
            media_processor=media_processor,
            media=media,
            router=router,
            gateway=gateway,
            reply_executor=reply_executor,
            replies=replies,
            worker=worker,
            scheduled=scheduled,
        )

    @staticmethod
    async def _connect_lease(url: str) -> asyncpg.Connection:
        return await asyncpg.connect(url, timeout=10, command_timeout=5)

    async def start(self) -> None:
        if self._closed:
            raise RuntimeError("a closed Runtime cannot be restarted")
        if self._start_task is None:
            self._start_task = asyncio.create_task(self._initialize(), name="runtime-start")
        try:
            await asyncio.shield(self._start_task)
        except BaseException:
            await self.aclose()
            raise

    async def _initialize(self) -> None:
        # Exclusivity precedes the shared pool and recovery of interrupted timers.
        await self.lease.acquire(self._lease_lost)
        await self.database.start()
        await repo.ensure_schema(self.database.pool)
        self.diagnostics.install()
        await self.providers.asr.start()
        if self._closed:
            raise RuntimeError("runtime closed during startup")
        nickname.initialize()
        nickname.register(self.bundle.default.bot.nicknames)
        self._worker_task = asyncio.create_task(self.worker.run_forever(), name="memory-worker")
        self._started = True
        log.info("runtime started")

    def _lease_lost(self) -> None:
        log.error("exclusive database session lost; revoking runtime work")
        self.gateway.abort()
        self.replies.abort()
        self.scheduled.abort()
        self.members.abort()
        self.media.abort()
        self.media_processor.abort()
        if self._worker_task is not None:
            self._worker_task.cancel()
        for task in self._maintenance:
            task.cancel()
        self._begin_close()

    def _begin_close(self) -> asyncio.Task:
        if self._close_task is None:
            self._closed = True
            self.gateway.quiesce()
            if self._start_task is not None and not self._start_task.done():
                self._start_task.cancel()
            self._close_task = asyncio.create_task(self._shutdown(), name="runtime-close")
        return self._close_task

    async def aclose(self) -> None:
        # A cancelled caller cannot abandon cleanup halfway through the resource stack.
        await asyncio.shield(self._begin_close())

    async def _shutdown(self) -> None:
        if self._start_task is not None:
            await asyncio.gather(self._start_task, return_exceptions=True)
        try:
            await self._stack.aclose()
            log.info("runtime stopped")
        finally:
            self.diagnostics.close()

    async def _close_database(self) -> None:
        await self.database.close()

    async def _close_observation(self) -> None:
        self.delivery.echo.close()

    async def _stop_worker(self) -> None:
        task, self._worker_task = self._worker_task, None
        if task is not None:
            task.cancel()
            await asyncio.gather(task, return_exceptions=True)

    def own_registration(self, unregister: Callable[[], None]) -> None:
        if self._closed:
            unregister()
            raise RuntimeError("runtime is closed")
        self._registrations.append(unregister)

    async def run_maintenance(self, operation: Callable[[Runtime], Awaitable[None]]) -> None:
        if self._closed or not self._started:
            return
        task = asyncio.current_task()
        assert task is not None
        self._maintenance.add(task)
        try:
            await operation(self)
        finally:
            self._maintenance.discard(task)

    async def _stop_maintenance(self) -> None:
        for unregister in self._registrations:
            try:
                unregister()
            except Exception:
                log.warning("unregistering maintenance failed", exc_info=True)
        self._registrations.clear()
        tasks = tuple(self._maintenance)
        for task in tasks:
            task.cancel()
        if tasks:
            await asyncio.gather(*tasks, return_exceptions=True)

    @staticmethod
    async def _close_step(name: str, close: Callable[[], Awaitable[None]]) -> None:
        try:
            await close()
        except Exception:
            log.warning("closing %s failed", name, exc_info=True)
