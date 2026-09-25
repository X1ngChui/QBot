"""Configuration loading and validation.

Settings are global. Persona files may vary only model-facing identity and group context.
The complete bundle is validated once at startup and remains fixed for the process lifetime.
"""

from __future__ import annotations

import logging
import os
from collections.abc import Mapping
from dataclasses import dataclass
from pathlib import Path
from types import MappingProxyType
from typing import Any, Literal

import yaml
from pydantic import BaseModel, ConfigDict, Field, field_validator, model_validator

from . import util
from .domain.ids import GroupId
from .prompting import PromptCatalog

log = logging.getLogger("qqbot.settings")


CONFIG_DIR = Path(os.getenv("CONFIG_DIR", "/app/config"))
DEFAULT_PERSONA_KEY = "default"


class _M(BaseModel):
    model_config = ConfigDict(extra="forbid", frozen=True)

    @model_validator(mode="before")
    @classmethod
    def _reject_legacy_keys(cls, value: Any) -> Any:
        """Fail old configuration names with a direct migration instruction."""

        if not isinstance(value, dict):
            return value
        migrations = {
            "llm": "capabilities",
            "backend": "provider",
            "base_url": "endpoint",
            "api_key_env": "credential_env",
        }
        found = [(old, migrations[old]) for old in migrations if old in value]
        if found:
            changes = ", ".join(f"{old} -> {new}" for old, new in found)
            raise ValueError(f"obsolete configuration key(s): {changes}")
        return value


#: How hard a model may think before answering. "off" disables deliberation; the
#: rest map to the vendor's own reasoning_effort grades. Deliberation bills as
#: output, so this is a cost knob as much as a quality one.
Effort = Literal["off", "low", "high", "max"]


class GatewayCfg(_M):
    #: How long shutdown waits for in-flight media patches. They are never cancelled;
    #: a hung one must not hold a deploy.
    shutdown_wait_sec: float = Field(5.0, ge=0)


class MediaCfg(_M):
    """Timing and retry policy for media resolution."""

    #: How long a reply waits for a picture or voice clip before building the
    #: prompt without it. Resolution continues for the next turn either way.
    wait_sec: float = 25.0
    #: Deadline for protocol-side media calls such as get_image and get_record.
    protocol_timeout_sec: float = Field(10.0, gt=0)
    #: Deadline for downloading a picture or clip from a link.
    http_timeout_sec: float = Field(20.0, gt=0)
    #: How long an unreadable picture rests before another attempt.
    unreadable_retry_sec: int = Field(600, ge=0)


class MembersCfg(_M):
    """Caching policy for platform member metadata."""

    cache_ttl_sec: int = Field(1800, ge=0)


class CommandsCfg(_M):
    """Presentation bounds for the group command console."""

    roster_max_entries: int = Field(60, ge=1, le=200)
    top_default_entries: int = Field(5, ge=1, le=100)
    top_max_entries: int = Field(20, ge=1, le=100)

    @model_validator(mode="after")
    def _consistent_limits(self) -> CommandsCfg:
        if self.top_default_entries > self.top_max_entries:
            raise ValueError("top_default_entries must not exceed top_max_entries")
        return self


class IdentityLinkCfg(_M):
    """Durable cross-account ownership challenge policy."""

    challenge_ttl_sec: int = Field(600, ge=60, le=3600)
    max_pending_challenges: int = Field(1000, ge=1, le=10000)
    max_pending_per_account: int = Field(3, ge=1, le=100)
    challenge_code_length: int = Field(8, ge=6, le=12)


