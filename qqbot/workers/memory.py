"""The background executor: it does what is on the queue.

The asynchronous chain:

    EXTRACT_MEMORY -> read a stretch of transcript, produce candidates by function call
    CONSOLIDATE    -> validate the candidates and write them into L1/L3
    EMBED          -> fill in vectors for new episodes

It runs inside one process, but all of its state is in the database: leases, retries and
backoff are the queue's guarantees. So this loop can be killed at any moment and started
again, and everything it does is re-entrant.
"""

from __future__ import annotations

import asyncio
import logging
import uuid
from datetime import datetime, timedelta

import openai

from ..core.archive import archive_author, archive_mentions, archive_sender, archive_text
from ..core.budget import BUDGET
from ..core.member_numbers import BOT_DISPLAY_NUMBER
from ..core.segments import number_at_mentions
from ..domain.archive import AuthorKind
from ..providers import providers
from ..providers.base import QuotaExhausted
from ..db import repo
from ..repositories import (
    EpisodeRepository, EventRepository, IdentityRepository, JobQueue, MemoryRepository,
    VectorRepository,
)
from ..repositories.job import Job, JobType
from ..services import ExtractionInput, MemoryConsolidator, MemoryExtractor
from ..services.memory_extractor import SourceLine, decay_classes
from ..services.context_builder import NOTE
from ..settings import Settings, config
from ..util import defang, fmt_when, now_local, sysmark, why

log = logging.getLogger("qqbot.worker")

#: How many rounds of pending candidates one consolidation job settles before
#: handing back. Each round settles everything it fetched, so the loop ends when
#: the queue is empty; the cap only bounds a job whose candidates keep arriving.
CONSOLIDATE_ROUNDS = 50

#: Episodes embedded per job. A backlog wider than this - a model switch, which
#: invalidates every stored vector at once - is paged across successive jobs
#: rather than held under one lease.
EMBED_PAGE = 200

#: Failures the outside world causes: a slow or refusing model, a used-up
#: allowance. Logged in one line, because a traceback through the client library
#: says nothing the message does not. Anything else is this code's own fault and
#: gets the traceback.
EXPECTED_FAILURES = (TimeoutError, QuotaExhausted, openai.APIError)


# Fact lifetimes come from the predicate class tables in memory_extractor - the kind of
# fact is the primary axis of forgetting, evidence a bounded multiplier on top. See
# MemoryRepository.decay for the reasoning. How long an unused name survives is
# config (memory.alias_unused_days / joke_unused_days).

#: Backoff after a failure. Exponential, capped at an hour - background work is not
#: urgent, and what is urgent is not burning money on retries.
BACKOFF = (timedelta(minutes=1), timedelta(minutes=5), timedelta(minutes=30),
           timedelta(hours=1))


def _transcript(lines: list[SourceLine]) -> str:
    """The batch as the model reads it. Written once, because the extractor and the
    validator have to be looking at the same text: the validator's one real check is that
    a quote appears in the transcript word for word, and two ways of building it is how
    that check starts rejecting things that were said."""
    return "\n".join(ln.text for ln in lines)


