"""Composition root for configured capabilities.

Provider identity is selected once at startup. Calls receive only reloadable generation
policy; endpoint, credentials, clients and concurrency stay owned by the built capability.
"""

from __future__ import annotations

import logging
from collections.abc import Callable

from ..settings import Settings, TextCfg, VisionCfg
from .base import EmbeddingModel, Providers, SearchEngine, TextModel, VisionModel
from .deepseek import deepseek_text, deepseek_vision
from .embedding import DashScopeEmbedding
from .local import local_text, local_vision
from .openai_responses import OpenAIResponses, openai_vision
from .sherpa import SherpaAsr
from .tavily import TavilySearch

log = logging.getLogger("qqbot.providers")

TextBuilder = Callable[[TextCfg], TextModel]
VisionBuilder = Callable[[VisionCfg], VisionModel]

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


def _instance(table: dict[str, type], name: str, capability: str):
    return _builder(table, name, capability)()


def build(settings: Settings) -> Providers:
    capabilities = settings.capabilities
    search = _instance(SEARCH_PROVIDERS, capabilities.search.provider, "search")
    bundle = Providers(
        text=_builder(
            TEXT_PROVIDERS, capabilities.text.provider, "text"
        )(capabilities.text),
        vision=_builder(
            VISION_PROVIDERS, capabilities.vision.provider, "vision"
        )(capabilities.vision),
        asr=SherpaAsr(),
        embedding=_instance(
            EMBEDDING_PROVIDERS, capabilities.embedding.provider, "embedding"
        ),
        search=search,
        page_reader=search,
    )
    log.info("providers: %s", bundle.describe())
    return bundle
