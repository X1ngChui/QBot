"""Backups own their child processes and never publish partial artifacts."""

import asyncio
from pathlib import Path

import pytest

from qqbot.operations import maintenance


class Process:
    def __init__(self, *, code=0, running=False, error=b""):
        self.returncode = None if running else code
        self.stderr = asyncio.StreamReader()
        self.stderr.feed_data(error)
        self.stderr.feed_eof()
        self.entered = asyncio.Event()
        self.done = asyncio.Event()
        self.killed = False
        if not running:
            self.done.set()

    async def wait(self):
        self.entered.set()
        await self.done.wait()
        return self.returncode

    def kill(self):
        self.killed = True
        self.returncode = -9
        self.done.set()


@pytest.fixture
def safe_environment(monkeypatch):
    monkeypatch.setattr(maintenance, "dsn", lambda **kwargs: "postgresql://fictional.invalid/test")
    monkeypatch.setattr(maintenance, "database_password", lambda: None)


async def test_only_verified_complete_dump_gets_a_public_filename(
    tmp_path, monkeypatch, safe_environment
):
    calls = []

    async def spawn(*args, **kwargs):
        calls.append(args)
        if args[0] == "pg_dump":
            target = Path(args[args.index("-f") + 1])
            assert target.suffix == ".partial"
            target.write_bytes(b"x" * maintenance.MIN_BACKUP_BYTES)
        else:
            assert not list(tmp_path.glob("*.dump"))
        return Process()

    monkeypatch.setattr(asyncio, "create_subprocess_exec", spawn)
    result = await maintenance.create_verified_backup(tmp_path)
    assert result.path.suffix == ".dump" and result.path.exists()
    assert not list(tmp_path.glob("*.partial"))
    assert [args[0] for args in calls] == ["pg_dump", "pg_restore"]


@pytest.mark.parametrize("failure", ["pg_dump", "pg_restore"])
async def test_failed_backup_preserves_previous_artifacts(
    tmp_path, monkeypatch, safe_environment, failure
):
    previous = tmp_path / "qqbot-previous.dump"
    previous.write_bytes(b"previous verified artifact")

    async def spawn(*args, **kwargs):
        if args[0] == "pg_dump":
            Path(args[args.index("-f") + 1]).write_bytes(b"x" * maintenance.MIN_BACKUP_BYTES)
        return Process(code=1 if args[0] == failure else 0, error=b"fictional failure")

    monkeypatch.setattr(asyncio, "create_subprocess_exec", spawn)
    with pytest.raises(maintenance.BackupError):
        await maintenance.create_verified_backup(tmp_path)
    assert previous.read_bytes() == b"previous verified artifact"
    assert list(tmp_path.iterdir()) == [previous]


@pytest.mark.parametrize("cancel", [True, False])
async def test_cancellation_or_timeout_kills_and_joins_process(
    tmp_path, monkeypatch, safe_environment, cancel
):
    process = Process(running=True)

    async def spawn(*args, **kwargs):
        Path(args[args.index("-f") + 1]).write_bytes(b"partial")
        return process

    monkeypatch.setattr(asyncio, "create_subprocess_exec", spawn)
    if not cancel:
        monkeypatch.setattr(maintenance, "BACKUP_TIMEOUT_SEC", 0.02)
    task = asyncio.create_task(maintenance.create_verified_backup(tmp_path))
    await process.entered.wait()
    if cancel:
        task.cancel()
    with pytest.raises(asyncio.CancelledError if cancel else maintenance.BackupError):
        await task
    assert process.killed and process.done.is_set()
    assert not list(tmp_path.iterdir())


async def test_stderr_retention_is_bounded(monkeypatch):
    async def spawn(*args, **kwargs):
        return Process(code=1, error=b"x" * 1_000_000 + b"last error")

    monkeypatch.setattr(asyncio, "create_subprocess_exec", spawn)
    code, error = await maintenance._command("fictional-program")
    assert code == 1 and len(error) == maintenance.STDERR_BYTES
    assert error.endswith(b"last error")


def test_invalid_retention_cannot_delete_all_backups(tmp_path):
    previous = tmp_path / "qqbot-previous.dump"
    previous.write_bytes(b"previous")
    with pytest.raises(ValueError):
        maintenance.rotate_backups(tmp_path, keep=0)
    assert previous.exists()
