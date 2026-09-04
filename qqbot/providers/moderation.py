"""The moderation capability: is this outgoing text safe to send?

One backend today, Tencent Cloud TMS (TextModeration) - chosen because the
question the exit guard actually asks is "will QQ's risk control punish the
account for this sentence", and Tencent's own moderation model is the closest
available proxy for Tencent's own risk control. It is the only judge: when the
call fails, core/censor.py holds the reply rather than falling back to
anything - unjudged text does not leave.

No vendor SDK: the TC3-HMAC-SHA256 signature is forty lines of stdlib, and the
project already carries httpx. Credentials follow the capability-not-platform
naming rule (MODERATION_SECRET_ID / MODERATION_SECRET_KEY).
"""

from __future__ import annotations

import base64
import hashlib
import hmac
import json
import logging
import time
from dataclasses import dataclass

import httpx

from ..core.budget import BUDGET
from ..util import read_api_key
from .base import Kind

log = logging.getLogger("qqbot.moderation")

_HOST = "tms.tencentcloudapi.com"
_SERVICE = "tms"
_VERSION = "2020-12-29"
_ACTION = "TextModeration"

#: Post-paid list price (CNY 25 per 10k calls). Leaning high on purpose: a
#: prepaid package bills less, and the ledger's rule is to overstate rather
#: than understate what a call cost.
_CNY_PER_CALL = 0.0025


@dataclass(frozen=True)
class Verdict:
    """One moderation answer. `suggestion` is the vendor's disposition verbatim
    (Pass / Review / Block); label and score say why - category and severity
    only, never the matched words, which must not reach a log."""

    suggestion: str
    label: str = ""
    score: int = 0


def _sign(secret_id: str, secret_key: str, payload: str, ts: int) -> dict[str, str]:
    """TC3-HMAC-SHA256 request headers for one TextModeration POST.

    Pure so the signature can be pinned in tests: same inputs, same headers.
    """
    date = time.strftime("%Y-%m-%d", time.gmtime(ts))
    canonical = (f"POST\n/\n\ncontent-type:application/json; charset=utf-8\n"
                 f"host:{_HOST}\n\ncontent-type;host\n"
                 + hashlib.sha256(payload.encode()).hexdigest())
    scope = f"{date}/{_SERVICE}/tc3_request"
    to_sign = (f"TC3-HMAC-SHA256\n{ts}\n{scope}\n"
               + hashlib.sha256(canonical.encode()).hexdigest())

    def _h(key: bytes, msg: str) -> bytes:
        return hmac.new(key, msg.encode(), hashlib.sha256).digest()

    k = _h(_h(_h(("TC3" + secret_key).encode(), date), _SERVICE), "tc3_request")
    sig = hmac.new(k, to_sign.encode(), hashlib.sha256).hexdigest()
    return {
        "Authorization": (f"TC3-HMAC-SHA256 Credential={secret_id}/{scope}, "
                          f"SignedHeaders=content-type;host, Signature={sig}"),
        "Content-Type": "application/json; charset=utf-8",
        "Host": _HOST,
        "X-TC-Action": _ACTION,
        "X-TC-Timestamp": str(ts),
        "X-TC-Version": _VERSION,
    }


class TencentTms:
    """TextModeration against the policy (BizType) configured in the console."""

    name = "tencent_tms"

    def __init__(self, cfg) -> None:
        self._cfg = cfg
        self._sid = read_api_key(cfg.secret_id_env)
        self._skey = read_api_key(cfg.secret_key_env)
        if not (self._sid and self._skey):
            raise RuntimeError(
                f"moderation backend needs {cfg.secret_id_env} and "
                f"{cfg.secret_key_env} in the environment")
        self._client: httpx.AsyncClient | None = None

    def _http(self) -> httpx.AsyncClient:
        if self._client is None:
            self._client = httpx.AsyncClient(timeout=self._cfg.timeout_sec)
        return self._client

    async def screen(self, text: str, *, group_id: str | None = None) -> Verdict:
        body: dict = {"Content": base64.b64encode(text.encode()).decode()}
        if self._cfg.biz_type:
            body["BizType"] = self._cfg.biz_type
        payload = json.dumps(body)
        headers = _sign(self._sid, self._skey, payload, int(time.time()))
        headers["X-TC-Region"] = self._cfg.region
        r = await self._http().post(f"https://{_HOST}", content=payload,
                                    headers=headers)
        # Booked before any parsing: the vendor bills the call, not the verdict,
        # and a response too broken to parse was still a billed call.
        await BUDGET.record(kind=Kind.MODERATION, model=self.name,
                            cny=_CNY_PER_CALL, group_id=group_id)
        r.raise_for_status()
        res = r.json().get("Response", {})
        if err := res.get("Error"):
            raise RuntimeError(f"TMS {err.get('Code')}: {err.get('Message')}")
        suggestion = res.get("Suggestion")
        if suggestion not in ("Pass", "Review", "Block"):
            # Anything unrecognized - an intercepting proxy's JSON, a renamed
            # vendor field - must land in the caller's fail-closed error path,
            # never default to Pass: that would send text nobody judged.
            raise RuntimeError(f"TMS returned no usable verdict: {suggestion!r}")
        return Verdict(
            suggestion=suggestion,
            label=res.get("Label") or "",
            score=int(res.get("Score") or 0),
        )

    async def aclose(self) -> None:
        if self._client is not None:
            await self._client.aclose()
            self._client = None


#: Every implemented backend. settings.ModerationCfg keeps a mirror of these
#: names in its validator (it cannot import this module without a cycle);
#: test_logic pins the mirror against this table.
BACKENDS: dict[str, type] = {"tencent_tms": TencentTms}


def build(settings):
    """The configured moderation backend, or None when the capability is off.

    Off is a supported state for test and dev contexts: the exit then has no
    guard at all. An unknown backend name still fails at boot - a typo must
    not silently disarm cloud moderation.
    """
    m = settings.moderation
    if not m.backend:
        return None
    try:
        cls = BACKENDS[m.backend]
    except KeyError:
        raise RuntimeError(f"unknown moderation backend: {m.backend!r}") from None
    return cls(m)
