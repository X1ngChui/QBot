"""Scheduled jobs. APScheduler via nonebot-plugin-apscheduler.

  nightly pipeline       02:30 - extraction drain, then decay, then pg_dump
                         (keep 14), then napcat media cleanup, in that order
  daily report to owners 00:00 - the moment the ledger day closes

The night is one job on purpose. The stages depend on each other softly -
decay must see the day's confirmations land before it retires anything, and
the dump should carry what the night learned - but extraction drains through
the job queue with retries backing off up to an hour, so independent crons
only ever ordered the stages by wall-clock luck. The pipeline waits for the
queue to empty between stages instead, with a deadline per wait so one stuck
job delays the night rather than cancelling it. Accepted cost: a restart
mid-pipeline skips that night's remaining stages - decay catches up the next
night, and the report's backup-age line is the alarm for a skipped dump.
"""

from __future__ import annotations

import asyncio
import logging
import os
import time
from datetime import timedelta
from pathlib import Path

import nonebot
from apscheduler.triggers.cron import CronTrigger
from nonebot_plugin_apscheduler import scheduler

from ..core import errors, output
from ..core.budget import hit_split
from ..core.media import NAPCAT_DATA_DIR
from ..core.state import REGISTRY
from ..db import repo
from ..db.pool import dsn
from ..providers import providers
from ..providers.base import Kind
from ..repositories import JobQueue
from ..repositories.job import JobType
from ..settings import config
from ..util import now_local, read_secret, tz

log = logging.getLogger("qqbot.tasks")


def _trigger(expr: str) -> CronTrigger:
    """One parser for registration and validation alike: settings validates these
    expressions with from_crontab, so registering through anything else invites
    the day-of-week trap (crontab counts 0=Sunday, APScheduler kwargs 0=Monday -
    a '* * * * 0' validated as Sunday would silently fire on Monday). The explicit
    timezone makes '04:00' mean 04:00 in the configured zone rather than whatever
    TZ the container was started with."""
    return CronTrigger.from_crontab(expr, timezone=tz())


# -- the nightly pipeline ----------------------------------------------------

async def _queue_groups() -> list[str]:
    scopes = ({str(g) for g in await repo.groups_with_state()}
              | {st.group_id for st in REGISTRY.all()})
    return [g for g in sorted(scopes) if not g.startswith("_")]


async def _drain_wait(queue: JobQueue, deadline: timedelta, stage: str) -> None:
    """Block until the job queue is empty of live work, or the deadline passes.

    Pending includes jobs sitting out a retry backoff - that is the point: the
    next stage must not start while this stage's work can still land. On the
    deadline the pipeline logs and continues; a wedged job already has the
    report's queue-depth line as its alarm, and holding the backup hostage to
    it would turn one failure into two.
    """
    started = now_local()
    while now_local() - started < deadline:
        depth = await queue.depth()
        if not any(depth.get(k) for k in ("pending", "running")):
            return
        await asyncio.sleep(config().default.schedule.drain_poll_sec)
    log.error("nightly: %s stage still has live jobs after %s, moving on",
              stage, deadline)


async def nightly() -> None:
    """The night's work, in dependency order: extract, decay, backup, sweep.

    One pipeline instead of four crons, because the order is load-bearing and
    a clock cannot enforce it: decay must not retire a fact whose confirmation
    is still queued behind an extraction retry, and the dump should carry what
    the night learned. Each stage waits for the queue to empty (with a
    deadline) before the next begins.

    Extraction is the single event point: messages are only archived during the
    day, and this is where they are read - oldest first, in chunks cut at
    conversation gaps, at off-peak prices by virtue of the hour. The worker
    drains until the unread floor, so one job per group covers however much the
    day held. Freshness is the accepted price: the reply path's window is the
    immediate context, and what extraction learns was always the past.

    Decay is free (one UPDATE per group, no model call) and runs on schedule
    rather than on traffic, because the entire point is to let go of what
    stopped being said.
    """
    queue = JobQueue("nightly")

    for gid in await _queue_groups():
        try:
            await queue.submit(JobType.EXTRACT_MEMORY, {"group_id": int(gid)},
                               priority=1)
        except Exception:
            log.exception("could not queue extraction for group %s", gid)
    sched = config().default.schedule
    await _drain_wait(
        queue, timedelta(hours=sched.extract_drain_hours), "extraction")

    for gid in await _queue_groups():
        try:
            await queue.submit(JobType.DECAY, {"group_id": int(gid)})
        except Exception:
            log.exception("could not queue forgetting for group %s", gid)
    try:
        if n := await queue.purge_done():
            log.info("purged %d finished jobs older than 30 days", n)
    except Exception:
        log.exception("job purge failed")
    await _drain_wait(
        queue, timedelta(minutes=sched.decay_drain_min), "decay")

    # The paid and destructive stages are behind the waits; these two are
    # plain local work and each guards itself.
    await backup()
    await clean_napcat_cache()


