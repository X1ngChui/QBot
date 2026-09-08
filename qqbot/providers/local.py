"""A self-hosted OpenAI-compatible endpoint: the toy backend.

The wire protocol is the generic one; the whole difference is money. A local
model burns watts, not CNY - billing it at the unknown-model rate would spend
the real daily budget on fake costs and silence the bot over electricity. So
every rate is zero, and the budget layer keeps working: calls are still
counted, attribution still lands in /top, and the ledger shows what ran, at a
cost of nothing.
"""

from __future__ import annotations

from .base import Rate
from .openai_compat import OpenAICompatChat

#: Free, in every direction. `source` names why, for the day someone wonders.
_FREE = Rate("Mtoken", in_hit=0.0, in_miss=0.0, out=0.0,
             source="self-hosted: watts, not CNY")


class LocalChat(OpenAICompatChat):
    name = "local"

    def rate_for(self, model: str) -> Rate:
        return _FREE