class DiagnosticsCfg(_M):
    """Operator-console and retained diagnostic output bounds."""

    debug_max_rounds: int = Field(50, ge=1, le=200)
    log_tail_default_lines: int = Field(15, ge=1, le=500)
    log_tail_max_lines: int = Field(60, ge=1, le=500)
    log_tail_scan_bytes: int = Field(65536, ge=4096, le=1048576)
    error_ring_entries: int = Field(200, ge=10, le=5000)
    error_message_chars: int = Field(300, ge=80, le=4000)
    daily_report_recent_errors: int = Field(8, ge=1, le=50)

    @model_validator(mode="after")
    def _consistent_limits(self) -> DiagnosticsCfg:
        if self.log_tail_default_lines > self.log_tail_max_lines:
            raise ValueError("log_tail_default_lines must not exceed log_tail_max_lines")
        if self.daily_report_recent_errors > self.error_ring_entries:
            raise ValueError("daily_report_recent_errors must not exceed error_ring_entries")
        return self


class SendMessagesCfg(_M):
    """Bounds enforced by the terminal send_messages tool."""

    #: Independent QQ messages one terminal call may submit.
    max_messages_per_call: int = Field(4, ge=1)
    #: Text characters allowed in each QQ message.
    max_text_chars_per_message: int = Field(2000, ge=400)


class WebSearchToolCfg(_M):
    #: Results requested from one search; Tavily accepts at most 20.
    count: int = Field(5, ge=1, le=20)
    #: Vendor search depth; advanced requests consume two credits instead of one.
    depth: Literal["basic", "advanced"] = "basic"


class SearchHistoryToolCfg(_M):
    context_lines: int = Field(5, ge=0)
    max_hits: int = Field(8, ge=1)
    max_query_terms: int = Field(8, ge=1)
    max_result_chars: int = Field(12000, ge=1000)


class RecallEventsToolCfg(_M):
    context_episodes: int = Field(2, ge=0)
    max_hits: int = Field(5, ge=1, le=10)


class ReadUrlToolCfg(_M):
    max_content_chars: int = Field(8000, ge=500)


class OpenImagesToolCfg(_M):
    max_images: int = Field(6, ge=1)


class ToolsCfg(_M):
    #: Calls one model round may request before later calls are refused in-band.
    max_calls_per_round: int = Field(8, ge=1)
    #: Safety tripwire for a tool loop whose model backend reports no spend.
    max_rounds: int = Field(20, ge=1)
    send_messages: SendMessagesCfg = Field(default_factory=SendMessagesCfg)
    web_search: WebSearchToolCfg = Field(default_factory=WebSearchToolCfg)
    search_history: SearchHistoryToolCfg = Field(default_factory=SearchHistoryToolCfg)
    recall_events: RecallEventsToolCfg = Field(default_factory=RecallEventsToolCfg)
    read_url: ReadUrlToolCfg = Field(default_factory=ReadUrlToolCfg)
    open_images: OpenImagesToolCfg = Field(default_factory=OpenImagesToolCfg)


class PromptCfg(_M):
    """What the model is shown of the conversation.

    A count, not a token budget: money bounds what a reply may spend, and every
    other block is rendered whole. These decide context and prefix-cache behaviour.
    """

    #: The history window, in chunks. Whole chunks rather than its own count, so the
    #: multiple holds by construction - two numbers whose ratio drifts is how a window
    #: ends up holding two and a half chunks and nobody can say what a slide leaves.
    window_chunks: int = Field(3, ge=1)
    #: How many messages leave the window at once. A prefix cache matches from the
    #: start, so every slide is a miss: sliding rarely is most of what there is to
    #: win, and one message per turn would miss on every reply.
    evict_chunk: int = Field(30, ge=1)
    #: How much of a forwarded chat record is rendered into the message that carries
    #: it: lines in all (nested records count towards the same total), how deep a
    #: record inside a record is still expanded, and the characters the whole block
    #: may take. Past any of them the rest is summarised as a count. The character
    #: bound must sit under tools.send_messages.max_text_chars_per_message, or the
    #: message's own cut would take
    #: the block's tail - and with it picture markers whose numbers were already
    #: handed out.
    forward_lines: int = Field(20, ge=1)
    forward_depth: int = Field(3, ge=1)
    forward_chars: int = Field(1500, ge=100)
    #: Bounds on the structured evidence memo kept beside a reply. Each digest and the
    #: whole rendered memo are capped independently; the memo expires because it exists
    #: only to support nearby follow-ups, not as another archive.
    evidence_result_chars: int = Field(1200, ge=100)
    evidence_total_chars: int = Field(4000, ge=500)
    evidence_ttl_days: int = Field(30, ge=1)
    #: Maximum length of the sanitized request summary inside one evidence item.
    #: Long enough to keep a boolean search expression whole; a cut expression reads
    #: as a different search.
    evidence_request_chars: int = Field(80, ge=20)

    @model_validator(mode="after")
    def _coherent_evidence_bounds(self) -> PromptCfg:
        if self.evidence_total_chars < self.evidence_result_chars:
            raise ValueError("evidence_total_chars must be at least evidence_result_chars")
        return self


