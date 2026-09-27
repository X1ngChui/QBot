"""Provider protocol constraints are not independently tunable YAML values."""

from datetime import timedelta
from types import SimpleNamespace
from unittest.mock import AsyncMock

import httpx
import pytest

from _budget import fake_budget
import _db as _test_db
from qqbot.services.members import MemberDirectory
from qqbot.configuration.schema import EmbeddingCfg
from qqbot.domain.memory.embedding import VECTOR_DIMENSIONS
from qqbot.gateway.segments import ImageRef
from qqbot.media.service import MediaProcessor
from qqbot.providers import deepseek, embedding
from qqbot.providers.base import RetryPolicy
from qqbot.providers.contracts import StoredImage


async def test_embedding_batches_and_reorders_by_explicit_input_index(monkeypatch):
    requests = []
    booked = AsyncMock()
    monkeypatch.setattr(embedding, "require_key", lambda *args: "synthetic")
    cfg = EmbeddingCfg(provider="dashscope", endpoint="https://example.invalid", model="fictional")
    adapter = embedding.DashScopeEmbedding(cfg, RetryPolicy(0, 0), budget=fake_budget())
    adapter._budget.record = booked
    await adapter._http.aclose()

    def handle(request):
        import json

        body = json.loads(request.content)
        requests.append(body)
        assert body["dimensions"] == VECTOR_DIMENSIONS
        return httpx.Response(
            200,
            json={
                "data": [
                    {"index": i, "embedding": [float(text)] * VECTOR_DIMENSIONS}
                    for i, text in reversed(list(enumerate(body["input"])))
                ]
            },
        )

    adapter._http = httpx.AsyncClient(transport=httpx.MockTransport(handle))
    try:
        result = await adapter.embed([str(i) for i in range(23)])
        assert [len(request["input"]) for request in requests] == [10, 10, 3]
        assert [vector[0] for vector in result] == list(range(23))
        assert booked.await_count == 3
    finally:
        await adapter.aclose()


@pytest.mark.parametrize(
    "data",
    [[], [{"index": 1, "embedding": [0] * VECTOR_DIMENSIONS}], [{"index": 0, "embedding": [0]}]],
)
async def test_malformed_embedding_is_charged_but_never_projected(data, monkeypatch):
    booked = AsyncMock()
    monkeypatch.setattr(embedding, "require_key", lambda *args: "synthetic")
    cfg = EmbeddingCfg(provider="dashscope", endpoint="https://example.invalid", model="fictional")
    adapter = embedding.DashScopeEmbedding(cfg, RetryPolicy(0, 0), budget=fake_budget())
    adapter._budget.record = booked
    await adapter._http.aclose()
    adapter._http = httpx.AsyncClient(
        transport=httpx.MockTransport(lambda request: httpx.Response(200, json={"data": data}))
    )
    try:
        with pytest.raises(ValueError, match="embedding response"):
            await adapter.embed(["fictional"])
        booked.assert_awaited_once()
    finally:
        await adapter.aclose()


async def test_file_cache_always_checks_adapter_namespace_and_age(bundle, monkeypatch):
    store = SimpleNamespace(
        cache_max_age=timedelta(minutes=10),
        cache_namespace="fictional-account",
        store=AsyncMock(return_value=StoredImage("fictional", "new-handle")),
    )
    processor = MediaProcessor(
        SimpleNamespace(text=SimpleNamespace(name="fictional", attachments=store)),
        object(),
        budget=fake_budget(),
        members=MemberDirectory(),
        cache=_test_db.media_cache,
        prompts=_test_db.test_bundle().prompts,
    )
    lookup = AsyncMock(side_effect=["old-handle", None])
    persist = AsyncMock()
    monkeypatch.setattr(processor._cache, "image_cache_file", lookup)
    monkeypatch.setattr(processor._cache, "image_cache_set_file", persist)
    processor._bytes = AsyncMock(return_value=b"GIF89a")
    ref = ImageRef(key="a" * 32)
    kwargs = {"bot": object(), "group_id": "1", "cfg": bundle.default}
    try:
        assert (await processor.ensure_uploaded(ref, **kwargs)).handle == "old-handle"
        assert (await processor.ensure_uploaded(ref, **kwargs)).handle == "new-handle"
        assert lookup.await_count == 2
        assert lookup.call_args.kwargs == {
            "provider": store.cache_namespace,
            "max_age": store.cache_max_age,
        }
        assert persist.call_args.kwargs["provider"] == store.cache_namespace
    finally:
        await processor.close()


async def test_deepseek_cache_namespace_changes_with_account_and_endpoint(bundle, monkeypatch):
    key = "synthetic-first"
    monkeypatch.setattr(deepseek, "require_key", lambda *args: key)
    adapter = deepseek.DeepSeekAttachmentStore(bundle.default.backends.text, RetryPolicy(0, 0))
    try:
        first = adapter.cache_namespace
        key = "synthetic-second"
        assert first != adapter.cache_namespace
        second = adapter.cache_namespace
        adapter._endpoint = "https://different.example.invalid"
        assert second != adapter.cache_namespace
        assert len(adapter.cache_namespace) == 32
        assert adapter.cache_max_age < timedelta(seconds=adapter.FILE_TTL_SEC)
    finally:
        await adapter.aclose()
