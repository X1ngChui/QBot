"""The background executor: it does what is on the queue.

The asynchronous chain:

    EXTRACT_MEMORY -> reserve exact events, checkpoint model output, project atomically
    EMBED          -> fill in vectors for new episodes

It runs inside one process, but all of its state is in the database: leases, retries and
backoff are the queue's guarantees. So this loop can be killed at any moment and started
again, and everything it does is re-entrant.
"""

from __future__ import annotations

from qqbot.clock import Clock

from collections.abc import Callable
import asyncpg

import asyncio
import logging
import re
import uuid
from collections.abc import Mapping
from datetime import timedelta

import openai

from qqbot.services.budget import Budget, BudgetExceeded, BudgetUnavailable
from qqbot.conversation.member_numbers import BOT_DISPLAY_NUMBER
from qqbot.gateway.segments import number_at_mentions
from qqbot.domain.archive import AuthorKind
from qqbot.domain.ids import AccountId, GroupId
from qqbot.domain.memory import ExtractionBatch
from qqbot.domain.memory import ExtractionSnapshot
from qqbot.domain.memory import ExtractionStatus
from qqbot.domain.memory import SnapshotLine
from qqbot.domain.memory import SnapshotTarget
from qqbot.providers.base import Providers
from qqbot.providers.base import QuotaExhausted
from qqbot.repositories import EpisodeRepository
from qqbot.repositories import ExtractionRepository
from qqbot.repositories import IdentityRepository
from qqbot.repositories import JobQueue
from qqbot.repositories import MemoryRepository
from qqbot.repositories import VectorRepository
from qqbot.workers.lease import ClaimLease, Deferred
from qqbot.repositories.job import LeaseLost
from qqbot.repositories.job import Job
from qqbot.repositories.job import JobType
from qqbot.services import ExtractionInput
from qqbot.services import MemoryConsolidator
from qqbot.services import MemoryExtractor
from qqbot.services.memory_extractor import SourceLine
from qqbot.services.memory_extractor import decay_classes
from qqbot.services.context_builder import NOTE
from qqbot.services.context_builder import render_hint
from qqbot.services.directory import NameCard
from qqbot.configuration import ConfigBundle
from qqbot.util import defang
from qqbot.util import sysmark
from qqbot.util import why

log = logging.getLogger("qqbot.worker")

#: Failures the outside world causes: a slow or refusing model, a used-up
#: allowance. Logged in one line, because a traceback through the client library
#: says nothing the message does not. Anything else is this code's own fault and
#: gets the traceback.
EXPECTED_FAILURES = (TimeoutError, QuotaExhausted, openai.APIError)
EXTRACTION_EVENT_LIMIT = 120
EXTRACTION_GAP = timedelta(minutes=30)

_VOICE_TEXT = re.compile(r"⟦语音:([^⟧]+)⟧")


def _eligible_text(archived, number_for) -> str:
    """Render only member-authored top-level text, mentions, and voice transcripts."""

    voices = iter(_VOICE_TEXT.findall(archived.text))
    parts: list[str] = []
    for segment in archived.segments:
        kind = segment.get("type")
        data = segment.get("data")
        if not isinstance(data, Mapping):
            continue
        if kind == "text":
            if text := defang(str(data.get("text") or "")).strip():
                parts.append(text)
        elif kind == "at":
            account = str(data.get("qq") or "")
            if not account or account == "all":
                continue
            label = defang(str(data.get("name") or account)).strip() or account
            code = number_for(account)
            parts.append(f"@{label}" + (sysmark(str(code)) if code is not None else ""))
        elif kind == "record" and (voice := next(voices, "")):
            parts.append(sysmark(f"语音:{voice}"))
    return " ".join(parts)


# Fact lifetimes come from the predicate class tables in memory_extractor - the kind of
# fact is the primary axis of forgetting, evidence a bounded multiplier on top. See
# MemoryRepository.decay for the reasoning. How long an unused name survives is
# config (memory.alias_unused_days / joke_unused_days).


def _transcript(lines: list[SourceLine]) -> str:
    """The batch as the model reads it. Written once, because the extractor and the
    validator have to be looking at the same text: the validator's one real check is that
    a quote appears in the transcript word for word, and two ways of building it is how
    that check starts rejecting things that were said."""
    return "\n".join(ln.text for ln in lines)


