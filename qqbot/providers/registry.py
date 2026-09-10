"""Which backend serves which capability, chosen by name from config.

Adding a backend is: write the subclass, add one line to the table below, name it in
settings.yaml. Nothing else in the codebase changes - that is the whole point of the ABCs
in `base`.

Selection happens once at startup from the top-level config. Per-group persona overrides
can retarget endpoint and model (those are passed per call), but not the backend class;
a group that tries is logged and ignored.
"""

from __future__ import annotations

import logging

from ..settings import Settings
from .base import (
    AsrModel, EmbeddingModel, Providers, SearchEngine, TextModel, VisionModel,
)
from .dashscope import DashScopeAsr
from .deepseek import DeepSeekChat, DeepSeekVision
from .embedding import DashScopeEmbedding
from .local import LocalChat
from .openai_compat import OpenAICompatAsr, OpenAICompatChat, OpenAICompatVision
from .sherpa import SherpaAsr
from .tavily import TavilySearch

log = logging.getLogger("qqbot.providers")

TEXT_BACKENDS: dict[str, type[TextModel]] = {
    "deepseek": DeepSeekChat,
    "openai_compat": OpenAICompatChat,
    # Self-hosted endpoint (the NPU/CPU toy): generic protocol, zero rates.
    "local": LocalChat,
}

VISION_BACKENDS: dict[str, type[VisionModel]] = {
    "deepseek": DeepSeekVision,
    "openai_compat": OpenAICompatVision,
}

ASR_BACKENDS: dict[str, type[AsrModel]] = {
    "dashscope": DashScopeAsr,
    "openai_compat": OpenAICompatAsr,
    # In-process sherpa-onnx (SenseVoice): CPU decoding, zero rates.
    "sherpa": SherpaAsr,
}

#: Its own block, never borrowed from another capability's: a capability that can move
#: platforms independently needs wiring that names it. Sharing one silently drags
#: embedding onto whatever endpoint the other capability moves to, and every reply
#: needing recall dies there on a 404.
EMBEDDING_BACKENDS: dict[str, type[EmbeddingModel]] = {
    "dashscope": DashScopeEmbedding,
}

SEARCH_BACKENDS: dict[str, type[SearchEngine]] = {
    "tavily": TavilySearch,
}


def _pick(table: dict, name: str, capability: str):
    try:
        return table[name]()
    except KeyError:
        raise RuntimeError(
            f"unknown {capability} backend {name!r}; "
            f"available: {', '.join(sorted(table))}"
        ) from None


def build(settings: Settings) -> Providers:
    llm = settings.llm
    bundle = Providers(
        text=_pick(TEXT_BACKENDS, llm.text.backend, "text"),
        vision=_pick(VISION_BACKENDS, llm.vision.backend, "vision"),
        asr=_pick(ASR_BACKENDS, llm.asr.backend, "asr"),
        embedding=_pick(EMBEDDING_BACKENDS, llm.embedding.backend, "embedding"),
        search=_pick(SEARCH_BACKENDS, llm.search.backend, "search"),
    )
    log.info("providers: %s", bundle.describe())
    return bundle
