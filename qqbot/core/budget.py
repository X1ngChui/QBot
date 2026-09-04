"""The budget: the one limit this system has (section 5.4).

Money is the only constraint. Anything free runs as often as it likes; anything paid
answers to money at two levels, both of which live here:

- The day. One shared cap; at it the bot stops answering and the workers stop the paid
  half of their work until the day rolls over.
- The operation. `with BUDGET.scope(cap) as spend:` meters everything charged inside the
  context - every backend books its own spend through `record`, which credits the ambient
  scope automatically - so a reply's tool loop is bounded by what it may cost, not by a
  count standing in for cost (a count is a proxy that silently drifts when prices move).

The running total is kept in memory and every charge is written to cost_ledger; on
startup the total is restored from the DB so a restart does not reset the day.
"""

from __future__ import annotations

import asyncio
import logging
from contextlib import contextmanager
from contextvars import ContextVar
from typing import Iterator

from ..db import repo
from ..providers.base import Kind
from ..util import today_local

log = logging.getLogger("qqbot.budget")


class Scope:
    """Money for one operation. Charged by `Budget.record` as the calls inside it are
    booked; the operation reads `remaining` and `can_afford` to decide what it may still
    do."""

    __slots__ = ("cap", "spent")

    def __init__(self, cap: float) -> None:
        self.cap = cap
        self.spent = 0.0

    @property
    def remaining(self) -> float:
        return max(0.0, self.cap - self.spent)

    def can_afford(self, cny: float) -> bool:
        return self.spent + cny <= self.cap

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
_WHO: ContextVar[str | None] = ContextVar("budget_who", default=None)


class Budget:
    def __init__(self) -> None:
        self._day = today_local()
        self._spent = 0.0
        self._loaded = False
        #: Guards the roll-and-add sequence in record(): _roll awaits a DB read, and a
        #: concurrent roll assigning self._spent would overwrite an increment another
        #: task applied in between.
        self._lock = asyncio.Lock()

    @contextmanager
    def scope(self, cap_cny: float) -> Iterator[Scope]:
        """Meter everything charged within this context against one cap."""
        s = Scope(cap_cny)
        token = _SCOPE.set(s)
        try:
            yield s
        finally:
            _SCOPE.reset(token)

    @contextmanager
    def attribute(self, user_id: str | None) -> Iterator[None]:
        """Book everything charged within this context to one account.

        The account, not the person: entities merge and split after the fact, and
        the ledger is append-only - person-level readings (the /top leaderboard)
        aggregate accounts through identity_account at query time, which is what
        makes them follow a later merge without rewriting history.
        """
        token = _WHO.set((user_id or "").strip() or None)
        try:
            yield
        finally:
            _WHO.reset(token)

    async def _roll(self) -> None:
        day = today_local()
        if not self._loaded or day != self._day:
            self._day = day
            self._spent = await repo.day_cost(day)
            self._loaded = True

    async def record(
        self,
        *,
        kind: str,
        model: str,
        cny: float,
        group_id: str | None = None,
        in_hit: int = 0,
        in_miss: int = 0,
        out: int = 0,
        calls: int = 1,
    ) -> float:
        """Book a charge the capability has already priced.

        The amount arrives already computed: only the backend knows what its vendor bills,
        and this class only has to know the running totals - the day's and, when an
        operation is being metered, the operation's.

        `calls` is the ledger's countable unit and defaults to one per booking; a
        capability whose vendor debits an allowance in larger units (an advanced
        search costs two credits) books that number, so a quota read back from the
        ledger meters what the vendor actually counts.
        """
        async with self._lock:
            await self._roll()
            self._spent += cny
        if (s := _SCOPE.get()) is not None:
            s.charge(cny)
        try:
            await repo.ledger_add(
                group_id=group_id, kind=kind, model=model,
                in_hit=in_hit, in_miss=in_miss, out=out, calls=calls, cny=cny,
                user_id=_WHO.get() or "",
            )
        except Exception:
            # Not re-raised: the call it is booking has already happened and been paid
            # for, and killing the reply afterwards would not unspend the money. But it is
            # logged as an error rather than a note, because what is now wrong is the
            # spend total - and therefore the daily cap, /stats and the report. A ledger
            # that quietly stops accepting rows reads exactly like a quiet day (a real
            # incident, not a hypothetical).
            log.exception("cost_ledger write failed: today's spend is now understated, "
                          "and the daily cap is measuring from the wrong number")
        return cny

    async def spent_today(self) -> float:
        # Under the lock like record(): _roll awaits a DB read and then assigns
        # _spent, and unlocked that assignment could erase an increment a concurrent
        # record() applied in between - the exact clobber the lock documents. It
        # only ever contends at day rollover and startup.
        async with self._lock:
            await self._roll()
            return self._spent

    async def exceeded(self, cap: float) -> bool:
        async with self._lock:
            await self._roll()
            return self._spent >= cap


BUDGET = Budget()


def _tokens(rows: list[dict], kind: str) -> tuple[int, int]:
    """One kind's prompt tokens across ledger rows, as (cache hits, cache misses)."""
    return (sum(int(r["in_hit"]) for r in rows if r["kind"] == kind),
            sum(int(r["in_miss"]) for r in rows if r["kind"] == kind))


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
        if h + m)
