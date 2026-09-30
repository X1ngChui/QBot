"""APScheduler adapter for importable scheduled job bodies."""

from __future__ import annotations

import functools
import logging

import nonebot
from apscheduler.triggers.cron import CronTrigger
from nonebot_plugin_apscheduler import scheduler

from qqbot.domain.ids import AccountId
from qqbot.operations import scheduled
from qqbot.runtime import Runtime
from zoneinfo import ZoneInfo

log = logging.getLogger("qqbot.tasks")


def _trigger(expr: str, zone: ZoneInfo) -> CronTrigger:
    """Parse registration with the same crontab semantics settings validation uses."""

    return CronTrigger.from_crontab(expr, timezone=zone)


async def _report(runtime: Runtime) -> None:
    try:
        bot = nonebot.get_bot()
    except Exception:
        log.exception("daily report: no bot connected, nothing sent")
        return

    async def send_private(user_id: AccountId, message: str) -> None:
        await bot.send_private_msg(user_id=int(user_id), message=message)

    await scheduled.daily_report(runtime, send_private)


def register(runtime: Runtime) -> None:
    settings = runtime.bundle.default.maintenance
    common = {
        "replace_existing": True,
        "coalesce": True,
        "misfire_grace_time": scheduled.MISFIRE_GRACE_SEC,
    }
    scheduler.add_job(
        functools.partial(runtime.run_maintenance, scheduled.nightly),
        _trigger(settings.nightly_cron, runtime.clock.zone),
        id="nightly",
        **common,
    )
    runtime.own_registration(lambda: scheduler.remove_job("nightly"))
    scheduler.add_job(
        functools.partial(runtime.run_maintenance, _report),
        _trigger(settings.report_cron, runtime.clock.zone),
        id="report",
        **common,
    )
    runtime.own_registration(lambda: scheduler.remove_job("report"))
    log.info("scheduled jobs registered")
