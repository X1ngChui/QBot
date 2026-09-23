"""APScheduler adapter for importable scheduled job bodies."""

from __future__ import annotations

import functools
import logging

import nonebot
from apscheduler.triggers.cron import CronTrigger
from nonebot_plugin_apscheduler import scheduler

from .. import scheduled
from ..runtime import Runtime
from ..util import tz

log = logging.getLogger("qqbot.tasks")


def _trigger(expr: str) -> CronTrigger:
    """Parse registration with the same crontab semantics settings validation uses."""

    return CronTrigger.from_crontab(expr, timezone=tz())


async def _report(runtime: Runtime) -> None:
    try:
        bot = nonebot.get_bot()
    except Exception:
        log.exception("daily report: no bot connected, nothing sent")
        return

    async def send_private(user_id: int, message: str) -> None:
        await bot.send_private_msg(user_id=user_id, message=message)

    await scheduled.daily_report(runtime, send_private)


def register(runtime: Runtime) -> None:
    settings = runtime.bundle.default.schedule
    common = {
        "replace_existing": True,
        "coalesce": True,
        "misfire_grace_time": settings.misfire_grace_sec,
    }
    scheduler.add_job(
        functools.partial(scheduled.nightly, runtime),
        _trigger(settings.nightly_cron),
        id="nightly",
        **common,
    )
    scheduler.add_job(
        functools.partial(_report, runtime),
        _trigger(settings.report_cron),
        id="report",
        **common,
    )
    log.info("scheduled jobs registered")
