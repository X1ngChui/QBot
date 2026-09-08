"""Backend selection and the per-backend quirks.

Each of these was previously either hardcoded in shared code or worked around in config.
Pinning them here is what keeps them from drifting back: a quirk that lives in a named
subclass can be tested, one that lives in a flag usually cannot.
"""

import asyncio
import os
import pathlib
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT))
os.environ.setdefault("CONFIG_DIR", str(ROOT / "config"))

from qqbot.providers import base
from qqbot.providers.dashscope import DashScopeAsr
from qqbot.providers.deepseek import DeepSeekChat
from qqbot.providers.openai_compat import OpenAICompatAsr, OpenAICompatChat
from qqbot.providers.registry import (ASR_BACKENDS, SEARCH_BACKENDS, TEXT_BACKENDS,
                                      VISION_BACKENDS, build)
from qqbot.providers.tavily import TavilySearch
from qqbot.settings import load_bundle

fails = []


def check(name, cond, detail=""):
    print(f"[{'ok ' if cond else 'FAIL'}] {name}  {detail}")
    if not cond:
        fails.append(name)


def main() -> int:
    settings = load_bundle().default

    # -- registry ---------------------------------------------------------
    bundle = build(settings)
    # Against what the config asks for, not a hardcoded vendor: the text slot
    # legitimately swings between deepseek and the local toy.
    _cfg_names = (f"text={settings.llm.text.backend}"
                  f" vision={settings.llm.vision.backend}"
                  f" asr={settings.llm.asr.backend}"
                  f" search={settings.llm.search.backend}")
    check("shipped config wires the expected backends",
          bundle.describe() == _cfg_names, bundle.describe())
    asyncio.run(bundle.aclose())

    bad = load_bundle().default
    bad.llm.vision.backend = "nope"
    try:
        build(bad)
        check("unknown backend is rejected", False, "it was accepted")
    except RuntimeError as e:
        check("unknown backend is rejected", "available:" in str(e), str(e))

    # -- the ABCs actually constrain --------------------------------------
    for table, abc_cls, label in (
        (TEXT_BACKENDS, base.TextModel, "text"),
        (VISION_BACKENDS, base.VisionModel, "vision"),
        (ASR_BACKENDS, base.AsrModel, "asr"),
        (SEARCH_BACKENDS, base.SearchEngine, "search"),
    ):
        bad = [n for n, c in table.items() if not issubclass(c, abc_cls)]
        check(f"every {label} backend implements its ABC", not bad, str(bad))
        unnamed = [n for n, c in table.items() if c.name != n]
        check(f"every {label} backend is registered under its own name", not unnamed, str(unnamed))

    class Incomplete(base.TextModel):
        name = "incomplete"

    try:
        Incomplete()
        check("an incomplete backend cannot be constructed", False, "it was constructed")
    except TypeError:
        check("an incomplete backend cannot be constructed", True)

    # -- deepseek: cache split, reasoning tokens, thinking switch ----------
    ds = DeepSeekChat()
    usage = {
        "prompt_tokens": 1000,
        "completion_tokens": 66,
        "prompt_cache_hit_tokens": 960,
        "prompt_cache_miss_tokens": 40,
        "completion_tokens_details": {"reasoning_tokens": 58},
    }
    check("deepseek reads the prompt-cache split", ds._usage_tokens(usage) == (960, 40, 66, 58),
          str(ds._usage_tokens(usage)))
    # A hit is 50x cheaper, so mistaking hits for misses would silently inflate cost.
    check("deepseek reports reasoning tokens", ds._usage_tokens(usage)[3] == 58)
    partial = {"prompt_tokens": 100, "completion_tokens": 5}
    check("deepseek falls back to all-miss when the split is absent",
          ds._usage_tokens(partial) == (0, 100, 5, 0), str(ds._usage_tokens(partial)))
    # One-sided reporting must derive the missing half from the total in *both*
    # directions - deriving only the miss meant a usage carrying miss alone billed
    # the hit share at nothing.
    miss_only = {"prompt_tokens": 100, "completion_tokens": 5,
                 "prompt_cache_miss_tokens": 30}
    check("a miss-only usage still bills the hit share",
          ds._usage_tokens(miss_only) == (70, 30, 5, 0), str(ds._usage_tokens(miss_only)))
    hit_only = {"prompt_tokens": 100, "completion_tokens": 5,
                "prompt_cache_hit_tokens": 60}
    check("a hit-only usage still bills the miss share",
          ds._usage_tokens(hit_only) == (60, 40, 5, 0), str(ds._usage_tokens(hit_only)))
    check("deepseek can be told not to deliberate",
          ds._terse_body() == {"thinking": {"type": "disabled"}}, str(ds._terse_body()))
    # The old "no extras unless terse" claim is superseded by the graded checks in
    # the deliberation section below - extras now depend on cfg.reasoning_effort too.

    # -- generic backend makes no vendor assumptions ----------------------
    gen = OpenAICompatChat()
    check("generic backend bills everything as a cache miss",
          gen._usage_tokens(usage) == (0, 1000, 66, 0), str(gen._usage_tokens(usage)))
    check("generic backend claims no thinking switch", gen._terse_body() == {})

    # -- dashscope ASR quirks ---------------------------------------------
    da, ga = DashScopeAsr(), OpenAICompatAsr()
    check("dashscope audio URI carries no media type",
          da._audio_uri("QUJD", "wav") == "data:;base64,QUJD", da._audio_uri("QUJD", "wav"))
    check("generic audio URI carries one",
          ga._audio_uri("QUJD", "wav") == "data:audio/wav;base64,QUJD", ga._audio_uri("QUJD", "wav"))
    check("dashscope enables language id and inverse text normalisation",
          da._extra_body() == {"asr_options": {"enable_lid": True, "enable_itn": True}},
          str(da._extra_body()))
    check("generic ASR sends no extra body", ga._extra_body() == {})

    # -- search is not a chat protocol at all ------------------------------
    check("search implements the ABC directly, not via the chat base",
          issubclass(TavilySearch, base.SearchEngine)
          and not issubclass(TavilySearch, OpenAICompatChat))

    # -- deliberation is graded by config, forced off where it measured as waste --
    from qqbot.settings import TextCfg
    def _tc(effort):
        return TextCfg.model_validate({
            "base_url": "https://x", "model": "m", "reasoning_effort": effort})
    dsc = DeepSeekChat()
    check("grade off disables deliberation",
          dsc._extra_body(cfg=_tc("off"), effort="off") == {"thinking": {"type": "disabled"}})
    check("a real grade enables it at that effort",
          dsc._extra_body(cfg=_tc("off"), effort="low")
          == {"thinking": {"type": "enabled"}, "reasoning_effort": "low"})
    from qqbot.settings import VisionCfg as _VC
    _vlow = _VC.model_validate({"base_url": "https://x", "model": "m",
                                "reasoning_effort": "low"})
    _voff = _VC.model_validate({"base_url": "https://x", "model": "m"})
    from qqbot.providers.deepseek import DeepSeekVision as _DV
    check("describing follows its own vision grade",
          _DV()._extra_body(_voff) == {"thinking": {"type": "disabled"}}
          and _DV()._extra_body(_vlow)
          == {"thinking": {"type": "enabled"}, "reasoning_effort": "low"})

    # -- embedding wires from its own block, never vision's ------------------
    # It borrowed vision's endpoint once, and followed it to a platform with no
    # /embeddings when vision moved - every reply needing recall died on a 404.
    from qqbot.providers.embedding import build as build_embedding
    emb_cfg = load_bundle().default
    emb = build_embedding(emb_cfg)
    check("embedding uses its own endpoint",
          emb._base == emb_cfg.llm.embedding.base_url.rstrip("/"), emb._base)
    emb_cfg.llm.vision.base_url = "https://moved.example/v1"
    check("and does not follow vision when vision moves",
          build_embedding(emb_cfg)._base == emb_cfg.llm.embedding.base_url.rstrip("/"))
    check("embedding dimensions come from config",
          emb.dimensions == emb_cfg.llm.embedding.dimensions)

    # -- pricing belongs to the backend that charges it ---------------------
    bundle2 = build(settings)
    llm = settings.llm
    rate = bundle2.text.rate_for(llm.text.model)
    if llm.text.backend == "local":
        # Self-hosted burns watts, not CNY: anything nonzero here would spend
        # the real daily budget on fake costs.
        check("the local backend is free in every direction",
              rate.unit == "Mtoken"
              and rate.in_hit == rate.in_miss == rate.out == 0.0, str(rate))
    else:
        check("text backend prices its own model",
              rate.unit == "Mtoken" and rate.in_miss > 0, str(rate))
        check("a cache hit is far cheaper than a miss",
              rate.in_miss / rate.in_hit > 10, f"{rate.in_miss}/{rate.in_hit}")
    check("asr is priced per second of audio",
          bundle2.asr.rate_for(llm.asr.model).unit == "second")
    check("search is priced per call",
          bundle2.search.rate_for("tavily").unit == "call")
    check("and a free-tier call really is free",
          bundle2.search.rate_for("tavily").units(1) == 0.0)
    check("every configured rate cites a source",
          all(c.rate_for(m).source for c, m in (
              (bundle2.text, llm.text.model), (bundle2.vision, llm.vision.model),
              (bundle2.asr, llm.asr.model), (bundle2.search, "tavily"))))
    # An unknown model must never look cheaper than a known one: underestimating means
    # spending money the budget gate never sees.
    unknown = bundle2.text.rate_for("no-such-model")
    check("an unknown model bills high, not low",
          unknown.out >= rate.out and unknown.in_miss >= rate.in_miss,
          f"unknown out={unknown.out} miss={unknown.in_miss}")
    # DeepSeek bills weekday working hours at double, in Beijing time. Getting the window
    # wrong is a silent 2x either way, and it is the number the daily cap is compared
    # against - so the boundaries are pinned rather than trusted to a comment.
    from datetime import datetime
    from zoneinfo import ZoneInfo
    from qqbot.providers import deepseek as ds
    BJ = ZoneInfo("Asia/Shanghai")

    def at(y, mo, d, h):
        return ds._at_peak(datetime(y, mo, d, h, 30, tzinfo=BJ))

    # 2026-08-11 is a Tuesday; 2026-08-15 a Saturday.
    check("a weekday morning is peak", at(2026, 8, 11, 10))
    check("and a weekday afternoon", at(2026, 8, 11, 15))
    check("lunch is not", not at(2026, 8, 11, 12) and not at(2026, 8, 11, 13))
    check("nor is the evening, which is when a group talks most",
          not at(2026, 8, 11, 21))
    check("nor early morning", not at(2026, 8, 11, 8))
    check("the window closes at six", at(2026, 8, 11, 17) and not at(2026, 8, 11, 18))
    check("and opens at nine", at(2026, 8, 11, 9) and not at(2026, 8, 11, 8))
    check("weekends are off-peak all day",
          not at(2026, 8, 15, 10) and not at(2026, 8, 16, 15))
    # The vendor bills in Beijing time whatever timezone this deployment reports in.
    # 02:30 in London is 09:30 in Beijing: off-peak by the hour as written, peak by the
    # only clock that bills. Reading .hour off whatever arrives would put the boundary
    # wherever the caller happens to be.
    check("the window is Beijing time, not the caller's",
          ds._at_peak(datetime(2026, 8, 11, 2, 30, tzinfo=ZoneInfo("Europe/London"))))

    # The vendor repriced effective 2026-08-17 00:00 Beijing. Both eras stay encoded and
    # the switch is a pure function of the clock, so both sides of the boundary are
    # pinned - a wrong era misbills every call silently.
    def rate_at(model, y, mo, d, h):
        return ds._rate_at(model, datetime(y, mo, d, h, 30, tzinfo=BJ))

    check("the day before the repricing bills at the old table",
          rate_at("deepseek-v4-flash", 2026, 8, 16, 21).out == 2.0,
          str(rate_at("deepseek-v4-flash", 2026, 8, 16, 21)))
    check("the day after bills at the new one",
          rate_at("deepseek-v4-flash", 2026, 8, 17, 21).out == 4.5,
          str(rate_at("deepseek-v4-flash", 2026, 8, 17, 21)))
    # 2026-08-17 is a Monday: peak doubling applies on top of the new table.
    peak_new = rate_at("deepseek-v4-flash", 2026, 8, 17, 10)
    check("new-era peak doubles the new off-peak",
          (peak_new.in_hit, peak_new.in_miss, peak_new.out) == (0.10, 3.0, 9.0),
          str(peak_new))
    check("pro's new off-peak output is 13.5",
          rate_at("deepseek-v4-pro", 2026, 8, 17, 21).out == 13.5)
    check("an unknown model still bills at the priciest new tier",
          rate_at("no-such-model", 2026, 8, 17, 21).out == 13.5)

    check("token arithmetic",
          abs(base.Rate("Mtoken", in_hit=0.02).tokens(1_000_000, 0, 0) - 0.02) < 1e-9)
    check("unit arithmetic",
          abs(base.Rate("call", per_unit=0.01).units(3) - 0.03) < 1e-9)
    asyncio.run(bundle2.aclose())

    print()
    print("FAILED:", fails if fails else "none")
    return 1 if fails else 0


sys.exit(main())