class TriggerCfg(_M):
    """When the bot answers: which names count as being addressed. There is no
    rate cap - every addressed message earns its one reply attempt, and money
    (plus the provider concurrency semaphore) is what bounds the pace.
    """

    # Every name the bot answers to, canonical plus variants.
    nicknames: list[str] = Field(default_factory=list)


# One section per capability, each carrying its own provider connection. Provider names
# are closed at validation time; transport quirks belong to the provider adapter rather
# than to YAML. The supported providers currently share the same connection shape, so
# Literal discriminators are clearer than parallel union classes with identical fields.
class _ProviderCfg(_M):
    """Common marker for externally configured provider capabilities."""


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
    timeout_sec: float | None = Field(None, gt=0)


class TextCfg(_ProviderCfg):
    provider: Literal["deepseek", "openai_responses", "local"]
    endpoint: str
    credential_env: str = "TEXT_API_KEY"
    model: str
    #: How hard replies may think. Other uses of this model carry their own grade:
    #: extraction below, describing in capabilities.vision.
    reasoning_effort: Effort = "off"
    #: Applied at startup only: the semaphore is built once and a restart resizes it
    #: without losing outstanding permits.
    max_concurrency: int = Field(3, ge=1)
    timeout_sec: float = Field(30.0, gt=0)
    retries: int = Field(2, ge=0)
    #: Reading a batch of transcript into memory candidates - the same account and
    #: endpoint, a different task. It is schema-guarded and eval-covered, which is
    #: what makes it the one place a cheaper tier can measurably do the reply
    #: model's work; it also reads a hundred messages per call, so it needs a
    #: timeout the reply path would never grant.
    extract: TextUseCfg = Field(default_factory=TextUseCfg)

    def for_extract(self) -> TextCfg:
        """This config as the extraction call should see it.

        One override point rather than three: everything the extraction call needs
        to differ in is named here, so no caller has to patch a copy or pass a
        setting as an argument.
        """
        use = self.extract
        return self.model_copy(
            update={
                "model": use.model or self.model,
                "reasoning_effort": use.reasoning_effort or self.reasoning_effort,
                "timeout_sec": use.timeout_sec or self.timeout_sec,
            }
        )


class VisionCfg(_ProviderCfg):
    provider: Literal["deepseek", "openai_responses", "local"]
    endpoint: str
    credential_env: str = "TEXT_API_KEY"
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
    #: How long a stored file id - where the original was filed with the reply
    #: model's backend - is trusted to still exist there. Past it, open_images
    #: re-uploads instead of handing the model an id the vendor may have dropped,
    #: which would fail the whole request the id rides in. Bounded below the
    #: shortest retention any backend keeps files for (DeepSeek: 30 days).
    file_max_age_days: int = Field(20, ge=1, le=29)
    max_images_per_min: int = Field(6, ge=1)
    max_concurrency: int = Field(1, ge=1, le=16)
    max_output_tokens: int = Field(4096, ge=256, le=8192)
    #: Bounds both halves of picture handling: the download that feeds the
    #: description call, and the upload that puts the original in front of the
    #: reply model.
    max_image_mb: float = Field(8.0, gt=0)
    timeout_sec: float = Field(30.0, gt=0)


