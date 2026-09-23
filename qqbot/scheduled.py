"""Importable scheduled job bodies, independent of NoneBot and APScheduler."""

from __future__ import annotations

import asyncio
import logging
import os
import time
from collections.abc import Awaitable, Callable
from datetime import timedelta
from pathlib import Path
from typing import TYPE_CHECKING

from .core import errors, output
from .core.budget import hit_split
from .core.media import NAPCAT_DATA_DIR
from .db import repo
from .domain.ids import GroupId
from .operations import BackupError, create_verified_backup, rotate_backups
from .providers.base import Kind
from .repositories import JobQueue
from .repositories.job import JobType
from .util import now_local, why

if TYPE_CHECKING:
    from .runtime import Runtime

log = logging.getLogger("qqbot.scheduled")
PrivateSender = Callable[[int, str], Awaitable[None]]


async def _queue_groups(runtime: Runtime) -> list[GroupId]:
    scopes = {GroupId(group_id) for group_id in await repo.groups_with_state()} | {
        state.group_id for state in runtime.registry.all()
    }
    return sorted(scopes)


async def _drain_wait(
    runtime: Runtime,
    queue: JobQueue,
    deadline: timedelta,
    stage: str,
    job_types: tuple[JobType, ...],
) -> None:
    """Wait for this stage's live work, ignoring independent queue traffic."""

    started = now_local()
    while now_local() - started < deadline:
        depth = await queue.depth(job_types=job_types)
        if not any(depth.get(key) for key in ("pending", "running")):
            return
        await asyncio.sleep(runtime.bundle.default.schedule.drain_poll_sec)
    log.error(
        "nightly: %s stage still has live jobs after %s, moving on",
        stage,
        deadline,
    )


async def nightly(runtime: Runtime) -> None:
    """Run extraction, decay, backup, and cache cleanup in dependency order."""

    queue = JobQueue("nightly")
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

    schedule = runtime.bundle.default.schedule
    await _drain_wait(
        runtime,
        queue,
        timedelta(hours=schedule.extract_drain_hours),
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
        runtime,
        queue,
        timedelta(minutes=schedule.decay_drain_min),
        "decay",
        (JobType.DECAY,),
    )

    try:
        if count := await repo.evidence_prune():
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
        keep=runtime.bundle.default.schedule.backup_keep,
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
    days = runtime.bundle.default.schedule.napcat_cache_days
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
    owners = [owner.strip() for owner in cfg.owners if owner.strip()]
    if not owners:
        log.info("no owners configured, daily report skipped")
        return

    yesterday = (now_local() - timedelta(days=1)).strftime("%Y-%m-%d")
    rows = await repo.day_breakdown(yesterday)
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

    image = await repo.image_cache_stats()
    refused = f"，其中后端拒看 {image['refused']} 条" if image.get("refused") else ""
    lines.append(f"图片缓存 {image['n']} 条，累计命中 {image['hits']} 次{refused}")

    used = await repo.month_calls(Kind.SEARCH, runtime.providers.search.name)
    lines.append(f"搜索额度 本月 {used}/{cfg.capabilities.search.monthly_quota}")

    depth = await JobQueue("report").depth()
    backlog = {key: value for key, value in depth.items() if key in ("pending", "running", "dead")}
    if backlog:
        lines.append(
            "记忆作业 " + "，".join(f"{key} {value}" for key, value in sorted(backlog.items()))
        )

    if fresh := await repo.groups_first_seen_on(yesterday):
        lines.append("新群：" + "、".join(str(group_id) for group_id in fresh))
    if muted := await repo.muted_groups():
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
            if age_hours < cfg.schedule.backup_stale_hours
            else f"{note}——已 {age_hours / 24:.1f} 天未更新！"
        )

    if hits := {key: value for key, value in output.STRIPPED.items() if value}:
        lines.append(
            "输出剥离（重启以来） "
            + "，".join(f"{key} {value}" for key, value in sorted(hits.items()))
        )

    recent = errors.recent(cfg.diagnostics.daily_report_recent_errors)
    if recent:
        lines.append(f"异常 {errors.count()} 条，最近几条：")
        lines.extend(f"  {at} {name.split('.')[-1]}: {message}" for at, name, message in recent)
    else:
        lines.append("无异常")

    text = "\n".join(lines)
    sent = 0
    for owner in owners:
        try:
            await send_private(int(owner), text)
            sent += 1
        except Exception:
            log.exception("daily report: could not reach owner %s", owner)
    if sent:
        errors.clear()