class MemoryWorker:
    def __init__(
        self,
        bundle: ConfigBundle,
        providers: Providers,
        *,
        clock: Clock,
        database: Callable[[], asyncpg.Pool],
        budget: Budget,
        worker_id: str | None = None,
    ) -> None:
        self._clock = clock
        self._database = database
        self._budget = budget
        self._bundle = bundle
        cfg = bundle.default
        self._cfg = cfg
        #: The memory mechanism's settings and extraction prompt are fixed at
        #: construction for this worker's lifetime.
        self._m = cfg.memory
        self._retry_backoff = tuple(timedelta(seconds=n) for n in (60, 300, 1800, 3600))
        self._queue = JobQueue(worker_id or f"memory-{uuid.uuid4().hex[:6]}", database)
        self._ids = IdentityRepository(database=database, clock=clock)
        self._mem = MemoryRepository(database=database, clock=clock)
        self._eps = EpisodeRepository(database=database)
        self._extractions = ExtractionRepository(database=database)
        self._extractor = MemoryExtractor(bundle, providers.text)
        self._consolidator = MemoryConsolidator(
            self._ids,
            self._mem,
            self._eps,
            self._extractions,
            self._queue,
            database=database,
            predicates=bundle.predicates,
        )
        # The bundle's own embedding backend: vectors are stored under the model that
        # produced them, so the store is keyed by the backend answering right now.
        self._embed = providers.embedding
        self._vec = VectorRepository(self._embed.name, database=database)

    async def run_forever(self, *, idle: float | None = None) -> None:
        idle = 5.0 if idle is None else idle
        while True:
            try:
                if not await self.step():
                    await asyncio.sleep(idle)
            except asyncio.CancelledError:
                raise
            except Exception:
                log.exception("worker loop error")
                await asyncio.sleep(idle)

    async def step(self) -> bool:
        """Execute one renewable claim; busy resources defer rather than finish it."""
        job = await self._queue.claim()
        if job is None:
            return False
        try:
            try:
                await ClaimLease(self._queue, job).run(lambda: self._dispatch(job))
            except Deferred as exc:
                await self._queue.defer(job, str(exc), backoff=exc.delay)
            except (BudgetExceeded, BudgetUnavailable) as exc:
                await self._queue.defer(job, str(exc), backoff=timedelta(hours=1))
            except LeaseLost:
                raise
            except Exception as exc:
                log.warning(
                    "job %s (%s) failed: %s",
                    job.id,
                    job.job_type,
                    why(exc),
                    exc_info=not isinstance(exc, EXPECTED_FAILURES),
                )
                await self._queue.fail(
                    job,
                    why(exc),
                    backoff=self._retry_backoff[min(job.retry_count, len(self._retry_backoff) - 1)],
                )
            else:
                if not await self._queue.done(job):
                    raise LeaseLost("claim expired before completion")
        except LeaseLost:
            log.info("job %s lost ownership; late results were not committed", job.id)
        return True

    async def _dispatch(self, job: Job) -> None:
        match job.job_type:
            case JobType.EXTRACT_MEMORY:
                await self.extract(GroupId(job.payload["group_id"]), fence=job)
            case JobType.EMBED:
                await self.embed(GroupId(job.payload["group_id"]), fence=job)
            case JobType.DECAY:
                await self.decay(GroupId(job.payload["group_id"]), fence=job)
            case _:
                log.info("ignoring unimplemented job type: %s", job.job_type)

    # -- extraction --------------------------------------------------------
    async def _known(self, group_id: GroupId, codes: dict[int, uuid.UUID]) -> str:
        """Render existing group, exact-account, holder, and episode memory."""

        subject = await self._ids.group_entity(group_id)
        out: list[str] = []
        if fixed := self._bundle.persona_for(group_id).group_knowledge.strip():
            out.append(f"本群固定资料：\n{fixed}")

        if group_facts := await self._mem.current_entity_facts(group_id, [subject]):
            out.append("本群未确认线索：")
            out += [
                "- "
                + render_hint(
                    "事实",
                    fact.predicate
                    + (f" {fact.object_key}" if fact.object_key else "")
                    + f" = {fact.object_value}",
                    fact.confidence,
                )
                for fact in sorted(
                    group_facts,
                    key=lambda item: (item.predicate, item.object_key or ""),
                )
            ]

        by_account = {account_id: code for code, account_id in codes.items()}
        accounts = {
            account.id: account for account in await self._ids.accounts_by_ids(list(by_account))
        }
        per: dict[int, list[str]] = {}
        notes: dict[int, list[str]] = {}
        exact = await self._mem.current_account_facts(group_id, list(by_account))
        for fact in exact:
            assert fact.subject_account_id is not None
            code = by_account.get(fact.subject_account_id)
            if code is None:
                continue
            if fact.predicate == NOTE:
                notes.setdefault(code, []).append("精确账号：" + defang(str(fact.object_value)))
            else:
                per.setdefault(code, []).append(
                    render_hint("事实", f"{fact.predicate} = {fact.object_value}", fact.confidence)
                )

        accounts_by_holder: dict[uuid.UUID, list[uuid.UUID]] = {}
        for account_id, account in accounts.items():
            accounts_by_holder.setdefault(account.entity_id, []).append(account_id)
        holder_ids = list(accounts_by_holder)
        holder_facts = await self._mem.current_entity_facts(group_id, holder_ids)
        for fact in holder_facts:
            assert fact.subject_entity_id is not None
            for account_id in accounts_by_holder.get(fact.subject_entity_id, ()):
                code = by_account[account_id]
                if fact.predicate == NOTE:
                    notes.setdefault(code, []).append(
                        "关联身份共享：" + defang(str(fact.object_value))
                    )
                else:
                    rendered = "关联集合共享：" + render_hint(
                        "事实", f"{fact.predicate} = {fact.object_value}", fact.confidence
                    )
                    if rendered not in per.setdefault(code, []):
                        per[code].append(rendered)

        aliases = await self._ids.aliases_for_many(group_id, holder_ids)
        for holder_id, holder_accounts in accounts_by_holder.items():
            for alias in aliases.get(holder_id, ()):
                if alias.is_usable:
                    continue
                if alias.target_account_id is None:
                    targets = holder_accounts
                elif alias.target_account_id in holder_accounts:
                    targets = (alias.target_account_id,)
                else:
                    continue
                hint = NameCard(alias.alias_text, alias.alias_type, alias.confidence).hint
                if alias.target_account_id is None:
                    hint = "关联集合共享：" + hint
                for account_id in targets:
                    per.setdefault(by_account[account_id], []).append(hint)

        for code in sorted(per.keys() | notes.keys()):
            out.append(f"{sysmark(str(code))}：")
            if manual := notes.get(code):
                out.append("人工备注：")
                out.extend("- " + text for text in sorted(manual))
            if hints := sorted(per.get(code, [])):
                out.append("未确认线索：")
                out.extend(f"- {hint}" for hint in hints)

        episodes = await self._eps.recent_active(
            group_id,
            limit=8,
        )
        if episodes:
            out.append("已记过的事：")
            out += [f"- {defang(episode.summary)}" for episode in episodes]
        return "\n".join(out)

    async def extract(self, group_id: GroupId, *, fence: Job | None = None) -> int:
        """Execute one bounded batch; only successful progress queues its continuation."""
        async with self._extractions.model_slot(group_id) as acquired:
            if not acquired:
                if fence is not None:
                    raise Deferred("extraction slot busy", delay=timedelta(seconds=15))
                return 0
            batch = await self._extractions.open(group_id)
            if batch is not None and batch.status is ExtractionStatus.STAGED:
                await self._apply_batch(batch, fence=fence)
                total = 0
            else:
                if await self._budget.exceeded():
                    raise Deferred("daily budget exhausted", delay=timedelta(hours=1))
                if batch is None:
                    batch = await self._extractions.claim(
                        group_id,
                        limit=EXTRACTION_EVENT_LIMIT,
                        floor=1,
                        gap=EXTRACTION_GAP,
                        fence=fence,
                    )
                if batch is None:
                    return 0
                total = await self._extract_batch(batch, fence=fence)
            if await self._extractions.has_unconsumed(group_id):
                await self._queue.submit(
                    JobType.EXTRACT_MEMORY, {"group_id": group_id}, fence=fence
                )
        log.info("group %s: extraction batch processed (%d candidates)", group_id, total)
        return total

    async def _render_snapshot(
        self,
        batch: ExtractionBatch,
    ) -> tuple[dict[int, uuid.UUID], str, list[SourceLine], ExtractionSnapshot]:
        events = sorted(
            batch.events,
            key=lambda event: (event.occurred_at, event.raw_event_id),
        )
        codes, roster, lines = await self._render(batch.group_id, events)
        occurred = {event.raw_event_id: event.occurred_at for event in events}
        snapshot = ExtractionSnapshot(
            account_codes=tuple(sorted(codes.items())),
            lines=tuple(
                SnapshotLine(
                    ordinal=line.ordinal,
                    event_id=line.event_id,
                    occurred_at=occurred[line.event_id],
                    text=line.text,
                    own=line.own,
                    event_type=line.event_type,
                    evidence_text=line.evidence_text,
                    author_account_id=line.author_account_id,
                    targets=line.targets,
                )
                for line in lines
            ),
        )
        return codes, roster, lines, snapshot

    async def _extract_batch(self, batch: ExtractionBatch, *, fence: Job | None = None) -> int:
        """Call the model outside a transaction, checkpoint, then project atomically."""

        codes, roster, lines, snapshot = await self._render_snapshot(batch)
        candidates = []
        if codes:
            known = await self._known(batch.group_id, codes)
            if not await self._extractions.begin_attempt(batch.id, fence=fence):
                log.warning("extraction %s cannot start another model attempt", batch.id)
                return 0
            candidates = await self._extractor.extract(
                ExtractionInput(
                    group_id=batch.group_id,
                    transcript=_transcript(lines),
                    roster=roster,
                    account_codes=codes,
                    lines=tuple(lines),
                    extraction_id=batch.id,
                    known=known,
                    self_names="、".join(self._cfg.bot.nicknames),
                )
            )
        await self._extractions.stage(batch.id, snapshot, candidates, fence=fence)
        staged = ExtractionBatch(
            id=batch.id,
            group_id=batch.group_id,
            status=ExtractionStatus.STAGED,
            events=batch.events,
            snapshot=snapshot,
        )
        await self._apply_batch(staged, fence=fence)
        if not candidates:
            log.info("group %s: nothing worth extracting in this batch", batch.group_id)
        return len(candidates)

    async def _apply_batch(
        self, batch: ExtractionBatch, *, fence: Job | None = None
    ) -> tuple[int, int]:
        if batch.snapshot is None:
            raise RuntimeError(f"staged extraction {batch.id} has no snapshot")
        written, rejected = await self._consolidator.apply(
            batch.id,
            group_id=batch.group_id,
            when=self._clock.now(),
            fence=fence,
        )
        if written or rejected:
            log.info(
                "group %s: extraction applied (%d written, %d rejected)",
                batch.group_id,
                written,
                rejected,
            )
        return written, rejected

    async def _render(
        self, group_id: GroupId, rows
    ) -> tuple[dict[int, uuid.UUID], str, list[SourceLine]]:
        """Render source-numbered lines and a stable code for each exact account."""

        codes: dict[int, uuid.UUID] = {}
        by_platform: dict[AccountId, int | None] = {}
        by_id: dict[uuid.UUID, int] = {}
        lines: list[SourceLine] = []
        roster: list[str] = []
        bot_accounts: set[AccountId] = set()

        rows = tuple(rows)
        users = {row.sender.account_id for row in rows}
        users.update(account for row in rows for account, _ in row.mentions)
        reading = await self._ids.extraction_identities(group_id, sorted(users))
        accounts_by_user = {account.platform_user_id: account for account in reading.accounts}
        aliases_by_account: dict[uuid.UUID, list[str]] = {}
        for alias in reading.aliases:
            assert alias.target_account_id is not None
            aliases_by_account.setdefault(alias.target_account_id, []).append(alias.alias_text)
        names: dict[str, dict[uuid.UUID, object]] = {}
        for account, alias in reading.names:
            names.setdefault(alias, {})[account.id] = account
        unique_names = [
            (alias, next(iter(accounts.values())))
            for alias, accounts in names.items()
            if len(accounts) == 1
        ]

        def assign_identity(account, display: str) -> int:
            if account.id in by_id:
                code = by_id[account.id]
                by_platform.setdefault(account.platform_user_id, code)
                return code
            code = len(codes) + 1
            codes[code] = account.id
            by_id[account.id] = code
            by_platform[account.platform_user_id] = code
            aliases = list(
                dict.fromkeys(
                    alias for alias in aliases_by_account.get(account.id, ()) if alias != display
                )
            )[:4]
            shown = display or (aliases[0] if aliases else "成员")
            roster.append(
                shown
                + sysmark(str(code))
                + (f"（也叫：{'、'.join(defang(alias) for alias in aliases)}）" if aliases else "")
            )
            return code

        def assign_account(account_id: AccountId, display: str) -> int | None:
            if account_id in by_platform:
                return by_platform[account_id]
            account = accounts_by_user.get(account_id)
            if account is None:
                by_platform[account_id] = None
                return None
            return assign_identity(account, display)

        for archived in rows:
            platform_id = archived.sender.account_id
            name = archived.sender.display_name
            self_account = archived.self_id
            if self_account:
                bot_accounts.add(self_account)
            own = archived.author_kind is AuthorKind.BOT
            author_account_id: uuid.UUID | None = None
            if own:
                bot_accounts.add(platform_id)
                by_platform[platform_id] = BOT_DISPLAY_NUMBER
                speaker_no: int | None = BOT_DISPLAY_NUMBER
            else:
                speaker_no = assign_account(platform_id, name)
                if speaker_no is not None:
                    author_account_id = codes[speaker_no]

            mentions = archived.mentions
            for account_id, display in mentions:
                key = account_id
                if key in bot_accounts or key == self_account:
                    by_platform[key] = BOT_DISPLAY_NUMBER
                else:
                    assign_account(key, display)

            text = number_at_mentions(
                archived.text,
                list(mentions),
                lambda account: by_platform.get(account),
            )
            if not text:
                continue

            evidence = ""
            targets: list[SnapshotTarget] = []
            if not own and archived.event_type == "message":
                evidence = _eligible_text(
                    archived,
                    lambda account: by_platform.get(account),
                )
                if author_account_id is not None:
                    targets.append(SnapshotTarget(author_account_id, "author"))
                for account_id, _display in mentions:
                    code = by_platform.get(account_id)
                    if code is None or code == BOT_DISPLAY_NUMBER:
                        continue
                    targets.append(SnapshotTarget(codes[code], "mention", sysmark(str(code))))
                for alias, account in unique_names:
                    if alias not in evidence:
                        continue
                    code = assign_identity(account, alias)
                    targets.append(SnapshotTarget(codes[code], "alias", alias))

            unique_targets = tuple(
                {
                    (target.account_id, target.reason, target.marker): target for target in targets
                }.values()
            )
            marker = sysmark(str(speaker_no)) if speaker_no is not None else ""
            who = (name or "机器人") + marker
            ordinal = len(lines) + 1
            lines.append(
                SourceLine(
                    ordinal=ordinal,
                    event_id=archived.raw_event_id,
                    event_type=archived.event_type,
                    own=own,
                    text=(
                        f"{sysmark(f'来源:{ordinal}')} "
                        f"{sysmark(self._clock.format(archived.occurred_at))} {who}: {text}"
                    ),
                    evidence_text=evidence,
                    author_account_id=author_account_id,
                    targets=unique_targets,
                )
            )
        return codes, "\n".join(roster), lines

    # -- forgetting --------------------------------------------------------
    async def decay(self, group_id: GroupId, *, fence: Job | None = None) -> tuple[int, int, int]:
        """Let go of what nothing has confirmed lately. Returns facts, names, episodes.

        Free - each store uses set-based database operations and no model call. It runs
        daily because stale facts sit in every reply prompt and stale episodes keep
        participating in semantic recall until they are retired.
        """
        stable, fast = decay_classes(self._bundle.predicates)
        half = self._bundle.predicates.half_life_days
        facts = await self._mem.decay(
            group_id,
            stable=stable,
            fast=fast,
            stable_days=half.stable,
            default_days=half.default,
            fast_days=half.fast,
            keep_predicates=(NOTE,),
            fence=fence,
        )
        names = await self._ids.decay_aliases(
            group_id,
            unused_days=self._m.alias_unused_days,
            joke_days=self._m.temporary_alias_days,
            fence=fence,
        )
        episodes = await self._eps.decay(group_id, ttl_days=self._m.episode_ttl_days, fence=fence)
        if facts or names or episodes:
            log.info(
                "group %s: retired %d facts, %d unconfirmed names and %d episodes",
                group_id,
                facts,
                names,
                episodes,
            )
        return facts, names, episodes

    # -- vectors -----------------------------------------------------------
    async def embed(self, group_id: GroupId, *, fence: Job | None = None) -> int:
        """Fill in vectors for episodes that have none. Incremental, never a full pass.

        One page per job. A page that comes back full means more is waiting, so the
        next page is queued as its own job rather than taken here: the lease stays
        short, and a backlog the size of the whole store (a model switch) drains
        across jobs at the queue's pace. The dedup index does not collapse the
        resubmit - this job is running, not pending.
        """
        if await self._budget.exceeded():
            raise Deferred("daily budget exhausted", delay=timedelta(hours=1))
        page_size = self._embed.batch_size
        todo = await self._vec.unembedded_episodes(group_id, limit=page_size)
        if not todo:
            return 0
        vecs = await self._embed.embed([summary for _, summary in todo], group_id=group_id)
        # strict: the vectors are paired with the episodes by position, so a backend
        # that answered with a different number of them would file each summary under
        # somebody else's vector rather than fail.
        if len(todo) != len(vecs):
            raise ValueError("embedding response has the wrong number of vectors")
        stored = 0
        for (eid, _), v in zip(todo, vecs, strict=True):
            stored += await self._vec.put_episode(
                group_id=group_id, episode_id=eid, embedding=v, fence=fence
            )
        if len(todo) == page_size:
            await self._queue.submit(JobType.EMBED, {"group_id": group_id}, fence=fence)
        log.info("group %s: embedded %d episodes", group_id, stored)
        return stored