class AsrCfg(_M):
    """Fixed in-process SenseVoice CPU resources and admission policy."""

    model_dir: str
    threads: int = Field(2, ge=1)
    queue_capacity: int = Field(8, ge=1)
    max_audio_sec: int = Field(300, ge=1)
    max_clips_per_min: int = Field(6, ge=1)


class SearchCfg(_ProviderCfg):
    provider: Literal["tavily"]
    endpoint: str
    credential_env: str = "SEARCH_API_KEY"
    #: The free tier's credit allowance per calendar month. At it search refuses; there
    #: is no paid fallback.
    monthly_quota: int = Field(1000, ge=0)
    #: HTTP proxy for the search client only - the endpoint is not reliably reachable
    #: from the deployment region directly. Empty means direct; model traffic never
    #: goes through this.
    proxy: str = ""
    timeout_sec: float = Field(20.0, gt=0)


class EmbeddingCfg(_ProviderCfg):
    provider: Literal["dashscope"]
    endpoint: str
    credential_env: str = "MEDIA_API_KEY"
    #: Both reach past this file. `dimensions` must equal the VECTOR(n) column in
    #: sql/init.sql or every insert fails, and vectors are stored under the model
    #: that produced them and searched under the current one - so changing `model`
    #: hides every stored vector until the nightly embed pass rebuilds them, with
    #: recall answering "nothing found" in the meantime.
    model: str
    dimensions: int = 2048
    timeout_sec: float = Field(60.0, gt=0)


class CapabilitiesCfg(_M):
    text: TextCfg
    vision: VisionCfg
    asr: AsrCfg
    embedding: EmbeddingCfg
    search: SearchCfg
    #: Retries for the plain-HTTP backends (embedding, search, file upload), which
    #: carry no retries setting of their own; the chat backends use text.retries.
    http_retries: int = Field(2, ge=0)
    #: The longest a vendor's Retry-After may hold any call. A vendor asking for
    #: minutes is asking the wrong client: a reply somebody is waiting for fails and
    #: is retried by the person, and a background batch is rescheduled by its queue.
    retry_after_cap_sec: float = Field(30.0, gt=0)


class BudgetCfg(_M):
    """The daily ceiling. One number, because there is only one thing to do at it: the
    bot answers only when addressed, so past the cap it goes silent until the day rolls
    over. There is no softer fallback mode to fall to.
    """

    daily_cny_cap: float = Field(5.0, ge=0)
    #: What one reply may spend, tool loop included. Money is the only limit - there is
    #: no per-day search count or per-reply round count, which are proxies for cost that
    #: drift whenever prices move. A reply that cannot afford another round stops
    #: searching and answers from what it has in one tool-less wrap-up round: the cap
    #: bounds the spending (overshoot is exactly that one round), it does not discard
    #: what was already paid for. Only the daily cap, checked before anything is spent,
    #: means silence.
    per_reply_cny: float = Field(0.30, ge=0)


