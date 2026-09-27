"""Verified PostgreSQL backups shared by scheduled jobs and manual maintenance."""

from __future__ import annotations

import asyncio
import os
import uuid
from dataclasses import dataclass
from datetime import UTC, datetime
from pathlib import Path

from qqbot.db.pool import dsn, database_password

MIN_BACKUP_BYTES = 10_000
BACKUP_TIMEOUT_SEC = 15 * 60
STDERR_BYTES = 1024


class BackupError(RuntimeError):
    """A dump could not be created or did not pass local verification."""


@dataclass(frozen=True, slots=True)
class VerifiedBackup:
    path: Path
    size: int
    verified_at: datetime


async def _stderr_tail(stream: asyncio.StreamReader) -> bytes:
    saved = b""
    while chunk := await stream.read(8192):
        saved = (saved + chunk)[-STDERR_BYTES:]
    return saved


async def _command(*args: str, env: dict | None = None) -> tuple[int, bytes]:
    async def execute() -> tuple[int, bytes]:
        spawning = asyncio.create_task(
            asyncio.create_subprocess_exec(
                *args,
                env=env,
                stdout=asyncio.subprocess.DEVNULL,
                stderr=asyncio.subprocess.PIPE,
            )
        )
        process = None
        errors = None
        try:
            process = await asyncio.shield(spawning)
            assert process.stderr is not None
            errors = asyncio.create_task(_stderr_tail(process.stderr))
            async with asyncio.timeout(BACKUP_TIMEOUT_SEC):
                code = await process.wait()
                return code, await errors
        finally:
            if process is None:
                process = await spawning
            if process.returncode is None:
                try:
                    process.kill()
                except ProcessLookupError:
                    pass
            await process.wait()
            if errors is not None:
                errors.cancel()
                await asyncio.gather(errors, return_exceptions=True)

    owner = asyncio.create_task(execute())
    try:
        return await asyncio.shield(owner)
    except asyncio.CancelledError:
        owner.cancel()
        while not owner.done():
            try:
                await asyncio.shield(owner)
            except (asyncio.CancelledError, Exception):
                pass
        if not owner.cancelled():
            owner.exception()
        raise


async def create_verified_backup(
    out_dir: Path,
    *,
    prefix: str = "qqbot",
) -> VerifiedBackup:
    """Publish a dump only after catalog verification; cancel and join owned processes."""
    if (
        not prefix
        or len(prefix) > 64
        or any(not (char.isascii() and (char.isalnum() or char in "-_")) for char in prefix)
    ):
        raise ValueError("backup prefix must be a short filename component")
    out_dir.mkdir(parents=True, exist_ok=True)
    stamp = datetime.now(UTC).strftime("%Y%m%d-%H%M%S")
    target = out_dir / f"{prefix}-{stamp}-{uuid.uuid4().hex}.dump"
    partial = target.with_suffix(".partial")
    env = dict(os.environ)
    if password := database_password():
        env["PGPASSWORD"] = password
    try:
        code, error = await _command(
            "pg_dump",
            "-Fc",
            "--no-password",
            "-d",
            dsn(with_password=False),
            "-f",
            str(partial),
            env=env,
        )
        if code:
            raise BackupError(f"pg_dump failed ({code}): {error.decode(errors='replace')[:500]}")
        code, error = await _command("pg_restore", "--list", str(partial))
        size = partial.stat().st_size if partial.exists() else 0
        if code or size < MIN_BACKUP_BYTES:
            detail = error.decode(errors="replace")[:300]
            raise BackupError(
                f"backup verification failed ({size} bytes, pg_restore {code}): {detail}"
            )
        if target.exists():
            raise BackupError("backup output already exists")
        partial.replace(target)
        return VerifiedBackup(path=target, size=size, verified_at=datetime.now(UTC))
    except TimeoutError as exc:
        raise BackupError("backup process exceeded its deadline") from exc
    finally:
        partial.unlink(missing_ok=True)


def rotate_backups(out_dir: Path, *, prefix: str = "qqbot", keep: int) -> None:
    """Keep the newest verified dump files after a new one has succeeded."""

    if keep < 1:
        raise ValueError("at least one verified backup must be retained")
    dumps = sorted(
        out_dir.glob(f"{prefix}-*.dump"),
        key=lambda path: path.stat().st_mtime,
        reverse=True,
    )
    for old in dumps[keep:]:
        old.unlink(missing_ok=True)
