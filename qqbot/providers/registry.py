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
from .base import AsrModel, Providers, SearchEngine, TextModel, VisionModel
from .dashscope import DashScopeAsr
from .deepseek import DeepSeekChat, DeepSeekVision
from .openai_compat import OpenAICompatAsr, OpenAICompatChat, OpenAICompatVision
from .tavily import TavilySearch

log = logging.getLogger("qqbot.providers")

TEXT_BACKENDS: dict[str, type[TextModel]] = {
    "deepseek": DeepSeekChat,
    "openai_compat": OpenAICompatChat,
}

VISION_BACKENDS: dict[str, type[VisionModel]] = {
    "deepseek": DeepSeekVision,
    "openai_compat": OpenAICompatVision,
}

ASR_BACKENDS: dict[str, type[AsrModel]] = {
    "dashscope": DashScopeAsr,
    "openai_compat": OpenAICompatAsr,
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
        search=_pick(SEARCH_BACKENDS, llm.search.backend, "search"),
    )
    log.info("providers: %s", bundle.describe())
    return bundle
