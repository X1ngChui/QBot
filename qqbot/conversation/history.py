"""Derive cache-stable history eviction and projection headroom from one preference."""

from dataclasses import dataclass


@dataclass(frozen=True, slots=True)
class HistoryWindow:
    messages: int = 90

    def __post_init__(self) -> None:
        if self.messages < 1:
            raise ValueError("history size must be positive")

    @property
    def chunk(self) -> int:
        return max(1, min(30, self.messages // 3))

    @property
    def capacity(self) -> int:
        return self.messages + 2 * self.chunk
