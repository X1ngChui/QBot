"""L3/L4 persistence: facts, evidence, candidates, episodes."""

from __future__ import annotations

import uuid
from collections.abc import AsyncIterator
from contextlib import asynccontextmanager
from datetime import datetime

import asyncpg

from ..db import pool
from ..domain.ids import GroupId
from ..util import tz_sql
from .identity import FAMILY
from ..domain.memory import (
    Candidate,
    Episode,
    EpisodeType,
    Fact,
    FactEvidence,
    FactStatus,
    MemoryType,
    earned_confidence,
)


@asynccontextmanager
async def _write_connection(
    conn: asyncpg.Connection | None,
) -> AsyncIterator[asyncpg.Connection]:
    if conn is not None:
        yield conn
        return
    async with pool().acquire() as owned, owned.transaction():
        yield owned


def _fact(row, *, subject: uuid.UUID | None = None) -> Fact:
    """One row as a Fact.

    `subject` overrides who it is about, and the reads that follow a merge pass it. A fact
    recorded before two accounts were declared one person still names the account it was
    filed against - deliberately, because rewriting those rows is what would make a split
    unrecoverable - so without the override a caller asking about the surviving person
    gets facts back keyed by an id it has never heard of, and drops them at render time
    for want of a name to put in front of them.
    """
    return Fact(
        id=row["id"],
        group_id=GroupId(row["group_id"]),
        subject_entity_id=(subject or row["subject_entity_id"])
        if row["subject_entity_id"] is not None
        else None,
        subject_account_id=row["subject_account_id"],
        predicate=row["predicate"],
        object_key=row["object_key"],
        object_entity_id=row["object_entity_id"],
        object_value=row["object_value"],
        memory_type=MemoryType(row["memory_type"]),
        confidence=row["confidence"],
        status=FactStatus(row["status"]),
        valid_from=row["valid_from"],
        valid_to=row["valid_to"],
        first_observed_at=row["first_observed_at"],
        last_confirmed_at=row["last_confirmed_at"],
        revision=row["revision"],
    )


