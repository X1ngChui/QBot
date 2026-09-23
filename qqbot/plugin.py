"""NoneBot2 plugin entry: wiring only, no logic.

Kept out of qqbot/__init__.py on purpose - importing any submodule would otherwise drag in
a live nonebot runtime, which breaks the standalone scripts (scripts/preflight.py).

Startup order matters: config (and its timezone) -> providers -> DB pool and schema ->
jieba and ASR -> memory worker -> scheduled jobs. Everything must be wired before the first
message arrives; in particular the embedding backend is set before the worker or any
retrieval can run.
"""

from __future__ import annotations

import logging

from nonebot import get_driver, on, on_message, on_notice, require
from nonebot.adapters.onebot.v11 import Adapter, Bot, GroupMessageEvent, NoticeEvent
from nonebot.plugin import PluginMetadata

require("nonebot_plugin_apscheduler")

# Below the require() on purpose: it has to run before anything that imports the
# scheduler plugin, so these cannot move to the top of the file.
from .gateway.nonebot_adapter import NapCatGroupMessageSentEvent
from .plugins import tasks
from .runtime import Runtime
from .settings import config

__plugin_meta__ = PluginMetadata(
    name="qqbot",
    description="QQ group chat AI bot",
    usage="/help",
)

log = logging.getLogger("qqbot")
driver = get_driver()

# NapCat reports the account's own messages under the extension post_type
# "message_sent". Register its full group-message shape before the reverse WebSocket
# connects; otherwise the adapter falls back to a generic Event whose get_message()
# deliberately raises and which on_message does not match.
Adapter.add_custom_model(NapCatGroupMessageSentEvent)

_runtime: Runtime | None = None


def _active() -> Runtime:
    if _runtime is None:
        raise RuntimeError("qqbot runtime is not started")
    return _runtime


@driver.on_startup
async def _startup() -> None:
    global _runtime
    runtime = Runtime.build(config())
    try:
        await runtime.start()
        tasks.register(runtime)
    except Exception:
        await runtime.aclose()
        raise
    _runtime = runtime
    log.info(
        "qqbot ready (%d persona file(s), timezone %s)",
        len(runtime.bundle.personas),
        runtime.bundle.default.timezone,
    )


@driver.on_shutdown
async def _shutdown() -> None:
    global _runtime
    runtime, _runtime = _runtime, None
    if runtime is not None:
        await runtime.aclose()
    log.info("qqbot stopped")


group_message = on_message(priority=10, block=False)


@group_message.handle()
async def _(bot: Bot, event: GroupMessageEvent) -> None:
    await _active().gateway.handle(bot, event)


# Self-authored messages are observations, not ordinary inbound commands. They use a
# distinct matcher type because NoneBot's on_message matcher accepts only post_type
# "message"; both routes converge immediately on the same gateway.
group_message_sent = on("message_sent", priority=10, block=False)


@group_message_sent.handle()
async def _(bot: Bot, event: NapCatGroupMessageSentEvent) -> None:
    await _active().gateway.handle(bot, event)


group_notice = on_notice(priority=10, block=False)


@group_notice.handle()
async def _(bot: Bot, event: NoticeEvent) -> None:
    # Recalls, joins, leaves, bans and pokes become transcript lines; the
    # gateway ignores every other notice kind.
    await _active().gateway.handle_notice(bot, event)
