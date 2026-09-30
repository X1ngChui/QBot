"""Importable scheduled job bodies, independent of NoneBot and APScheduler."""

from __future__ import annotations

import asyncio
import logging
import os
import time
from collections.abc import Awaitable, Callable
from datetime import UTC, datetime, timedelta
from dataclasses import dataclass
from enum import StrEnum
from zoneinfo import ZoneInfo

from apscheduler.triggers.cron import CronTrigger
from pathlib import Path
from typing import TYPE_CHECKING

from qqbot.domain.ids import AccountId
from qqbot.operations.limits import DIAGNOSTIC_LIMITS
from qqbot.delivery import output
from qqbot.services.budget import hit_split
from qqbot.media.service import NAPCAT_DATA_DIR
from qqbot.domain.ids import GroupId
from qqbot.operations.maintenance import BACKUP_TIMEOUT_SEC
from qqbot.operations.maintenance import BackupError
from qqbot.operations.maintenance import create_verified_backup
from qqbot.operations.maintenance import rotate_backups
from qqbot.providers.base import Kind
from qqbot.repositories import JobQueue
from qqbot.repositories.job import JobType
from qqbot.util import why

if TYPE_CHECKING:
    from qqbot.runtime import Runtime

log = logging.getLogger("qqbot.operations.scheduled")
PrivateSender = Callable[[AccountId, str], Awaitable[None]]
EXTRACTION_STAGE_TIMEOUT = timedelta(hours=3)
DECAY_STAGE_TIMEOUT = timedelta(minutes=30)
STAGE_POLL_SEC = 30
MISFIRE_GRACE_SEC = 3600


class StageEnd(StrEnum):
    COMPLETE = "complete"
    TIMED_OUT = "timed_out"
    FAILED = "failed"


@dataclass(frozen=True, slots=True)
class StageResult:
    name: str
    end: StageEnd
    remaining: tuple[tuple[str, int], ...] = ()


def backup_is_overdue(last: datetime, now: datetime, *, cron: str, zone: str) -> bool:
    trigger = CronTrigger.from_crontab(cron, timezone=ZoneInfo(zone))
    next_run = trigger.get_next_fire_time(None, last + timedelta(microseconds=1))
    if next_run is None:
        return True
    completion_budget = (
        EXTRACTION_STAGE_TIMEOUT + DECAY_STAGE_TIMEOUT + timedelta(seconds=2 * BACKUP_TIMEOUT_SEC)
    )
    return now.astimezone(UTC) > next_run.astimezone(UTC) + completion_budget


async def _queue_groups(runtime: Runtime) -> list[GroupId]:
    scopes = {GroupId(group_id) for group_id in await runtime.groups.groups_with_state()} | {
        state.group_id for state in runtime.registry.all()
    }
    return sorted(scopes)


async def _drain_wait(
    queue: JobQueue,
    deadline: timedelta,
    stage: str,
    job_types: tuple[JobType, ...],
) -> StageResult:
    """Report a bounded phase result; failure cannot silently skip the backup phase."""
    depth = {}
    try:
        async with asyncio.timeout(deadline.total_seconds()):
            while True:
                depth = await queue.depth(job_types=job_types)
                if not any(depth.get(key) for key in ("pending", "running")):
                    return StageResult(stage, StageEnd.COMPLETE)
                await asyncio.sleep(STAGE_POLL_SEC)
    except TimeoutError:
        end = StageEnd.TIMED_OUT
    except Exception:
        log.exception("nightly: %s phase cannot read job progress", stage)
        end = StageEnd.FAILED
    log.warning("nightly: %s phase ended %s with backlog %s", stage, end.value, depth)
    return StageResult(stage, end, tuple(sorted(depth.items())))


async def _memory_stages(runtime: Runtime) -> None:
    """Queue and observe extraction and decay as separate bounded phases."""

    queue = JobQueue("nightly", runtime.database.pool)
    groups = await _queue_groups(runtime)
    for group_id in groups:
        try:
            await queue.submit(
                JobType.EXTRACT_MEMORY,
                {"group_id": group_id},
                priority=1,
            )
        except Exception:
            log.exception("could not queue extraction for group %s", group_id)

    schedule = runtime.bundle.default.maintenance
    await _drain_wait(
        queue,
        EXTRACTION_STAGE_TIMEOUT,
        "extraction",
        (JobType.EXTRACT_MEMORY,),
    )

    for group_id in groups:
        try:
            await queue.submit(JobType.DECAY, {"group_id": group_id})
        except Exception:
            log.exception("could not queue forgetting for group %s", group_id)
    try:
        if count := await queue.purge_done(days=schedule.completed_job_keep_days):
            log.info(
                "purged %d finished jobs older than %d days",
                count,
                schedule.completed_job_keep_days,
            )
    except Exception:
        log.exception("job purge failed")
    await _drain_wait(
        queue,
        DECAY_STAGE_TIMEOUT,
        "decay",
        (JobType.DECAY,),
    )


async def nightly(runtime: Runtime) -> None:
    """A failed memory phase must not suppress independently verifiable backups."""
    try:
        await _memory_stages(runtime)
    except Exception:
        log.exception("nightly: memory phase failed; continuing independent maintenance")
    try:
        if count := await runtime.evidence.evidence_prune():
            log.info("pruned %d expired reply evidence memos", count)
    except Exception:
        log.exception("reply evidence pruning failed")

    try:
        await backup(runtime)
    except (OSError, BackupError) as exc:
        log.error("backup could not run: %s", why(exc))
    await clean_napcat_cache(runtime)


