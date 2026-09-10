"""NoneBot2 plugin entry: wiring only, no logic.

Kept out of qqbot/__init__.py on purpose - importing any submodule would otherwise drag in
a live nonebot runtime, which breaks the standalone scripts (scripts/preflight.py).

Startup order matters: config (and its timezone) -> DB pool and schema -> providers ->
jieba -> scheduled jobs -> memory worker. Everything must be wired before the first
message arrives; in particular the embedding backend is set before the worker or any
retrieval can run.
"""

from __future__ import annotations

import asyncio
import logging

from nonebot import get_driver, on_message, on_notice, require
from nonebot.adapters.onebot.v11 import Bot, GroupMessageEvent, NoticeEvent
from nonebot.plugin import PluginMetadata

require("nonebot_plugin_apscheduler")

# Below the require() on purpose: it has to run before anything that imports the
# scheduler plugin, so these cannot move to the top of the file.
from . import util
from .core import errors, nickname
from .core.pipeline import GATEWAY
from .core.media import MEDIA
from .db import close_pool, init_pool
from .workers import MemoryWorker
from .db import repo
# Imported for its registrations: defining the command handlers IS the whole effect, so
# the name is never read below. This "unused import" is the entire ops console - drop it
# and every command silently falls through to the reply path, with nothing in the log.
from .plugins import commands as _commands  # noqa: F401
from .plugins import tasks
from .providers import build_default, providers, set_providers
from .settings import config

__plugin_meta__ = PluginMetadata(
    name="qqbot",
    description="QQ group chat AI bot",
    usage="/reload /mute /unmute /stats",
)

log = logging.getLogger("qqbot")
driver = get_driver()

_worker_task: asyncio.Task | None = None


@driver.on_startup
async def _startup() -> None:
    errors.install()
    bundle = config()
    # Before anything reads a clock: the scheduler, the budget's day boundary and the
    # prompt all take their local time from here.
    util.set_timezone(bundle.default.timezone)
    log.info(
        "config loaded: %d persona file(s), timezone %s (now %s)",
        len(bundle.personas), bundle.default.timezone, util.describe_now(),
    )

    await init_pool()
    await repo.ensure_schema()

    # Wire the five capabilities once, here, from the backend names in config.
    # Everything downstream asks for a capability and never learns which platform
    # answers. What was picked is logged by build() itself, so a /reload's rebuild
    # says so too.
    set_providers(build_default())

    nickname.initialize()
    nickname.register(bundle.default.trigger.nicknames)
    for gid in bundle.personas:
        cfg, _ = bundle.for_group(gid)
        nickname.register(cfg.trigger.nicknames)

    tasks.register()
    global _worker_task

    # The consumer end of the memory chain. Extraction and consolidation are queued,
    # and this is what takes them off the queue - without it the inbound path keeps
    # filing jobs nobody runs, and the queue only ever grows. Its vector backend comes
    # from the bundle wired above, like every other capability.
    worker = MemoryWorker(bundle.default)
    _worker_task = asyncio.create_task(worker.run_forever())
    log.info("qqbot ready (memory worker running)")


@driver.on_shutdown
async def _shutdown() -> None:
    # Cancel, then actually wait: cancel() only schedules, and closing the pool
    # while the worker is mid-transaction turns every shutdown into a burst of
    # spurious connection errors - at worst a close() hung on a connection the
    # cancelled task releases at its own pace.
    running = [t for t in (_worker_task,) if t is not None]
    for task in running:
        task.cancel()
    if running:
        await asyncio.gather(*running, return_exceptions=True)
    await GATEWAY.shutdown()
    await MEDIA.close()
    await providers().aclose()
    await close_pool()
    log.info("qqbot stopped")


group_message = on_message(priority=10, block=False)


@group_message.handle()
async def _(bot: Bot, event: GroupMessageEvent) -> None:
    await GATEWAY.handle(bot, event)


group_notice = on_notice(priority=10, block=False)


@group_notice.handle()
async def _(bot: Bot, event: NoticeEvent) -> None:
    # Recalls, joins, leaves, bans and pokes become transcript lines; the
    # gateway ignores every other notice kind.
    await GATEWAY.handle_notice(bot, event)
