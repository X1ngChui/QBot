"""Validate staged extraction candidates and project accepted structured memory."""

from __future__ import annotations

import logging
import uuid
from dataclasses import dataclass
from datetime import datetime

import asyncpg

from ..db import pool
from ..domain.ids import GroupId
from ..domain.identity import (
    ALIAS_MAX_CHARS,
    Alias,
    AliasEvidence,
    AliasType,
    EvidenceType,
    normalize,
)
from ..domain.memory import (
    Candidate,
    CandidateType,
    Episode,
    ExtractionSnapshot,
    Fact,
    FactEvidence,
    MemoryType,
    RejectReason,
    SnapshotLine,
    earned_confidence,
)
from ..repositories import (
    EpisodeRepository,
    ExtractionRepository,
    IdentityRepository,
    JobQueue,
    MemoryRepository,
)
from ..repositories.job import JobType
from ..settings import config
from .memory_extractor import (
    ALIAS_KINDS,
    GROUP_TERM,
    GROUP_TOPIC,
    multi_valued,
    opposites,
    predicate_names,
)

log = logging.getLogger("qqbot.consolidate")


def _text(payload: dict, key: str) -> str:
    value = payload.get(key)
    return value.strip() if isinstance(value, str) else ""


def _fact_kind(predicate: str) -> MemoryType:
    entry = config().predicates.person.get(predicate)
    return MemoryType(entry.kind) if entry else MemoryType.ATTRIBUTE


@dataclass(frozen=True, slots=True)
class Verdict:
    ok: bool
    reason: RejectReason | None = None

    @classmethod
    def no(cls, reason: RejectReason) -> Verdict:
        return cls(False, reason)


PASS = Verdict(True)


class Validator:
    """Pure validation against one immutable account-scoped snapshot."""

    def __init__(self, snapshot: ExtractionSnapshot) -> None:
        self._snapshot = snapshot
        self._codes = snapshot.codes

    def _source(self, candidate: Candidate, payload: dict) -> SnapshotLine | None:
        source = payload.get("source")
        quote = _text(payload, "quote")
        if not isinstance(source, int) or not quote:
            return None
        line = self._snapshot.sources.get(source)
        if (
            line is None
            or line.own
            or line.event_type != "message"
            or quote not in line.evidence_text
            or candidate.source_event_id != line.event_id
        ):
            return None
        return line

    def source_lines(self, candidate: Candidate) -> tuple[SnapshotLine, ...]:
        """Return every validated source line, or an empty tuple."""

        if candidate.candidate_type is not CandidateType.EPISODE:
            line = self._source(candidate, candidate.payload)
            return (line,) if line is not None else ()

        sources = candidate.payload.get("sources")
        if not isinstance(sources, list) or not 1 <= len(sources) <= 8:
            return ()
        lines: list[SnapshotLine] = []
        ordinals: set[int] = set()
        for index, source in enumerate(sources):
            if not isinstance(source, dict):
                return ()
            ordinal = source.get("source")
            quote = _text(source, "quote")
            if not isinstance(ordinal, int) or ordinal in ordinals or not quote:
                return ()
            line = self._snapshot.sources.get(ordinal)
            if (
                line is None
                or line.own
                or line.event_type != "message"
                or quote not in line.evidence_text
                or (index == 0 and candidate.source_event_id != line.event_id)
            ):
                return ()
            ordinals.add(ordinal)
            lines.append(line)
        return tuple(lines)

    def check(self, candidate: Candidate) -> Verdict:
        payload = candidate.payload
        if not isinstance(payload, dict) or not payload:
            return Verdict.no(RejectReason.EMPTY)

        sources = self.source_lines(candidate)
        if not sources:
            return Verdict.no(RejectReason.MALFORMED)
        if candidate.candidate_type is CandidateType.EPISODE:
            return PASS if _text(payload, "summary") else Verdict.no(RejectReason.EMPTY)

        quote = _text(payload, "quote")
        if candidate.candidate_type is CandidateType.GROUP_FACT:
            return self._check_group(payload, quote)

        code = payload.get("account")
        if not isinstance(code, int) or code not in self._codes:
            return Verdict.no(RejectReason.UNKNOWN_ENTITY)
        target_id = self._codes[code]
        resolutions = [
            target for target in sources[0].targets if target.account_id == target_id
        ]
        if not resolutions or not any(
            not target.marker or target.marker in quote for target in resolutions
        ):
            return Verdict.no(RejectReason.MALFORMED)

        if candidate.candidate_type is CandidateType.ALIAS:
            return self._check_alias(payload, quote)
        if candidate.candidate_type is CandidateType.FACT:
            return self._check_fact(payload)
        return Verdict.no(RejectReason.MALFORMED)

    @staticmethod
    def _check_alias(payload: dict, quote: str) -> Verdict:
        text = _text(payload, "alias")
        if not text:
            return Verdict.no(RejectReason.EMPTY)
        if payload.get("kind") not in ALIAS_KINDS or len(text) > ALIAS_MAX_CHARS:
            return Verdict.no(RejectReason.MALFORMED)
        return PASS if text in quote else Verdict.no(RejectReason.MALFORMED)

    @staticmethod
    def _check_fact(payload: dict) -> Verdict:
        if payload.get("predicate") not in predicate_names():
            return Verdict.no(RejectReason.MALFORMED)
        if not _text(payload, "object"):
            return Verdict.no(RejectReason.EMPTY)
        return PASS

    @staticmethod
    def _check_group(payload: dict, quote: str) -> Verdict:
        kind = payload.get("kind")
        if kind == GROUP_TERM:
            term = _text(payload, "term")
            if not term or not _text(payload, "meaning"):
                return Verdict.no(RejectReason.EMPTY)
            return PASS if term in quote else Verdict.no(RejectReason.MALFORMED)
        if kind == GROUP_TOPIC:
            return PASS if _text(payload, "topic") else Verdict.no(RejectReason.EMPTY)
        return Verdict.no(RejectReason.MALFORMED)

    @staticmethod
    def ambiguous_aliases(candidates: list[Candidate]) -> set[str]:
        seen: dict[str, set[int]] = {}
        for candidate in candidates:
            if candidate.candidate_type is not CandidateType.ALIAS or not isinstance(
                candidate.payload, dict
            ):
                continue
            text = normalize(_text(candidate.payload, "alias"))
            code = candidate.payload.get("account")
            if text and isinstance(code, int):
                seen.setdefault(text, set()).add(code)
        return {text for text, codes in seen.items() if len(codes) > 1}


