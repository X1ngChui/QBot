"""Bailian (DashScope) ASR backend, over its OpenAI-compatible endpoint.

It differs from the generic class in two ways, both here:

1. The data URI carries no media type - `data:;base64,...`. Sending `data:audio/wav;...`
   is rejected.
2. `asr_options` enables language identification and inverse text normalisation, so
   numbers and dates come back written the way people write them rather than spelled out.

One account serves both, which is why they default to the same credential name - see
config/settings.yaml.
"""

from __future__ import annotations

from typing import Any

from .base import Rate
from .openai_compat import OpenAICompatAsr


# Billed per second of audio, not per token: USD 0.000035/s (OpenRouter's listing;
# the vendor's own pages print no per-second figure) at the 7.14 CNY/USD the
# vendor's CNY and USD price pages imply.
_ASR_PRICES = {
    "qwen3-asr-flash": Rate("second", per_unit=0.00025, source="openrouter, converted"),
}
_ASR_UNKNOWN = Rate("second", per_unit=0.001, source="pessimistic guess")


class DashScopeAsr(OpenAICompatAsr):
    name = "dashscope"

    def rate_for(self, model: str) -> Rate:
        return _ASR_PRICES.get(model, _ASR_UNKNOWN)

    def _audio_uri(self, b64: str, fmt: str) -> str:
        return f"data:;base64,{b64}"

    def _extra_body(self) -> dict[str, Any]:
        return {"asr_options": {"enable_lid": True, "enable_itn": True}}
