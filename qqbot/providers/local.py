"""Self-hosted Responses capabilities with zero CNY rates."""

from __future__ import annotations

from ..settings import TextCfg, VisionCfg
from .base import Rate
from .openai_responses import ResponsesCodec, ResponsesTextModel, ResponsesVisionModel

_FREE = Rate(
    "Mtoken", in_hit=0.0, in_miss=0.0, out=0.0,
    source="self-hosted: watts, not CNY",
)


class LocalResponsesCodec(ResponsesCodec):
    name = "local"

    def request_extras(self, effort):
        return {} if effort.value == "off" else super().request_extras(effort)


def local_text(cfg: TextCfg) -> ResponsesTextModel:
    return ResponsesTextModel(
        cfg,
        name="local",
        codec=LocalResponsesCodec(),
        rate_for=lambda _model: _FREE,
        key_required=False,
    )


def local_vision(cfg: VisionCfg) -> ResponsesVisionModel:
    return ResponsesVisionModel(
        cfg,
        name="local",
        codec=LocalResponsesCodec(),
        rate_for=lambda _model: _FREE,
        key_required=False,
    )
