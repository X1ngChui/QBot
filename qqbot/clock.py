"""A runtime-owned display and accounting clock; database leases retain database time."""

from collections.abc import Callable
from dataclasses import dataclass, field
from datetime import UTC, date, datetime
from zoneinfo import ZoneInfo

WEEKDAYS = ("周一", "周二", "周三", "周四", "周五", "周六", "周日")


def utc_now() -> datetime:
    return datetime.now(UTC)


@dataclass(frozen=True, slots=True)
class Clock:
    timezone: str
    wall: Callable[[], datetime] = field(default=utc_now, repr=False, compare=False)
    zone: ZoneInfo = field(init=False)

    def __post_init__(self) -> None:
        object.__setattr__(self, "zone", ZoneInfo(self.timezone))

    def now(self) -> datetime:
        value = self.wall()
        if value.utcoffset() is None:
            raise ValueError("clock readings must be timezone-aware")
        return value.astimezone(self.zone)

    def today(self) -> date:
        return self.now().date()

    def format(self, value: datetime) -> str:
        if value.utcoffset() is None:
            raise ValueError("display timestamps must be timezone-aware")
        return value.astimezone(self.zone).strftime("%m-%d %H:%M")

    def describe(self) -> str:
        value = self.now()
        offset = value.strftime("%z")
        return (
            f"{value:%Y年%m月%d日} {WEEKDAYS[value.weekday()]} {value:%H:%M}"
            f"（UTC{offset[:3]}:{offset[3:]}）"
        )
