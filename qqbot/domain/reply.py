"""Observable reply outcomes retain side effects independently of termination."""

from __future__ import annotations

from dataclasses import dataclass
from enum import StrEnum


class ReplyEnd(StrEnum):
    FINISHED = "finished"
    TIMEOUT = "timeout"
    FAILED = "failed"
    CANCELLED = "cancelled"
    OVERLOADED = "overloaded"
    EXPIRED = "expired"
    UNOBSERVED = "unobserved"
    BUDGET = "budget"
    BLOCKED = "blocked"
    MUTED = "muted"


@dataclass(frozen=True, slots=True)
class ReplyOutcome:
    end: ReplyEnd
    acknowledged: int = 0
    observed: int = 0
    uncertain: bool = False

    @property
    def sent(self) -> bool:
        return self.acknowledged > 0


@dataclass(slots=True)
class ReplyProgress:
    """Session-owned facts survive an exception after a platform side effect."""

    acknowledged: int = 0
    observed: int = 0
    uncertain: bool = False
    sending: bool = False

    def begin_send(self) -> None:
        self.sending = True

    def confirm(self) -> None:
        self.sending = False
        self.acknowledged += 1

    def observe(self) -> None:
        self.observed += 1

    def finish(self, end: ReplyEnd) -> ReplyOutcome:
        if end is ReplyEnd.FINISHED and (self.uncertain or self.observed < self.acknowledged):
            end = ReplyEnd.UNOBSERVED
        return ReplyOutcome(end, self.acknowledged, self.observed, self.uncertain or self.sending)