# -- backup (nightly stage 3) ------------------------------------------------


async def backup() -> None:
    out_dir = Path(os.getenv("BACKUP_DIR", "/app/backups"))
    out_dir.mkdir(parents=True, exist_ok=True)
    target = out_dir / f"qqbot-{now_local().strftime('%Y%m%d-%H%M')}.dump"

    env = dict(os.environ)
    if pwd := read_secret("DATABASE_PASSWORD_FILE", "DATABASE_PASSWORD"):
        env["PGPASSWORD"] = pwd
    proc = await asyncio.create_subprocess_exec(
        "pg_dump", "-Fc", "--no-password", "-d", dsn(with_password=False),
        "-f", str(target), env=env,
        stdout=asyncio.subprocess.PIPE,
        stderr=asyncio.subprocess.PIPE,
    )
    _, err = await proc.communicate()
    if proc.returncode != 0:
        log.error("pg_dump failed (%s): %s", proc.returncode, (err or b"").decode()[:500])
        target.unlink(missing_ok=True)
        return

    # A dump that exists is not yet a backup. Exit code 0 with a truncated or
    # unreadable archive is exactly the failure that ages the last good dump out
    # of retention fourteen quiet days later - so the archive must prove it can
    # be listed before anything older is deleted on its account.
    check = await asyncio.create_subprocess_exec(
        "pg_restore", "--list", str(target),
        stdout=asyncio.subprocess.DEVNULL,
        stderr=asyncio.subprocess.PIPE,
    )
    _, cerr = await check.communicate()
    # The size floor only catches truncation: even a schema-only dump of this
    # database clears 10KB, while a partial write from a full disk may not.
    if check.returncode != 0 or target.stat().st_size < 10_000:
        log.error("backup failed verification, kept nothing and rotated nothing: "
                  "%s (%d bytes): %s", target.name, target.stat().st_size,
                  (cerr or b"").decode()[:300])
        target.unlink(missing_ok=True)
        return

    keep = config().default.schedule.backup_keep
    dumps = sorted(out_dir.glob("qqbot-*.dump"), key=lambda p: p.stat().st_mtime, reverse=True)
    for old in dumps[keep:]:
        old.unlink(missing_ok=True)
    log.info("backup written and verified: %s (%.1f MB)",
             target.name, target.stat().st_size / 1e6)


# -- napcat media cache cleanup (nightly stage 4) ----------------------------


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


async def clean_napcat_cache() -> None:
    """NTQQ's media cache is the reason the partition needs 50 GB; left alone it grows
    into the tens of GB."""
    root = Path(NAPCAT_DATA_DIR)
    if not root.is_dir():
        log.warning("napcat data dir not found: %s", root)
        return
    days = config().default.schedule.napcat_cache_days
    cutoff = time.time() - days * 86400

    targets: list[Path] = []
    for pattern in ("nt_qq_*/nt_data/Pic", "nt_qq_*/nt_data/Ptt", "nt_qq_*/nt_data/Video",
                    "nt_qq_*/nt_data/File", "NapCat/temp"):
        targets.extend(p for p in root.glob(pattern) if p.is_dir())

    total_n = total_b = 0
    for t in targets:
        n, b = await asyncio.get_running_loop().run_in_executor(None, _sweep_dir, t, cutoff)
        total_n += n
        total_b += b
    log.info("napcat cache cleanup: %d files, %.1f MB freed", total_n, total_b / 1e6)


# -- daily report (its own trigger) ------------------------------------------


