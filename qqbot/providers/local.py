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

#: Free, in every direction; `source` names why.
_FREE = Rate("Mtoken", in_hit=0.0, in_miss=0.0, out=0.0,
             source="self-hosted: watts, not CNY")


class LocalChat(OpenAICompatChat):
    name = "local"
    #: A self-hosted server usually checks no credential, so an empty key is not a
    #: misconfiguration here; the client sends a placeholder the SDK accepts. A key
    #: that is set is still used, for a server that does check one.
    key_required = False

    def rate_for(self, model: str) -> Rate:
        return _FREE
