"""Process composition root for lifecycle resources.

The NoneBot plugin adapts platform events into this object; core modules never import a
live framework runtime or discover mutable capability singletons on their own.
"""

from __future__ import annotations

import asyncio
import logging
from dataclasses import dataclass, field

from . import util
from .core import errors, nickname, retrieval
from .core.commands import CommandRouter
from .core.delivery import GroupDelivery
from .core.media import MediaCoordinator, MediaProcessor
from .core.pipeline import Gateway
from .core.state import Registry
from .db import close_pool, init_pool, repo
from .gateway.ingest import Ingestor
from .providers.base import Providers
from .providers.registry import build as build_providers
from .repositories import IdentityLinkRepository, IdentityRepository
from .services import Directory, IdentityLinkService, IdentityResolver
from .settings import ConfigBundle, config
from .workers import MemoryWorker

log = logging.getLogger("qqbot.runtime")


@dataclass(slots=True)
class Runtime:
    bundle: ConfigBundle
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
    worker: MemoryWorker
    _worker_task: asyncio.Task[None] | None = field(default=None, init=False, repr=False)
    _started: bool = field(default=False, init=False, repr=False)
    _closed: bool = field(default=False, init=False, repr=False)

    @classmethod
    def build(
        cls,
        bundle: ConfigBundle | None = None,
        *,
        providers: Providers | None = None,
    ) -> Runtime:
        """Compose exactly one process-owned object graph."""

        bundle = bundle or config()
        capabilities = providers or build_providers(bundle.default)
        identities = IdentityRepository()
        resolver = IdentityResolver(identities)
        directory = retrieval.build_directory(
            identities=identities,
            resolver=resolver,
        )
        links = IdentityLinkService(
            bundle.default.identity_link,
            resolver,
            identities,
            IdentityLinkRepository(),
        )
        ingestor = Ingestor(resolver)
        registry = Registry()
        delivery = GroupDelivery()
        media_processor = MediaProcessor(
            bundle.default.media,
            capabilities,
            directory,
        )
        media = MediaCoordinator(media_processor)
        router = CommandRouter(delivery, registry, directory, links, capabilities)
        gateway = Gateway(
            ingestor=ingestor,
            registry=registry,
            router=router,
            delivery=delivery,
            media=media,
            providers=capabilities,
            directory=directory,
        )
        worker = MemoryWorker(bundle.default, capabilities)
        return cls(
            bundle=bundle,
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
            worker=worker,
        )

    async def start(self) -> None:
        """Initialize external resources, then start the background consumer."""

        if self._started:
            return
        if self._closed:
            raise RuntimeError("a closed Runtime cannot be restarted")
        diagnostics = self.bundle.default.diagnostics
        errors.install(
            entries=diagnostics.error_ring_entries,
            message_chars=diagnostics.error_message_chars,
        )
        util.set_timezone(self.bundle.default.timezone)
        await init_pool()
        await repo.ensure_schema()
        await self.providers.asr.start()
        nickname.initialize()
        nickname.register(self.bundle.default.trigger.nicknames)
        self._worker_task = asyncio.create_task(self.worker.run_forever())
        self._started = True
        log.info("runtime started")

    async def aclose(self) -> None:
        """Close every owned resource in dependency order, despite individual failures."""

        if self._closed:
            return
        self._closed = True

        task = self._worker_task
        self._worker_task = None
        if task is not None:
            task.cancel()
            await asyncio.gather(task, return_exceptions=True)

        await self._close_step("gateway", self.gateway.shutdown())
        await self._close_step(
            "media coordinator",
            self.media.close(timeout=self.bundle.default.gateway.shutdown_wait_sec),
        )
        await self._close_step("media processor", self.media_processor.close())
        await self._close_step("providers", self.providers.aclose())
        await self._close_step("database pool", close_pool())
        log.info("runtime stopped")

    @staticmethod
    async def _close_step(name: str, awaitable) -> None:
        try:
            await awaitable
        except Exception:
            log.warning("closing %s failed", name, exc_info=True)