class MemoryRepository:
    """Facts and candidates.

    There is one way in: `supersede`. It holds the invariant that one subject has one
    current fact per predicate per group - held by a database transaction, rather than
    by every caller remembering to read before it writes.
    """

    async def current_facts(
        self,
        group_id: GroupId,
        subject_ids: list[uuid.UUID],
        *,
        limit: int = 200,
        _conn: asyncpg.Connection | None = None,
    ) -> list[Fact]:
        """What currently holds about these people. The group filter comes first; that
        is where isolation is enforced.

        `limit` is per subject, not per call: the worker asks for a whole roster at
        once, and one shared cap would let a few talkative members crowd everyone
        else out of the answer. The id tie-break keeps the order stable between two
        reads of the same data, which the prompt cache depends on.
        """
        if not subject_ids:
            return []
        rows = await (_conn or pool()).fetch(
            """WITH RECURSIVE family AS (
                   SELECT id, id AS root FROM entity WHERE id = ANY($2::uuid[])
                   UNION ALL
                   SELECT e.id, f.root FROM entity e JOIN family f ON e.merged_into = f.id
               ), holder_accounts AS (
                   SELECT a.id, a.entity_id AS root
                     FROM identity_account a
                    WHERE a.entity_id = ANY($2::uuid[])
               )
               SELECT top.* FROM (SELECT DISTINCT root FROM family) r
               CROSS JOIN LATERAL (
                   SELECT m.*, r.root
                     FROM memory_fact m
                    WHERE m.group_id=$1 AND m.status='active' AND m.valid_to IS NULL
                      AND (
                          m.subject_entity_id IN (
                              SELECT id FROM family WHERE root=r.root
                          )
                          OR m.subject_account_id IN (
                              SELECT id FROM holder_accounts WHERE root=r.root
                          )
                      )
                    ORDER BY m.confidence DESC, m.last_confirmed_at DESC NULLS LAST, m.id
                    LIMIT $3
               ) top""",
            group_id.to_db(),
            subject_ids,
            limit,
        )
        return [_fact(r, subject=r["root"]) for r in rows]

    async def current_account_facts(
        self,
        group_id: GroupId,
        account_ids: list[uuid.UUID],
        *,
        limit: int = 200,
        _conn: asyncpg.Connection | None = None,
    ) -> list[Fact]:
        """Current facts attached to exact accounts, without holder expansion."""

        if not account_ids:
            return []
        rows = await (_conn or pool()).fetch(
            """SELECT * FROM memory_fact
                WHERE group_id=$1 AND subject_account_id=ANY($2::uuid[])
                  AND status='active' AND valid_to IS NULL
                ORDER BY subject_account_id, confidence DESC,
                         last_confirmed_at DESC NULLS LAST, id
                LIMIT $3""",
            group_id.to_db(),
            account_ids,
            limit * len(account_ids),
        )
        return [_fact(row) for row in rows]

    async def current_entity_facts(
        self,
        group_id: GroupId,
        entity_ids: list[uuid.UUID],
        *,
        limit: int = 200,
        _conn: asyncpg.Connection | None = None,
    ) -> list[Fact]:
        """Current holder-scoped facts, excluding exact-account rows."""

        if not entity_ids:
            return []
        rows = await (_conn or pool()).fetch(
            """WITH RECURSIVE family AS (
                   SELECT id, id AS root FROM entity WHERE id=ANY($2::uuid[])
                   UNION ALL
                   SELECT e.id, f.root FROM entity e JOIN family f ON e.merged_into=f.id
               )
               SELECT top.* FROM (SELECT DISTINCT root FROM family) r
               CROSS JOIN LATERAL (
                   SELECT m.*, r.root FROM memory_fact m
                    WHERE m.group_id=$1 AND m.status='active' AND m.valid_to IS NULL
                      AND m.subject_entity_id IN (
                          SELECT id FROM family WHERE root=r.root
                      )
                    ORDER BY m.confidence DESC,
                             m.last_confirmed_at DESC NULLS LAST, m.id
                    LIMIT $3
               ) top""",
            group_id.to_db(),
            entity_ids,
            limit,
        )
        return [_fact(row, subject=row["root"]) for row in rows]

    async def supersede(
        self,
        fact: Fact,
        evidence: list[FactEvidence],
        *,
        when: datetime,
        observed_at: datetime | None = None,
        _conn: asyncpg.Connection | None = None,
    ) -> Fact:
        """Write a new fact and close out the one it overturns.

        Confirming the same object again adds no row; it moves last_confirmed_at and
        the confidence instead. Something a group repeats every week should not pile up
        as fifty identical records.

        Two clocks: `when` is the moment this write happens and closes what it
        supersedes; `observed_at` is when the conversation that stated the fact
        took place and stamps valid_from / first_observed_at /
        last_confirmed_at, so a record ages from what was said, not from the
        night it was written. They differ by however
        long the fact waited in the candidate queue - hours for the nightly drain, days
        after a retry - and a fact dated by its write would age from the wrong day.
        Callers with no source event (a manual note) leave it unset.
        """
        observed_at = observed_at or when
        async with _write_connection(_conn) as conn:
            if fact.subject_account_id is not None:
                prevs = await conn.fetch(
                    """SELECT * FROM memory_fact
                        WHERE group_id IS NOT DISTINCT FROM $1
                          AND subject_account_id=$2 AND predicate=$3
                          AND object_key IS NOT DISTINCT FROM $4
                          AND status='active' AND valid_to IS NULL
                        ORDER BY created_at DESC
                        FOR UPDATE""",
                    fact.group_id.to_db() if fact.group_id is not None else None,
                    fact.subject_account_id,
                    fact.predicate,
                    fact.object_key,
                )
            else:
                prevs = await conn.fetch(
                    FAMILY.format(arg="$2")
                    + """
                       SELECT m.* FROM memory_fact m
                         JOIN family ON m.subject_entity_id = family.id
                        WHERE m.group_id IS NOT DISTINCT FROM $1 AND m.predicate=$3
                          AND m.object_key IS NOT DISTINCT FROM $4
                          AND m.status='active' AND m.valid_to IS NULL
                        ORDER BY m.created_at DESC
                        FOR UPDATE OF m""",
                    fact.group_id.to_db() if fact.group_id is not None else None,
                    fact.subject_entity_id,
                    fact.predicate,
                    fact.object_key,
                )
            # A merged family can hold one current row per pre-merge account. The newest
            # matching row is confirmed; every other current row for this key is closed,
            # or a merged person keeps answering with two contradictory "current" facts.
            prev = next(
                (
                    p
                    for p in prevs
                    if p["object_entity_id"] == fact.object_entity_id
                    and p["object_value"] == fact.object_value
                ),
                prevs[0] if prevs else None,
            )
            for extra in prevs:
                if prev is not None and extra["id"] == prev["id"]:
                    continue
                await conn.execute(
                    """UPDATE memory_fact
                          SET status='superseded', valid_to=$2,
                              revision=revision+1, updated_at=NOW()
                        WHERE id=$1""",
                    extra["id"],
                    when,
                )

            if prev is not None:
                same = (
                    prev["object_entity_id"] == fact.object_entity_id
                    and prev["object_value"] == fact.object_value
                )
                if same:
                    await self._add_evidence(conn, prev["id"], evidence)
                    # Confidence is recomputed from the whole trail rather than nudged:
                    # a Wilson lower bound over distinct supporting events, so this is
                    # the branch that makes repetition mean something. The incoming
                    # confidence still acts as a floor, because a manual note arrives
                    # at 1.0 and must not be argued down by arithmetic.
                    counts = await conn.fetchrow(
                        """SELECT
                             count(DISTINCT raw_event_id)
                               FILTER (WHERE relation='supports') AS r,
                             count(DISTINCT raw_event_id)
                               FILTER (WHERE relation='contradicts') AS s
                            FROM memory_fact_evidence WHERE fact_id=$1""",
                        prev["id"],
                    )
                    await conn.execute(
                        """UPDATE memory_fact
                              SET last_confirmed_at=$2,
                                  confidence=GREATEST($3::float8, $4::float8),
                                  revision=revision+1, updated_at=NOW()
                            WHERE id=$1""",
                        prev["id"],
                        observed_at,
                        earned_confidence(counts["r"] or 0, counts["s"] or 0),
                        fact.confidence,
                    )
                    return _fact(
                        await conn.fetchrow("SELECT * FROM memory_fact WHERE id=$1", prev["id"])
                    )

                await conn.execute(
                    """UPDATE memory_fact
                          SET status='superseded', valid_to=$2,
                              revision=revision+1, updated_at=NOW()
                        WHERE id=$1""",
                    prev["id"],
                    when,
                )

            row = await conn.fetchrow(
                """INSERT INTO memory_fact
                       (group_id, subject_entity_id, subject_account_id,
                        predicate, object_key, object_entity_id, object_value,
                        memory_type, confidence, status, valid_from,
                        first_observed_at, last_confirmed_at)
                   VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,'active',$10,$10,$10)
                RETURNING *""",
                fact.group_id.to_db() if fact.group_id is not None else None,
                fact.subject_entity_id,
                fact.subject_account_id,
                fact.predicate,
                fact.object_key,
                fact.object_entity_id,
                fact.object_value,
                fact.memory_type.value,
                fact.confidence,
                observed_at,
            )
            await self._add_evidence(conn, row["id"], evidence)
            return _fact(row)

    @staticmethod
    async def _add_evidence(conn, fact_id: uuid.UUID, evidence: list[FactEvidence]) -> None:
        for ev in evidence:
            await conn.execute(
                """INSERT INTO memory_fact_evidence
                       (fact_id, raw_event_id, relation, evidence_score)
                   VALUES ($1,$2,$3,$4)""",
                fact_id,
                ev.raw_event_id,
                ev.relation.value,
                ev.score,
            )

    async def retract(
        self,
        fact_id: uuid.UUID,
        *,
        _conn: asyncpg.Connection | None = None,
    ) -> None:
        await (_conn or pool()).execute(
            """UPDATE memory_fact SET status='retracted', valid_to=NOW(),
                                      revision=revision+1, updated_at=NOW()
                WHERE id=$1""",
            fact_id,
        )

    async def decay(
        self,
        group_id: GroupId,
        *,
        stable: tuple[str, ...],
        fast: tuple[str, ...],
        stable_days: float,
        default_days: float,
        fast_days: float,
        keep_predicates: tuple[str, ...] = (),
    ) -> int:
        """Retire facts nothing has confirmed for a while. Returns how many.

        Memory that only ever grows is not memory, it is an archive - and the model reads
        the whole roster on every turn, so anything stale in there keeps being handed back
        as though it were current.

        The base lifetime is set by what *kind* of fact it is, and only secondarily by
        how often it was seen. Weighting evidence count first gets both halves wrong: a
        much-repeated piece of ephemera ("this week's game") outlives a once-stated
        stable fact ("works as a doctor"), and a uniform decay rate is the one setting
        measured to be worse than no decay at all (arXiv 2608.04746) - it discounts
        durable facts and ephemeral ones by the same clock. So predicates carry a class -
        where somebody lives moves slowly, what they are currently playing moves fast -
        and the class sets the half-life.

        Evidence still helps, but bounded and spaced: the multiplier is logarithmic in
        *distinct days* of support, capped at 4x. Log because stability gains saturate
        (the FSRS shape); distinct days because five confirmations in one conversation
        are one event, while five spread over weeks are five - the spacing effect, and
        the same reason platform names confirm by enduring a second day.

        `keep_predicates` never expires. Explicit command input is not a guess that fades.

        What ages out is marked expired, not superseded: nothing contradicted it, it
        simply stopped coming up, and a reader of the history should be able to tell
        the two apart.
        """
        rows = await pool().fetch(
            """WITH support AS (
                   -- Days are counted by when the supporting messages were SENT, not
                   -- when their evidence was written: a drain that reads several days
                   -- at once writes all of it in one transaction, and by write time
                   -- a fact restated every day for a week would count as one day.
                   -- Bucketed in the configured zone, like the analogous alias
                   -- stability count: bare ::date buckets in the session zone (UTC
                   -- here), where an evening conversation straddling local midnight
                   -- counts as two days of support and evening-heavy traffic
                   -- systematically inflates the lifetime multiplier.
                   SELECT f.id,
                          GREATEST(count(DISTINCT
                                     (COALESCE(r.occurred_at, e.created_at)
                                      AT TIME ZONE $8)::date), 1) AS days,
                          CASE WHEN f.predicate = ANY($2::text[])
                                 THEN $4::float
                               WHEN f.predicate = ANY($3::text[])
                                 THEN $6::float
                               ELSE $5::float END AS base
                     FROM memory_fact f
                     LEFT JOIN memory_fact_evidence e
                            ON e.fact_id = f.id AND e.relation = 'supports'
                     LEFT JOIN raw_event r ON r.id = e.raw_event_id
                    WHERE f.group_id = $1 AND f.status = 'active' AND f.valid_to IS NULL
                      AND NOT (f.predicate = ANY($7::text[]))
                    GROUP BY f.id, f.predicate
               )
               UPDATE memory_fact f
                  SET status = $9, valid_to = NOW(),
                      revision = revision + 1, updated_at = NOW()
                 FROM support s
                WHERE f.id = s.id
                  AND COALESCE(f.last_confirmed_at, f.first_observed_at, f.created_at)
                      < NOW() - (s.base * LEAST(4.0, 1 + ln(1 + s.days))
                                 * INTERVAL '1 day')
             RETURNING f.id""",
            group_id.to_db(),
            list(stable),
            list(fast),
            stable_days,
            default_days,
            fast_days,
            list(keep_predicates),
            tz_sql(),
            FactStatus.EXPIRED.value,
        )
        return len(rows)

    # -- candidate audit ----------------------------------------------------
    async def settle(
        self,
        candidate: Candidate,
        *,
        _conn: asyncpg.Connection | None = None,
    ) -> None:
        await (_conn or pool()).execute(
            """UPDATE memory_candidate
                  SET status=$2, reject_reason=$3, processed_at=NOW()
                WHERE id=$1""",
            candidate.id,
            candidate.status.value,
            candidate.reject_reason,
        )


