"""Accounted-spend stop-loss, task-local attribution and fail-closed paid admission.

Concurrent requests can overshoot a monetary threshold: there is no price reservation.
Uncertain ledger writes latch protection for this runtime; restarting cannot reconstruct
charges lost before persistence and is not an accounting-repair procedure.
"""

from __future__ import annotations

from qqbot.domain.ids import AccountId
import asyncio
import logging
from contextlib import contextmanager
from contextvars import ContextVar
from collections.abc import Callable, Iterator
from datetime import date
from enum import StrEnum
import math

from qqbot.repositories.ledger import LedgerRepository
from qqbot.domain.ids import GroupId
from qqbot.providers.base import Kind, QuotaExhausted

log = logging.getLogger("qqbot.budget")


class Scope:
    """Money for one operation, charged by `Budget.record` as the calls inside it
    are booked."""

    __slots__ = ("cap", "spent")

    def __init__(self, cap: float) -> None:
        self.cap = cap
        self.spent = 0.0

    @property
    def exhausted(self) -> bool:
        """Whether this scope has spent its cap.

        A reading of money already spent, never a forecast of the next call.
        Forecasting needs a price for the model, so a model the price table
        does not know fails the forecast on every call - which stops the work
        without spending anything at all, and looks nothing like a budget
        problem from the outside.
        """
        return self.spent >= self.cap

    def charge(self, cny: float) -> None:
        self.spent += cny


#: The scope of the operation currently running, if any. A context variable rather than
#: an argument threaded through every layer: the charge is booked several calls deep in a
#: provider backend, which has no business knowing whose budget it is spending - and a
#: context variable follows the async task, so two groups replying at once do not read
#: each other's meter.
_SCOPE: ContextVar[Scope | None] = ContextVar("budget_scope", default=None)

#: Whose action caused the spend now being booked, if anybody's. Same construction and
#: same reason as the scope above: attribution is known where the work starts (the
#: pipeline knows who addressed the bot, who posted the picture) and booked several
#: calls deep in a backend that must not learn about accounts. It rides created tasks
#: too - asyncio copies the context at create_task - so a single-flight describe is
#: attributed to whoever launched the flight.
_WHO: ContextVar[AccountId | None] = ContextVar("budget_who", default=None)


class LedgerHealth(StrEnum):
    UNAVAILABLE = "unavailable"
    READY = "ready"
    UNCERTAIN = "uncertain"


class BudgetUnavailable(QuotaExhausted):
    """Paid admission is closed because the ledger cannot establish a safe reading."""


class BudgetExceeded(QuotaExhausted):
    """An already-accounted spending threshold has been reached."""


