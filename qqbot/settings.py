"""Configuration loading and validation.

Two layers: settings.yaml defaults, overridden per group by personas/group_*.yaml.
Pydantic validates; /reload re-reads from disk and swaps the global singleton atomically
(readers never take a lock).
"""

from __future__ import annotations

import copy
import os
from pathlib import Path
from typing import Any, Literal

import yaml
from pydantic import BaseModel, ConfigDict, Field, field_validator

from . import util
#: Every prompt the system may read, by key. The texts themselves are data -
#: config/prompts/<key>.txt, one file per key, edited without touching code and
#: re-read on /reload - and this manifest is what makes the data checkable: a missing
#: file and a stray file both fail the load, so the key set cannot drift silently.
#: The rationale behind each text's wording lives in config/prompts/README.md.
PROMPT_KEYS = frozenset({
    # transcript legend, shared by reply and extraction; plus each side's addendum
    "legend", "legend_reply_note", "extract_legend_note",
    # the reply path's standing rules
    "reading_rules", "private_rules", "tone_rules", "tone_reply_note", "reply_final",
    # the whole extraction rulebook (its joke-vs-fact rule lives inline, beside
    # the fact criteria it qualifies - there is no memory-side tone addendum)
    "extract",
    # media and tools
    "describe_image", "inspect_image",
    "tool_web_search", "tool_search_history", "tool_recall_events",
    "tool_read_url", "tool_inspect_image",
})

CONFIG_DIR = Path(os.getenv("CONFIG_DIR", "/app/config"))
DEFAULT_PERSONA_KEY = "default"


class _M(BaseModel):
    model_config = ConfigDict(extra="forbid")


class GatewayCfg(_M):
    dedup_ttl_sec: int = 300
    max_msg_len: int = 2000


class TriggerCfg(_M):
    """When the bot answers: which names count as being addressed. There is no
    rate cap - every addressed message earns its one reply attempt, and money
    (plus the provider concurrency semaphore) is what bounds the pace.
    """

    # Every name the bot answers to, canonical plus variants.
    nicknames: list[str] = Field(default_factory=list)


# One section per capability, each carrying its own endpoint, model and credential name
# so it can be moved independently: four providers, each swappable, and no automatic
# fallback between them.
#
# Defaults name capabilities, never vendors: which platform serves a capability is a
# deployment fact that belongs in the YAML values, not in the schema. base_url and model
# have no defaults at all for the same reason - settings.yaml must state them, so nobody
# ends up silently talking to whatever the code happened to assume.
#
# `backend` picks the implementation class (see providers/registry.py). Backend quirks -
# how usage is reported, how to ask it not to deliberate, what extra fields a request
# needs - belong in that class, not in config.
class TextCfg(_M):
    backend: str = "openai_compat"
    base_url: str
    api_key_env: str = "TEXT_API_KEY"
    model: str
    #: Whether this model accepts image file blocks in messages. Off, the prompt
    #: carries descriptions only and pictures are not uploaded to the Files store;
    #: a text-only model sent a file block rejects the whole request.
    reads_images: bool = False
    #: How hard replies may think: "off" disables deliberation, low/high/max map to
    #: the vendor's reasoning_effort grades. Extraction and describing carry their own
    #: grades (memory.consolidate.reasoning_effort, llm.vision.reasoning_effort).
    reasoning_effort: Literal["off", "low", "high", "max"] = "off"
    max_concurrency: int = 3
    timeout_sec: float = 30.0
    retries: int = 2


class VisionCfg(_M):
    backend: str = "openai_compat"
    base_url: str
    api_key_env: str = "MEDIA_API_KEY"
    model: str
    #: Deliberation grade for the describing call. "low" keeps sanity-check thinking
    #: (what a meme actually shows) at a fraction of "high"'s thought-token bill;
    #: "off" disables deliberation.
    reasoning_effort: Literal["off", "low", "high", "max"] = "off"
    max_images_per_min: int = 6
    max_image_mb: float = 8.0
    timeout_sec: float = 30.0


