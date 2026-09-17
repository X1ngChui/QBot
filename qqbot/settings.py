"""Configuration loading and validation.

Two layers: settings.yaml defaults, overridden per group by personas/group_*.yaml.
Pydantic validates; /reload re-reads from disk and swaps the global singleton atomically
(readers never take a lock).
"""

from __future__ import annotations

import copy
import logging
import os
from pathlib import Path
from typing import Any, ClassVar, Literal

import yaml
from pydantic import BaseModel, ConfigDict, Field, field_validator

from . import util

log = logging.getLogger("qqbot.settings")
#: Every prompt the system may read, by key. The texts are data: each key is a
#: `<key>.txt` under `prompts_dir`, edited without touching code and re-read on
#: /reload. This manifest is the only list of them - the filename is the key, so
#: a misspelled name is a missing file and fails the load rather than shipping a
#: prompt nobody reads. Each text's role and how they compose is described in
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
    "describe_image",
    "tool_web_search", "tool_search_history", "tool_recall_events",
    "tool_read_url", "tool_open_images", "tool_send_message",
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
    #: Longest message text kept on arrival and sent as a reply. Command answers
    #: reserve a 200-character margin under it for their own framing, so the floor
    #: keeps them a usable width.
    max_msg_len: int = Field(2000, ge=400)
    #: How long a reply waits for a picture or a voice clip to be understood before
    #: building the prompt without it. The work is never cancelled - it lands for the
    #: next turn either way - so this only decides whether the person waits or the
    #: answer does.
    media_wait_sec: float = 25.0
    #: How long the group's member list is reused before being fetched again. Names
    #: are read on every message, and the platform call is the expensive part.
    member_cache_ttl_sec: int = 1800
    #: Deadline for the protocol side's media calls (get_image, get_record),
    #: under its own half-minute default. A picture the platform
    #: can no longer serve does not fail there, it hangs - and a reply would stand
    #: still for the whole of it.
    protocol_call_timeout_sec: float = Field(10.0, gt=0)
    #: Deadline for downloading a picture or a clip from a link.
    media_http_timeout_sec: float = Field(20.0, gt=0)
    #: How long a picture no route could read is left alone before another attempt:
    #: one dead picture must not cost every reply that looks at it a full timeout.
    unreadable_retry_sec: int = Field(600, ge=0)
    #: How long shutdown waits for in-flight archive writes and media patches. They
    #: are never cancelled, only waited for; a hung one must not hold a deploy.
    shutdown_wait_sec: float = Field(5.0, ge=0)


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
    #: bound must sit under gateway.max_msg_len, or the message's own cut would take
    #: the block's tail - and with it picture markers whose numbers were already
    #: handed out.
    forward_lines: int = Field(20, ge=1)
    forward_depth: int = Field(3, ge=1)
    forward_chars: int = Field(1500, ge=100)
    #: How much of the trajectory entry kept beside each of the bot's own replies
    #: survives: characters of each tool result, and of the whole entry. A digest
    #: for follow-ups on the same topic, not a replay - the tools are still there
    #: when more is needed. Sized so the digest reaches the answer: a search result
    #: opens with the conversation around its first hit, and a couple of hundred
    #: characters recorded only the chatter leading up to what was found.
    trace_result_chars: int = Field(1200, ge=100)
    trace_total_chars: int = Field(4000, ge=500)
    #: Caps on the provenance marker appended to the bot's own archived line: how
    #: many tool uses it names, and how much of each query survives. A record, not
    #: a transcript - enough for a later turn to see what an answer rested on. The
    #: query cap is set where a boolean expression survives whole (cut in half it
    #: reads as a different search); it only guards against a runaway string.
    provenance_items: int = Field(4, ge=1)
    provenance_query_chars: int = Field(80, ge=20)


class TriggerCfg(_M):
    """When the bot answers: which names count as being addressed. There is no
    rate cap - every addressed message earns its one reply attempt, and money
    (plus the provider concurrency semaphore) is what bounds the pace.
    """

    # Every name the bot answers to, canonical plus variants.
    nicknames: list[str] = Field(default_factory=list)