class Budget:
    def __init__(
        self, ledger: LedgerRepository, *, daily_cap: float, today: Callable[[], date | str]
    ) -> None:
        if not math.isfinite(daily_cap) or daily_cap < 0:
            raise ValueError("daily cap must be nonnegative and finite")
        self.ledger = ledger
        self._daily_cap = daily_cap
        self._today = today
        self._day = self._date()
        self._spent = 0.0
        self._loaded = False
        self._write_uncertain = False
        self._lock = asyncio.Lock()

    def _date(self) -> date:
        value = self._today()
        return value if isinstance(value, date) else date.fromisoformat(value)

    @property
    def health(self) -> LedgerHealth:
        if self._write_uncertain:
            return LedgerHealth.UNCERTAIN
        if not self._loaded or self._day != self._date():
            return LedgerHealth.UNAVAILABLE
        return LedgerHealth.READY

    @contextmanager
    def scope(self, cap_cny: float, *, reuse: bool = False) -> Iterator[Scope]:
        if reuse and (current := _SCOPE.get()) is not None:
            yield current
            return
        scope = Scope(cap_cny)
        token = _SCOPE.set(scope)
        try:
            yield scope
        finally:
            _SCOPE.reset(token)

    @contextmanager
    def attribute(self, user_id: AccountId | None) -> Iterator[None]:
        token = _WHO.set(user_id)
        try:
            yield
        finally:
            _WHO.reset(token)

    async def _roll(self) -> None:
        day = self._date()
        if self._loaded and day == self._day:
            return
        self._loaded = False
        try:
            spent = await self.ledger.day_cost(day)
            if not math.isfinite(spent) or spent < 0:
                raise ValueError("invalid ledger total")
        except Exception:
            log.exception("day total unavailable; paid admission is closed")
            return
        self._day = day
        self._spent = spent
        self._loaded = True

    async def check(self) -> None:
        """Check immediately before each provider attempt, including transport retries."""
        async with self._lock:
            await self._roll()
            if self.health is not LedgerHealth.READY:
                raise BudgetUnavailable(f"ledger is {self.health.value}")
            if self._spent >= self._daily_cap:
                raise BudgetExceeded("daily spending threshold reached")
            scope = _SCOPE.get()
            if scope is not None and scope.exhausted:
                raise BudgetExceeded("operation spending threshold reached")

    async def record(
        self,
        *,
        kind: str,
        model: str,
        cny: float,
        group_id: GroupId | None = None,
        in_hit: int = 0,
        in_miss: int = 0,
        out: int = 0,
        calls: int = 1,
    ) -> float:
        """Book completed work without retrying an ambiguously committed write."""
        if not math.isfinite(cny) or cny < 0:
            self._write_uncertain = True
            raise ValueError("charge must be a nonnegative finite amount")
        if (scope := _SCOPE.get()) is not None:
            scope.charge(cny)
        try:
            async with self._lock:
                await self._roll()
                self._spent += cny
                await self.ledger.ledger_add(
                    day=self._date(),
                    group_id=group_id,
                    kind=kind,
                    model=model,
                    in_hit=in_hit,
                    in_miss=in_miss,
                    out=out,
                    calls=calls,
                    cny=cny,
                    user_id=_WHO.get(),
                )
        except asyncio.CancelledError:
            self._write_uncertain = True
            raise
        except Exception:
            # A successful later read cannot prove whether this write committed.
            self._write_uncertain = True
            log.exception("ledger write uncertain; paid admission remains closed")
        return cny

    async def spent_today(self) -> float:
        async with self._lock:
            await self._roll()
            if self.health is not LedgerHealth.READY:
                raise BudgetUnavailable(f"ledger is {self.health.value}")
            return self._spent

    async def exceeded(self) -> bool:
        async with self._lock:
            await self._roll()
            return self.health is not LedgerHealth.READY or self._spent >= self._daily_cap


def _tokens(rows: list[dict], kind: str) -> tuple[int, int]:
    """One kind's prompt tokens across ledger rows, as (cache hits, cache misses)."""
    return (
        sum(int(r["in_hit"]) for r in rows if r["kind"] == kind),
        sum(int(r["in_miss"]) for r in rows if r["kind"] == kind),
    )


def hit_split(rows: list[dict]) -> str:
    """The prefix-cache hit rate over a day's ledger rows, split by what the tokens
    were spent on. "" when the rows carry no prompt tokens at all.

    One blended number hides the signal: the reply rate measures prompt-layout
    discipline and high is its only healthy reading, while extraction reads each
    message for the first time by design - its rate is structurally low (the fixed
    prompt hits, the fresh transcript cannot), so blending the two lets a big
    nightly drain read as a reply-path regression. Split, each number means
    one thing. Everything else (vision describes, mostly) is one bucket: no single
    remainder spends enough to deserve its own line.
    """
    hits = sum(int(r["in_hit"]) for r in rows)
    miss = sum(int(r["in_miss"]) for r in rows)
    reply, extract = _tokens(rows, Kind.REPLY), _tokens(rows, Kind.EXTRACT)
    other = (hits - reply[0] - extract[0], miss - reply[1] - extract[1])
    return "　".join(
        f"{label} {h / (h + m) * 100:.0f}%"
        for label, (h, m) in (("回复", reply), ("归纳", extract), ("其他", other))
        if h + m
    )