class AsrCfg(_M):
    backend: str = "openai_compat"
    base_url: str
    api_key_env: str = "MEDIA_API_KEY"
    model: str
    max_audio_sec: int = 300
    timeout_sec: float = 60.0


class SearchCfg(_M):
    backend: str
    base_url: str
    api_key_env: str = "SEARCH_API_KEY"
    count: int = 5
    #: Result depth the vendor is asked for; "basic" is one credit, "advanced" two.
    depth: str = "basic"
    #: The free tier's credit allowance per calendar month. At it search refuses; there
    #: is no paid fallback.
    monthly_quota: int = 1000
    #: HTTP proxy for the search client only - the endpoint is not reliably reachable
    #: from the deployment region directly. Empty means direct; model traffic never
    #: goes through this.
    proxy: str = ""
    timeout_sec: float = 20.0


class EmbeddingCfg(_M):
    backend: str = "dashscope"
    base_url: str
    api_key_env: str = "MEDIA_API_KEY"
    model: str
    dimensions: int = 2048
    timeout_sec: float = 60.0


class LlmCfg(_M):
    text: TextCfg
    vision: VisionCfg
    asr: AsrCfg
    search: SearchCfg
    embedding: EmbeddingCfg


class BudgetCfg(_M):
    """The daily ceiling. One number, because there is only one thing to do at it: the
    bot answers only when addressed, so past the cap it goes silent until the day rolls
    over. There is no softer fallback mode to fall to.
    """

    daily_cny_cap: float = 5.0
    #: What one reply may spend, tool loop included. Money is the only limit - there is
    #: no per-day search count or per-reply round count, which are proxies for cost that
    #: drift whenever prices move. A reply that cannot afford another round is dropped:
    #: limits mean silence, same as the daily cap.
    per_reply_cny: float = 0.30


#: How much transcript one extraction chunk holds at most. The worker owns the
#: behaviour (workers.memory re-exports this as WINDOW, and cuts chunks shorter at
#: conversation gaps); the number lives here because /relearn's watermark reset
#: keeps exactly this many messages unread, and services may not import workers.
EXTRACT_WINDOW = 120


class ConsolidateCfg(_M):
    #: Deliberation grade for the extraction pass. The schema and the Validator carry
    #: most of the think-it-through duty, so "low" buys ambiguity checks (joke or
    #: fact, which predicate) without the ~6k thought tokens a pass that "high"
    #: measured at; "off" disables thinking entirely.
    #:
    #: The batching knobs (every_n_msgs, idle_min, idle_min_msgs) are gone with the
    #: real-time triggers: extraction now runs once nightly (schedule.extract_cron),
    #: draining the day oldest-first in gap-aligned chunks - there is no second
    #: choice left to configure.
    reasoning_effort: Literal["off", "low", "high", "max"] = "off"


class MemoryCfg(_M):
    consolidate: ConsolidateCfg = Field(default_factory=ConsolidateCfg)


class ScheduleCfg(_M):
    #: The day's one extraction drain. Small hours by design: the day's transcript
    #: is complete, the vendor bills off-peak, and nobody is waiting.
    extract_cron: str = "30 2 * * *"
    forget_cron: str = "0 4 * * *"
    backup_cron: str = "30 4 * * *"
    cache_clean_cron: str = "0 5 * * *"
    report_cron: str = "0 9 * * *"
    #: At least 1: pruning keeps the newest `backup_keep` dumps, and 0 would read as
    #: "no pruning" while actually deleting the backup just written, every night.
    backup_keep: int = Field(14, ge=1)
    napcat_cache_days: int = 7

    @field_validator("extract_cron", "forget_cron", "backup_cron",
                     "cache_clean_cron", "report_cron")
    @classmethod
    def _five_fields(cls, v: str) -> str:
        # Checked at load with the scheduler's own parser, because the scheduler
        # only parses these at startup: a typo accepted by /reload would otherwise
        # detonate at the next restart, hours or days later, as a boot failure far
        # from its cause - and counting fields alone lets '30 25 * * *' through.
        if len(v.split()) != 5:
            raise ValueError(f"cron expression needs 5 fields: {v!r}")
        try:
            from apscheduler.triggers.cron import CronTrigger
        except ImportError:
            return v          # bare test venv; the container always has it
        try:
            CronTrigger.from_crontab(v)
        except ValueError as e:
            raise ValueError(f"invalid cron expression {v!r}: {e}") from None
        return v