class MemoryCfg(_M):
    """How much the memory pipeline reads, and how fast it lets go.

    Model settings for the extraction call are not here - they belong to the text
    capability that runs it (capabilities.text.extract). This is the machinery around it.

    Loaded once with the worker, so one process uses one memory policy.
    """

    #: Messages one extraction chunk holds at most.
    extract_window: int = Field(120, ge=10)
    #: A full chunk is trimmed back to the last conversation gap of at least this
    #: long in its tail half, so batch boundaries fall where conversations end
    #: rather than mid-topic - which is what keeps one episode from becoming two
    #: half-known ones.
    batch_gap_min: int = Field(30, ge=1)
    #: Below this many unread messages a drain does not bother: a pass pays for the
    #: rules, the schemas and the known facts before reading a line.
    drain_floor: int = Field(20, ge=0)
    #: Passes one nightly drain may run. It can bind before the budget does; a group
    #: sustaining more than this every day has outgrown the memory budget itself, and
    #: the backlog carries over rather than being skipped.
    max_passes: int = Field(10, ge=1)
    #: How many already-recorded episodes the extractor is reminded of, so it
    #: recognises a conversation it has already written down.
    known_episodes: int = Field(8, ge=0)
    #: Confirmed aliases shown beside one exact account in extraction context.
    roster_aliases_per_account: int = Field(4, ge=0, le=20)
    #: How long an episode remains available to semantic recall. Importance is not a
    #: lifetime signal yet: extraction writes the same placeholder score on every episode,
    #: so retention stays a plain age until that score has real meaning.
    episode_ttl_days: float = Field(90.0, gt=0)
    #: How long a name the model merely guessed at survives without being used again,
    #: and how long one it marked as a joke does. Most jokes are true for an afternoon.
    alias_unused_days: float = Field(30.0, gt=0)
    joke_unused_days: float = Field(7.0, gt=0)
    #: How long a claimed memory job stays claimed. A full drain is several model
    #: calls and can outlive a short lease, and a deploy overlap would then pay for
    #: the same transcript twice.
    job_lease_min: int = Field(30, ge=1)
    worker_idle_sec: float = Field(5.0, ge=0.1, le=60.0)
    embedding_page_size: int = Field(200, ge=1, le=1000)
    worker_retry_backoff_sec: tuple[int, ...] = (60, 300, 1800, 3600)

    @field_validator("worker_retry_backoff_sec")
    @classmethod
    def _valid_backoff(cls, value: tuple[int, ...]) -> tuple[int, ...]:
        if not 1 <= len(value) <= 8:
            raise ValueError("worker_retry_backoff_sec needs 1 to 8 entries")
        if any(item < 1 or item > 86400 for item in value):
            raise ValueError("worker_retry_backoff_sec entries must be 1..86400")
        if tuple(sorted(value)) != value:
            raise ValueError("worker_retry_backoff_sec must be non-decreasing")
        return value


class ScheduleCfg(_M):
    #: The scheduler registers this complete block once at startup.
    #:
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
    napcat_cache_days: int = Field(7, ge=1)
    #: How long the pipeline waits for each stage's jobs to drain before moving on
    #: anyway. Extraction's worst honest night is max_passes model calls per group
    #: plus retry backoff; decay is one UPDATE per group. Moving on late beats a
    #: night with no backup.
    extract_drain_hours: float = Field(3.0, gt=0)
    decay_drain_min: float = Field(30.0, gt=0)
    #: How often a drain-wait checks the queue. Nobody is waiting at 03:00.
    drain_poll_sec: float = Field(30.0, gt=0)
    #: How late a missed trigger may still fire. APScheduler's default is seconds, so
    #: a loop busy at the trigger moment would silently skip that night.
    misfire_grace_sec: int = Field(3600, ge=0)
    #: How old the newest dump may be before the daily report calls it out. Just over
    #: a day, so an ordinary night's backup never trips it.
    backup_stale_hours: float = Field(26.0, gt=0)
    completed_job_keep_days: int = Field(30, ge=1, le=3650)

    @field_validator("nightly_cron", "report_cron")
    @classmethod
    def _five_fields(cls, v: str) -> str:
        # Parse with the scheduler's own implementation during startup. This catches
        # invalid ranges immediately; counting fields alone lets '30 25 * * *' through.
        if len(v.split()) != 5:
            raise ValueError(f"cron expression needs 5 fields: {v!r}")
        try:
            from apscheduler.triggers.cron import CronTrigger
        except ImportError:
            return v  # bare test venv; the container always has it
        try:
            CronTrigger.from_crontab(v)
        except ValueError as e:
            raise ValueError(f"invalid cron expression {v!r}: {e}") from None
        return v