async def backup(runtime: Runtime) -> None:
    out_dir = Path(os.getenv("BACKUP_DIR", "/app/backups"))
    artifact = await create_verified_backup(out_dir)
    rotate_backups(
        out_dir,
        keep=runtime.bundle.default.maintenance.backup_keep,
    )
    log.info(
        "backup written and verified: %s (%.1f MB)",
        artifact.path.name,
        artifact.size / 1e6,
    )


def _sweep_dir(root: Path, cutoff: float) -> tuple[int, int]:
    removed = freed = 0
    for path in root.rglob("*"):
        try:
            if path.is_file() and path.stat().st_mtime < cutoff:
                size = path.stat().st_size
                path.unlink()
                removed += 1
                freed += size
        except OSError:
            continue
    return removed, freed


async def clean_napcat_cache(runtime: Runtime) -> None:
    """Remove old NTQQ media files from the shared NapCat data directory."""

    root = Path(NAPCAT_DATA_DIR)
    if not root.is_dir():
        log.warning("napcat data dir not found: %s", root)
        return
    days = runtime.bundle.default.maintenance.napcat_cache_days
    cutoff = time.time() - days * 86400

    targets: list[Path] = []
    for pattern in (
        "nt_qq_*/nt_data/Pic",
        "nt_qq_*/nt_data/Ptt",
        "nt_qq_*/nt_data/Video",
        "nt_qq_*/nt_data/File",
        "NapCat/temp",
    ):
        targets.extend(path for path in root.glob(pattern) if path.is_dir())

    total_count = total_bytes = 0
    for target in targets:
        count, size = await asyncio.get_running_loop().run_in_executor(
            None,
            _sweep_dir,
            target,
            cutoff,
        )
        total_count += count
        total_bytes += size
    log.info(
        "napcat cache cleanup: %d files, %.1f MB freed",
        total_count,
        total_bytes / 1e6,
    )


async def daily_report(runtime: Runtime, send_private: PrivateSender) -> None:
    """Build the closed ledger report and send it independently to each owner."""

    cfg = runtime.bundle.default
    owners = cfg.bot.owners
    if not owners:
        log.info("no owners configured, daily report skipped")
        return

    yesterday = (runtime.clock.now() - timedelta(days=1)).strftime("%Y-%m-%d")
    rows = await runtime.budget.ledger.day_breakdown(yesterday)
    total = sum(float(row["cny"]) for row in rows)
    lines = [
        f"[{yesterday}] 昨日小结",
        f"总花费 ¥{total:.3f} / 上限 ¥{cfg.budget.daily_cny_cap:.2f}",
    ]
    for row in rows:
        lines.append(
            f"  {row['kind']:<8}{row['model']:<18} {int(row['calls'])} 次  ¥{float(row['cny']):.3f}"
        )
    if cache := hit_split(rows):
        lines.append(f"前缀缓存命中率 {cache}")

    image = await runtime.media_cache.image_cache_stats()
    refused = f"，其中后端拒看 {image['refused']} 条" if image.get("refused") else ""
    lines.append(f"图片缓存 {image['n']} 条，累计命中 {image['hits']} 次{refused}")

    used = await runtime.budget.ledger.month_calls(Kind.SEARCH, runtime.providers.search.name)
    lines.append(f"搜索额度 本月 {used}/{cfg.backends.search.monthly_quota}")

    depth = await JobQueue("report", runtime.database.pool).depth()
    backlog = {key: value for key, value in depth.items() if key in ("pending", "running", "dead")}
    if backlog:
        lines.append(
            "记忆作业 " + "，".join(f"{key} {value}" for key, value in sorted(backlog.items()))
        )

    if fresh := await runtime.groups.groups_first_seen_on(yesterday):
        lines.append("新群：" + "、".join(str(group_id) for group_id in fresh))
    if muted := await runtime.groups.muted_groups():
        lines.append("已静音：" + "、".join(str(group_id) for group_id in muted))

    out_dir = Path(os.getenv("BACKUP_DIR", "/app/backups"))
    dumps = sorted(
        out_dir.glob("qqbot-*.dump"),
        key=lambda path: path.stat().st_mtime,
        reverse=True,
    )
    if not dumps:
        lines.append("备份：目录中没有备份文件！")
    else:
        age_hours = (time.time() - dumps[0].stat().st_mtime) / 3600
        note = f"备份 {dumps[0].name}（{dumps[0].stat().st_size / 1e6:.1f} MB）"
        lines.append(
            note
            if not backup_is_overdue(
                datetime.fromtimestamp(dumps[0].stat().st_mtime, UTC),
                runtime.clock.now(),
                cron=cfg.maintenance.nightly_cron,
                zone=cfg.bot.timezone,
            )
            else f"{note}——已 {age_hours / 24:.1f} 天未更新！"
        )

    if hits := {key: value for key, value in output.STRIPPED.items() if value}:
        lines.append(
            "输出剥离（重启以来） "
            + "，".join(f"{key} {value}" for key, value in sorted(hits.items()))
        )

    recent = runtime.diagnostics.recent(DIAGNOSTIC_LIMITS.daily_report_recent_errors)
    if recent:
        lines.append(f"异常 {runtime.diagnostics.count()} 条，最近几条：")
        lines.extend(f"  {at} {name.split('.')[-1]}: {message}" for at, name, message in recent)
    else:
        lines.append("无异常")

    text = "\n".join(lines)
    sent = 0
    for owner in owners:
        try:
            await send_private(owner, text)
            sent += 1
        except Exception:
            log.exception("daily report: could not reach owner %s", owner)
    if sent:
        runtime.diagnostics.clear()