class Settings(_M):
    # Everyone allowed to run ops commands and receive the daily report. A list because
    # a bot outliving one person's attention needs more than one pair of hands.
    owners: list[str] = Field(default_factory=list)
    # IANA zone name. Applied at startup to every local-time reading: the clock the model
    # is told, the cron schedules, and the day the budget rolls over on.
    timezone: str = "Asia/Shanghai"
    gateway: GatewayCfg = Field(default_factory=GatewayCfg)
    trigger: TriggerCfg = Field(default_factory=TriggerCfg)
    llm: LlmCfg
    budget: BudgetCfg = Field(default_factory=BudgetCfg)
    memory: MemoryCfg = Field(default_factory=MemoryCfg)
    schedule: ScheduleCfg = Field(default_factory=ScheduleCfg)


class Persona(_M):
    """A group's character.

    A group file states only what differs from default.yaml; anything it leaves out is
    inherited. That keeps the shared parts - how it talks, how it reads speaker names,
    who its owner is - in one place instead of N copies that drift apart as they are
    edited.

    `system_prompt_extra` exists because sharing happens *inside* the prompt, not only
    between fields: the base prompt is inherited and the group's own paragraphs are
    appended to it. Setting `system_prompt` outright replaces the base entirely, for a
    group that wants nothing shared.
    """

    name: str = "小X"
    system_prompt: str = ""
    system_prompt_extra: str = ""
    group_knowledge: str = ""
    overrides: dict[str, Any] = Field(default_factory=dict)


def _merge_persona(base: Persona, override: Persona | None) -> Persona:
    """Fold a group persona onto the default and compose the final prompt.

    Only fields the group file actually set are taken (model_fields_set), so a field left
    out inherits rather than being overwritten by its schema default - otherwise every
    group would silently fall back to the default name.
    """
    if override is None:
        merged = base.model_copy(deep=True)
    else:
        data = base.model_dump()
        for field in override.model_fields_set:
            data[field] = getattr(override, field)
        merged = Persona.model_validate(data)

    if merged.system_prompt_extra.strip():
        merged = merged.model_copy(
            update={
                "system_prompt": (
                    merged.system_prompt.rstrip()
                    + "\n\n"
                    + merged.system_prompt_extra.strip()
                ),
                "system_prompt_extra": "",
            }
        )
    return merged


def _deep_merge(base: dict, patch: dict) -> dict:
    out = copy.deepcopy(base)
    for k, v in patch.items():
        if isinstance(v, dict) and isinstance(out.get(k), dict):
            out[k] = _deep_merge(out[k], v)
        else:
            out[k] = copy.deepcopy(v)
    return out


def _read_yaml(path: Path) -> dict:
    with path.open("r", encoding="utf-8") as f:
        data = yaml.safe_load(f) or {}
    if not isinstance(data, dict):
        raise ValueError(f"{path}: top level must be a mapping")
    return data