class MemoryConsolidator:
    """Apply one staged extraction atomically, including follow-up embed work."""

    def __init__(
        self,
        ids: IdentityRepository,
        mem: MemoryRepository,
        eps: EpisodeRepository,
        extractions: ExtractionRepository,
        queue: JobQueue,
    ) -> None:
        self._ids = ids
        self._mem = mem
        self._eps = eps
        self._extractions = extractions
        self._queue = queue

    async def apply(
        self,
        extraction_id: uuid.UUID,
        *,
        group_id: GroupId,
        when: datetime,
    ) -> tuple[int, int]:
        async with pool().acquire() as conn, conn.transaction():
            row = await conn.fetchrow(
                """SELECT group_id, status, snapshot
                     FROM memory_extraction WHERE id=$1 FOR UPDATE""",
                extraction_id,
            )
            if row is None:
                raise RuntimeError(f"unknown extraction {extraction_id}")
            if GroupId(row["group_id"]) != group_id:
                raise RuntimeError(f"extraction {extraction_id} belongs to another group")
            if row["status"] == "applied":
                return 0, 0
            if row["status"] != "staged":
                raise RuntimeError(
                    f"extraction {extraction_id} is {row['status']!r}, expected staged"
                )
            if row["snapshot"] is None:
                raise RuntimeError(f"extraction {extraction_id} has no staged snapshot")

            snapshot = ExtractionSnapshot.from_payload(row["snapshot"])
            candidates = await self._extractions.candidates(extraction_id, conn=conn)
            validator = Validator(snapshot)
            ambiguous = validator.ambiguous_aliases(candidates)
            written = rejected = 0
            episode_written = False

            for candidate in candidates:
                verdict = validator.check(candidate)
                if (
                    verdict.ok
                    and candidate.candidate_type is CandidateType.ALIAS
                    and normalize(_text(candidate.payload, "alias")) in ambiguous
                ):
                    verdict = Verdict.no(RejectReason.AMBIGUOUS_ALIAS)
                if not verdict.ok:
                    await self._mem.settle(
                        candidate.rejected(verdict.reason.value),
                        _conn=conn,
                    )
                    rejected += 1
                    continue

                source_lines = validator.source_lines(candidate)
                try:
                    if candidate.candidate_type is CandidateType.EPISODE:
                        await self._write_episode(
                            candidate,
                            group_id,
                            extraction_id,
                            source_lines,
                            _conn=conn,
                        )
                        episode_written = True
                    elif candidate.candidate_type is CandidateType.GROUP_FACT:
                        await self._write_group_fact(
                            candidate,
                            group_id,
                            when,
                            source_lines[0].occurred_at,
                            _conn=conn,
                        )
                    elif candidate.candidate_type is CandidateType.ALIAS:
                        await self._write_alias(
                            candidate,
                            group_id,
                            snapshot.codes[candidate.payload["account"]],
                            _conn=conn,
                        )
                    else:
                        await self._write_fact(
                            candidate,
                            group_id,
                            snapshot.codes[candidate.payload["account"]],
                            when,
                            source_lines[0].occurred_at,
                            _conn=conn,
                        )
                except (KeyError, TypeError, ValueError):
                    await self._mem.settle(
                        candidate.rejected(RejectReason.MALFORMED.value),
                        _conn=conn,
                    )
                    rejected += 1
                    continue
                await self._mem.settle(candidate.accepted(), _conn=conn)
                written += 1

            if episode_written:
                await self._queue.submit(
                    JobType.EMBED,
                    {"group_id": group_id},
                    _conn=conn,
                )
            await conn.execute(
                """UPDATE memory_extraction
                      SET status='applied', applied_at=NOW()
                    WHERE id=$1""",
                extraction_id,
            )
        return written, rejected

    async def _write_alias(
        self,
        candidate: Candidate,
        group_id: GroupId,
        target_id: uuid.UUID,
        *,
        _conn: asyncpg.Connection,
    ) -> None:
        await self._ids.upsert_alias(
            Alias(
                alias_text=candidate.payload["alias"].strip(),
                target_entity_id=None,
                target_account_id=target_id,
                group_id=group_id,
                alias_type=AliasType(candidate.payload["kind"]),
            ),
            [
                AliasEvidence(
                    EvidenceType.LLM_INFERENCE,
                    candidate.source_event_id,
                )
            ],
            _conn=_conn,
        )

    async def _write_episode(
        self,
        candidate: Candidate,
        group_id: GroupId,
        extraction_id: uuid.UUID,
        sources: tuple[SnapshotLine, ...],
        *,
        _conn: asyncpg.Connection,
    ) -> None:
        times = [source.occurred_at for source in sources]
        score = candidate.confidence if candidate.confidence is not None else 0.5
        await self._eps.add(
            Episode(
                id=candidate.id,
                group_id=group_id,
                summary=candidate.payload["summary"].strip(),
                started_at=min(times),
                ended_at=max(times),
                importance=score,
                confidence=score,
                extraction_id=extraction_id,
                event_ids=tuple(dict.fromkeys(source.event_id for source in sources)),
            ),
            _conn=_conn,
        )

    async def _write_group_fact(
        self,
        candidate: Candidate,
        group_id: GroupId,
        when: datetime,
        observed_at: datetime,
        *,
        _conn: asyncpg.Connection,
    ) -> None:
        payload = candidate.payload
        subject = await self._ids.group_entity(group_id, _conn=_conn)
        if payload["kind"] == GROUP_TERM:
            predicate = GROUP_TERM
            key = payload["term"].strip()
            value = payload["meaning"].strip()
            for fact in await self._mem.current_entity_facts(
                group_id,
                [subject],
                _conn=_conn,
            ):
                if (
                    fact.predicate == GROUP_TERM
                    and fact.object_key
                    and normalize(fact.object_key) == normalize(key)
                ):
                    key = fact.object_key
                    break
        else:
            predicate, key, value = GROUP_TOPIC, None, payload["topic"].strip()
        await self._mem.supersede(
            Fact(
                subject_entity_id=subject,
                predicate=predicate,
                object_key=key,
                object_value=value,
                group_id=group_id,
                memory_type=MemoryType.GROUP,
                confidence=earned_confidence(1),
            ),
            [FactEvidence(candidate.source_event_id)],
            when=when,
            observed_at=observed_at,
            _conn=_conn,
        )

    async def _write_fact(
        self,
        candidate: Candidate,
        group_id: GroupId,
        target_id: uuid.UUID,
        when: datetime,
        observed_at: datetime,
        *,
        _conn: asyncpg.Connection,
    ) -> None:
        predicate = candidate.payload["predicate"]
        value = candidate.payload["object"].strip()
        if opposite := opposites().get(predicate):
            current = await self._mem.current_account_facts(
                group_id,
                [target_id],
                _conn=_conn,
            )
            for fact in current:
                if (
                    fact.predicate == opposite
                    and fact.object_key
                    and normalize(fact.object_key) == normalize(value)
                ):
                    await self._mem.retract(fact.id, _conn=_conn)

        await self._mem.supersede(
            Fact(
                subject_entity_id=None,
                subject_account_id=target_id,
                predicate=predicate,
                object_key=value if predicate in multi_valued() else None,
                object_value=value,
                group_id=group_id,
                memory_type=_fact_kind(predicate),
                confidence=earned_confidence(1),
            ),
            [FactEvidence(candidate.source_event_id)],
            when=when,
            observed_at=observed_at,
            _conn=_conn,
        )