class MemoryWorker:
    def __init__(self, cfg: Settings, *, worker_id: str | None = None) -> None:
        self._cfg = cfg
        #: The memory mechanism's settings and extraction prompt are frozen at
        #: construction. A reload changing either is rejected until restart.
        self._m = cfg.memory
        # Process-unique by default: the job queue's locked_by guards compare this
        # id, and during a deploy overlap two processes sharing a fixed name could
        # accept each other's stale fail()/done() calls. Tests pass a fixed id.
        self._queue = JobQueue(worker_id or f"memory-{uuid.uuid4().hex[:6]}")
        self._ids = IdentityRepository()
        self._mem = MemoryRepository()
        self._eps = EpisodeRepository()
        self._events = EventRepository()
        self._extractor = MemoryExtractor(cfg)
        self._consolidator = MemoryConsolidator(self._ids, self._mem, self._eps)
        # The bundle's own embedding backend: vectors are stored under the model that
        # produced them, so the store is keyed by the backend answering right now.
        self._embed = providers().embedding
        self._vec = VectorRepository(self._embed.name)

    async def run_forever(self, *, idle: float = 5.0) -> None:
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
        """Do one job. Returns whether there was one to do."""
        # The lease (memory.job_lease_min) has to outlast a full drain - several
        # background model calls - or a deploy overlap reclaims the running job
        # and pays for the same transcript twice. Its cost is only that a crashed
        # worker's job waits this long to be retried.
        job = await self._queue.claim(
            lease=timedelta(minutes=self._m.job_lease_min))
        if job is None:
            return False
        try:
            await self._dispatch(job)
        except Exception as e:
            log.warning("job %s (%s) failed: %s", job.id, job.job_type, why(e),
                        exc_info=not isinstance(e, EXPECTED_FAILURES))
            await self._queue.fail(
                job, why(e), backoff=BACKOFF[min(job.retry_count, len(BACKOFF) - 1)])
        else:
            await self._queue.done(job.id)
        return True

    async def _dispatch(self, job: Job) -> None:
        match job.job_type:
            case JobType.EXTRACT_MEMORY:
                await self.extract(int(job.payload["group_id"]))
            case JobType.CONSOLIDATE:
                await self.consolidate(int(job.payload["group_id"]))
            case JobType.EMBED:
                await self.embed(int(job.payload["group_id"]))
            case JobType.DECAY:
                await self.decay(int(job.payload["group_id"]))
            case _:
                log.info("ignoring unimplemented job type: %s", job.job_type)

    # -- extraction --------------------------------------------------------
    async def _next_unread(self, group_id: int) -> list:
        """The oldest unread chunk, in ingest (created_at) order.

        The rows come from EventRepository.next_unread, which owns the watermark
        predicate (and explains the ingest-order axis); what happens here is
        policy over them. Cut at a conversation gap when more remains (see
        _gap_cut), so batch boundaries fall where conversations end rather than
        mid-topic - which is what keeps one episode from being split into two
        half-known ones.
        """
        rows = await self._events.next_unread(group_id, limit=self._m.extract_window)
        if len(rows) < self._m.extract_window:
            return rows
        rows = self._gap_cut(rows)
        # A truncated chunk must never end mid-tie: the watermark is a bare
        # created_at and the unread filter is strictly greater, so a row sharing
        # the last row's timestamp but left outside the chunk would be marked read
        # without ever being read - silently, forever. Ties across separate insert
        # transactions are near-impossible (NOW() at microseconds), which is why
        # this trims rather than carries an id in the watermark; if the whole
        # chunk somehow shares one timestamp, it is taken whole.
        last = rows[-1]["created_at"]
        trimmed = [r for r in rows if r["created_at"] != last]
        return trimmed or rows

    def _gap_cut(self, rows: list) -> list:
        """Trim a full chunk back to the last conversation gap in its tail half.

        Only a full chunk is cut - a partial one already ends where the group
        stopped talking. The cut point must be a created_at prefix (the watermark
        axis), so the gap is measured between created_at-consecutive rows using
        their occurred_at - identical in practice, see _next_unread.
        """
        gap = timedelta(minutes=self._m.batch_gap_min)
        for i in range(len(rows) - 1, self._m.extract_window // 2, -1):
            if rows[i]["occurred_at"] - rows[i - 1]["occurred_at"] >= gap:
                return rows[:i]
        return rows

    async def _replay(self, group_id: int, *, ending_at: uuid.UUID | None,
                      size: int) -> list:
        """Reproduce exactly the batch some earlier extraction read.

        Validation runs later, as its own job, and re-fetching "the unread
        messages" then reads past a watermark that has moved: quotes from the
        already-read rows could no longer be found, so records that were correctly
        quoted would come back rejected as if the model had invented them. The
        anchor (the batch's last row) plus the stored batch size name the exact
        set.
        """
        return await self._events.batch_ending_at(group_id, anchor=ending_at, size=size)

    async def _known(self, group_id: int, codes: dict[int, uuid.UUID]) -> str:
        """What is already on record for this group, for the model to work against.

        Without it the extractor re-derives everything from scratch on every batch, and
        since it words each answer slightly differently, storage sees a new object every
        time and supersedes the old one - so evidence never accumulates, and the branch
        that raises confidence when something is confirmed again never runs.

        Rendered close to the tool arguments rather than as prose, so that "already
        recorded" is recognisable as the same thing the model is about to propose.
        """
        subject = await self._ids.group_entity(group_id)
        out: list[str] = []

        # The owner's hand-written group background, the same text the reply
        # path reads as fixed material. Injected here because understanding is
        # upstream of extraction: the slang, the standing relationships, who the
        # other bots are. Read-only like the per-member notes - the prompt's
        # fixed-material section forbids deriving candidates from it, and the
        # Validator's quote rule holds mechanically (it is not a transcript
        # line, so nothing quoted from it can validate).
        if fixed := config().persona_for(str(group_id)).group_knowledge.strip():
            out.append(f"本群固定资料：\n{fixed}")

        if group_facts := await self._mem.current_facts(group_id, [subject]):
            out.append("本群：")
            # defang stored values on render, like every other path that reads
            # old rows back into a prompt.
            out += [f"- {f.predicate}"
                    + (f" {defang(str(f.object_key))}" if f.object_key else "")
                    + f" = {defang(str(f.object_value))}"
                    for f in sorted(group_facts,
                                    key=lambda f: (f.predicate, f.object_key or ""))]

        by_entity = {eid: code for code, eid in codes.items()}
        facts = await self._mem.current_facts(group_id, list(by_entity))
        per: dict[int, list[str]] = {}
        notes: dict[int, str] = {}
        for f in facts:
            code = by_entity.get(f.subject_entity_id)
            if code is None:
                continue
            if f.predicate == NOTE:
                # Injected read-only, under its own label: an owner's note is
                # often the comprehension key (a real name, a schedule) that
                # resolves references nothing else can. The prompt forbids
                # deriving candidates from it, and the Validator's quote rule
                # holds the line mechanically - a note is not a transcript
                # line, so nothing quoted from it can ever validate.
                notes[code] = str(f.object_value)
                continue
            per.setdefault(code, []).append(
                f"{f.predicate} = {defang(str(f.object_value))}")
        for code in sorted(per.keys() | notes.keys()):
            bits = "；".join(sorted(per.get(code, [])))
            note = f"备注：{notes[code]}" if code in notes else ""
            joined = "；".join(x for x in (bits, note) if x)
            # The same reserved account-code form the roster and the transcript
            # lines wear, so "already recorded" is recognisably about the same
            # person the model is about to cite.
            out.append(f"{sysmark(str(code))}：{joined}")

        # Episodes are appended, never superseded - one thing that happened does not
        # overturn another - so a conversation read twice becomes two near-identical
        # records of the same event unless the model is told the first one exists.
        seen: list[str] = []
        for eid in dict.fromkeys(codes.values()):
            for ep in await self._eps.involving(group_id, eid, limit=self._m.known_episodes):
                if ep.summary not in seen:
                    seen.append(ep.summary)
        if seen:
            out.append("已记过的事：")
            out += [f"- {defang(s)}" for s in seen[:self._m.known_episodes]]
        return "\n".join(out)

    async def extract(self, group_id: int) -> int:
        """Drain the unread transcript in gap-aligned chunks. One model call each.

        The nightly single event point (schedule.nightly_cron): the whole day is
        read here, oldest first, each chunk cut where a conversation ended. Two
        gates per pass, in the order that costs least to check: the day's budget,
        then whether enough is unread to be worth a pass at all (memory.drain_floor).
        This is the only place that knows a model call is about to happen, so it is
        the only place where "nothing has been said since last time" can reliably
        stop one.
        """
        total = 0
        for _ in range(self._m.max_passes):
            if await BUDGET.exceeded(self._cfg.budget.daily_cny_cap):
                # The same cap that silences replies gates learning too: background
                # work is the least urgent spend there is.
                log.info("group %s: daily cap reached, extraction waits for "
                         "tomorrow", group_id)
                break

            unread, _newest = await self._events.unread_since_extract(group_id)
            if not unread or unread < self._m.drain_floor:
                if not total:
                    log.info("group %s: %d unread, not worth a pass", group_id, unread)
                break

            rows = await self._next_unread(group_id)
            if not rows:
                break
            total += await self._extract_pass(group_id, rows)
        if total:
            log.info("group %s: %d candidates extracted", group_id, total)
        return total

    async def _extract_pass(self, group_id: int, rows: list) -> int:
        """One chunk through the model. Returns how many candidates were staged."""
        # The batch anchor is the created_at-last row - the row the watermark will
        # land on - captured before the rendering sort below reorders for reading.
        anchor = rows[-1]["id"]
        rows = sorted(rows, key=lambda r: (r["occurred_at"], r["id"]))

        codes, roster, lines = await self._render(group_id, rows)
        if not codes:
            # Nobody here the extractor may learn from (say, only the bot spoke).
            # No paid call happens, the read is complete, so the watermark moves -
            # a batch of nothing must not be re-read forever.
            await repo.mark_extracted(group_id, max(r["created_at"] for r in rows))
            return 0

        cands = await self._extractor.extract(ExtractionInput(
            group_id=group_id, transcript=_transcript(lines), roster=roster,
            account_codes=codes, lines=tuple(lines), source_event_id=anchor,
            batch_size=len(rows),
            known=await self._known(group_id, codes),
            self_names="、".join(self._cfg.trigger.nicknames),
        ))
        # The watermark moves for what was read, not for what was learned from it: a
        # batch of nothing but stickers is still a batch nobody should pay to read
        # twice. But it moves only once the paid read *succeeded*: marked first, a
        # timed-out extraction would leave the job's retry facing "nothing unread"
        # and the batch skipped forever. Marked before staging on purpose: the model
        # call is the fragile, expensive step; if staging fails the loss is one
        # batch's candidates, not a second charge for the same transcript.
        await repo.mark_extracted(group_id, max(r["created_at"] for r in rows))
        if not cands:
            log.info("group %s: nothing worth extracting in this batch", group_id)
            return 0
        await self._mem.stage(cands)
        # Queued per pass, not once per drain: a later pass failing mid-drain must
        # not strand what the earlier passes already staged - pending candidates
        # would otherwise sit until some future drain happened to stage more. The
        # pending-dedup index keeps repeat submits to one job either way.
        await self._queue.submit(JobType.CONSOLIDATE, {"group_id": group_id},
                                 priority=2)
        return len(cands)

    async def _render(
        self, group_id: int, rows
    ) -> tuple[dict[int, uuid.UUID], str, list[SourceLine]]:
        """Lay the raw events out as a transcript, with a code per account.

        The code is a position within this batch, not a database id: the model only has
        to tell these people apart from each other, and handing it a UUID would cost
        tokens for nothing.

        Each line keeps the event it came from, so a record is attributed to the message
        that produced it rather than to the batch. The evidence chain depends on that
        per-line citation to answer how many *different* people used a name - the only
        route by which a name the model merely observed can ever be confirmed. If every
        citation pointed at one message, the count would always be one and no observed
        nickname could reach the prompt.

        An account with no identity is the bot itself: every member gets an entity
        at ingest, and nothing ever creates one for the bot. Its lines render with
        the self marker after the name, codeless and outside the roster, and stay
        out of evidence by construction: their SourceLine is own=True, source_of
        skips it, and a candidate quoting one fails validation. Dropping them
        instead would keep the self-loop just as firmly shut, but would feed the
        extractor one-sided conversations in which "like you said" points at a
        reply that does not exist. The model reads both halves; only the members'
        half counts.
        """
        codes: dict[int, uuid.UUID] = {}
        by_account: dict[str, int | None] = {}
        lines: list[SourceLine] = []
        roster: list[str] = []
        bot_accounts: set[str] = set()

        async def assign_account(account: str, display: str) -> int | None:
            if account in by_account:
                return by_account[account]
            eid = await self._identity_of(account)
            if eid is None:
                by_account[account] = None
                return None
            code = len(codes) + 1
            by_account[account] = code
            codes[code] = eid
            akas = await self._known_names(group_id, eid, display)
            shown = display or (akas[0] if akas else "成员")
            roster.append(
                shown
                + sysmark(str(code))
                + (
                    f"（也叫：{'、'.join(defang(alias) for alias in akas)}）"
                    if akas
                    else ""
                )
            )
            return code

        for r in rows:
            uid = r["platform_user_id"]
            payload = r["payload"] or {}
            name = archive_sender(payload, fallback=uid)
            if not uid:
                continue
            self_account = str(payload.get("self_id") or "")
            if self_account:
                bot_accounts.add(self_account)
            author = archive_author(payload, uid)
            legacy = author is None
            own = author is AuthorKind.BOT
            if own:
                bot_accounts.add(uid)
                by_account[uid] = BOT_DISPLAY_NUMBER
                speaker_no: int | None = BOT_DISPLAY_NUMBER
            else:
                speaker_no = await assign_account(uid, name)
                if legacy and speaker_no is None:
                    # Legacy rows had no authorship discriminator. Preserve their old
                    # classification only there; new member rows never take this branch.
                    own = True
                    bot_accounts.add(uid)
                    by_account[uid] = BOT_DISPLAY_NUMBER
                    speaker_no = BOT_DISPLAY_NUMBER
                    log.debug("legacy archive row %s inferred as bot-authored", r["id"])
            mentions = archive_mentions(
                payload,
                self_id=self_account,
                self_name=(self._cfg.trigger.nicknames[0]
                           if self._cfg.trigger.nicknames else "机器人"),
            )
            for account, display in mentions:
                if account in bot_accounts or account == self_account:
                    by_account[account] = BOT_DISPLAY_NUMBER
                else:
                    await assign_account(account, display)
            text = number_at_mentions(
                archive_text(r), mentions, lambda account: by_account.get(account)
            )
            if text:
                marker = sysmark(str(speaker_no)) if speaker_no is not None else ""
                who = (name or "机器人") + marker
                # The send time leads each line, exactly as the reply prompt stamps its
                # history: a batch can span days (the idle floor), and without stamps
                # two conversations hours apart read as one and get merged into one
                # episode. occurred_at is fixed, so a batch re-rendered at consolidation
                # time is byte-identical to what the extractor read.
                lines.append(SourceLine(
                    event_id=r["id"],
                    own=own,
                    text=f"{sysmark(fmt_when(r['occurred_at']))} {who}: {text}"))
        return codes, "\n".join(roster), lines

    async def _identity_of(self, user_id: str) -> uuid.UUID | None:
        acc = await self._ids.account_of("qq", user_id)
        return acc.entity_id if acc else None

    async def _known_names(self, group_id: int, eid: uuid.UUID,
                           current: str) -> list[str]:
        """Usable names for this account besides its current card, for the roster.

        The comprehension key the transcript alone cannot provide: with bare
        current cards, every in-chat nickname is a guess, and the extractor's
        no-guessing rule then drops records it could have filed with certainty.
        A lookup failure degrades to a bare roster line, never to a lost batch.
        """
        try:
            aliases = await self._ids.aliases_for(group_id, eid)
        except Exception as e:
            log.warning("group %s: alias lookup for the roster failed: %s",
                        group_id, why(e))
            return []
        out = [a.alias_text for a in aliases
               if a.is_usable and a.alias_text != current]
        return list(dict.fromkeys(out))[:4]

    # -- consolidation -----------------------------------------------------
    async def consolidate(self, group_id: int, *, when: datetime | None = None) -> tuple[int, int]:
        """Validate what is pending and write what survives.

        Candidates are grouped by the batch they came from, and each group is checked
        against that batch reproduced from its anchor. Pending work can span several
        extractions - one failure and the next run picks up both - and a quote from the
        older one is not in the newer one's messages, so validating them all against a
        single window would reject correctly-quoted records as inventions.

        Pending candidates are read a page at a time, and the job keeps going until a
        page comes back empty: a nightly drain of several chunks stages more than
        one page, and a job that settled only the first would leave the rest waiting
        for whatever extraction happened next.
        """
        written = rejected = 0
        for _ in range(CONSOLIDATE_ROUNDS):
            cands = await self._mem.pending(group_id)
            if not cands:
                break
            # Anchor and size together name a batch: two batches can end at the
            # same row with different widths, and the account codes are positions
            # within one render.
            batches: dict[tuple[uuid.UUID | None, int], list] = {}
            for c in cands:
                batches.setdefault((c.batch_event_id, c.batch_size), []).append(c)

            for (anchor, size), batch in batches.items():
                rows = await self._replay(group_id, ending_at=anchor, size=size)
                rows = sorted(rows, key=lambda r: (r["occurred_at"], r["id"]))
                codes, _roster, lines = await self._render(group_id, rows)
                # Members' lines only, as the extractor's source_of sees them: the
                # bot's own lines are context, never evidence, and a quote the bot
                # echoed back must not count as found twice.
                w, r = await self._consolidator.consolidate(
                    batch, group_id=group_id, codes=codes,
                    lines=tuple(ln.text for ln in lines if not ln.own),
                    when=when or now_local(),
                    # Records are dated by the conversation, not by tonight's write.
                    occurred={r["id"]: r["occurred_at"] for r in rows},
                )
                written += w
                rejected += r
        if not written and not rejected:
            return 0, 0
        log.info("group %s: consolidated %d, rejected %d", group_id, written, rejected)
        if written:
            # Episodes are retrieved by participant first and by similarity second, so a
            # new one is reachable immediately and its vector only sharpens the ranking.
            # Queued rather than computed here for that reason: the embedding is an
            # improvement, not a prerequisite, and it should not be able to fail the
            # consolidation that produced it.
            await self._queue.submit(JobType.EMBED, {"group_id": group_id})
        return written, rejected

    # -- forgetting --------------------------------------------------------
    async def decay(self, group_id: int) -> tuple[int, int, int]:
        """Let go of what nothing has confirmed lately. Returns facts, names, episodes.

        Free - each store uses set-based database operations and no model call. It runs
        daily because stale facts sit in every reply prompt and stale episodes keep
        participating in semantic recall until they are retired.
        """
        stable, fast = decay_classes()
        half = config().predicates.half_life_days
        facts = await self._mem.decay(
            group_id,
            stable=stable, fast=fast,
            stable_days=half.stable, default_days=half.default, fast_days=half.fast,
            keep_predicates=(NOTE,))
        names = await self._ids.decay_aliases(
            group_id, unused_days=self._m.alias_unused_days,
            joke_days=self._m.joke_unused_days)
        episodes = await self._eps.decay(
            group_id, ttl_days=self._m.episode_ttl_days)
        if facts or names or episodes:
            log.info(
                "group %s: retired %d facts, %d unconfirmed names and %d episodes",
                group_id, facts, names, episodes,
            )
        return facts, names, episodes

    # -- vectors -----------------------------------------------------------
    async def embed(self, group_id: int) -> int:
        """Fill in vectors for episodes that have none. Incremental, never a full pass.

        One page per job. A page that comes back full means more is waiting, so the
        next page is queued as its own job rather than taken here: the lease stays
        short, and a backlog the size of the whole store (a model switch) drains
        across jobs at the queue's pace. The dedup index does not collapse the
        resubmit - this job is running, not pending.
        """
        if await BUDGET.exceeded(self._cfg.budget.daily_cny_cap):
            # Not dropped: nothing else queues an embed for episodes already
            # written, so the job comes back after the longest backoff and keeps
            # coming back until the ledger day has rolled over.
            await self._queue.submit(JobType.EMBED, {"group_id": group_id},
                                     delay=BACKOFF[-1])
            return 0
        todo = await self._vec.unembedded_episodes(group_id, limit=EMBED_PAGE)
        if not todo:
            return 0
        if len(todo) == EMBED_PAGE:
            await self._queue.submit(JobType.EMBED, {"group_id": group_id})
        vecs = await self._embed.embed(
            [summary for _, summary in todo], cfg=self._cfg.capabilities.embedding,
            group_id=str(group_id))
        # strict: the vectors are paired with the episodes by position, so a backend
        # that answered with a different number of them would file each summary under
        # somebody else's vector rather than fail.
        stored = 0
        for (eid, _), v in zip(todo, vecs, strict=True):
            stored += await self._vec.put_episode(
                group_id=group_id, episode_id=eid, embedding=v)
        log.info("group %s: embedded %d episodes", group_id, stored)
        return stored