def _episode(r) -> Episode:
    return Episode(
        id=r["id"],
        group_id=GroupId(r["group_id"]),
        episode_type=EpisodeType(r["episode_type"])
        if r["episode_type"]
        else EpisodeType.DISCUSSION,
        title=r["title"],
        summary=r["summary"],
        started_at=r["started_at"],
        ended_at=r["ended_at"],
        importance=r["importance"] or 0.0,
        confidence=r["confidence"] or 0.0,
        status=r["status"],
        revision=r["revision"],
        extraction_id=r["extraction_id"],
    )


class EpisodeRepository:
    """Episodic memory with exact extraction and source-event provenance."""

    async def add(
        self,
        ep: Episode,
        *,
        _conn: asyncpg.Connection | None = None,
    ) -> Episode:
        """Write one episode, idempotently on its id.

        The consolidator derives the id from the candidate it came from, so a job
        retried after a crash between writing the episode and settling the candidate
        finds its own row and adds nothing - every insert here tolerates a duplicate.
        The first write's content stands; a retry carries the same content anyway.
        """
        async with _write_connection(_conn) as conn:
            await conn.execute(
                """INSERT INTO episode
                       (id, group_id, episode_type, title, summary, started_at,
                        ended_at, importance, confidence, status, extraction_id)
                   VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11)
                   ON CONFLICT (id) DO NOTHING""",
                ep.id,
                ep.group_id.to_db(),
                ep.episode_type.value,
                ep.title,
                ep.summary,
                ep.started_at,
                ep.ended_at,
                ep.importance,
                ep.confidence,
                ep.status,
                ep.extraction_id,
            )
            for eid in ep.event_ids:
                await conn.execute(
                    """INSERT INTO episode_event (episode_id, raw_event_id)
                       VALUES ($1,$2) ON CONFLICT DO NOTHING""",
                    ep.id,
                    eid,
                )
            return ep

    async def decay(self, group_id: GroupId, *, ttl_days: float) -> int:
        """Retire old episodes and discard their rebuildable vector projections.

        The episode and source-event links remain as the durable account of what happened.
        Only live recall and its derived index expire. Updating and deleting under one
        under one transaction also closes the race with an embed writer, which locks the
        same episode row before adding a projection.
        """
        async with pool().acquire() as conn, conn.transaction():
            rows = await conn.fetch(
                """UPDATE episode
                      SET status='expired', revision=revision+1
                    WHERE group_id=$1 AND status='active'
                      AND COALESCE(ended_at, started_at, created_at)
                          < NOW() - ($2::float * INTERVAL '1 day')
                RETURNING id""",
                group_id.to_db(),
                ttl_days,
            )
            ids = [row["id"] for row in rows]
            if ids:
                await conn.execute(
                    """DELETE FROM embedding_index
                        WHERE object_type='episode' AND object_id=ANY($1::uuid[])""",
                    ids,
                )
        return len(rows)

    async def recent_active(self, group_id: GroupId, *, limit: int) -> list[Episode]:
        """Recent active group episodes used only to suppress duplicate extraction."""

        rows = await pool().fetch(
            """SELECT * FROM episode
                WHERE group_id=$1 AND status='active'
                ORDER BY COALESCE(ended_at, started_at, created_at) DESC, id
                LIMIT $2""",
            group_id.to_db(),
            limit,
        )
        return [_episode(row) for row in rows]

    async def around(
        self, group_id: GroupId, ids: list[uuid.UUID], ctx: int
    ) -> dict[uuid.UUID, list[Episode]]:
        """Per id, the ctx active episodes either side of it in group time order
        plus the episode itself, each window sorted oldest first.

        Order is (started_at, id) - the UUID breaks ties without meaning
        anything, the same trick the archive search uses. An undated episode
        cannot be placed on that line, so it neither gets neighbours nor
        appears as one: absent from the result, and the caller renders the hit
        alone.
        """
        if not ids or ctx <= 0:
            return {}
        rows = await pool().fetch(
            """SELECT h.id AS hit_id, n.* FROM unnest($2::uuid[]) AS h(id)
                 JOIN episode he ON he.id = h.id AND he.group_id=$1
                                AND he.status='active'
                                AND he.started_at IS NOT NULL
                CROSS JOIN LATERAL (
                  (SELECT e.* FROM episode e
                    WHERE e.group_id=$1 AND e.status='active'
                      AND e.started_at IS NOT NULL
                      AND (e.started_at, e.id) <= (he.started_at, he.id)
                    ORDER BY e.started_at DESC, e.id DESC LIMIT $3)
                  UNION ALL
                  (SELECT e.* FROM episode e
                    WHERE e.group_id=$1 AND e.status='active'
                      AND e.started_at IS NOT NULL
                      AND (e.started_at, e.id) > (he.started_at, he.id)
                    ORDER BY e.started_at ASC, e.id ASC LIMIT $4)
                ) n""",
            group_id.to_db(),
            ids,
            ctx + 1,
            ctx,
        )
        out: dict[uuid.UUID, list[Episode]] = {}
        for r in rows:
            out.setdefault(r["hit_id"], []).append(_episode(r))
        for eps in out.values():
            eps.sort(key=lambda e: (e.started_at, e.id))
        return out

    async def by_ids(self, group_id: GroupId, ids: list[uuid.UUID]) -> list[Episode]:
        """The episodes behind a set of ids, in the order the ids arrive.

        The group filter is not redundant with the ids: the ids come back from a vector
        search, and if that layer ever leaked across groups, this is the read that turns
        the leak into nothing rather than into a prompt.
        """
        if not ids:
            return []
        rows = await pool().fetch(
            """SELECT e.* FROM episode e
                WHERE e.group_id=$1 AND e.id=ANY($2::uuid[]) AND e.status='active'""",
            group_id.to_db(),
            ids,
        )
        by_id = {r["id"]: r for r in rows}
        return [_episode(by_id[i]) for i in ids if i in by_id]
