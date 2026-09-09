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
#: Every prompt the system may read, by key. The texts are data: each key is a
#: `<key>.txt` under `prompts_dir`, edited without touching code and re-read on
#: /reload. This manifest is the only list of them - the filename is the key, so
#: a misspelled name is a missing file and fails the load rather than shipping a
#: prompt nobody reads. The rationale behind each text's wording lives in
#: config/prompts/README.md.
PROMPT_KEYS = frozenset({
    # transcript legend, shared by reply and extraction; plus each side's addendum
    "legend", "legend_reply_note", "extract_legend_note",
    # the reply path's standing rules (identity and credibility share one heading)
    "identity_rules", "credibility_rules",
    # tone_rules is the shared discernment core (what counts as said-in-earnest);
    # each path appends its own consequence note - reply: how to play along,
    # extract: what not to record. One judgment, stated once.
    "private_rules", "tone_rules", "tone_reply_note", "tone_extract_note",
    "reply_final",
    # the rest of the extraction rulebook
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


#: How hard a model may think before answering. "off" disables deliberation; the
#: rest map to the vendor's own reasoning_effort grades. Deliberation bills as
#: output, so this is a cost knob as much as a quality one.
Effort = Literal["off", "low", "high", "max"]


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
class TextUseCfg(_M):
    """How one use of the text model differs from the reply path's settings.

    Only what a purpose can legitimately change on its own: which model answers,
    how hard it thinks, how long it may take. Endpoint, credential, concurrency
    and retries stay shared - they describe the account, not the task, and two
    copies of an endpoint is how one of them ends up pointing at a dead one.
    None (or "" for the model) means "whatever the reply path uses".
    """

    model: str = ""
    reasoning_effort: Effort | None = None
    timeout_sec: float | None = None


class TextCfg(_M):
    backend: str = "openai_compat"
    base_url: str
    api_key_env: str = "TEXT_API_KEY"
    model: str
    #: Whether this model accepts image file blocks in messages. Off, the prompt
    #: carries descriptions only and pictures are not uploaded to the Files store;
    #: a text-only model sent a file block rejects the whole request.
    reads_images: bool = False
    #: How hard replies may think. Other uses of this model carry their own grade:
    #: extraction below, describing in llm.vision.
    reasoning_effort: Effort = "off"
    max_concurrency: int = 3
    timeout_sec: float = 30.0
    retries: int = 2
    #: Reading a batch of transcript into memory candidates - the same account and
    #: endpoint, a different task. It is schema-guarded and eval-covered, which is
    #: what makes it the one place a cheaper tier can measurably do the reply
    #: model's work; it also reads a hundred messages per call, so it needs a
    #: timeout the reply path would never grant.
    extract: TextUseCfg = Field(default_factory=TextUseCfg)

    def for_extract(self) -> "TextCfg":
        """This config as the extraction call should see it.

        One override point instead of three: before this, extraction patched the
        model onto a copy, passed its grade as a call argument, and kept its
        timeout as a constant in Python because config had nowhere to put it.
        """
        use = self.extract
        return self.model_copy(update={
            "model": use.model or self.model,
            "reasoning_effort": use.reasoning_effort or self.reasoning_effort,
            "timeout_sec": use.timeout_sec or self.timeout_sec,
        })


class VisionCfg(_M):
    backend: str = "openai_compat"
    base_url: str
    api_key_env: str = "MEDIA_API_KEY"
    model: str
    #: Deliberation grade for the describing call. "low" keeps sanity-check thinking
    #: (what a meme actually shows) at a fraction of "high"'s thought-token bill.
    reasoning_effort: Effort = "off"
    #: How long a stored description stays current. Past it, the next sighting of
    #: that picture pays to describe it again. Age rather than a model stamp,
    #: because a vendor can put a better model behind an unchanged id - which no
    #: stamp would notice. 0 disables expiry entirely.
    #:
    #: Refreshing is lazy by construction: only a picture actually posted again is
    #: looked up, so one nobody ever reposts is never paid for twice, however old
    #: its description gets. Already-archived transcript lines keep the wording
    #: they were written with; this decides what the *next* sighting reads.
    description_ttl_days: int = Field(15, ge=0)
    max_images_per_min: int = 6
    max_image_mb: float = 8.0
    timeout_sec: float = 30.0


class AsrCfg(_M):
    backend: str = "openai_compat"
    #: Empty is valid for in-process backends (sherpa), which have no endpoint;
    #: the API-backed ones fail their first call without it, loudly enough.
    base_url: str = ""
    api_key_env: str = "MEDIA_API_KEY"
    model: str
    #: For the sherpa backend only: directory holding the ONNX bundle
    #: (model.int8.onnx + tokens.txt), as seen from inside the container.
    model_dir: str = ""
    #: CPU threads for in-process decoding. Clips are short and rare; two threads
    #: keep a clip under a second without contending with the event loop's core.
    threads: int = 2
    max_audio_sec: int = 300
    #: Transcription happens on arrival, so a burst of long clips spends real
    #: money before the daily cap can matter - the same reason pictures carry
    #: max_images_per_min. Clips are rarer, so the same number is generous.
    max_clips_per_min: int = 6
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
    embedding: EmbeddingCfg
    search: SearchCfg


class BudgetCfg(_M):
    """The daily ceiling. One number, because there is only one thing to do at it: the
    bot answers only when addressed, so past the cap it goes silent until the day rolls
    over. There is no softer fallback mode to fall to.
    """

    daily_cny_cap: float = 5.0
    #: What one reply may spend, tool loop included. Money is the only limit - there is
    #: no per-day search count or per-reply round count, which are proxies for cost that
    #: drift whenever prices move. A reply that cannot afford another round stops
    #: searching and answers from what it has in one tool-less wrap-up round: the cap
    #: bounds the spending (overshoot is exactly that one round), it does not discard
    #: what was already paid for. Only the daily cap, checked before anything is spent,
    #: means silence.
    per_reply_cny: float = 0.30


#: How much transcript one extraction chunk holds at most. The worker owns the
#: behaviour (workers.memory re-exports this as WINDOW, and cuts chunks shorter at
#: conversation gaps); the number lives here because /relearn's watermark reset
#: keeps exactly this many messages unread, and services may not import workers.
EXTRACT_WINDOW = 120


class RetrievalCfg(_M):
    #: Lines of surrounding conversation each search_history hit carries - this
    #: many before and this many after, windows merged into one block when hits
    #: sit close. Chat is written in fragments: the matched line is routinely a
    #: bare answer to the line above it, and the archive has no other way to
    #: read "around" a hit. Context is fetched by SQL, so the only cost is
    #: prompt tokens (roughly thirty a line) in a result that evicts next turn.
    #: 0 restores bare hits.
    history_context: int = 5
    #: Same idea for recall_events, in coarser units: episodes this many before
    #: and after each recalled one, by group time order. An episode summarises a
    #: whole stretch of conversation, so a couple either side already frames the
    #: story ("what led to this, what came of it"); undated episodes stand
    #: alone. 0 restores bare recall.
    episode_context: int = 2


class ScheduleCfg(_M):
    #: The nightly pipeline: extraction drain, then decay, then backup, then the
    #: NapCat cache sweep - one trigger, stages run in order (tasks.nightly). One
    #: trigger rather than four, because extraction drains through the job queue
    #: and a night of retries would walk it past any fixed decay time. Small hours
    #: by design: the day's transcript is complete, the vendor bills off-peak, and
    #: nobody is waiting.
    nightly_cron: str = "30 2 * * *"
    #: The daily report keeps its own trigger, at the moment the ledger day
    #: closes (midnight in the configured timezone - the same boundary the
    #: budget resets on), so the closed day is reported at once rather than
    #: hours later. No ordering stake in the pipeline: the report reads the
    #: day that just ended, and its backup-age line alarms on a pipeline that
    #: died a night ago.
    report_cron: str = "0 0 * * *"
    #: At least 1: pruning keeps the newest `backup_keep` dumps, and 0 would read as
    #: "no pruning" while actually deleting the backup just written, every night.
    backup_keep: int = Field(14, ge=1)
    napcat_cache_days: int = 7

    @field_validator("nightly_cron", "report_cron")
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


class AgreementCfg(_M):
    """The user agreement. The version is a number the owner sets - bumping it
    voids every older acceptance - and the text lives in its own file, named
    here by path. Both fields are mandatory and the file must exist and be
    non-empty, so a deployment without an agreement fails at load instead of
    showing members a placeholder."""

    #: Acceptances are stored against this number; raise it to re-ask everyone.
    version: int
    #: Path of the file holding the full text /terms shows, relative to the
    #: config directory (absolute allowed). The accept instruction is appended
    #: in code, so an edited body can never lose it.
    file: str


class Settings(_M):
    """Field order mirrors settings.yaml's narrative: who runs it and on what
    clock, when it speaks, how messages come in, which models serve it, what it
    may spend, how much it reads around a memory, when the jobs run, and finally
    where the config directory keeps its files."""

    # Everyone allowed to run ops commands and receive the daily report. A list because
    # a bot outliving one person's attention needs more than one pair of hands.
    owners: list[str] = Field(default_factory=list)
    # IANA zone name. Applied at startup to every local-time reading: the clock the model
    # is told, the cron schedules, and the day the budget rolls over on.
    timezone: str = "Asia/Shanghai"
    trigger: TriggerCfg = Field(default_factory=TriggerCfg)
    gateway: GatewayCfg = Field(default_factory=GatewayCfg)
    llm: LlmCfg
    budget: BudgetCfg = Field(default_factory=BudgetCfg)
    retrieval: RetrievalCfg = Field(default_factory=RetrievalCfg)
    schedule: ScheduleCfg = Field(default_factory=ScheduleCfg)

    # -- files: paths relative to the config directory (absolute allowed) ----
    personas_dir: str = "personas"
    agreement: AgreementCfg
    #: Where the prompt texts live, one `<key>.txt` per entry in PROMPT_KEYS.
    prompts_dir: str = "prompts"


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
                 prompts: dict[str, str] | None = None,
                 agreement_text: str = ""):
        self._raw = raw_settings
        self.default = Settings.model_validate(raw_settings)
        self.personas = personas
        #: Model-facing text by key, loaded from the prompt files. Read through
        #: ptext(); empty only for a bundle built outside load_bundle.
        self.prompts: dict[str, str] = prompts or {}
        #: The user agreement's full text, loaded from agreement.file at the
        #: same moment as everything else - /reload swaps it atomically with
        #: the version number it belongs to. Empty only for a bundle built
        #: outside load_bundle.
        self.agreement_text: str = agreement_text
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


def _resolve(root: Path, p: str) -> Path:
    q = Path(p)
    return q if q.is_absolute() else root / q


def load_bundle(config_dir: Path | None = None) -> ConfigBundle:
    root = config_dir or CONFIG_DIR
    raw = _read_yaml(root / "settings.yaml")
    # Validated up front: the directory layout below comes from the config
    # itself, so the schema has to hold before anything else is read.
    settings = Settings.model_validate(raw)

    personas: dict[str, Persona] = {}
    persona_dir = _resolve(root, settings.personas_dir)
    if persona_dir.is_dir():
        for path in sorted(persona_dir.glob("*.yaml")):
            stem = path.stem
            key = stem[len("group_"):] if stem.startswith("group_") else stem
            personas[key] = Persona.model_validate(_read_yaml(path))

    # Prompts are data: one file per manifest key, named by the key. Each must
    # exist and be non-empty - a missing one would silently blank an
    # instruction. A test config points prompts_dir at the real texts instead
    # of copying every file into fixtures.
    prompt_dir = _resolve(root, settings.prompts_dir)
    prompts: dict[str, str] = {}
    for key in sorted(PROMPT_KEYS):
        ppath = prompt_dir / f"{key}.txt"
        try:
            body = ppath.read_text(encoding="utf-8-sig").strip()
        except OSError as e:
            raise ValueError(f"prompt file unreadable: {key}: {ppath}: {e}") from None
        if not body:
            raise ValueError(f"prompt file is empty: {key}: {ppath}")
        prompts[key] = body

    # The agreement text follows its configured path, loaded with everything
    # else so /reload swaps text and version together. Mandatory like the
    # prompts: a missing or empty file fails the load, never a placeholder.
    apath = _resolve(root, settings.agreement.file)
    try:
        agreement_text = apath.read_text(encoding="utf-8-sig").strip()
    except OSError as e:
        raise ValueError(f"agreement file unreadable: {apath}: {e}") from None
    if not agreement_text:
        raise ValueError(f"agreement file is empty: {apath}")

    bundle = ConfigBundle(raw, personas, prompts, agreement_text)
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