async def daily_report() -> None:
    cfg = config().default
    owners = [o.strip() for o in cfg.owners if o.strip()]
    if not owners:
        log.info("no owners configured, daily report skipped")
        return

    yesterday = (now_local() - timedelta(days=1)).strftime("%Y-%m-%d")
    rows = await repo.day_breakdown(yesterday)
    total = sum(float(r["cny"]) for r in rows)

    lines = [f"[{yesterday}] 昨日小结",
             f"总花费 ¥{total:.3f} / 上限 ¥{cfg.budget.daily_cny_cap:.2f}"]
    for r in rows:
        lines.append(
            f"  {r['kind']:<8}{r['model']:<18} {int(r['calls'])} 次  ¥{float(r['cny']):.3f}"
        )

    if cache := hit_split(rows):
        lines.append(f"前缀缓存命中率 {cache}")

    img = await repo.image_cache_stats()
    # The refusal count is the one worth watching: those rows describe nothing, and a
    # number that climbs says the vision backend is turning away more than it looks at.
    refused = f"，其中后端拒看 {img['refused']} 条" if img.get("refused") else ""
    lines.append(f"图片缓存 {img['n']} 条，累计命中 {img['hits']} 次{refused}")

    used = await repo.month_calls(Kind.SEARCH, providers().search.name)
    lines.append(f"搜索额度 本月 {used}/{cfg.llm.search.monthly_quota}")

    # The memory queue, because a stuck worker has no other symptom. Nothing goes
    # missing and nothing errors in the group - the bot simply stops learning, and the
    # only visible trace is a number here that climbs every day.
    depth = await JobQueue("report").depth()
    if backlog := {k: v for k, v in depth.items() if k in ("pending", "running", "dead")}:
        lines.append("记忆作业 " + "，".join(f"{k} {v}" for k, v in sorted(backlog.items())))

    if fresh := await repo.groups_first_seen_on(yesterday):
        lines.append("新群：" + "、".join(str(g) for g in fresh))

    if muted := await repo.muted_groups():
        lines.append("已静音：" + "、".join(str(g) for g in muted))

    # The one failure mode the verified backup still has is "quietly stopped
    # running" - a crash before the job, a skipped misfire, a vanished mount.
    # The report states the newest dump's age so that failure has a face.
    out_dir = Path(os.getenv("BACKUP_DIR", "/app/backups"))
    dumps = sorted(out_dir.glob("qqbot-*.dump"), key=lambda p: p.stat().st_mtime,
                   reverse=True)
    if not dumps:
        lines.append("备份：目录里没有任何备份文件！")
    else:
        age_h = (time.time() - dumps[0].stat().st_mtime) / 3600
        note = f"备份 {dumps[0].name}（{dumps[0].stat().st_size / 1e6:.1f} MB）"
        lines.append(note if age_h < cfg.schedule.backup_stale_hours
                     else f"{note}——已 {age_h / 24:.1f} 天未更新！")

    # Every stripper hit is a near-miss leak: the model wrote a system marker and
    # only the stripper kept it from a group. Counts since the last restart -
    # a climbing number here means format discipline is regressing at the source.
    if hits := {k: v for k, v in output.STRIPPED.items() if v}:
        lines.append("输出剥离（重启以来） "
                     + "，".join(f"{k} {v}" for k, v in sorted(hits.items())))

    errs = errors.recent(8)
    if errs:
        lines.append(f"异常 {errors.count()} 条，最近几条：")
        lines.extend(f"  {t} {name.split('.')[-1]}: {msg}" for t, name, msg in errs)
    else:
        lines.append("无异常")

    try:
        bot = nonebot.get_bot()
    except Exception:
        log.exception("daily report: no bot connected, nothing sent")
        return

    # One owner at a time, each failing on its own: a bot that has never spoken to one of
    # them privately cannot message them, and that must not swallow the report for the
    # rest. The ring is only cleared once somebody has actually received it, so a total
    # failure leaves the errors to be reported tomorrow instead of dropping them.
    text = "\n".join(lines)
    sent = 0
    for owner in owners:
        try:
            await bot.send_private_msg(user_id=int(owner), message=text)
            sent += 1
        except Exception:
            log.exception("daily report: could not reach owner %s", owner)
    if sent:
        errors.clear()


# -- registration -----------------------------------------------------------


def register() -> None:
    s = config().default.schedule
    # misfire_grace_time: APScheduler's default is seconds - a loop busy at the
    # trigger moment would silently skip that night. An hour of grace runs it
    # late instead; coalesce folds a pile-up into one run.
    common = {"replace_existing": True, "coalesce": True,
              "misfire_grace_time": s.misfire_grace_sec}
    scheduler.add_job(nightly, _trigger(s.nightly_cron), id="nightly", **common)
    scheduler.add_job(daily_report, _trigger(s.report_cron), id="report", **common)
    log.info("scheduled jobs registered")