class PredicateCfg(_M):
    """One thing that may be recorded about a person.

    Everything a predicate is lives in this one entry: what the extraction model may
    offer, how many of them a person may hold at once, how fast it is forgotten, and
    the words it renders as in the prompt. One entry rather than a table per aspect
    spread across code: split up, adding a predicate means several edits in several
    files, and each omission fails quietly - a missing verb puts the bare English
    predicate in front of the model.
    """

    #: How the fact reads in Chinese. `{{object}}` marks where the object goes for a
    #: predicate that does not read verb-first, the way an allergy does; without it
    #: the object simply follows the verb.
    verb: str = Field(min_length=1)

    @field_validator("verb")
    @classmethod
    def _closed_object_slot(cls, value: str) -> str:
        """Allow only one non-executable object slot in configurable wording."""

        slot = "{{object}}"
        if value.count(slot) > 1:
            raise ValueError("predicate verb may contain {{object}} at most once")
        rest = value.replace(slot, "")
        if "{{" in rest or "}}" in rest or "{" in rest or "}" in rest:
            raise ValueError("predicate verb supports only the optional {{object}} slot")
        return value

    def render(self, object_value: str) -> str:
        """Insert one object without interpreting any syntax it contains."""

        slot = "{{object}}"
        if slot in self.verb:
            return self.verb.replace(slot, object_value)
        return f"{self.verb}{object_value}"

    #: single: a person holds one at a time, and a new value closes the old one
    #: (which is what makes "he moved" expressible). multi: they sit side by side,
    #: each ageing on its own evidence.
    cardinality: Literal["single", "multi"]
    #: Which half-life this predicate is forgotten on. Where somebody lives changes
    #: over years, what they are playing over weeks; one clock for both is wrong for
    #: both.
    decay: Literal["stable", "default", "fast"] = "default"
    #: How the fact is classified once stored.
    kind: Literal["attribute", "preference", "relation"] = "attribute"
    #: Recording this retracts the named predicate about the same object: somebody
    #: who has gone off a thing is not also somebody who likes it. Must be mutual.
    opposite: str | None = None
    #: The line the extraction model is shown for this predicate: what it means and
    #: where its boundary with the neighbouring predicates runs. Rendered into the
    #: extraction prompt, so a predicate cannot be added without saying what it is.
    rule: str = Field(min_length=1)


class HalfLifeCfg(_M):
    """Base lifetimes in days, one per decay class. A fact nothing has confirmed
    for its base lifetime (stretched by how many distinct days supported it, up
    to fourfold) is expired outright - see MemoryRepository.decay."""

    stable: float = Field(90.0, gt=0)
    default: float = Field(30.0, gt=0)
    fast: float = Field(14.0, gt=0)


class PredicateTable(_M):
    """The whole of predicates.yaml: the clocks, and the predicates themselves."""

    half_life_days: HalfLifeCfg = Field(default_factory=HalfLifeCfg)
    person: dict[str, PredicateCfg]

    @field_validator("person")
    @classmethod
    def _coherent(cls, table: dict[str, PredicateCfg]) -> dict[str, PredicateCfg]:
        # The name goes into a JSON-schema enum, a database column and an index, so
        # it has to be plain. Checked here rather than trusted, because a predicate
        # that differs from another by a space is two predicates nothing reconciles.
        for name, p in table.items():
            if not name.replace("_", "").isalnum() or not name.islower():
                raise ValueError(
                    f"predicate name must be lower-case letters, digits and _: {name!r}"
                )
            if name in RESERVED_PREDICATES:
                raise ValueError(f"{name!r} is reserved: it has its own tool and its own shape")
            if p.opposite is None:
                continue
            other = table.get(p.opposite)
            if other is None:
                raise ValueError(f"{name}: opposite names no predicate: {p.opposite!r}")
            if other.opposite != name:
                raise ValueError(f"{name} and {p.opposite} disagree about being opposites")
        return table


#: Names the person table may not use. `note` is explicit command input, and the
#: model must have no way to write over it; `topic` and `term` are about the group
#: rather than about a person, and reach memory through their own tools.
RESERVED_PREDICATES = frozenset({"note", "topic", "term"})


