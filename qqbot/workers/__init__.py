"""The background executor. All of its state is in the database, so it can be killed
and restarted at any moment."""

from .memory import MemoryWorker

__all__ = ["MemoryWorker"]
