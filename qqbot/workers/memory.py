"""The background executor: it does what is on the queue.

The asynchronous chain (design doc 53):

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

from ..core.budget import BUDGET
from ..db import pool, repo
from ..providers.embedding import EmbeddingModel
from ..repositories import (
    EpisodeRepository, IdentityRepository, JobQueue, MemoryRepository, VectorRepository,
)
from ..repositories.job import Job, JobType
from ..services import ExtractionInput, MemoryConsolidator, MemoryExtractor
from ..services.memory_extractor import (DEFAULT_DAYS, FAST_DAYS, FAST_PREDICATES,
                                          STABLE_DAYS, STABLE_PREDICATES, SourceLine)
from ..services.context_builder import NOTE
from ..settings import EXTRACT_WINDOW, Settings, config, ptext
from ..util import defang, fmt_when, now_local, sysmark, why

log = logging.getLogger("qqbot.worker")

#: How much transcript one extraction chunk holds at most. Enough to see whole
#: conversations, not so much that a single call becomes expensive. The number lives
#: in settings (the /relearn reset keeps this many unread); this is the name the
#: worker code and the tests use.
WINDOW = EXTRACT_WINDOW
#: A full chunk is trimmed back to the last conversation gap of at least this long
#: in its tail half, so batch boundaries fall where conversations end.
BATCH_GAP = timedelta(minutes=30)
#: Below this many unread messages a drain does not bother: a pass pays for the
#: rules, the schemas and the known facts before reading a line, and a handful of
#: messages simply waits for tomorrow's drain. /relearn overrides (force).
DRAIN_FLOOR = 20
#: Passes one drain may run - 610 to 1200 messages a night with gap-cut chunks.
#: It CAN bind before the budget does; accepted: a group sustaining more than that
#: every day has outgrown the memory budget itself, and the backlog carries over
#: rather than being skipped.
MAX_PASSES = 10
#: What WINDOW was before the nightly redesign. Candidates staged back then carry
#: no batch_size, and their batches must be replayed at the width they were made
#: with - replaying at the new width prepends rows the model never saw, which
#: shifts every account code and files records against the wrong people.
LEGACY_WINDOW = 60
#: How many already-recorded episodes the extractor is reminded of. Enough to recognise
#: the conversation it is reading as one it has already written down, not so many that the
#: reminder costs more than the batch.
KNOWN_EPISODES = 8


def transcript_legend() -> str:
    """What the extraction prompt is told about the markers in a transcript.

    Composed here rather than in the service, because the pieces belong to the layer
    that renders messages and the service must not reach up into it - a worker may
    know about both. A function rather than a constant so prompt overrides are read
    when the worker is built. The extract_legend_note paragraph is one the reply path
    does not need: these markers are annotations this system wrote, not things a
    member typed - without it the model records that the group can send pictures.
    """
    return ptext("legend") + "\n\n" + ptext("extract_legend_note")


#: Fact lifetimes come from the predicate class tables in memory_extractor - the kind of
#: fact is the primary axis of forgetting, evidence a bounded multiplier on top. See
#: MemoryRepository.decay for the reasoning and the citations.
#: How long a name the model guessed at survives without being used again.
ALIAS_UNUSED_DAYS = 30.0
#: And how long one it marked as a joke does. Most are true for an afternoon.
JOKE_UNUSED_DAYS = 7.0

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
    def __init__(
        self, cfg: Settings, *, embed: EmbeddingModel, worker_id: str | None = None,
    ) -> None:
        self._cfg = cfg
        # Process-unique by default: the job queue's locked_by guards compare this
        # id, and during a deploy overlap two processes sharing a fixed name could
        # accept each other's stale fail()/done() calls. Tests pass a fixed id.
        self._queue = JobQueue(worker_id or f"memory-{uuid.uuid4().hex[:6]}")
        self._ids = IdentityRepository()
        self._mem = MemoryRepository()
        self._eps = EpisodeRepository()
        self._extractor = MemoryExtractor(cfg, legend=transcript_legend())
        self._consolidator = MemoryConsolidator(self._ids, self._mem, self._eps)
        self._embed = embed
        self._vec = VectorRepository(embed.name)

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
        # Half an hour, not the default ten minutes: a full drain is up to
        # MAX_PASSES background model calls and can outlive a short lease, and a
        # deploy overlap would then reclaim the running job and pay for the same
        # transcript twice. The cost of the longer lease is only that a crashed
        # worker's job waits this long to be retried - nobody is watching at 02:30.
        job = await self._queue.claim(lease=timedelta(minutes=30))
        if job is None:
            return False
        try:
            await self._dispatch(job)
        except Exception as e:
            log.warning("job %s (%s) failed: %s", job.id, job.job_type, why(e))
            await self._queue.fail(
                job, why(e), backoff=BACKOFF[min(job.retry_count, len(BACKOFF) - 1)])
        else:
            await self._queue.done(job.id)
        return True

    async def _dispatch(self, job: Job) -> None:
        match job.job_type:
            case JobType.EXTRACT_MEMORY:
                await self.extract(int(job.payload["group_id"]),
                                   force=bool(job.payload.get("force")))
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

        Oldest-first from the watermark, because the nightly drain reads a whole
        day in several passes: newest-first would mark everything read after the
        first pass and silently skip the rest. Cut at a conversation gap when more
        remains (see _gap_cut), so batch boundaries fall where conversations end
        rather than mid-topic - which is what keeps one episode from being split
        into two half-known ones.

        created_at, not occurred_at, on both the filter and the order: the
        watermark lives on created_at, and a batch must be a contiguous prefix of
        that axis or marking its end would skip rows never read. The two axes
        agree except for out-of-order redelivery; rendering re-sorts by
        occurred_at so the model still reads conversation order.

        The watermark predicate here is the same one db.repo.unread_since_extract
        gates on; a change to one must reach the other, or the gate and the fetch
        disagree about what "unread" means.
        """
        rows = await pool().fetch(
            """SELECT id, platform_user_id, occurred_at, created_at, payload, plain_text
                 FROM raw_event
                WHERE group_id=$1 AND event_type='message'
                  AND created_at > COALESCE(
                      (SELECT last_extract_at FROM group_state WHERE group_id=$1),
                      'epoch')
                ORDER BY created_at ASC, id ASC LIMIT $2""",
            group_id, WINDOW,
        )
        if len(rows) < WINDOW:
            return list(rows)
        rows = self._gap_cut(list(rows))
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

    @staticmethod
    def _gap_cut(rows: list) -> list:
        """Trim a full chunk back to the last conversation gap in its tail half.

        Only a full chunk is cut - a partial one already ends where the group
        stopped talking. The cut point must be a created_at prefix (the watermark
        axis), so the gap is measured between created_at-consecutive rows using
        their occurred_at - identical in practice, see _next_unread.
        """
        for i in range(len(rows) - 1, WINDOW // 2, -1):
            if rows[i]["occurred_at"] - rows[i - 1]["occurred_at"] >= BATCH_GAP:
                return rows[:i]
        return rows

    async def _replay(self, group_id: int, *, ending_at: uuid.UUID | None,
                      size: int | None) -> list:
        """Reproduce exactly the batch some earlier extraction read.

        Validation runs later, as its own job, and re-fetching "the unread
        messages" then reads past a watermark that has moved: quotes from the
        already-read rows could no longer be found, so records that were correctly
        quoted would come back rejected as if the model had invented them. The
        anchor (the batch's last row) plus the stored batch size name the exact
        set; a candidate from before the size column was added falls back to the
        fixed window its batch was made with. Ordered by (created_at, id) at both
        ends because ties must not reorder between the read and the replay.
        """
        if size is None:
            # Legacy batches were the newest LEGACY_WINDOW by occurred_at ending at
            # the anchor; reproduce them the way they were made, at the width they
            # were made with.
            return list(reversed(await pool().fetch(
                """SELECT id, platform_user_id, occurred_at, created_at, payload,
                          plain_text
                     FROM raw_event
                    WHERE group_id=$1 AND event_type='message'
                      AND ($3::uuid IS NULL
                           OR (occurred_at, id)
                               <= (SELECT occurred_at, id FROM raw_event WHERE id=$3))
                    ORDER BY occurred_at DESC, id DESC LIMIT $2""",
                group_id, LEGACY_WINDOW, ending_at,
            )))
        return list(reversed(await pool().fetch(
            """SELECT id, platform_user_id, occurred_at, created_at, payload, plain_text
                 FROM raw_event
                WHERE group_id=$1 AND event_type='message'
                  AND ($3::uuid IS NULL
                       OR (created_at, id)
                           <= (SELECT created_at, id FROM raw_event WHERE id=$3))
                ORDER BY created_at DESC, id DESC LIMIT $2""",
            group_id, size, ending_at,
        )))

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
            note = f"拥有者注：{notes[code]}" if code in notes else ""
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
            for ep in await self._eps.involving(group_id, eid, limit=KNOWN_EPISODES):
                if ep.summary not in seen:
                    seen.append(ep.summary)
        if seen:
            out.append("已记过的事：")
            out += [f"- {defang(s)}" for s in seen[:KNOWN_EPISODES]]
        return "\n".join(out)

    async def extract(self, group_id: int, *, force: bool = False) -> int:
        """Drain the unread transcript in gap-aligned chunks. One model call each.

        The nightly single event point (schedule.nightly_cron): the whole day is
        read here, oldest first, each chunk cut where a conversation ended. Two
        gates per pass, in the order that costs least to check: the day's budget,
        then whether enough is unread to be worth a pass at all (DRAIN_FLOOR;
        `force` - /relearn - reads whatever there is). This is the only place that
        knows a model call is about to happen, so it is the only place where
        "nothing has been said since last time" can reliably stop one.
        """
        total = 0
        for _ in range(MAX_PASSES):
            if await BUDGET.exceeded(self._cfg.budget.daily_cny_cap):
                # The same cap that silences replies gates learning too: background
                # work is the least urgent spend there is.
                log.info("group %s: daily cap reached, extraction waits for "
                         "tomorrow", group_id)
                break

            unread, _newest = await repo.unread_since_extract(group_id)
            if not unread or (unread < DRAIN_FLOOR and not force):
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
        # twice. But it moves only once the paid read *succeeded* - marked first, a
        # timed-out extraction left the job's retry facing "nothing unread", and the
        # batch was skipped forever. Marked before staging on purpose: the model call
        # is the fragile, expensive step; if staging fails the loss is one batch's
        # candidates, not a second charge for the same transcript.
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
        by_account: dict[str, int] = {}
        lines: list[SourceLine] = []
        roster: list[str] = []

        for r in rows:
            uid = r["platform_user_id"]
            payload = r["payload"]
            sender = (payload or {}).get("sender") or {}
            # defang on render: rows archived before Sender.parse neutralized
            # names can carry anything, and the account code appended below is
            # only unforgeable if the name half cannot contain the brackets.
            name = defang((sender.get("card") or sender.get("nickname")
                           or uid or "")).strip()
            if not uid:
                continue
            if uid not in by_account:
                eid = await self._identity_of(uid)
                if eid is None:
                    by_account[uid] = 0  # remember the answer; one lookup per account
                else:
                    code = len(codes) + 1
                    by_account[uid] = code
                    codes[code] = eid
                    akas = await self._known_names(group_id, eid, name)
                    roster.append(name + sysmark(str(code))
                                  + (f"（也叫：{'、'.join(defang(a) for a in akas)}）"
                                     if akas else ""))
            if not by_account[uid]:
                text = self._plain(r)
                if text:
                    who = (name + sysmark("你")) if name else sysmark("你")
                    lines.append(SourceLine(
                        event_id=r["id"], own=True,
                        text=f"{sysmark(fmt_when(r['occurred_at']))} {who}: {text}"))
                continue
            text = self._plain(r)
            if text:
                # The send time leads each line, exactly as the reply prompt stamps its
                # history: a batch can span days (the idle floor), and without stamps
                # two conversations hours apart read as one and get merged into one
                # episode. occurred_at is fixed, so a batch re-rendered at consolidation
                # time is byte-identical to what the extractor read.
                lines.append(SourceLine(
                    event_id=r["id"],
                    text=f"{sysmark(fmt_when(r['occurred_at']))} "
                         f"{name}{sysmark(str(by_account[uid]))}: {text}"))
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

    @staticmethod
    def _plain(row) -> str:
        """The stored reading, falling back to the payload's text segments.

        The stored reading is the one the group actually saw: @-mentions resolved to
        names, pictures replaced by their descriptions. Re-deriving it from the segments
        would hand the extractor bare account numbers and an empty picture marker, and
        then ask it who was being talked about.
        """
        if text := (row["plain_text"] or "").strip():
            return text
        payload = row["payload"] or {}
        parts = [
            (seg.get("data") or {}).get("text", "").strip()
            for seg in payload.get("segments") or []
            if seg.get("type") == "text"
        ]
        return " ".join(p for p in parts if p)

    # -- consolidation -----------------------------------------------------
    async def consolidate(self, group_id: int, *, when: datetime | None = None) -> tuple[int, int]:
        """Validate what is pending and write what survives.

        Candidates are grouped by the batch they came from, and each group is checked
        against that batch reproduced from its anchor. Pending work can span several
        extractions - one failure and the next run picks up both - and a quote from the
        older one is not in the newer one's messages, so validating them all against a
        single window would reject correctly-quoted records as inventions.
        """
        cands = await self._mem.pending(group_id)
        if not cands:
            return 0, 0

        batches: dict[uuid.UUID | None, list] = {}
        for c in cands:
            batches.setdefault(c.batch_event_id, []).append(c)

        written = rejected = 0
        for anchor, batch in batches.items():
            rows = await self._replay(group_id, ending_at=anchor,
                                      size=batch[0].batch_size)
            rows = sorted(rows, key=lambda r: (r["occurred_at"], r["id"]))
            codes, _roster, lines = await self._render(group_id, rows)
            w, r = await self._consolidator.consolidate(
                batch, group_id=group_id, codes=codes,
                lines=tuple(ln.text for ln in lines), when=when or now_local(),
            )
            written += w
            rejected += r
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
    async def decay(self, group_id: int) -> tuple[int, int]:
        """Let go of what nothing has confirmed lately. Returns (facts, names).

        Free - it is one UPDATE each, no model call. It runs daily because the cost of
        not running it is paid on every single reply: the roster sits in the system block,
        so one stale line is re-read on every turn until somebody notices it by hand.
        """
        facts = await self._mem.decay(
            group_id,
            stable=STABLE_PREDICATES, fast=FAST_PREDICATES,
            stable_days=STABLE_DAYS, default_days=DEFAULT_DAYS, fast_days=FAST_DAYS,
            keep_predicates=(NOTE,))
        names = await self._ids.decay_aliases(
            group_id, unused_days=ALIAS_UNUSED_DAYS, joke_days=JOKE_UNUSED_DAYS)
        if facts or names:
            log.info("group %s: retired %d facts and %d unconfirmed names",
                     group_id, facts, names)
        return facts, names

    # -- vectors -----------------------------------------------------------
    async def embed(self, group_id: int) -> int:
        """Fill in vectors for episodes that have none. Incremental, never a full pass."""
        if await BUDGET.exceeded(self._cfg.budget.daily_cny_cap):
            return 0
        todo = await self._vec.unembedded_episodes(group_id)
        if not todo:
            return 0
        vecs = await self._embed.embed([summary for _, summary in todo],
                                       group_id=str(group_id))
        for (eid, _), v in zip(todo, vecs):
            await self._vec.put(group_id=group_id, object_type="episode",
                                object_id=eid, embedding=v)
        log.info("group %s: embedded %d episodes", group_id, len(todo))
        return len(todo)
