"""Composition root for configured capabilities.

Provider identity and client policy are selected once at startup. Calls receive only
request data and request-specific options; clients keep their immutable configuration.
"""

from __future__ import annotations

import logging
from collections.abc import Callable

from qqbot.services.budget import Budget
from qqbot.configuration import Settings
from qqbot.configuration import TextCfg
from qqbot.configuration import VisionCfg
from qqbot.providers.base import EmbeddingModel
from qqbot.providers.base import Providers
from qqbot.providers.base import RetryPolicy
from qqbot.providers.base import SearchEngine
from qqbot.providers.base import TextModel
from qqbot.providers.base import VisionModel
from qqbot.providers.deepseek import deepseek_text
from qqbot.providers.deepseek import deepseek_vision
from qqbot.providers.embedding import DashScopeEmbedding
from qqbot.providers.local import local_text
from qqbot.providers.local import local_vision
from qqbot.providers.openai_responses import OpenAIResponses
from qqbot.providers.openai_responses import openai_vision
from qqbot.providers.sherpa import SherpaAsr
from qqbot.providers.tavily import TavilySearch

log = logging.getLogger("qqbot.providers")

TextBuilder = Callable[[TextCfg, RetryPolicy, Budget], TextModel]
VisionBuilder = Callable[[VisionCfg, RetryPolicy, Budget], VisionModel]

TEXT_PROVIDERS: dict[str, TextBuilder] = {
    "deepseek": deepseek_text,
    "openai_responses": OpenAIResponses,
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


def build(settings: Settings, budget: Budget) -> Providers:
    capabilities = settings.backends
    retry = RetryPolicy(
        retries=2,
        retry_after_cap_sec=30,
    )
    search_type = _builder(SEARCH_PROVIDERS, capabilities.search.provider, "search")
    embedding_type = _builder(EMBEDDING_PROVIDERS, capabilities.embedding.provider, "embedding")
    search = search_type(capabilities.search, retry, budget)
    bundle = Providers(
        text=_builder(TEXT_PROVIDERS, capabilities.text.provider, "text")(
            capabilities.text, retry, budget
        ),
        vision=_builder(VISION_PROVIDERS, capabilities.vision.provider, "vision")(
            capabilities.vision, retry, budget
        ),
        asr=SherpaAsr(capabilities.asr, budget),
        embedding=embedding_type(capabilities.embedding, retry, budget),
        search=search,
        page_reader=search,
    )
    log.info("providers: %s", bundle.describe())
    return bundle
