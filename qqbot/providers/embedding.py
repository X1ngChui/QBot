"""The embedding capability.

The choice was measured rather than assumed (design goal 2): DashScope's
text-embedding-v4 at 2048 dimensions. Over the same set of Chinese probes, the separation
between related and unrelated sentences was 0.345 at 1024 dimensions and 0.388 at 2048.
The larger one wins.

The cost is that pgvector's hnsw cannot index 2048 dimensions. That trade is deliberate:
retrieval always filters by group first, which leaves a few hundred rows, and an exact
scan over those is both more accurate than an approximate one and fast enough that the
difference is not visible.

The endpoint accepts at most BATCH inputs per request and answers anything larger with a
400 whose text does not say so. Batching therefore happens in this layer, and no caller
has to know it exists.
"""

from __future__ import annotations

import logging
from collections.abc import Sequence

import httpx

from ..core.budget import BUDGET
from ..util import read_api_key
from .base import Capability, Kind, Rate

log = logging.getLogger("qqbot.embed")

#: The endpoint's per-request ceiling. Exceeding it is a 400, and the error text does
#: not say that this is why.
BATCH = 10


class EmbeddingModel(Capability):
    """Text in, vectors out. An implementation is responsible for its own batching."""

    async def embed(self, texts: Sequence[str], *, group_id: str | None = None
                    ) -> list[list[float]]:
        raise NotImplementedError

    @property
    def dimensions(self) -> int:
        raise NotImplementedError


def build(settings) -> EmbeddingModel:
    """The embedding backend, from its own config block.

    Its own block, never borrowed from another capability's: a capability that
    can move platforms independently needs wiring that names it, or retargeting
    the other capability silently drags embedding onto an endpoint that does not
    serve it and every reply needing recall dies on a 404.
    """
    e = settings.llm.embedding
    table = {"dashscope": DashScopeEmbedding}
    try:
        cls = table[e.backend]
    except KeyError:
        raise RuntimeError(
            f"unknown embedding backend {e.backend!r}; available: "
            + ", ".join(sorted(table))) from None
    return cls(base_url=e.base_url, api_key_env=e.api_key_env,
               model=e.model, dimensions=e.dimensions, timeout=e.timeout_sec)


class DashScopeEmbedding(EmbeddingModel):
    name = "dashscope"

    def __init__(self, *, base_url: str, api_key_env: str, model: str,
                 dimensions: int = 2048, timeout: float = 60.0) -> None:
        self._base = base_url.rstrip("/")
        self._key_env = api_key_env
        self._model = model
        self._dims = dimensions
        self._timeout = timeout
        self._http: httpx.AsyncClient | None = None

    @property
    def dimensions(self) -> int:
        return self._dims

    def rate_for(self, model: str) -> Rate:
        # Bailian price list: text-embedding-v4 is CNY 0.5 per million tokens, input
        # only.
        return Rate("Mtoken", in_miss=0.5, source="bailian price list, rechecked 2026-08-27")

    def _client(self) -> httpx.AsyncClient:
        if self._http is None:
            self._http = httpx.AsyncClient(timeout=self._timeout)
        return self._http

    async def embed(self, texts: Sequence[str], *, group_id: str | None = None
                    ) -> list[list[float]]:
        out: list[list[float]] = []
        key = read_api_key(self._key_env)
        if not key:
            raise RuntimeError(
                f"no embedding API key: {self._key_env} resolved to nothing")
        for i in range(0, len(texts), BATCH):
            chunk = list(texts[i:i + BATCH])
            r = await self._client().post(
                f"{self._base}/embeddings",
                headers={"Authorization": f"Bearer {key}"},
                json={"model": self._model, "input": chunk, "dimensions": self._dims},
            )
            r.raise_for_status()
            body = r.json()
            # Booked like every other paid capability: unbooked, this spend was
            # invisible to the daily cap, /stats and the report. The vendor reports
            # input tokens; missing usage bills the batch's characters instead,
            # which for Chinese text leans high - the safe direction.
            usage = body.get("usage") or {}
            tokens = int(usage.get("total_tokens") or usage.get("prompt_tokens")
                         or sum(len(t) for t in chunk))
            await BUDGET.record(
                kind=Kind.EMBED, model=self._model,
                cny=self.rate_for(self._model).tokens(0, tokens, 0),
                group_id=group_id, in_miss=tokens,
            )
            data = sorted(body["data"], key=lambda d: d.get("index", 0))
            out.extend(d["embedding"] for d in data)
        return out

    async def aclose(self) -> None:
        if self._http is not None:
            await self._http.aclose()
            self._http = None
