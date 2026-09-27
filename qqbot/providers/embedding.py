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
import math
from collections.abc import Sequence

import httpx

from qqbot.services.budget import Budget
from qqbot.domain.ids import GroupId
from qqbot.domain.memory.embedding import VECTOR_DIMENSIONS
from qqbot.configuration import EmbeddingCfg
from qqbot.util import require_key
from qqbot.providers.base import EmbeddingModel
from qqbot.providers.base import Kind
from qqbot.providers.base import Rate
from qqbot.providers.base import RetryPolicy
from qqbot.providers.base import with_retry

log = logging.getLogger("qqbot.embed")

#: The endpoint's per-request ceiling. Exceeding it is a 400, and the error text does
#: not say that this is why.
BATCH = 10


class DashScopeEmbedding(EmbeddingModel):
    name = "dashscope"
    dimensions = VECTOR_DIMENSIONS
    batch_size = BATCH

    def __init__(self, cfg: EmbeddingCfg, retry: RetryPolicy, budget: Budget) -> None:
        self._budget = budget
        self._cfg = cfg
        self._retry = retry
        self._http = httpx.AsyncClient(timeout=cfg.timeout_sec)

    def rate_for(self, model: str) -> Rate:
        # Bailian price list: text-embedding-v4 is CNY 0.5 per million tokens, input
        # only.
        return Rate("Mtoken", in_miss=0.5, source="bailian price list, rechecked 2026-08-27")

    async def embed(
        self, texts: Sequence[str], *, group_id: GroupId | None = None
    ) -> list[list[float]]:
        cfg = self._cfg
        out: list[list[float]] = []
        key = require_key(cfg.credential_env, "embedding")
        base = cfg.endpoint.rstrip("/")
        for i in range(0, len(texts), BATCH):
            chunk = list(texts[i : i + BATCH])

            async def post(chunk=chunk) -> httpx.Response:
                await self._budget.check()
                r = await self._http.post(
                    f"{base}/embeddings",
                    headers={"Authorization": f"Bearer {key}"},
                    json={"model": cfg.model, "input": chunk, "dimensions": self.dimensions},
                )
                r.raise_for_status()
                return r

            # Retried per batch, so a momentary refusal costs one request's worth
            # of sleep, and a batch already booked below is never sent twice.
            body = (await with_retry(post, what="embedding", policy=self._retry)).json()
            # Booked like every other paid capability: unbooked, this spend was
            # invisible to the daily cap, /stats and the report. The vendor reports
            # input tokens; missing usage bills the batch's characters instead,
            # which for Chinese text leans high - the safe direction.
            usage = body.get("usage") or {}
            tokens = int(
                usage.get("total_tokens")
                or usage.get("prompt_tokens")
                or sum(len(t) for t in chunk)
            )
            await self._budget.record(
                kind=Kind.EMBED,
                model=cfg.model,
                cny=self.rate_for(cfg.model).tokens(0, tokens, 0),
                group_id=group_id,
                in_miss=tokens,
            )
            data = body.get("data")
            if not isinstance(data, list) or len(data) != len(chunk):
                raise ValueError("embedding response has the wrong number of vectors")
            by_index = {item.get("index"): item for item in data if isinstance(item, dict)}
            if set(by_index) != set(range(len(chunk))):
                raise ValueError("embedding response indexes do not match the input")
            for index in range(len(chunk)):
                values = by_index[index].get("embedding")
                if not isinstance(values, list) or len(values) != self.dimensions:
                    raise ValueError("embedding response violates the vector width contract")
                vector = [float(value) for value in values]
                if not all(math.isfinite(value) for value in vector):
                    raise ValueError("embedding response contains a non-finite coordinate")
                out.append(vector)
        return out

    async def aclose(self) -> None:
        await self._http.aclose()