class DatabaseCfg(_M):
    """The connection pool. Small on purpose: this is one bot on one machine, and
    a pool larger than the work only moves contention into postgres.

    Built once at startup and never resized, so edits apply at the next restart.
    """

    pool_min: int = Field(2, ge=1)
    pool_max: int = Field(8, ge=1)
    #: Ceiling on any single statement, so a query that will never finish fails
    #: instead of holding a connection for the life of the process.
    command_timeout_sec: float = Field(20.0, gt=0)


class Settings(_M):
    """Field order mirrors settings.yaml's narrative: who runs it and on what
    clock, when it speaks, how messages come in, which models serve it, what it
    may spend, how much it reads around a memory, when the jobs run, and finally
    where the config directory keeps its files."""

    # Everyone allowed to run ops commands and receive the daily report. A list because
    # a bot outliving one person's attention needs more than one pair of hands.
    owners: list[str] = Field(default_factory=list)

    @field_validator("owners", mode="before")
    @classmethod
    def _ids_as_text(cls, v: Any) -> Any:
        # Account ids are numbers to YAML unless quoted, and the checks compare
        # strings: an unquoted entry is the same person, not a schema error.
        return [str(x) for x in v] if isinstance(v, list) else v

    # IANA zone name. Applied at startup to every local-time reading: the clock the model
    # is told, the cron schedules, and the day the budget rolls over on.
    timezone: str = "Asia/Shanghai"
    trigger: TriggerCfg = Field(default_factory=TriggerCfg)
    gateway: GatewayCfg = Field(default_factory=GatewayCfg)
    media: MediaCfg = Field(default_factory=MediaCfg)
    members: MembersCfg = Field(default_factory=MembersCfg)
    commands: CommandsCfg = Field(default_factory=CommandsCfg)
    identity_link: IdentityLinkCfg = Field(default_factory=IdentityLinkCfg)
    diagnostics: DiagnosticsCfg = Field(default_factory=DiagnosticsCfg)
    tools: ToolsCfg = Field(default_factory=ToolsCfg)
    capabilities: CapabilitiesCfg
    budget: BudgetCfg = Field(default_factory=BudgetCfg)
    prompt: PromptCfg = Field(default_factory=PromptCfg)
    memory: MemoryCfg = Field(default_factory=MemoryCfg)
    schedule: ScheduleCfg = Field(default_factory=ScheduleCfg)
    database: DatabaseCfg = Field(default_factory=DatabaseCfg)

    # -- files: paths relative to the config directory (absolute allowed) ----
    personas_dir: str = "personas"
    #: Directory containing the single strictly validated prompts.yaml bundle.
    prompts_dir: str = "prompts"
    #: What may be recorded about a person, one entry per predicate.
    predicates_file: str = "predicates.yaml"


class Persona(_M):
    """A group's model-facing identity and standing context.

    A group file states only what differs from default.yaml; anything it leaves out is
    inherited. Settings are deliberately absent: behavior and resource policy are global.

    `system_prompt_extra` exists because sharing happens *inside* the prompt, not only
    between fields: the base prompt is inherited and the group's own paragraphs are
    appended to it. Setting `system_prompt` outright replaces the base entirely, for a
    group that wants nothing shared.
    """

    name: str = "小X"
    system_prompt: str = ""
    system_prompt_extra: str = ""
    group_knowledge: str = ""


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
                    merged.system_prompt.rstrip() + "\n\n" + merged.system_prompt_extra.strip()
                ),
                "system_prompt_extra": "",
            }
        )
    return merged


def _read_yaml(path: Path) -> dict:
    with path.open("r", encoding="utf-8") as f:
        data = yaml.safe_load(f) or {}
    if not isinstance(data, dict):
        raise ValueError(f"{path}: top level must be a mapping")
    return data