# One section per capability, each carrying its own endpoint, model and credential name
# so it can be moved independently: five capabilities, each swappable, and no automatic
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
class _BackendCfg(_M):
    """A capability section: its `backend` must name a class the registry lists.

    Checked at load rather than at construction, because the bundle is built once at
    startup: a name misspelled in an edit would pass /reload and fail the next restart,
    days later, as a boot error far from its cause.
    """

    #: Which registry table the name is looked up in; each section names its own.
    capability: ClassVar[str] = ""

    @field_validator("backend", check_fields=False)
    @classmethod
    def _known_backend(cls, v: str) -> str:
        # Imported here, not at module scope: the registry imports this module for
        # the config classes, and the tables are the single list of known names.
        from .providers import registry
        table = getattr(registry, f"{cls.capability.upper()}_BACKENDS")
        if v not in table:
            raise ValueError(
                f"unknown {cls.capability} backend {v!r}; "
                f"available: {', '.join(sorted(table))}")
        return v


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


class TextCfg(_BackendCfg):
    capability = "text"
    backend: str = "openai_compat"
    base_url: str
    api_key_env: str = "TEXT_API_KEY"
    model: str
    #: How hard replies may think. Other uses of this model carry their own grade:
    #: extraction below, describing in llm.vision.
    reasoning_effort: Effort = "off"
    #: Applied at startup only: the semaphore is built once, and resizing it under
    #: load would lose the permits already handed out. A change is logged and takes
    #: effect at the next start.
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
        return self.model_copy(update={
            "model": use.model or self.model,
            "reasoning_effort": use.reasoning_effort or self.reasoning_effort,
            "timeout_sec": use.timeout_sec or self.timeout_sec,
        })


class VisionCfg(_BackendCfg):
    capability = "vision"
    backend: str = "openai_compat"
    base_url: str
    api_key_env: str = "TEXT_API_KEY"
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
    #: Bounds both halves of picture handling: the download that feeds the
    #: description call, and the upload that puts the original in front of the
    #: reply model.
    max_image_mb: float = Field(8.0, gt=0)
    timeout_sec: float = Field(30.0, gt=0)


class AsrCfg(_BackendCfg):
    capability = "asr"
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
    threads: int = Field(2, ge=1)
    max_audio_sec: int = Field(300, ge=1)
    #: Transcription happens on arrival, so a burst of long clips spends real
    #: money before the daily cap can matter - the same reason pictures carry
    #: max_images_per_min. Clips are rarer, so the same number is generous.
    max_clips_per_min: int = Field(6, ge=1)
    timeout_sec: float = Field(60.0, gt=0)


class SearchCfg(_BackendCfg):
    capability = "search"
    backend: str
    base_url: str
    api_key_env: str = "SEARCH_API_KEY"
    #: Results per search; the vendor takes at most 20.
    count: int = Field(5, ge=1, le=20)
    #: Result depth the vendor is asked for; "basic" is one credit, "advanced" two.
    #: Closed on purpose: the credit booking keys on the exact word, and a
    #: misspelling would be metered as basic while the vendor charged advanced.
    depth: Literal["basic", "advanced"] = "basic"
    #: The free tier's credit allowance per calendar month. At it search refuses; there
    #: is no paid fallback.
    monthly_quota: int = Field(1000, ge=0)
    #: HTTP proxy for the search client only - the endpoint is not reliably reachable
    #: from the deployment region directly. Empty means direct; model traffic never
    #: goes through this.
    proxy: str = ""
    timeout_sec: float = Field(20.0, gt=0)


class EmbeddingCfg(_BackendCfg):
    capability = "embedding"
    backend: str = "dashscope"
    base_url: str
    api_key_env: str = "MEDIA_API_KEY"
    #: Both reach past this file. `dimensions` must equal the VECTOR(n) column in
    #: sql/init.sql or every insert fails, and vectors are stored under the model
    #: that produced them and searched under the current one - so changing `model`
    #: hides every stored vector until the nightly embed pass rebuilds them, with
    #: recall answering "nothing found" in the meantime.
    model: str
    dimensions: int = 2048
    timeout_sec: float = Field(60.0, gt=0)


