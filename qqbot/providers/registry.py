"""Composition root for configured capabilities.

Provider identity and client policy are selected once at startup. Calls receive only
request data and request-specific options; clients keep their immutable configuration.
"""

from __future__ import annotations

import logging
from collections.abc import Callable

from ..settings import Settings, TextCfg, VisionCfg
from .base import EmbeddingModel, Providers, RetryPolicy, SearchEngine, TextModel, VisionModel
from .deepseek import deepseek_text, deepseek_vision
from .embedding import DashScopeEmbedding
from .local import local_text, local_vision
from .openai_responses import OpenAIResponses, openai_vision
from .sherpa import SherpaAsr
from .tavily import TavilySearch

log = logging.getLogger("qqbot.providers")

TextBuilder = Callable[[TextCfg, RetryPolicy], TextModel]
VisionBuilder = Callable[[VisionCfg, RetryPolicy], VisionModel]

TEXT_PROVIDERS: dict[str, TextBuilder] = {
    "deepseek": deepseek_text,
    "openai_responses": lambda cfg, retry: OpenAIResponses(cfg, retry),
    "local": local_text,
}
VISION_PROVIDERS: dict[str, VisionBuilder] = {
    "deepseek": deepseek_vision,
    "openai_responses": openai_vision,
    "local": local_vision,
}
EMBEDDING_PROVIDERS: dict[str, type[EmbeddingModel]] = {"dashscope": DashScopeEmbedding}
SEARCH_PROVIDERS: dict[str, type[SearchEngine]] = {"tavily": TavilySearch}


def _builder(table: dict[str, Callable], name: str, capability: str) -> Callable:
    build = table.get(name)
    if build is None:
        raise RuntimeError(
            f"unknown {capability} provider {name!r}; available: {', '.join(sorted(table))}"
        )
    return build


def build(settings: Settings) -> Providers:
    capabilities = settings.capabilities
    retry = RetryPolicy(
        retries=capabilities.http_retries,
        retry_after_cap_sec=capabilities.retry_after_cap_sec,
    )
    search_type = _builder(SEARCH_PROVIDERS, capabilities.search.provider, "search")
    embedding_type = _builder(EMBEDDING_PROVIDERS, capabilities.embedding.provider, "embedding")
    search = search_type(capabilities.search, retry)
    bundle = Providers(
        text=_builder(TEXT_PROVIDERS, capabilities.text.provider, "text")(capabilities.text, retry),
        vision=_builder(VISION_PROVIDERS, capabilities.vision.provider, "vision")(
            capabilities.vision, retry
        ),
        asr=SherpaAsr(capabilities.asr),
        embedding=embedding_type(capabilities.embedding, retry),
        search=search,
        page_reader=search,
    )
    log.info("providers: %s", bundle.describe())
    return bundle
