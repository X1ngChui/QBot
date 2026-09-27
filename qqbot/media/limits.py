"""Code-owned bounds for this mechanism; not operator configuration."""

from dataclasses import dataclass


@dataclass(frozen=True, slots=True)
class MediaIO:
    work_timeout_sec: float = 180.0
    wait_sec: float = 25.0
    protocol_timeout_sec: float = 10.0
    http_timeout_sec: float = 20.0
    unreadable_retry_sec: int = 600
    shutdown_wait_sec: float = 5.0


MEDIA_IO = MediaIO()
