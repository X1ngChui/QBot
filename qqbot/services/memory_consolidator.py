"""Validate candidates, then write them down.

Where the line is enforced: the model may only produce candidates, and
this is what writes long-term memory.

Validation is a pure function (`Validator.check`) that touches no database, so every rule
can be tested on its own.
"""

from __future__ import annotations

import logging
import uuid
from dataclasses import dataclass
from datetime import datetime

from ..domain.identity import Alias, AliasEvidence, AliasType, EvidenceType, normalize
from ..domain.memory import (
    earned_confidence,
    Candidate, CandidateType, Episode, Fact, FactEvidence, MemoryType, Participant,
    RejectReason,
)
from ..repositories import EpisodeRepository, IdentityRepository, MemoryRepository
from ..settings import config
from .memory_extractor import (
    ALIAS_KINDS, GROUP_TERM, GROUP_TOPIC, multi_valued, opposites, predicate_names,
)

log = logging.getLogger("qqbot.consolidate")

def _fact_kind(pred: str) -> MemoryType:
    """How a fact is classified once stored, from the predicate table."""
    entry = config().predicates.person.get(pred)
    return MemoryType(entry.kind) if entry else MemoryType.ATTRIBUTE


@dataclass(frozen=True, slots=True)
class Verdict:
    """The verdict. When ok is false, reason is always set."""

    ok: bool
    reason: RejectReason | None = None

    @classmethod
    def no(cls, reason: RejectReason) -> Verdict:
        return cls(False, reason)


PASS = Verdict(True)


class Validator:
    """Pure logic: whether one candidate may enter long-term memory.

    `codes` is the set of account codes this batch allows, and `lines` is the batch itself,
    one entry per message. Both are supplied by the caller; the validator looks nothing up
    itself, which is what makes it testable on its own.

    The batch arrives as separate lines rather than as one blob because the two checks
    below want different things from it. A quote has to be inside a single message - it is
    a thing somebody said, and text that only matches once the lines are joined spans a
    line break, which nobody typed. A name or a term only has to appear somewhere in the
    conversation, by anyone.
    """

    def __init__(self, codes: dict[int, uuid.UUID], lines: tuple[str, ...]) -> None:
        self._codes = codes
        self._lines = lines
        self._transcript = "\n".join(lines)

    def check(self, c: Candidate) -> Verdict:
        p = c.payload
        if not isinstance(p, dict) or not p:
            return Verdict.no(RejectReason.EMPTY)

        quote = (p.get("quote") or "").strip()
        if not quote or not any(quote in line for line in self._lines):
            # The quote has to be present word for word, inside one message. This is the
            # only check that stops something that sounds plausible but was never said.
            return Verdict.no(RejectReason.MALFORMED)
        if c.source_event_id is None:
            # The extractor could not say which message the quote came from, so nothing
            # here can be filed as evidence of anything.
            return Verdict.no(RejectReason.MALFORMED)

        # A fact about the group names nobody, so the account checks below do not apply
        # to it. Branching on the type first, rather than treating a missing account as a
        # failure, is what lets one validator cover both.
        if c.candidate_type is CandidateType.GROUP_FACT:
            return self._check_group(p)
        if c.candidate_type is CandidateType.EPISODE:
            return self._check_episode(p)

        code = p.get("account")
        if not isinstance(code, int) or code not in self._codes:
            # A model that cannot produce a code will invent one. Anything not on the
            # list is discarded. This is also what keeps the bot out of its own memory:
            # nothing gives the bot an entity, so it never takes a code in the roster the
            # worker renders, and a candidate naming it could only carry an invented one.
            return Verdict.no(RejectReason.UNKNOWN_ENTITY)

        if c.candidate_type is CandidateType.ALIAS:
            return self._check_alias(p)
        if c.candidate_type is CandidateType.FACT:
            return self._check_fact(p)
        return Verdict.no(RejectReason.MALFORMED)

    def _check_alias(self, p: dict) -> Verdict:
        text = (p.get("alias") or "").strip()
        if not text:
            return Verdict.no(RejectReason.EMPTY)
        if p.get("kind") not in ALIAS_KINDS:
            return Verdict.no(RejectReason.MALFORMED)
        if text not in self._transcript:
            return Verdict.no(RejectReason.MALFORMED)
        return PASS

    def _check_fact(self, p: dict) -> Verdict:
        if p.get("predicate") not in predicate_names():
            return Verdict.no(RejectReason.MALFORMED)
        if not (p.get("object") or "").strip():
            return Verdict.no(RejectReason.EMPTY)
        return PASS

    def _check_group(self, p: dict) -> Verdict:
        kind = p.get("kind")
        if kind == GROUP_TERM:
            if not (p.get("term") or "").strip() or not (p.get("meaning") or "").strip():
                return Verdict.no(RejectReason.EMPTY)
            # The word has to be one the group actually used. A definition of a word that
            # appears nowhere in the transcript is the model explaining its own vocabulary.
            if (p.get("term") or "").strip() not in self._transcript:
                return Verdict.no(RejectReason.MALFORMED)
            return PASS
        if kind == GROUP_TOPIC:
            if not (p.get("topic") or "").strip():
                return Verdict.no(RejectReason.EMPTY)
            return PASS
        return Verdict.no(RejectReason.MALFORMED)

    def _check_episode(self, p: dict) -> Verdict:
        if not (p.get("summary") or "").strip():
            return Verdict.no(RejectReason.EMPTY)
        codes = p.get("participants")
        if not isinstance(codes, list) or not codes:
            return Verdict.no(RejectReason.MALFORMED)
        # An episode is retrieved by who took part in it, so a participant the roster does
        # not contain makes it unreachable - and one invented code would file the whole
        # thing under a person who was not there.
        if any(not isinstance(x, int) or x not in self._codes for x in codes):
            return Verdict.no(RejectReason.UNKNOWN_ENTITY)
        return PASS

    def ambiguous_aliases(self, cands: list[Candidate]) -> set[str]:
        """Names in this batch that point at more than one account. A name belongs to one
        person, so writing both means both are wrong - and neither is written."""
        seen: dict[str, set[int]] = {}
        for c in cands:
            if c.candidate_type is not CandidateType.ALIAS:
                continue
            text = normalize(c.payload.get("alias") or "")
            code = c.payload.get("account")
            if text and isinstance(code, int):
                seen.setdefault(text, set()).add(code)
        return {t for t, codes in seen.items() if len(codes) > 1}