class ConfigBundle:
    """One disk read: global defaults, every persona, and a cache of merged per-group Settings."""

    def __init__(self, raw_settings: dict, personas: dict[str, Persona],
                 prompts: dict[str, str] | None = None):
        self._raw = raw_settings
        self.default = Settings.model_validate(raw_settings)
        self.personas = personas
        #: Model-facing text by key, loaded from the prompt files. Read through
        #: ptext(); empty only for a bundle built outside load_bundle.
        self.prompts: dict[str, str] = prompts or {}
        self._merged: dict[str, Settings] = {}
        self._resolved_personas: dict[str, Persona] = {}

    def persona_for(self, group_id: str) -> Persona:
        cached = self._resolved_personas.get(group_id)
        if cached is None:
            base = self.personas.get(DEFAULT_PERSONA_KEY, Persona())
            cached = _merge_persona(base, self.personas.get(group_id))
            self._resolved_personas[group_id] = cached
        return cached

    def for_group(self, group_id: str) -> tuple[Settings, Persona]:
        persona = self.persona_for(group_id)
        cached = self._merged.get(group_id)
        if cached is None:
            if persona.overrides:
                cached = Settings.model_validate(_deep_merge(self._raw, persona.overrides))
            else:
                cached = self.default
            self._merged[group_id] = cached
        return cached, persona


def load_bundle(config_dir: Path | None = None) -> ConfigBundle:
    root = config_dir or CONFIG_DIR
    raw = _read_yaml(root / "settings.yaml")

    personas: dict[str, Persona] = {}
    pdir = root / "personas"
    if pdir.is_dir():
        for path in sorted(pdir.glob("*.yaml")):
            stem = path.stem
            key = stem[len("group_"):] if stem.startswith("group_") else stem
            personas[key] = Persona.model_validate(_read_yaml(path))

    # Prompts are data: one file per manifest key, no defaults in code. Both drift
    # directions fail the load - a missing file would silently blank an instruction,
    # a stray file is a typo shipping a prompt nobody reads. PROMPTS_DIR lets a test
    # config borrow the real texts instead of copying every prompt file into fixtures.
    pdir = Path(os.getenv("PROMPTS_DIR") or (root / "prompts"))
    found = {p.stem: p for p in pdir.glob("*.txt")} if pdir.is_dir() else {}
    missing = PROMPT_KEYS - set(found)
    stray = set(found) - PROMPT_KEYS
    if missing or stray:
        raise ValueError(
            f"prompt files out of step with the manifest in {pdir}: "
            + (f"missing {sorted(missing)} " if missing else "")
            + (f"unknown {sorted(stray)}" if stray else ""))
    prompts = {k: p.read_text(encoding="utf-8-sig").strip() for k, p in found.items()}

    bundle = ConfigBundle(raw, personas, prompts)
    # Validate eagerly: a bad config should blow up at startup / reload, not on the
    # first incoming message. Every group's merged overrides included - for_group
    # merges lazily, and before this a typo in one group's overrides sailed through
    # /reload and then failed on that group's every message (no reply, no archive)
    # until the file was fixed. Warming the merge cache here is a free side effect.
    _ = bundle.default
    for gid in personas:
        bundle.for_group(gid)
    # The zone follows the config wherever config is loaded - workers and tests
    # included, not only the app entrypoint. Set here rather than by each caller,
    # because a process that forgot ran on the fallback zone silently.
    util.set_timezone(bundle.default.timezone)
    return bundle


_bundle: ConfigBundle | None = None


def config() -> ConfigBundle:
    global _bundle
    if _bundle is None:
        _bundle = load_bundle()
    return _bundle


def ptext(key: str) -> str:
    """The model-facing text registered under this key.

    Loaded from config/prompts/<key>.txt - the file is the source of truth. Read at
    use time on purpose: /reload swaps the bundle, and a call site that captured the
    string at import would keep the old wording forever. The extraction prompt is the
    one composition frozen earlier (worker construction).
    """
    return config().prompts[key]


def reload_config() -> ConfigBundle:
    """Swap the global singleton. On validation failure the old config is kept and the
    error propagates."""
    global _bundle
    fresh = load_bundle()
    _bundle = fresh
    return fresh
