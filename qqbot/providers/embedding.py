"""Bailian (DashScope) embedding backend.

The choice was measured rather than assumed: text-embedding-v4 at 2048
dimensions. Over the same set of Chinese probes, the separation between related and
unrelated sentences was 0.345 at 1024 dimensions and 0.388 at 2048. The larger one wins.

The cost is that pgvector's hnsw cannot index 2048 dimensions. That trade is deliberate:
retrieval always filters by group first, which leaves a few hundred rows, and an exact
scan over those is both more accurate than an approximate one and fast enough that the
difference is not visible.

The endpoint accepts at most BATCH inputs per request and answers anything larger with a
400 whose text does not say so. Batching therefore happens here, and no caller has to
know it exists.
"""

from __future__ import annotations

import logging
from collections.abc import Sequence

import httpx

from ..core.budget import BUDGET
from ..settings import EmbeddingCfg
from ..util import require_key
from .base import EmbeddingModel, Kind, Rate, retire, with_retry

log = logging.getLogger("qqbot.embed")

#: The endpoint's per-request ceiling. Exceeding it is a 400, and the error text does
#: not say that this is why.
BATCH = 10


class DashScopeEmbedding(EmbeddingModel):
    name = "dashscope"

    def __init__(self) -> None:
        self._http: httpx.AsyncClient | None = None
        #: What the cached client was built for, so a /reload that moves the endpoint
        #: or the timeout rebuilds it - the same rule the chat clients follow.
        self._id: tuple = ()

    def rate_for(self, model: str) -> Rate:
        # Bailian price list: text-embedding-v4 is CNY 0.5 per million tokens, input
        # only.
        return Rate("Mtoken", in_miss=0.5,
                    source="bailian price list, rechecked 2026-08-27")

    def _client(self, cfg: EmbeddingCfg) -> httpx.AsyncClient:
        ident = (cfg.endpoint, cfg.timeout_sec)
        if self._http is None or self._id != ident:
            if self._http is not None:
                retire(self._http.aclose())
            self._http = httpx.AsyncClient(timeout=cfg.timeout_sec)
            self._id = ident
        return self._http

    async def embed(self, texts: Sequence[str], *, cfg: EmbeddingCfg,
                    group_id: str | None = None) -> list[list[float]]:
        out: list[list[float]] = []
        key = require_key(cfg.credential_env, "embedding")
        base = cfg.endpoint.rstrip("/")
        for i in range(0, len(texts), BATCH):
            chunk = list(texts[i:i + BATCH])

            async def post(chunk=chunk) -> httpx.Response:
                r = await self._client(cfg).post(
                    f"{base}/embeddings",
                    headers={"Authorization": f"Bearer {key}"},
                    json={"model": cfg.model, "input": chunk,
                          "dimensions": cfg.dimensions},
                )
                r.raise_for_status()
                return r

            # Retried per batch, so a momentary refusal costs one request's worth
            # of sleep, and a batch already booked below is never sent twice.
            body = (await with_retry(post, what="embedding")).json()
            # Booked like every other paid capability: unbooked, this spend was
            # invisible to the daily cap, /stats and the report. The vendor reports
            # input tokens; missing usage bills the batch's characters instead,
            # which for Chinese text leans high - the safe direction.
            usage = body.get("usage") or {}
            tokens = int(usage.get("total_tokens") or usage.get("prompt_tokens")
                         or sum(len(t) for t in chunk))
            await BUDGET.record(
                kind=Kind.EMBED, model=cfg.model,
                cny=self.rate_for(cfg.model).tokens(0, tokens, 0),
                group_id=group_id, in_miss=tokens,
            )
            data = sorted(body["data"], key=lambda d: d.get("index", 0))
            out.extend(d["embedding"] for d in data)
        return out

    async def aclose(self) -> None:
        if self._http is not None:
            await self._http.aclose()
            self._http = None
            self._id = ()