class MemoryConsolidator:
    """Writes the candidates that pass into L1/L3, and records the verdict against each
    candidate."""

    def __init__(self, ids: IdentityRepository, mem: MemoryRepository,
                 eps: EpisodeRepository) -> None:
        self._ids = ids
        self._mem = mem
        self._eps = eps

    async def consolidate(
        self, cands: list[Candidate], *, group_id: int, codes: dict[int, uuid.UUID],
        lines: tuple[str, ...], when: datetime,
    ) -> tuple[int, int]:
        """Validate and write one batch. Returns (written, rejected).

        `lines` must be the batch the model read, not whatever is recent now - see
        MemoryWorker.consolidate, which reproduces it from the anchor stored on each
        candidate.
        """
        v = Validator(codes, lines)
        ambiguous = v.ambiguous_aliases(cands)
        written = rejected = 0

        for c in cands:
            verdict = v.check(c)
            if verdict.ok and c.candidate_type is CandidateType.ALIAS \
                    and normalize(c.payload.get("alias") or "") in ambiguous:
                verdict = Verdict.no(RejectReason.AMBIGUOUS_ALIAS)

            if not verdict.ok:
                await self._mem.settle(c.rejected(verdict.reason.value))
                rejected += 1
                log.info("group %s: candidate rejected (%s): %s",
                         group_id, verdict.reason.value, c.payload)
                continue

            if c.candidate_type is CandidateType.EPISODE:
                await self._write_episode(c, group_id, codes, when)
            elif c.candidate_type is CandidateType.GROUP_FACT:
                await self._write_group_fact(c, group_id, when)
            elif c.candidate_type is CandidateType.ALIAS:
                await self._write_alias(c, group_id, codes[c.payload["account"]])
            else:
                await self._write_fact(c, group_id, codes[c.payload["account"]], when)
            await self._mem.settle(c.accepted())
            written += 1

        return written, rejected

    async def _write_alias(self, c: Candidate, group_id: int,
                           entity_id: uuid.UUID) -> None:
        # The model's judgement is worth one piece of LLM_INFERENCE evidence (0.25),
        # which does not reach confirmed. Confirming takes an @ or somebody typing it in -
        # which is the whole point of there being a candidate layer.
        await self._ids.upsert_alias(
            Alias(
                alias_text=c.payload["alias"].strip(),
                target_entity_id=entity_id,
                group_id=group_id,
                alias_type=AliasType(c.payload["kind"]),
            ),
            [AliasEvidence(EvidenceType.LLM_INFERENCE, c.source_event_id)],
        )

    async def _write_episode(self, c: Candidate, group_id: int,
                             codes: dict[int, uuid.UUID], when: datetime) -> None:
        """One thing that happened, with the people it happened to.

        Retrieval finds an episode by participant before it considers what the text looks
        like, which is why the participants are the part that has to be right - a summary
        that resembles the question is worth nothing if it is filed under the wrong
        people. The raw event it came from rides along as the evidence.
        """
        p = c.payload
        await self._eps.add(Episode(
            group_id=group_id,
            summary=p["summary"].strip(),
            started_at=when,
            ended_at=when,
            # The extractor does not score importance yet, so this is a constant in
            # practice and episode ranking degrades to recency - acceptable. Written so
            # an explicit 0.0 is not coerced the day a real score arrives.
            importance=c.confidence if c.confidence is not None else 0.5,
            confidence=c.confidence if c.confidence is not None else 0.5,
            participants=tuple(
                Participant(entity_id=codes[x]) for x in dict.fromkeys(p["participants"])
            ),
            event_ids=(c.source_event_id,) if c.source_event_id else (),
        ))

    async def _write_group_fact(self, c: Candidate, group_id: int,
                                when: datetime) -> None:
        """A fact whose subject is the group itself.

        A term's object_key is the word being defined, so redefining a word supersedes
        the old meaning while a second word is a second row. The topic has no key: a
        group has one, and a new one supersedes it.
        """
        p = c.payload
        subject = await self._ids.group_entity(group_id)
        if p["kind"] == GROUP_TERM:
            predicate, key, value = GROUP_TERM, p["term"].strip(), p["meaning"].strip()
            # One word, one row, whatever the capitalisation or width: supersession
            # matches object_key by equality, so "YYDS" and "yyds" would otherwise
            # be two definitions aging separately. Fold to the spelling already on
            # file - the display keeps the form the group established first.
            for f in await self._mem.current_facts(group_id, [subject]):
                if (f.predicate == GROUP_TERM and f.object_key
                        and normalize(f.object_key) == normalize(key)):
                    key = f.object_key
                    break
        else:
            predicate, key, value = GROUP_TOPIC, None, p["topic"].strip()
        await self._mem.supersede(
            Fact(
                subject_entity_id=subject,
                predicate=predicate,
                object_key=key,
                object_value=value,
                group_id=group_id,
                memory_type=MemoryType.GROUP,
                # Earned, not guessed: what one supporting event is worth. Rises as
                # later batches confirm it - see MemoryRepository.supersede.
                confidence=earned_confidence(1),
            ),
            [FactEvidence(c.source_event_id)] if c.source_event_id else [],
            when=when,
        )

    async def _write_fact(self, c: Candidate, group_id: int,
                          entity_id: uuid.UUID, when: datetime) -> None:
        pred = c.payload["predicate"]
        obj = c.payload["object"].strip()

        # Saying somebody has gone off a thing retracts their liking it. The two are
        # separate rows about the same object, so nothing else would ever reconcile them
        # and both would end up in the prompt.
        if (opposite := opposites().get(pred)) is not None:
            for f in await self._mem.current_facts(group_id, [entity_id]):
                if f.predicate == opposite and f.object_key == obj:
                    await self._mem.retract(f.id)

        await self._mem.supersede(
            Fact(
                subject_entity_id=entity_id,
                predicate=pred,
                # The key is what makes a multi-valued predicate multi-valued: each
                # object is its own row under the one-current-fact index. Single-valued
                # predicates leave it empty, so a new value supersedes the old.
                object_key=obj if pred in multi_valued() else None,
                object_value=obj,
                group_id=group_id,
                memory_type=_fact_kind(pred),
                confidence=earned_confidence(1),
            ),
            [FactEvidence(c.source_event_id)] if c.source_event_id else [],
            when=when,
        )
