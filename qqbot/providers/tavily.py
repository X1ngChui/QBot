"""Tavily web search backend.

Not a chat protocol - a plain POST to /search - so this implements the ABC directly.
Called on its own rather than through a model's built-in search: built-in search
bills twice and splices results at a position we do not control, which would break the
prefix cache.

The free tier is a monthly credit allowance rather than a per-call price, and the two
facts below follow from that:

- A call within the allowance is free, and its ledger row carries cny=0 with calls=1.
  The calendar-month call count in cost_ledger IS the quota meter - the allowance is
  read back from the ledger, never tracked separately.
- At the allowance the backend refuses (QuotaExhausted) instead of billing. There is
  no paid fallback and no degraded answer by design: the exception propagates and the
  reply is dropped.

The endpoint is not reliably reachable from the deployment region directly, so the
client can be pointed through an HTTP proxy (cfg.proxy). Only this capability's client
uses it - model traffic stays direct.
"""

from __future__ import annotations

import asyncio
import logging

import httpx

from ..core.budget import BUDGET
from ..db import repo
from ..settings import SearchCfg, WebSearchToolCfg
from ..util import require_key
from .base import Kind, QuotaExhausted, Rate, SearchEngine, retire, with_retry

log = logging.getLogger("qqbot.search")

#: What one credit costs within the free tier. Zero is the true price, not a missing
#: number: the constraint is the monthly allowance, enforced in search() itself.
_FREE = Rate("call", per_unit=0.0, source="tavily free tier, 1000 credits/month")


class TavilySearch(SearchEngine):
    name = "tavily"

    def __init__(self) -> None:
        self._client: httpx.AsyncClient | None = None
        #: What the cached client was built for - the same rule as the other
        #: clients, so a /reload that changes either the proxy or the timeout
        #: rebuilds it. The endpoint is not part of it: it is named per request.
        self._id: tuple[float, str] | None = None
        self._quota_lock = asyncio.Lock()

    def rate_for(self, model: str) -> Rate:
        return _FREE

    def _http(self, cfg: SearchCfg) -> httpx.AsyncClient:
        ident = (cfg.timeout_sec, cfg.proxy)
        if self._client is None or self._id != ident:
            if self._client is not None:
                retire(self._client.aclose())
            self._client = httpx.AsyncClient(timeout=cfg.timeout_sec, proxy=cfg.proxy or None)
            self._id = ident
        return self._client

    async def aclose(self) -> None:
        if self._client is not None:
            await self._client.aclose()
            self._client = None
            self._id = None

    async def _admit(self, cfg: SearchCfg, credits: int) -> str:
        """Read the meter and resolve the key; the credential a call may go out with.

        The meter is read before the vendor is called, so a call past the monthly
        allowance never leaves the building. Counted in vendor credits via the
        ledger's calls column, under this backend's model name: rows from a
        previous backend do not eat this one's allowance.
        """
        used = await repo.month_calls(Kind.SEARCH, self.name)
        if used + credits > cfg.monthly_quota:
            raise QuotaExhausted(f"search allowance used up: {used}/{cfg.monthly_quota} this month")
        return require_key(cfg.credential_env, "search")

    async def _post(self, cfg: SearchCfg, key: str, path: str, body: dict) -> httpx.Response:
        async def once() -> httpx.Response:
            r = await self._http(cfg).post(
                f"{cfg.endpoint.rstrip('/')}{path}",
                headers={"Authorization": f"Bearer {key}"},
                json=body,
            )
            r.raise_for_status()
            return r

        return await with_retry(once, what=f"search {path}")

    async def _credited_post(
        self,
        cfg: SearchCfg,
        *,
        path: str,
        body: dict,
        credits: int,
        group_id: str | None,
    ) -> httpx.Response:
        """Admit, execute, and book one request against the shared credit meter."""
        async with self._quota_lock:
            key = await self._admit(cfg, credits)
            response = await self._post(cfg, key, path, body)
            # Booked even at zero cost: calls are the vendor-credit meter.
            await BUDGET.record(
                kind=Kind.SEARCH,
                model=self.name,
                cny=0.0,
                group_id=group_id,
                calls=credits,
            )
        return response

    async def search(
        self,
        query: str,
        *,
        cfg: SearchCfg,
        options: WebSearchToolCfg,
        group_id: str | None = None,
    ) -> list[dict]:
        credits = 2 if options.depth == "advanced" else 1
        r = await self._credited_post(
            cfg,
            path="/search",
            body={
                "query": query,
                "search_depth": options.depth,
                "max_results": options.count,
            },
            credits=credits,
            group_id=group_id,
        )
        items = r.json().get("results") or []
        return [
            {
                "title": (it.get("title") or "").strip(),
                "link": (it.get("url") or "").strip(),
                "content": " ".join((it.get("content") or "").split()),
            }
            for it in items[: options.count]
        ]

    async def read_page(
        self, url: str, *, cfg: SearchCfg, group_id: str | None = None
    ) -> str:
        """One page's readable text, off the same monthly allowance as search.

        The vendor debits extraction from the same credit pool (basic depth: one
        credit per request of up to five URLs; this sends one), so the meter, the
        refusal at the allowance and the proxy path are all the search call's,
        reused. Booked pessimistically at one credit per call - under-counting a
        shared pool is how the vendor starts refusing while the meter reads full.
        """
        r = await self._credited_post(
            cfg,
            path="/extract",
            body={"urls": [url], "extract_depth": "basic"},
            credits=1,
            group_id=group_id,
        )
        results = r.json().get("results") or []
        text = (results[0].get("raw_content") or "") if results else ""
        return " ".join(text.split())