class LlmCfg(_M):
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
    #: How many archive hits one search returns. A model that wants more searches
    #: again with better words; nothing here is paginated.
    history_hits: int = Field(8, ge=1)
    #: Terms one search expression may carry. Past this the question wants splitting
    #: into two searches - a query is a filter, not a program.
    max_query_terms: int = Field(8, ge=1)
    #: How much of one page read_url hands the model. The one input with no bound of
    #: its own: a web page can be any size, and past the model's context the request
    #: fails outright rather than degrading. A cut page is told it was cut.
    url_content_chars: int = Field(8000, ge=500)
    #: How many tool calls one round of the reply loop may carry. Each result is
    #: appended to the prompt, and a round asking for thirty pages at once would
    #: grow the next request past the model's context before money had a chance to
    #: bind. The rest of the round is answered with a note.
    max_tool_calls_per_round: int = Field(8, ge=1)
    #: How many pictures one open_images call may fetch; each is a file block in the
    #: next request.
    open_images_max: int = Field(6, ge=1)
    #: How much of one search_history answer the model is handed. Hits are bounded
    #: by count and each line by gateway.max_msg_len, but their product can still
    #: outgrow the model's context, where the request fails outright rather than
    #: degrading. A cut answer is told it was cut.
    history_chars: int = Field(12000, ge=1000)
    #: A tripwire on the tool loop, not a policy: money ends the loop, and only a
    #: backend reporting zero cost could make a money-bounded loop unbounded. Past
    #: this many rounds the reply is abandoned with an error in the log.
    max_rounds: int = Field(20, ge=1)


class MemoryCfg(_M):
    """How much the memory pipeline reads, and how fast it lets go.

    Model settings for the extraction call are not here - they belong to the text
    capability that runs it (llm.text.extract). This is the machinery around it.

    Read once, when the worker is constructed: a /reload cannot change a batch that
    is already being read, so edits here apply at the next restart.
    """

    #: Messages one extraction chunk holds at most. /relearn's watermark reset keeps
    #: exactly this many unread, so the two move together.
    extract_window: int = Field(120, ge=10)
    #: A full chunk is trimmed back to the last conversation gap of at least this
    #: long in its tail half, so batch boundaries fall where conversations end
    #: rather than mid-topic - which is what keeps one episode from becoming two
    #: half-known ones.
    batch_gap_min: int = Field(30, ge=1)
    #: Below this many unread messages a drain does not bother: a pass pays for the
    #: rules, the schemas and the known facts before reading a line. /relearn forces
    #: past it.
    drain_floor: int = Field(20, ge=0)
    #: Passes one nightly drain may run. It can bind before the budget does; a group
    #: sustaining more than this every day has outgrown the memory budget itself, and
    #: the backlog carries over rather than being skipped.
    max_passes: int = Field(10, ge=1)
    #: How many already-recorded episodes the extractor is reminded of, so it
    #: recognises a conversation it has already written down.
    known_episodes: int = Field(8, ge=0)
    #: How long a name the model merely guessed at survives without being used again,
    #: and how long one it marked as a joke does. Most jokes are true for an afternoon.
    alias_unused_days: float = Field(30.0, gt=0)
    joke_unused_days: float = Field(7.0, gt=0)
    #: How long a claimed memory job stays claimed. A full drain is several model
    #: calls and can outlive a short lease, and a deploy overlap would then pay for
    #: the same transcript twice.
    job_lease_min: int = Field(30, ge=1)


class ScheduleCfg(_M):
    #: The two crons and misfire_grace_sec below are handed to the scheduler at
    #: startup: /reload validates them and accepts the file, but the registered jobs
    #: keep their old triggers (and their old timezone) until the next restart. The
    #: rest of this class is read when the night actually runs.
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


class PredicateCfg(_M):
    """One thing that may be recorded about a person.

    Everything a predicate is lives in this one entry: what the extraction model may
    offer, how many of them a person may hold at once, how fast it is forgotten, and
    the words it renders as in the prompt. One entry rather than a table per aspect
    spread across code: split up, adding a predicate means several edits in several
    files, and each omission fails quietly - a missing verb puts the bare English
    predicate in front of the model.
    """

    #: How the fact reads in Chinese. `{}` marks where the object goes for a
    #: predicate that does not read verb-first, the way an allergy does; without it
    #: the object simply follows the verb.
    verb: str = Field(min_length=1)
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
                    f"predicate name must be lower-case letters, digits and _: {name!r}")
            if name in RESERVED_PREDICATES:
                raise ValueError(
                    f"{name!r} is reserved: it has its own tool and its own shape")
            if p.opposite is None:
                continue
            other = table.get(p.opposite)
            if other is None:
                raise ValueError(f"{name}: opposite names no predicate: {p.opposite!r}")
            if other.opposite != name:
                raise ValueError(
                    f"{name} and {p.opposite} disagree about being opposites")
        return table