@dataclass(frozen=True, slots=True)
class ConfigBundle:
    """One immutable startup snapshot of settings and prompt resources."""

    default: Settings
    personas: Mapping[GroupId, Persona]
    prompts: PromptCatalog
    predicates: PredicateTable
    _default_persona: Persona
    _resolved_personas: Mapping[GroupId, Persona]

    def __init__(
        self,
        raw_settings: dict,
        personas: Mapping[GroupId | str, Persona],
        prompts: PromptCatalog | None = None,
        predicates: PredicateTable | None = None,
    ) -> None:
        default = personas.get(DEFAULT_PERSONA_KEY, Persona())
        groups: dict[GroupId, Persona] = {}
        for key, persona in personas.items():
            if key == DEFAULT_PERSONA_KEY:
                continue
            try:
                gid = GroupId(key)
            except ValueError:
                raise ValueError(f"persona group id must be numeric: {key!r}") from None
            groups[gid] = persona
        resolved_default = _merge_persona(default, None)
        resolved = {gid: _merge_persona(default, persona) for gid, persona in groups.items()}
        object.__setattr__(self, "default", Settings.model_validate(raw_settings))
        object.__setattr__(self, "personas", MappingProxyType(groups))
        object.__setattr__(self, "prompts", prompts or PromptCatalog.empty())
        object.__setattr__(
            self,
            "predicates",
            predicates or PredicateTable(person={}),
        )
        object.__setattr__(self, "_default_persona", resolved_default)
        object.__setattr__(self, "_resolved_personas", MappingProxyType(resolved))

    def persona_for(self, group: GroupId) -> Persona:
        return self._resolved_personas.get(group, self._default_persona)

    def for_group(self, group: GroupId) -> tuple[Settings, Persona]:
        return self.default, self.persona_for(group)


def _resolve(root: Path, p: str) -> Path:
    q = Path(p)
    return q if q.is_absolute() else root / q


def load_bundle(config_dir: Path | None = None) -> ConfigBundle:
    root = config_dir or CONFIG_DIR
    raw = _read_yaml(root / "settings.yaml")
    # Validated up front: the directory layout below comes from the config
    # itself, so the schema has to hold before anything else is read.
    settings = Settings.model_validate(raw)

    personas: dict[GroupId | str, Persona] = {}
    persona_dir = _resolve(root, settings.personas_dir)
    if persona_dir.is_dir():
        for path in sorted(persona_dir.glob("*.yaml")):
            stem = path.stem
            if stem == DEFAULT_PERSONA_KEY:
                key: GroupId | str = DEFAULT_PERSONA_KEY
            elif stem.startswith("group_") and stem[6:].isdigit():
                key = GroupId(stem[6:])
            else:
                raise ValueError(
                    f"{path}: persona file must be default.yaml or group_<numeric id>.yaml"
                )
            personas[key] = Persona.model_validate(_read_yaml(path))

    # Prompt wording is customizable, but its role and slot surface are code-owned.
    # The complete catalog validates before startup continues.
    prompt_dir = _resolve(root, settings.prompts_dir)
    prompts = PromptCatalog.load(prompt_dir)

    # The predicate table is mandatory too: without it the extractor would offer
    # the model an empty enum and every fact would be rejected, silently and all
    # night. The prompt has to carry the block that explains it, or the model gets
    # an enum with no meanings attached.
    ppath = _resolve(root, settings.predicates_file)
    try:
        predicates = PredicateTable.model_validate(_read_yaml(ppath))
    except OSError as e:
        raise ValueError(f"predicate file unreadable: {ppath}: {e}") from None
    if not predicates.person:
        raise ValueError(f"predicate file lists no predicates: {ppath}")
    bundle = ConfigBundle(raw, personas, prompts, predicates)
    # Resolve every configured persona before startup accepts the bundle.
    for group_id in bundle.personas:
        bundle.persona_for(group_id)
    return bundle


_bundle: ConfigBundle | None = None


def config() -> ConfigBundle:
    global _bundle
    if _bundle is None:
        _bundle = load_bundle()
        util.set_timezone(_bundle.default.timezone)
    return _bundle


def prompt_catalog() -> PromptCatalog:
    """Return the startup-validated prompt template bundle."""

    return config().prompts
