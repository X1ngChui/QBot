"""Verified PostgreSQL backups shared by scheduled jobs and manual maintenance."""

from __future__ import annotations

import asyncio
import os
from dataclasses import dataclass
from datetime import UTC, datetime
from pathlib import Path

from .db.pool import dsn
from .util import read_secret

MIN_BACKUP_BYTES = 10_000


class BackupError(RuntimeError):
    """A dump could not be created or did not pass local verification."""


@dataclass(frozen=True, slots=True)
class VerifiedBackup:
    path: Path
    size: int
    verified_at: datetime


async def create_verified_backup(
    out_dir: Path,
    *,
    prefix: str = "qqbot",
) -> VerifiedBackup:
    """Create a custom-format dump and prove pg_restore can read its catalog."""

    out_dir.mkdir(parents=True, exist_ok=True)
    target = out_dir / f"{prefix}-{datetime.now(UTC).strftime('%Y%m%d-%H%M%S')}.dump"
    env = dict(os.environ)
    if password := read_secret("DATABASE_PASSWORD_FILE", "DATABASE_PASSWORD"):
        env["PGPASSWORD"] = password

    proc = await asyncio.create_subprocess_exec(
        "pg_dump",
        "-Fc",
        "--no-password",
        "-d",
        dsn(with_password=False),
        "-f",
        str(target),
        env=env,
        stdout=asyncio.subprocess.PIPE,
        stderr=asyncio.subprocess.PIPE,
    )
    _, err = await proc.communicate()
    if proc.returncode != 0:
        target.unlink(missing_ok=True)
        detail = (err or b"").decode(errors="replace")[:500]
        raise BackupError(f"pg_dump failed ({proc.returncode}): {detail}")

    check = await asyncio.create_subprocess_exec(
        "pg_restore",
        "--list",
        str(target),
        stdout=asyncio.subprocess.DEVNULL,
        stderr=asyncio.subprocess.PIPE,
    )
    _, check_err = await check.communicate()
    size = target.stat().st_size if target.exists() else 0
    if check.returncode != 0 or size < MIN_BACKUP_BYTES:
        target.unlink(missing_ok=True)
        detail = (check_err or b"").decode(errors="replace")[:300]
        raise BackupError(
            f"backup verification failed ({size} bytes, pg_restore {check.returncode}): {detail}"
        )

    return VerifiedBackup(path=target, size=size, verified_at=datetime.now(UTC))


def rotate_backups(out_dir: Path, *, prefix: str = "qqbot", keep: int) -> None:
    """Keep the newest verified dump files after a new one has succeeded."""

    dumps = sorted(
        out_dir.glob(f"{prefix}-*.dump"),
        key=lambda path: path.stat().st_mtime,
        reverse=True,
    )
    for old in dumps[keep:]:
        old.unlink(missing_ok=True)
