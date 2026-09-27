"""Self-hosted Responses capabilities with zero CNY rates."""

from __future__ import annotations

from qqbot.services.budget import Budget
from qqbot.configuration import TextCfg
from qqbot.configuration import VisionCfg
from qqbot.providers.base import Rate
from qqbot.providers.base import RetryPolicy
from qqbot.providers.openai_responses import ResponsesCodec
from qqbot.providers.openai_responses import ResponsesTextModel
from qqbot.providers.openai_responses import ResponsesVisionModel

_FREE = Rate(
    "Mtoken",
    in_hit=0.0,
    in_miss=0.0,
    out=0.0,
    source="self-hosted: watts, not CNY",
)


class LocalResponsesCodec(ResponsesCodec):
    name = "local"

    def request_extras(self, effort):
        return {} if effort.value == "off" else super().request_extras(effort)


def local_text(cfg: TextCfg, retry: RetryPolicy, budget: Budget) -> ResponsesTextModel:
    return ResponsesTextModel(
        cfg,
        retry,
        name="local",
        codec=LocalResponsesCodec(),
        rate_for=lambda _model: _FREE,
        budget=budget,
        key_required=False,
    )


def local_vision(cfg: VisionCfg, retry: RetryPolicy, budget: Budget) -> ResponsesVisionModel:
    return ResponsesVisionModel(
        cfg,
        retry,
        name="local",
        codec=LocalResponsesCodec(),
        rate_for=lambda _model: _FREE,
        budget=budget,
        key_required=False,
    )