#: Where the rendered predicate table is dropped into the extraction prompt. A plain
#: token rather than a format field: the prompt is Chinese prose full of braces-free
#: punctuation, and str.format would trip over any brace somebody typed.
PREDICATE_SLOT = "{{谓词表}}"

#: Names the person table may not use. `note` is what an owner typed by hand, and the
#: model must have no way to write over it; `topic` and `term` are about the group
#: rather than about a person, and reach memory through their own tools.
RESERVED_PREDICATES = frozenset({"note", "topic", "term"})


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
    #: How often one member is re-shown the pointer at /terms. The full text re-sent
    #: every time reads as spam, and a member who has not accepted still costs
    #: nothing - the pointer is sent instead of a reply, not as well as one.
    prompt_every_sec: float = Field(600.0, gt=0)


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
    llm: LlmCfg
    budget: BudgetCfg = Field(default_factory=BudgetCfg)
    prompt: PromptCfg = Field(default_factory=PromptCfg)
    retrieval: RetrievalCfg = Field(default_factory=RetrievalCfg)
    memory: MemoryCfg = Field(default_factory=MemoryCfg)
    schedule: ScheduleCfg = Field(default_factory=ScheduleCfg)
    database: DatabaseCfg = Field(default_factory=DatabaseCfg)

    # -- files: paths relative to the config directory (absolute allowed) ----
    personas_dir: str = "personas"
    agreement: AgreementCfg
    #: Where the prompt texts live, one `<key>.txt` per entry in PROMPT_KEYS.
    prompts_dir: str = "prompts"
    #: What may be recorded about a person, one entry per predicate.
    predicates_file: str = "predicates.yaml"


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
                 agreement_text: str = "",
                 predicates: PredicateTable | None = None):
        self._raw = raw_settings
        self.default = Settings.model_validate(raw_settings)
        self.personas = personas
        #: What may be recorded about a person, from predicates_file. Read through
        #: `predicates()`; empty only for a bundle built outside load_bundle.
        self.predicates: PredicateTable = predicates or PredicateTable(person={})
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
                cached = Settings.model_validate(
                    _deep_merge(self._raw, self._without_backends(group_id, persona.overrides)))
            else:
                cached = self.default
            self._merged[group_id] = cached
        return cached, persona

    @staticmethod
    def _without_backends(group_id: str, overrides: dict) -> dict:
        """A group's overrides with the keys no group may change dropped, and logged.

        The backend classes are built once at startup from the top-level config;
        a group may repoint endpoint and model (passed per call) but cannot change
        which class serves it. The daily cap and the monthly search allowance are
        totals over every group, compared against one shared ledger: a group's
        own number there would let it keep spending after the rest went quiet.
        Left in, any of these would read back from the group's merged settings
        as though it applied.
        """
        out = overrides
        llm = overrides.get("llm")
        if isinstance(llm, dict):
            touched = [cap for cap, sec in llm.items()
                       if isinstance(sec, dict) and "backend" in sec]
            if touched:
                log.warning("group %s overrides llm.%s.backend: backends are chosen "
                            "once at startup from the top-level config, override "
                            "ignored", group_id, "/".join(touched))
                out = copy.deepcopy(out)
                for cap in touched:
                    del out["llm"][cap]["backend"]
        for path in (("budget", "daily_cny_cap"), ("llm", "search", "monthly_quota")):
            node = out
            for key in path[:-1]:
                node = node.get(key) if isinstance(node, dict) else None
            if isinstance(node, dict) and path[-1] in node:
                log.warning("group %s overrides %s: a total over every group, "
                            "override ignored", group_id, ".".join(path))
                if out is overrides:
                    out = copy.deepcopy(overrides)
                node = out
                for key in path[:-1]:
                    node = node[key]
                del node[path[-1]]
        return out


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
    if PREDICATE_SLOT not in prompts["extract"]:
        raise ValueError(
            f"the extract prompt must carry {PREDICATE_SLOT}, "
            "where the predicate table is rendered")

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

    bundle = ConfigBundle(raw, personas, prompts, agreement_text, predicates)
    # Validate eagerly: a bad config should blow up at startup / reload, not on the
    # first incoming message. Every group's merged overrides included, because
    # for_group merges lazily: a typo in one group's overrides would otherwise pass
    # /reload and then fail on that group's every message - no reply, no archive -
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
